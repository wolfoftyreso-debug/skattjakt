-- Durable job queue, analysis state machine, model cost accounting, rate
-- limiting and retention (sections 13, 14, 46, 65, 67, 68).

-- ---------------------------------------------------------------------------
-- Job queue
-- ---------------------------------------------------------------------------
--
-- Deliberately NOT under row-level security, for the same reason as api_tokens
-- in 0002: a queue is scanned across tenants by definition, and a worker has to
-- see the next job before it knows which tenant it belongs to. Applying RLS
-- here would require the worker to hold a BYPASSRLS role, which is strictly
-- worse — that role would also bypass isolation on the tables that hold the
-- customer's economy.
--
-- The trade is made safe by what the table is allowed to contain: identifiers,
-- state, timing and a correlation id. No amounts, no document text, no names.
-- The `payload` column is intentionally absent; a job carries a subject id and
-- the worker reads the subject through a tenant-scoped transaction, so the only
-- path to customer data still runs through RLS.

CREATE TABLE jobs (
    id              UUID PRIMARY KEY,
    kind            TEXT NOT NULL CHECK (kind IN ('analysis', 'extraction', 'retention')),
    company_id      UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    subject_id      UUID NOT NULL,
    idempotency_key TEXT NOT NULL,
    state           TEXT NOT NULL CHECK (state IN (
                        'queued', 'running', 'retrying',
                        'succeeded', 'failed', 'cancelled', 'dead_lettered')),
    attempt         INT  NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    max_attempts    INT  NOT NULL CHECK (max_attempts > 0),
    run_after       TIMESTAMPTZ NOT NULL DEFAULT now(),
    leased_until    TIMESTAMPTZ,
    leased_by       TEXT,
    correlation_id  UUID NOT NULL,
    traceparent     TEXT,
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- A lease without a holder, or a holder without a deadline, is a job that
    -- can never be reaped. The database refuses to store one.
    CONSTRAINT lease_is_whole CHECK (
        (leased_until IS NULL AND leased_by IS NULL)
        OR (leased_until IS NOT NULL AND leased_by IS NOT NULL)
    ),
    -- Only a running job may hold a lease.
    CONSTRAINT lease_only_while_running CHECK (
        state = 'running' OR leased_until IS NULL
    )
);

-- Idempotency (section 13). Unique per tenant and kind, so a retried request
-- returns the existing job instead of starting a second one. Scoped by company
-- so one tenant's key cannot collide with, or probe for, another's.
CREATE UNIQUE INDEX jobs_idempotency ON jobs (company_id, kind, idempotency_key);

-- The claim query: oldest claimable job of a kind.
CREATE INDEX jobs_claimable ON jobs (kind, run_after) WHERE state = 'queued';
-- The reaper: running jobs whose lease has expired.
CREATE INDEX jobs_leases ON jobs (leased_until) WHERE state = 'running';
CREATE INDEX jobs_subject ON jobs (subject_id);
CREATE INDEX jobs_company ON jobs (company_id);

-- Every move, recorded in the same transaction as the move itself (section 14).
-- Append-only for the application, like audit_events: a state history that can
-- be rewritten is not a history.
CREATE TABLE job_transitions (
    id             BIGSERIAL PRIMARY KEY,
    job_id         UUID NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    from_state     TEXT NOT NULL,
    to_state       TEXT NOT NULL,
    event          TEXT NOT NULL,
    attempt        INT  NOT NULL,
    detail         TEXT,
    correlation_id UUID NOT NULL,
    at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX job_transitions_job ON job_transitions (job_id, at);

-- The dead letter queue (section 13). A separate table rather than a state
-- flag, because a dead-lettered job needs things a job row does not have: who
-- acknowledged it, when, and what they decided.
CREATE TABLE dead_letters (
    job_id          UUID PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL,
    company_id      UUID NOT NULL,
    subject_id      UUID NOT NULL,
    attempts        INT  NOT NULL,
    last_error      TEXT,
    correlation_id  UUID NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    acknowledged_at TIMESTAMPTZ,
    acknowledged_by TEXT,
    resolution      TEXT
);

CREATE INDEX dead_letters_open ON dead_letters (created_at) WHERE acknowledged_at IS NULL;

-- ---------------------------------------------------------------------------
-- Model cost accounting (sections 46, 68, 69)
-- ---------------------------------------------------------------------------
--
-- Costs are integers in micro-öre. Öre alone is too coarse — a cheap call can
-- cost a fraction of an öre and would round to zero, so a thousand of them
-- would cost nothing at all. Micro-öre keeps the sum honest and keeps the
-- arithmetic away from floating point, for the same reason Money does.

ALTER TABLE model_runs
    ADD COLUMN requested_model  TEXT,
    ADD COLUMN served_by_model  TEXT,
    ADD COLUMN was_fallback     BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN cost_micro_ore   BIGINT  NOT NULL DEFAULT 0 CHECK (cost_micro_ore >= 0),
    ADD COLUMN correlation_id   UUID;

CREATE INDEX model_runs_fallbacks ON model_runs (created_at) WHERE was_fallback;

-- Per-analysis spend, so the budget check is a single row read rather than an
-- aggregate over model_runs on every call.
CREATE TABLE analysis_budgets (
    analysis_id     UUID PRIMARY KEY REFERENCES analysis_jobs(id) ON DELETE CASCADE,
    company_id      UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    limit_micro_ore BIGINT NOT NULL CHECK (limit_micro_ore > 0),
    spent_micro_ore BIGINT NOT NULL DEFAULT 0 CHECK (spent_micro_ore >= 0),
    calls           INT    NOT NULL DEFAULT 0,
    exceeded_at     TIMESTAMPTZ,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Rate limiting (section 67)
-- ---------------------------------------------------------------------------
--
-- A fixed window counter in the database rather than in process memory. The API
-- runs several replicas; an in-memory limiter would multiply every quota by the
-- replica count, which is the same as not having one.

CREATE TABLE rate_limit_counters (
    company_id   UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    bucket       TEXT NOT NULL,
    window_start TIMESTAMPTZ NOT NULL,
    count        INT NOT NULL DEFAULT 0,
    PRIMARY KEY (company_id, bucket, window_start)
);

CREATE INDEX rate_limit_sweep ON rate_limit_counters (window_start);

-- ---------------------------------------------------------------------------
-- Retention and deletion (section 65)
-- ---------------------------------------------------------------------------

CREATE TABLE retention_policies (
    company_id        UUID PRIMARY KEY REFERENCES companies(id) ON DELETE CASCADE,
    document_days     INT NOT NULL DEFAULT 730 CHECK (document_days > 0),
    analysis_days     INT NOT NULL DEFAULT 730 CHECK (analysis_days > 0),
    -- The audit trail outlives the data it describes on purpose: it holds
    -- identifiers and outcomes, not the customer's economy, and it is the only
    -- record of what was deleted and when.
    audit_days        INT NOT NULL DEFAULT 3650 CHECK (audit_days > 0),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A deletion request the customer made (section 65). Recorded before anything
-- is removed, so an interrupted deletion can be resumed rather than half-done.
CREATE TABLE deletion_requests (
    id            UUID PRIMARY KEY,
    company_id    UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    scope         TEXT NOT NULL CHECK (scope IN ('document', 'analysis', 'company')),
    subject_id    UUID,
    requested_by  TEXT NOT NULL,
    requested_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- What was actually removed, filled in as each store confirms.
    db_done_at    TIMESTAMPTZ,
    blobs_done_at TIMESTAMPTZ,
    completed_at  TIMESTAMPTZ,
    error         TEXT
);

CREATE INDEX deletion_requests_open ON deletion_requests (requested_at) WHERE completed_at IS NULL;

-- ---------------------------------------------------------------------------
-- Rule change governance (section 53)
-- ---------------------------------------------------------------------------
--
-- A rule set version is promoted to production by inserting a row here, and
-- only by a reviewer who is not the proposer. The constraint is in the
-- database because a workflow enforced only by process is a workflow that gets
-- skipped at 17:55 on a Friday.

CREATE TABLE rule_set_approvals (
    rule_set_version TEXT PRIMARY KEY,
    proposed_by      TEXT NOT NULL,
    proposed_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- What changes, and which past analyses it would have changed. Filled from
    -- the evidence graph before approval.
    change_summary   TEXT NOT NULL,
    affected_analyses INT NOT NULL DEFAULT 0,
    reviewed_by      TEXT,
    reviewed_at      TIMESTAMPTZ,
    approved         BOOLEAN,
    review_note      TEXT,

    CONSTRAINT reviewer_is_not_the_proposer CHECK (
        reviewed_by IS NULL OR reviewed_by <> proposed_by
    ),
    CONSTRAINT a_decision_names_its_reviewer CHECK (
        approved IS NULL OR (reviewed_by IS NOT NULL AND reviewed_at IS NOT NULL)
    )
);

-- ---------------------------------------------------------------------------
-- Grants and policies
-- ---------------------------------------------------------------------------

GRANT SELECT, INSERT, UPDATE, DELETE ON jobs, rate_limit_counters, analysis_budgets,
    retention_policies, deletion_requests TO skattjakt_app;
GRANT SELECT, INSERT ON job_transitions, rule_set_approvals TO skattjakt_app;
GRANT SELECT, INSERT, UPDATE ON dead_letters TO skattjakt_app;
GRANT USAGE, SELECT ON SEQUENCE job_transitions_id_seq TO skattjakt_app;

-- Append-only, like audit_events.
REVOKE UPDATE, DELETE ON job_transitions FROM skattjakt_app;
-- A rule approval is a record of a decision. It is not editable after the fact;
-- a changed mind is a new version.
REVOKE UPDATE, DELETE ON rule_set_approvals FROM skattjakt_app;
-- A dead letter is acknowledged, never deleted.
REVOKE DELETE ON dead_letters FROM skattjakt_app;

-- The tenant-scoped tables added here join the same RLS regime as the rest.
DO $$
DECLARE
    t TEXT;
    tenant_tables TEXT[] := ARRAY[
        'analysis_budgets', 'rate_limit_counters', 'retention_policies', 'deletion_requests'
    ];
BEGIN
    FOREACH t IN ARRAY tenant_tables LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I USING (company_id = current_company_id()) '
            'WITH CHECK (company_id = current_company_id())', t);
    END LOOP;
END
$$;

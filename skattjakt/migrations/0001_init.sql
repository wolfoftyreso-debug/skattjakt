-- Skattjakt initial schema.
--
-- Tenant isolation is enforced by Postgres row-level security, not by
-- application code remembering to add a WHERE clause. The application connects
-- as a non-superuser role that is subject to RLS, and sets
-- `skattjakt.company_id` for the duration of each transaction; every policy
-- keys off that setting. A query that forgets its tenant filter returns
-- nothing rather than another company's financial statements.

CREATE EXTENSION IF NOT EXISTS "pgcrypto";

-- ---------------------------------------------------------------------------
-- Tenants and people
-- ---------------------------------------------------------------------------

CREATE TABLE companies (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name              TEXT        NOT NULL CHECK (length(trim(name)) > 0),
    -- Ten digits, no separator. Validated in the domain layer, constrained here.
    org_number        CHAR(10)    NOT NULL UNIQUE CHECK (org_number ~ '^[0-9]{10}$'),
    fiscal_year_start DATE        NOT NULL,
    fiscal_year_end   DATE        NOT NULL,
    profile           JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT fiscal_year_ordered CHECK (fiscal_year_end > fiscal_year_start)
);

CREATE TABLE users (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email      TEXT        NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE company_members (
    company_id UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    user_id    UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role       TEXT        NOT NULL CHECK (role IN ('owner', 'member', 'advisor')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (company_id, user_id)
);

-- ---------------------------------------------------------------------------
-- Documents
-- ---------------------------------------------------------------------------

CREATE TABLE documents (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id        UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    kind              TEXT        NOT NULL,
    original_filename TEXT        NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX documents_company_idx ON documents (company_id, created_at DESC);

-- Immutable. A re-upload creates a new version; nothing here is ever updated.
CREATE TABLE document_versions (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    document_id    UUID        NOT NULL REFERENCES documents (id) ON DELETE CASCADE,
    company_id     UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    version        INTEGER     NOT NULL CHECK (version >= 1),
    mime_type      TEXT        NOT NULL,
    byte_size      BIGINT      NOT NULL CHECK (byte_size > 0),
    sha256         CHAR(64)    NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    storage_key    TEXT        NOT NULL,
    page_count     INTEGER,
    accounts_state TEXT        NOT NULL DEFAULT 'unknown',
    uploaded_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (document_id, version)
);

CREATE INDEX document_versions_company_idx ON document_versions (company_id);

-- ---------------------------------------------------------------------------
-- Financial facts
-- ---------------------------------------------------------------------------

CREATE TABLE financial_facts (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id            UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    document_version_id   UUID        NOT NULL REFERENCES document_versions (id) ON DELETE CASCADE,
    period_start          DATE        NOT NULL,
    period_end            DATE        NOT NULL,
    kind                  TEXT        NOT NULL,
    -- Integer öre. Never a float: an economic estimate must not carry a
    -- rounding artefact.
    value_ore             BIGINT      NOT NULL,
    currency              CHAR(3)     NOT NULL DEFAULT 'SEK',
    account               TEXT,
    source_page           INTEGER,
    source_text           TEXT,
    extraction_confidence REAL        NOT NULL CHECK (extraction_confidence BETWEEN 0 AND 1),
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX financial_facts_company_period_idx
    ON financial_facts (company_id, period_end, kind);

-- ---------------------------------------------------------------------------
-- Analyses
-- ---------------------------------------------------------------------------

CREATE TABLE analysis_jobs (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id           UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    -- The exact document versions pinned at creation, so a later upload cannot
    -- change what a finished analysis was based on.
    document_version_ids UUID[]      NOT NULL,
    status               TEXT        NOT NULL DEFAULT 'pending'
                                     CHECK (status IN ('pending', 'running', 'succeeded', 'failed')),
    stage                TEXT        NOT NULL DEFAULT 'queued',
    rule_set_version     TEXT        NOT NULL,
    result               JSONB,
    error                TEXT,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at           TIMESTAMPTZ,
    finished_at          TIMESTAMPTZ
);

CREATE INDEX analysis_jobs_company_idx ON analysis_jobs (company_id, created_at DESC);

-- What was asked of a model and what came back. Never the reasoning trace:
-- section 21 permits conclusions, evidence, calculations, a rationale summary
-- and validation state, and nothing else.
CREATE TABLE model_runs (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    analysis_id          UUID        NOT NULL REFERENCES analysis_jobs (id) ON DELETE CASCADE,
    company_id           UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    provider             TEXT        NOT NULL,
    model                TEXT        NOT NULL,
    task                 TEXT        NOT NULL,
    prompt_version       TEXT        NOT NULL,
    document_version_ids UUID[]      NOT NULL DEFAULT '{}',
    status               TEXT        NOT NULL CHECK (status IN ('succeeded', 'refused', 'failed')),
    input_tokens         INTEGER     NOT NULL DEFAULT 0,
    output_tokens        INTEGER     NOT NULL DEFAULT 0,
    latency_ms           BIGINT      NOT NULL DEFAULT 0,
    output               JSONB,
    error                TEXT,
    started_at           TIMESTAMPTZ NOT NULL,
    finished_at          TIMESTAMPTZ NOT NULL
);

CREATE INDEX model_runs_analysis_idx ON model_runs (analysis_id);

-- ---------------------------------------------------------------------------
-- Rules, opportunities and evidence
-- ---------------------------------------------------------------------------

-- A record of which rule set version was in force, so an old analysis can be
-- read against the rules it actually ran under.
CREATE TABLE rule_versions (
    version     TEXT PRIMARY KEY,
    jurisdiction TEXT       NOT NULL,
    rule_count  INTEGER     NOT NULL,
    definition  JSONB       NOT NULL,
    loaded_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE opportunities (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    analysis_id         UUID        NOT NULL REFERENCES analysis_jobs (id) ON DELETE CASCADE,
    company_id          UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    category            TEXT        NOT NULL,
    status              TEXT        NOT NULL
                                    CHECK (status IN ('identified', 'investigate', 'verify', 'warning', 'rejected')),
    title               TEXT        NOT NULL,
    rationale           TEXT        NOT NULL,
    impact_low_ore      BIGINT      NOT NULL DEFAULT 0,
    impact_high_ore     BIGINT      NOT NULL DEFAULT 0,
    currency            CHAR(3)     NOT NULL DEFAULT 'SEK',
    confidence_score    SMALLINT    NOT NULL CHECK (confidence_score BETWEEN 0 AND 100),
    confidence_band     TEXT        NOT NULL,
    risk                TEXT        NOT NULL,
    effort              TEXT        NOT NULL,
    urgency             TEXT        NOT NULL,
    priority_score      REAL        NOT NULL,
    priority_band       TEXT        NOT NULL,
    rule_ids            TEXT[]      NOT NULL DEFAULT '{}',
    missing_information TEXT[]      NOT NULL DEFAULT '{}',
    recommended_action  TEXT        NOT NULL,
    rejection_reason    TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- An interval, never a point: the low bound may not exceed the high one.
    CONSTRAINT impact_ordered CHECK (impact_high_ore >= impact_low_ore)
);

CREATE INDEX opportunities_analysis_idx ON opportunities (analysis_id, priority_score DESC);

CREATE TABLE opportunity_evidence (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    opportunity_id UUID        NOT NULL REFERENCES opportunities (id) ON DELETE CASCADE,
    company_id     UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    position       INTEGER     NOT NULL,
    item           JSONB       NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (opportunity_id, position)
);

CREATE TABLE calculations (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    opportunity_id UUID        NOT NULL REFERENCES opportunities (id) ON DELETE CASCADE,
    company_id     UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    method         TEXT        NOT NULL,
    -- The expression as stored with the rule, so the arithmetic can be re-run
    -- verbatim years later.
    expression     JSONB       NOT NULL,
    inputs         JSONB       NOT NULL,
    result_low_ore  BIGINT     NOT NULL,
    result_high_ore BIGINT     NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Audit
-- ---------------------------------------------------------------------------

-- Append-only. The revoke below removes UPDATE and DELETE from the application
-- role, so a critical step cannot be rewritten after the fact.
CREATE TABLE audit_events (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id  UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    actor       TEXT        NOT NULL,
    event_type  TEXT        NOT NULL,
    subject_id  UUID,
    -- Metadata only. Document contents and extracted amounts never go here.
    detail      JSONB       NOT NULL DEFAULT '{}'::jsonb,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX audit_events_company_idx ON audit_events (company_id, occurred_at DESC);

-- ---------------------------------------------------------------------------
-- Row-level security
-- ---------------------------------------------------------------------------

-- The application role. Not a superuser and not the table owner, so RLS
-- applies to it — a BYPASSRLS or owning role would silently see everything.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'skattjakt_app') THEN
        CREATE ROLE skattjakt_app NOLOGIN;
    END IF;
END
$$;

GRANT USAGE ON SCHEMA public TO skattjakt_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO skattjakt_app;

-- Audit events are append-only for the application.
REVOKE UPDATE, DELETE ON audit_events FROM skattjakt_app;

-- Reads the tenant for the current transaction. Returns NULL when unset, and
-- every policy below then matches nothing: no tenant, no rows.
CREATE OR REPLACE FUNCTION current_company_id() RETURNS UUID
LANGUAGE sql STABLE AS $$
    SELECT NULLIF(current_setting('skattjakt.company_id', true), '')::uuid
$$;

DO $$
DECLARE
    t TEXT;
    tenant_tables TEXT[] := ARRAY[
        'documents', 'document_versions', 'financial_facts', 'analysis_jobs',
        'model_runs', 'opportunities', 'opportunity_evidence', 'calculations',
        'audit_events', 'company_members'
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

-- The companies table keys on its own id rather than a company_id column.
ALTER TABLE companies ENABLE ROW LEVEL SECURITY;
ALTER TABLE companies FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON companies
    USING (id = current_company_id())
    WITH CHECK (id = current_company_id());

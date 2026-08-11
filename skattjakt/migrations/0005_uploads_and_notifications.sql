-- Direct uploads and notification delivery.
--
-- Both exist because of the mobile clients that do not exist yet, and both
-- would be breaking changes to add afterwards.
--
-- Uploads
-- =======
--
-- Today a document is posted as JSON through the API. That is fine from a
-- desktop browser on an office connection and is the wrong shape for a phone:
-- a 30 MB scanned annual report crosses a mobile network, is buffered whole in
-- an API pod, and any drop in the last second means starting again. It also
-- couples upload throughput to API pod memory, so one customer photographing
-- their accounts can push a replica into an OOM kill.
--
-- The answer is the standard one: the client asks for a ticket, uploads
-- straight to object storage, and tells the API when it is done. The API never
-- sees the bytes. This table is the ticket, and it exists so an upload is a
-- tracked object with an owner, an expiry and a state — not an opaque URL
-- floating around with no record that it was issued.
--
-- Notifications
-- =============
--
-- An analysis takes minutes. A browser can poll; a phone that is in someone's
-- pocket cannot, and asking it to would drain the battery for a result that
-- arrives once. So the result has to be able to reach out, which means an
-- outbox — because a notification that is sent inside the transaction that
-- produced it is a notification that gets sent twice on a retry and lost on a
-- rollback.

-- ---------------------------------------------------------------------------
-- Upload tickets
-- ---------------------------------------------------------------------------

CREATE TABLE upload_tickets (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id      UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,

    -- Who asked. Nullable because a company token has no person behind it, and
    -- recording a fabricated user would be worse than recording none.
    requested_by    UUID        REFERENCES users (id) ON DELETE SET NULL,

    -- Where the bytes will land. Derived from identifiers, never from the
    -- client's filename — the same rule that makes path traversal structurally
    -- impossible for the proxied upload path.
    storage_key     TEXT        NOT NULL UNIQUE,

    -- What the client said it would send. Checked against reality on
    -- completion: a ticket for a 2 MB text file that produces a 40 MB blob was
    -- not used for what it was issued for.
    declared_name   TEXT        NOT NULL CHECK (length(declared_name) <= 400),
    declared_type   TEXT        NOT NULL,
    declared_size   BIGINT      NOT NULL CHECK (declared_size > 0),

    -- What actually arrived.
    observed_size   BIGINT      CHECK (observed_size IS NULL OR observed_size >= 0),
    observed_sha256 TEXT,

    state           TEXT        NOT NULL DEFAULT 'issued'
                    CHECK (state IN ('issued', 'completed', 'rejected', 'expired')),
    rejected_reason TEXT        CHECK (rejected_reason IN (
        'size_mismatch', 'hash_mismatch', 'too_large', 'unsupported_type', 'not_found'
    )),

    -- Short. A ticket is a bearer capability to write into the customer's
    -- storage; it should live long enough to upload a large file on a poor
    -- connection and not one minute longer.
    expires_at      TIMESTAMPTZ NOT NULL,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at    TIMESTAMPTZ,

    -- The document version this ticket produced, once it is accepted.
    document_version_id UUID REFERENCES document_versions (id) ON DELETE SET NULL,

    CONSTRAINT a_rejection_states_its_reason CHECK (
        state <> 'rejected' OR rejected_reason IS NOT NULL
    ),
    CONSTRAINT a_completed_ticket_has_a_document CHECK (
        state <> 'completed' OR document_version_id IS NOT NULL
    )
);

CREATE INDEX upload_tickets_pending ON upload_tickets (expires_at) WHERE state = 'issued';
CREATE INDEX upload_tickets_by_company ON upload_tickets (company_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Notifications
-- ---------------------------------------------------------------------------
--
-- An outbox, not a send-at-the-time call.
--
-- The transaction that finishes an analysis writes a row here and commits. A
-- separate worker delivers it. That separation is what makes the guarantee
-- statable: a notification is written exactly when the thing it describes
-- becomes true, and delivery retries without re-running the analysis.
--
-- Sending inline would mean either sending inside the transaction — so a
-- rollback leaves a customer told about a result that does not exist — or
-- sending after it, so a crash in between loses the notification with no record
-- that it was owed.

CREATE TABLE notifications (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id   UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,

    -- Who it is for. Nullable: some notifications are for the company rather
    -- than for a person, and are delivered to every member.
    user_id      UUID        REFERENCES users (id) ON DELETE CASCADE,

    kind         TEXT        NOT NULL CHECK (kind IN (
        'analysis_completed', 'analysis_failed', 'document_processed',
        'member_invited', 'security_alert'
    )),

    -- The payload is deliberately identifiers and a kind, never prose and never
    -- an amount.
    --
    -- A push notification's body is rendered on the delivery side from the kind
    -- and the recipient's language, and it is shown on a lock screen — which is
    -- the one display surface the customer does not control. "Din analys är
    -- klar" belongs there; "Vi hittade 186 000 kr" does not, and a payload that
    -- can carry the second is a payload that eventually does.
    subject_id   UUID,
    subject_kind TEXT        CHECK (subject_kind IN ('analysis', 'document', 'company')),

    -- Delivery. `channels` is what was asked for; `delivered_channels` is what
    -- succeeded, so a partial delivery is visible rather than being either a
    -- silent success or a retry that sends the email twice.
    channels           TEXT[] NOT NULL DEFAULT '{}',
    delivered_channels TEXT[] NOT NULL DEFAULT '{}',

    state        TEXT        NOT NULL DEFAULT 'pending'
                 CHECK (state IN ('pending', 'delivering', 'delivered', 'failed', 'suppressed')),
    attempt      INTEGER     NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    max_attempts INTEGER     NOT NULL DEFAULT 5,
    run_after    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_error   TEXT,

    -- Idempotency, scoped per tenant like every other key in this schema.
    -- Without it, a worker that retries an analysis after a lost lease notifies
    -- the customer twice about one result.
    dedupe_key   TEXT        NOT NULL,

    correlation_id UUID      NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT delivered_is_a_subset_of_requested CHECK (
        delivered_channels <@ channels
    )
);

CREATE UNIQUE INDEX notifications_dedupe ON notifications (company_id, kind, dedupe_key);
CREATE INDEX notifications_due ON notifications (run_after)
    WHERE state IN ('pending', 'delivering');

-- ---------------------------------------------------------------------------
-- Notification preferences
-- ---------------------------------------------------------------------------
--
-- Per user, per kind. Defaults live in code rather than as rows, so a user who
-- has never touched their settings has no rows here at all — which is the
-- difference between "has not chosen" and "chose the defaults", and it matters
-- the day a default changes.

CREATE TABLE notification_preferences (
    user_id    UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind       TEXT        NOT NULL,
    channels   TEXT[]      NOT NULL DEFAULT '{}',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, kind)
);

-- ---------------------------------------------------------------------------
-- Row-level security
-- ---------------------------------------------------------------------------
--
-- `upload_tickets` and `notifications` both carry a company_id and both hold
-- things one tenant must not see about another, so they take the same policy
-- as every other tenant table. `notification_preferences` keys on the user
-- rather than the company — a person's preferences follow them across the
-- companies they belong to — so it is guarded by the user scoping in the
-- queries, the same way `devices` is.

DO $$
DECLARE
    t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY['upload_tickets', 'notifications'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I USING (company_id = current_company_id()) '
            'WITH CHECK (company_id = current_company_id())', t);
    END LOOP;
END
$$;

GRANT SELECT, INSERT, UPDATE, DELETE
    ON upload_tickets, notifications, notification_preferences TO skattjakt_app;

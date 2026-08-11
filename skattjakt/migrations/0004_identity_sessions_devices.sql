-- Identity, sessions and devices.
--
-- Why this migration exists
-- =========================
--
-- Until now a caller authenticated with one long-lived bearer token per
-- company. That works for a single web client and fails for everything the
-- product is meant to become:
--
--   * It has no user. `audit_events.actor` can only ever say "someone holding
--     the company token". For a product where a business owner and an external
--     accountant both work on the same accounts, "who did this" is a question
--     the system must be able to answer.
--   * It cannot be revoked narrowly. A lost phone means rotating the company's
--     only credential, which signs out everyone on every device.
--   * It never expires. A permanent credential sitting in a phone's keychain is
--     a permanent credential.
--   * It carries no device. There is nowhere to put a push token, and no way to
--     show a customer "you are signed in on these three devices".
--
-- Every one of those becomes a breaking change across three clients if it is
-- deferred until after a mobile client ships. So the session model is built now,
-- even though only the web client exists.
--
-- What this migration deliberately does NOT decide
-- ================================================
--
-- How a human proves who they are. `user_credentials.method` is the seam. A
-- password verifier is implemented because it is self-contained and testable;
-- Swedish BankID and a platform OIDC provider are the methods this product
-- actually wants, and both slot in by adding a method and a verifier without
-- touching sessions, devices, the API contract or the clients.
--
-- The company token is NOT removed. It keeps working, unchanged, so nothing
-- that exists today breaks. It is now recorded as what it is: a machine
-- credential, not a person.

-- ---------------------------------------------------------------------------
-- Credentials
-- ---------------------------------------------------------------------------

CREATE TABLE user_credentials (
    user_id          UUID        PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,

    -- The seam described above. 'password' is verified here; 'federated' means
    -- an external identity provider vouched for the subject and this row holds
    -- no secret at all.
    method           TEXT        NOT NULL CHECK (method IN ('password', 'federated')),

    -- Argon2id. Nullable because a federated credential has no hash to store,
    -- and storing an empty string instead would make "has no password" and
    -- "has an unusable password" the same state.
    password_hash    TEXT,

    -- For 'federated': which provider, and the subject it asserted. Unique
    -- together so two users cannot claim the same external identity.
    provider         TEXT,
    external_subject TEXT,

    -- Set when a credential must be replaced before the account is usable —
    -- an operator-forced reset, or a password that predates a policy change.
    must_change      BOOLEAN     NOT NULL DEFAULT FALSE,

    -- Failed attempts since the last success, and how long the account is
    -- locked. In the database rather than in memory because several API
    -- replicas serve the same account, and an in-process counter would
    -- multiply the allowed attempts by the replica count.
    failed_attempts  INTEGER     NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    locked_until     TIMESTAMPTZ,

    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT password_credentials_carry_a_hash CHECK (
        method <> 'password' OR password_hash IS NOT NULL
    ),
    CONSTRAINT federated_credentials_carry_a_subject CHECK (
        method <> 'federated' OR (provider IS NOT NULL AND external_subject IS NOT NULL)
    ),
    -- A federated credential must not also carry a password: two ways to
    -- authenticate as one user is two attack surfaces, and the weaker one wins.
    CONSTRAINT federated_credentials_hold_no_password CHECK (
        method <> 'federated' OR password_hash IS NULL
    )
);

CREATE UNIQUE INDEX user_credentials_external_identity
    ON user_credentials (provider, external_subject)
    WHERE provider IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Devices
-- ---------------------------------------------------------------------------
--
-- A device is where a session lives. It exists as its own row rather than as
-- columns on `sessions` because a device outlives any one session: a customer
-- signs out and back in on the same phone, and the push token, the display name
-- and the "you are signed in here" list should survive that.

CREATE TABLE devices (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,

    platform      TEXT        NOT NULL CHECK (platform IN ('web', 'ios', 'android')),

    -- What the customer sees in their device list. Supplied by the client and
    -- therefore untrusted: length-bounded here so it cannot be used to bloat a
    -- row, and it is CONFIDENTIAL rather than PUBLIC because "Anna's iPhone"
    -- names a person.
    display_name  TEXT        NOT NULL CHECK (length(display_name) <= 120),

    -- A stable per-installation identifier the client generates. Used to
    -- recognise a returning installation instead of creating a new device row
    -- on every sign-in. Not a security boundary: it is client-supplied and is
    -- scoped to the user, so it can only ever collide with that user's own
    -- devices.
    install_id    TEXT        NOT NULL CHECK (length(install_id) <= 200),

    -- Push delivery. Nullable because a device may decline notifications, and
    -- a web device may have no push channel at all.
    push_token    TEXT,
    push_provider TEXT        CHECK (push_provider IN ('apns', 'fcm', 'web_push')),

    -- Set when the provider tells us a token is dead, so the notification
    -- sender can skip it without deleting the device the customer can see.
    push_failed_at TIMESTAMPTZ,

    last_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT a_push_token_names_its_provider CHECK (
        push_token IS NULL OR push_provider IS NOT NULL
    )
);

CREATE UNIQUE INDEX devices_per_user_install ON devices (user_id, install_id);
CREATE INDEX devices_pushable ON devices (user_id)
    WHERE push_token IS NOT NULL AND push_failed_at IS NULL;

-- ---------------------------------------------------------------------------
-- Sessions
-- ---------------------------------------------------------------------------
--
-- One row per signed-in device. Holds both tokens as SHA-256, never in the
-- clear: a database dump must not yield a usable credential, which is the same
-- rule `api_tokens` already follows.
--
-- Opaque tokens with a database lookup, not JWTs. The reasoning is in
-- SKATTJAKT_ENGINEERING_DECISIONS.md: every authenticated request already opens
-- a transaction to set the tenant for row-level security, so the lookup is free
-- in practice — and a JWT that can be revoked needs a denylist, which is the
-- same lookup with a signing key to manage and a family of algorithm-confusion
-- bugs attached.

CREATE TABLE sessions (
    id                 UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id            UUID        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    device_id          UUID        NOT NULL REFERENCES devices (id) ON DELETE CASCADE,

    -- The tenant this session is currently acting in. A user may belong to
    -- several companies — an accountant with many clients is the normal case,
    -- not an edge case — and the session names which one is active, so a
    -- request never has to be trusted to say.
    company_id         UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,

    access_token_hash  TEXT        NOT NULL UNIQUE,
    access_expires_at  TIMESTAMPTZ NOT NULL,

    refresh_token_hash TEXT        NOT NULL UNIQUE,
    refresh_expires_at TIMESTAMPTZ NOT NULL,

    -- Refresh rotation with reuse detection.
    --
    -- Every refresh mints a new token in the same family and increments the
    -- generation. Presenting a refresh token whose generation is behind the
    -- family's current one means two parties hold tokens from one family —
    -- which happens when one was stolen. The whole family is then revoked,
    -- signing out the thief and the customer, and the customer signing in again
    -- is the intended outcome. Silently issuing a new token to both is how a
    -- stolen refresh token becomes permanent access.
    family_id          UUID        NOT NULL,
    generation         INTEGER     NOT NULL DEFAULT 0 CHECK (generation >= 0),

    client_kind        TEXT        NOT NULL CHECK (client_kind IN ('web', 'ios', 'android')),

    -- For the customer's own "signed in from" list. The address is stored as a
    -- SHA-256 rather than in the clear: it is enough to tell two locations
    -- apart, and it is not personal data sitting in a table that many queries
    -- touch.
    ip_hash            TEXT,

    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at       TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Set when this generation is rotated away.
    --
    -- A rotation inserts the next generation and stamps this on the old row
    -- rather than overwriting the old row's hashes. That difference is the
    -- whole of reuse detection: if the old hash were overwritten, a replayed
    -- refresh token would simply not be found, and "stolen token" would be
    -- indistinguishable from "token that never existed" — so the family would
    -- never be torn down. Kept distinct from `revoked_at` because superseded
    -- and revoked call for different answers.
    superseded_at      TIMESTAMPTZ,

    revoked_at         TIMESTAMPTZ,
    revoked_reason     TEXT        CHECK (revoked_reason IN (
        'signed_out', 'refresh_reuse', 'password_changed', 'operator', 'expired'
    )),

    CONSTRAINT a_revocation_states_its_reason CHECK (
        (revoked_at IS NULL AND revoked_reason IS NULL)
        OR (revoked_at IS NOT NULL AND revoked_reason IS NOT NULL)
    ),
    -- An access token that outlives its refresh token is a session that cannot
    -- be extended but can still be used, which is a state nothing should be
    -- able to reach.
    CONSTRAINT refresh_outlives_access CHECK (refresh_expires_at >= access_expires_at)
);

CREATE INDEX sessions_by_user   ON sessions (user_id)
    WHERE revoked_at IS NULL AND superseded_at IS NULL;
CREATE INDEX sessions_by_family ON sessions (family_id);
CREATE INDEX sessions_expiring  ON sessions (refresh_expires_at) WHERE revoked_at IS NULL;

-- Sessions sit outside row-level security, for the same reason `api_tokens`
-- does and with the same bound on the risk: authentication happens before a
-- tenant is known, so a policy keyed on the tenant cannot be applied to the
-- lookup that establishes it. The table holds hashes, timestamps and
-- identifiers — no amounts, no document text, no name beyond a device label.

-- ---------------------------------------------------------------------------
-- Company membership gains an invitation trail
-- ---------------------------------------------------------------------------
--
-- `company_members` already existed and was never used. Making it real needs
-- one thing it lacked: a record of how a person came to have access, because
-- "who let this accountant into our accounts, and when" is a question a
-- customer is entitled to ask.

ALTER TABLE company_members
    ADD COLUMN invited_by UUID REFERENCES users (id) ON DELETE SET NULL,
    ADD COLUMN invited_at TIMESTAMPTZ,
    ADD COLUMN accepted_at TIMESTAMPTZ;

-- ---------------------------------------------------------------------------
-- The company token is now labelled for what it is
-- ---------------------------------------------------------------------------

ALTER TABLE api_tokens
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'company'
        CHECK (kind IN ('company', 'service'));

COMMENT ON COLUMN api_tokens.kind IS
    'A machine credential, never a person. Person access goes through sessions, '
    'which carry a user, a device and an expiry. Kept because integrations and '
    'the existing web client depend on it.';

-- ---------------------------------------------------------------------------
-- Grants
-- ---------------------------------------------------------------------------

GRANT SELECT, INSERT, UPDATE, DELETE ON user_credentials, devices, sessions TO skattjakt_app;

-- The application may not rewrite how someone came to have access, for the same
-- reason it may not rewrite the audit trail.
REVOKE UPDATE (invited_by, invited_at) ON company_members FROM skattjakt_app;

-- ---------------------------------------------------------------------------
-- Membership lookup for authentication
-- ---------------------------------------------------------------------------
--
-- Authentication has a chicken-and-egg problem with row-level security:
-- `company_members` is a tenant table, and the policy needs a tenant, and
-- finding the tenant is precisely what the query is for.
--
-- There are three ways out and only one of them is acceptable:
--
--   1. Give the application a BYPASSRLS role. That role would also bypass
--      isolation on every table holding the customer's economy, so a single
--      forgotten WHERE clause anywhere becomes a cross-tenant leak. No.
--   2. Take `company_members` out of RLS. It holds who has access to which
--      company — exactly the kind of thing isolation is for. No.
--   3. Expose one narrow SECURITY DEFINER function that answers only the
--      question authentication asks, and nothing else.
--
-- These are (3). They return a user's own memberships and nothing further: no
-- amounts, no documents, no other user's rows. `search_path` is pinned so a
-- caller cannot shadow `company_members` with a table of their own — the
-- classic SECURITY DEFINER escalation.

CREATE FUNCTION membership_role(p_user_id UUID, p_company_id UUID)
RETURNS TEXT
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
    SELECT role FROM company_members
    WHERE user_id = p_user_id AND company_id = p_company_id AND accepted_at IS NOT NULL
$$;

CREATE FUNCTION memberships_for_user(p_user_id UUID)
RETURNS TABLE (company_id UUID, role TEXT, created_at TIMESTAMPTZ)
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
    SELECT m.company_id, m.role, m.created_at
    FROM company_members m
    WHERE m.user_id = p_user_id AND m.accepted_at IS NOT NULL
    ORDER BY CASE m.role WHEN 'owner' THEN 0 WHEN 'member' THEN 1 ELSE 2 END, m.created_at
$$;

-- Callable by the application, and by nobody else. A SECURITY DEFINER function
-- granted to PUBLIC is a hole with a nice interface.
REVOKE ALL ON FUNCTION membership_role(UUID, UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION memberships_for_user(UUID) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION membership_role(UUID, UUID) TO skattjakt_app;
GRANT EXECUTE ON FUNCTION memberships_for_user(UUID) TO skattjakt_app;

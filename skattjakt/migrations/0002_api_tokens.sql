-- API tokens.
--
-- A token identifies exactly one company. The tenant therefore comes from the
-- credential, not from anything the client sends: a company id in a request
-- body can never widen the caller's scope, because it is not what the scope is
-- derived from.
--
-- Only the SHA-256 of a token is stored. A database dump does not yield working
-- credentials, and a lost token is reissued rather than recovered.

CREATE TABLE api_tokens (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id  UUID        NOT NULL REFERENCES companies (id) ON DELETE CASCADE,
    -- Lowercase hex SHA-256 of the bearer token.
    token_hash  CHAR(64)    NOT NULL UNIQUE CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    -- Shown in listings so a token can be identified without revealing it.
    label       TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ,
    revoked_at  TIMESTAMPTZ
);

CREATE INDEX api_tokens_company_idx ON api_tokens (company_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON api_tokens TO skattjakt_app;

-- Deliberately NOT under row-level security.
--
-- Token lookup is what *establishes* the tenant, so it necessarily runs before
-- one is set. An RLS policy here would make every lookup return zero rows and
-- authentication could never succeed. The compensating control is that the
-- lookup is by `token_hash`, which is unique and unguessable, and that it is
-- the only query the application makes before setting the tenant.

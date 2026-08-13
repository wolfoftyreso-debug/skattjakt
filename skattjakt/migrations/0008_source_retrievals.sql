-- Where the source registry's retrieval state actually lives.
--
-- The problem this table exists for
-- =================================
--
-- `rules/se-ruleset.json` names 24 primary sources and, until now, also carried
-- how far each had been checked. That file is embedded into the binary with
-- `include_str!`, which is right for the rules themselves — a deployed build
-- must carry the exact rule set it was tested against — and wrong for the
-- retrieval state, for a reason that only shows up in operation:
--
--   A verification that can only be recorded at build time can only be as
--   current as the last build. The law does not change on our release
--   schedule. A rule set built in March and running in November has a
--   verification from March and no way to say so.
--
-- So the *claim* stays in the binary (which paragraph, what it is assumed to
-- say, which strings must appear) and the *check* moves here, where a running
-- worker can write it and every analysis can read it. The registry in the
-- binary supplies the default for a source nobody has checked yet.
--
-- Not tenant data
-- ===============
--
-- The law is the same for every company, so there is no company_id and no
-- row-level security policy — the same shape as `rule_versions`.
--
-- The deployment runs one database role, `skattjakt_app`, shared by the API and
-- the worker, so the grant below cannot separate "may read the state" from "may
-- write it". What actually stops a bad write is the two CHECK constraints: a
-- row claiming `verified` or `mismatch` must carry the hash and timestamp of
-- what was read. That is weaker than a role split — a caller who can execute
-- arbitrary SQL could fabricate both — and it is stated here rather than
-- overclaimed. Splitting the role would mean a second credential, a second
-- connection string and a manifest change, and is the right next step if the
-- retrieval state is ever worth attacking.

CREATE TABLE source_retrievals (
    source_id       TEXT PRIMARY KEY,

    -- The ladder, weakest first. Deliberately the same four values as
    -- `SourceState` in the rule engine, checked here so a bad write is a
    -- failed statement rather than a state nothing knows how to read.
    state           TEXT        NOT NULL
                                CHECK (state IN ('unretrieved', 'unreachable', 'mismatch', 'verified')),

    -- When the document was last actually read. Distinct from last_checked_at:
    -- a fetch that fails is a fact about the network, and must not be allowed
    -- to look like a fresh reading of the law.
    retrieved_at    TIMESTAMPTZ,

    -- SHA-256 of the text that was read. Present for `verified` and for
    -- `mismatch` — a contradiction is a fact about one version of a document,
    -- and without the hash nobody can tell later whether the page changed
    -- again between the mismatch and somebody looking at it.
    sha256          TEXT,

    -- Why, for a state that needs explaining.
    note            TEXT,

    -- When a check was last attempted, successfully or not. This is what the
    -- sweep interval is measured against.
    last_checked_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Consecutive failed attempts. A source unreachable once is a bad minute;
    -- unreachable for a week is a broken URL or a moved document, and the two
    -- deserve different attention.
    failure_streak  INT         NOT NULL DEFAULT 0 CHECK (failure_streak >= 0),

    -- A verified state is granted by a retrieval that recorded what it read,
    -- and by nothing else. The rule engine refuses to load a registry that
    -- breaks this; the database refuses to store it. Both, because this is the
    -- invariant the whole registry rests on: without it the state is a word
    -- somebody typed.
    CONSTRAINT verified_carries_its_evidence CHECK (
        state <> 'verified' OR (sha256 IS NOT NULL AND retrieved_at IS NOT NULL)
    ),

    -- Likewise a contradiction: it is only meaningful if we know what was read.
    CONSTRAINT mismatch_carries_its_evidence CHECK (
        state <> 'mismatch' OR (sha256 IS NOT NULL AND retrieved_at IS NOT NULL AND note IS NOT NULL)
    )
);

-- The sweep asks one question — "what is the oldest check?" — every time it
-- wakes up.
CREATE INDEX source_retrievals_by_check ON source_retrievals (last_checked_at);

-- No DELETE. A retrieval record is history: superseding it is an UPDATE, and
-- there is no operation in the product that means "this check never happened".
GRANT SELECT, INSERT, UPDATE ON source_retrievals TO skattjakt_app;

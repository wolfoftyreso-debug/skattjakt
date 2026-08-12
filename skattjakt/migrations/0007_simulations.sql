-- Monte Carlo simulations.
--
-- A general probability and simulation layer, usable by anything in the
-- product that has to reason about an uncertain number rather than a known
-- one. It is deliberately *not* wired into findings, and the reason is worth
-- writing down where the schema is, because it is the one place a future
-- change would be tempting and wrong.
--
--   Skattjakt's domain rule is that money is a `MoneyRange` and that no type
--   in the product can express a single-figure tax saving. A simulated P50 is
--   a single figure. It is a well-founded one — it comes from a model somebody
--   defined, with a seed and an audit trail — but it is a statement about the
--   model, not about the company's accounts, and it is not evidence. So
--   simulations live in their own tables, are read through their own
--   endpoints, and no column here is referenced by `opportunities`. A finding
--   still needs a document value and a cited rule; a simulation is neither.
--
-- Versioning
-- ==========
--
-- Section 4 requires inputs to be version-managed, and section 12 requires a
-- result to name exactly what produced it. Both are served by the same shape:
-- `simulation_versions` is append-only and holds the whole specification as
-- stored JSON plus its SHA-256; a run references a version rather than a
-- simulation, so editing a model can never change what an old result meant.
--
-- The normalised `simulation_inputs` and `simulation_outputs` tables are
-- derived from that JSON, not an alternative source of truth. They exist so
-- the inputs of a version can be queried, listed and shown without parsing
-- JSON in the application — and `spec` is what the engine actually runs.
--
-- Storage of results
-- ==================
--
-- Section 16 asks for raw simulation data, statistical aggregates and
-- visualisation data to be kept apart. Only the last two are stored. The raw
-- samples — up to ten million doubles per output — are held in memory for the
-- length of a run and discarded: they are reproducible from the seed at any
-- time, and persisting eighty megabytes per run to avoid a two-second
-- recomputation is the wrong trade in every direction.

-- ---------------------------------------------------------------------------
-- The model
-- ---------------------------------------------------------------------------

CREATE TABLE simulations (
    id              UUID PRIMARY KEY,
    company_id      UUID        NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    name            TEXT        NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    description     TEXT,
    -- Which version is current. A pointer rather than a flag on the version
    -- row, so "the current one" is a single value that cannot be true twice.
    current_version INT         NOT NULL DEFAULT 1 CHECK (current_version >= 1),
    created_by      UUID        REFERENCES users(id) ON DELETE SET NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX simulations_by_company ON simulations (company_id, created_at DESC, id DESC);

COMMENT ON TABLE simulations IS
    'A Monte Carlo model. Holds no numbers itself — every version of the model '
    'is a row in simulation_versions, and every result is a row in '
    'simulation_runs.';

CREATE TABLE simulation_versions (
    id            UUID PRIMARY KEY,
    company_id    UUID        NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    simulation_id UUID        NOT NULL REFERENCES simulations(id) ON DELETE CASCADE,
    version       INT         NOT NULL CHECK (version >= 1),
    -- The specification exactly as the engine will run it. The authority.
    spec          JSONB       NOT NULL,
    -- SHA-256 of the canonical JSON. A run stores this too, so "the same
    -- inputs" is something that can be checked rather than assumed.
    spec_hash     TEXT        NOT NULL CHECK (spec_hash ~ '^[0-9a-f]{64}$'),
    -- Why this version exists. Section 13's WHY, at the point the model
    -- changed rather than at the point it was run.
    note          TEXT,
    created_by    UUID        REFERENCES users(id) ON DELETE SET NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (simulation_id, version)
);

CREATE INDEX simulation_versions_by_simulation
    ON simulation_versions (simulation_id, version DESC);

CREATE TABLE simulation_inputs (
    id                UUID PRIMARY KEY,
    company_id        UUID    NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    version_id        UUID    NOT NULL REFERENCES simulation_versions(id) ON DELETE CASCADE,
    position          INT     NOT NULL,
    -- The identifier used in expressions.
    input_id          TEXT    NOT NULL,
    name              TEXT    NOT NULL,
    distribution_kind TEXT    NOT NULL,
    parameters        JSONB   NOT NULL,
    unit              TEXT,
    -- Where the number came from. Nullable, because a model can be sketched
    -- before it is sourced — and shown as unsourced in the interface, which is
    -- the point of storing it separately rather than burying it in the spec.
    source            TEXT,
    confidence        TEXT    CHECK (confidence IN ('low', 'medium', 'high')),
    description       TEXT,
    constraints       JSONB,
    -- Derived from the distribution, stored for listing and sorting only. The
    -- distribution in `parameters` remains the only thing anyone samples from.
    mean              DOUBLE PRECISION NOT NULL,
    std_dev           DOUBLE PRECISION NOT NULL,
    UNIQUE (version_id, input_id)
);

CREATE TABLE simulation_outputs (
    id                 UUID PRIMARY KEY,
    company_id         UUID    NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    version_id         UUID    NOT NULL REFERENCES simulation_versions(id) ON DELETE CASCADE,
    position           INT     NOT NULL,
    output_id          TEXT    NOT NULL,
    name               TEXT    NOT NULL,
    expression         TEXT    NOT NULL,
    unit               TEXT,
    description        TEXT,
    target             DOUBLE PRECISION,
    target_direction   TEXT    NOT NULL DEFAULT 'at_least'
                               CHECK (target_direction IN ('at_least', 'at_most')),
    critical_threshold DOUBLE PRECISION,
    UNIQUE (version_id, output_id)
);

-- ---------------------------------------------------------------------------
-- Runs
-- ---------------------------------------------------------------------------

CREATE TABLE simulation_runs (
    id                    UUID PRIMARY KEY,
    company_id            UUID        NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    simulation_id         UUID        NOT NULL REFERENCES simulations(id) ON DELETE CASCADE,
    -- The version, not the simulation. Editing a model must not change what an
    -- old result meant.
    version_id            UUID        NOT NULL REFERENCES simulation_versions(id) ON DELETE CASCADE,

    state                 TEXT        NOT NULL DEFAULT 'queued'
                                      CHECK (state IN ('queued', 'running', 'succeeded',
                                                       'failed', 'cancelled')),

    -- The seed is a full 64-bit value and Postgres has no unsigned integer, so
    -- it is stored as its decimal string. A BIGINT would work by bit-casting,
    -- at the cost of an audit record where half the seeds display as negative
    -- numbers that do not match what the API returned. The API transports it
    -- as a string for the related reason that a JSON number loses precision
    -- above 2^53 — and a seed that survives storage but not transport is not a
    -- reproducible seed.
    seed                  TEXT        NOT NULL CHECK (seed ~ '^[0-9]{1,20}$'),
    iterations            INT         NOT NULL CHECK (iterations > 0),
    completed_iterations  INT         NOT NULL DEFAULT 0 CHECK (completed_iterations >= 0),

    engine_version        TEXT        NOT NULL,
    spec_hash             TEXT        NOT NULL,
    -- Where it ran. Section 3 asks the system to decide between local and
    -- server-side execution by size; this records which way it went, so a slow
    -- request and a queued job are distinguishable afterwards.
    execution             TEXT        NOT NULL CHECK (execution IN ('inline', 'queued')),

    -- Section 13's WHY. Free text, supplied by whoever asked for the run.
    reason                TEXT,
    requested_by          UUID        REFERENCES users(id) ON DELETE SET NULL,
    requested_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at            TIMESTAMPTZ,
    finished_at           TIMESTAMPTZ,
    duration_ms           BIGINT,
    iterations_per_second DOUBLE PRECISION,

    -- What the engine noticed: rejected draws, clamped samples, convergence
    -- warnings. Stored with the run because it qualifies the result.
    quality               JSONB,
    error                 TEXT,

    -- Cancellation is a request, not an act: the worker holding the run polls
    -- this and stops at the end of its current batch. A row the API could
    -- simply mark 'cancelled' would leave a worker running a job nobody is
    -- waiting for.
    cancel_requested      BOOLEAN     NOT NULL DEFAULT FALSE,
    cancel_requested_by   UUID        REFERENCES users(id) ON DELETE SET NULL
);

CREATE INDEX simulation_runs_by_simulation
    ON simulation_runs (simulation_id, requested_at DESC, id DESC);
CREATE INDEX simulation_runs_by_company
    ON simulation_runs (company_id, requested_at DESC, id DESC);
-- The worker's cancellation poll and the "is anything still running" query.
CREATE INDEX simulation_runs_in_flight
    ON simulation_runs (state) WHERE state IN ('queued', 'running');

-- ---------------------------------------------------------------------------
-- Results
-- ---------------------------------------------------------------------------
--
-- Columns rather than a JSON blob for the statistics, because these are the
-- numbers anyone will ever want to query across runs: which scenarios reached
-- their target, how a P90 moved between two versions of a model. A JSONB blob
-- makes every one of those questions a full scan and a cast.

CREATE TABLE simulation_statistics (
    run_id                       UUID   NOT NULL REFERENCES simulation_runs(id) ON DELETE CASCADE,
    company_id                   UUID   NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    output_id                    TEXT   NOT NULL,
    name                         TEXT   NOT NULL,
    unit                         TEXT,
    sample_count                 BIGINT NOT NULL,

    mean                         DOUBLE PRECISION NOT NULL,
    median                       DOUBLE PRECISION NOT NULL,
    minimum                      DOUBLE PRECISION NOT NULL,
    maximum                      DOUBLE PRECISION NOT NULL,
    std_dev                      DOUBLE PRECISION NOT NULL,
    variance                     DOUBLE PRECISION NOT NULL,

    p5                           DOUBLE PRECISION NOT NULL,
    p10                          DOUBLE PRECISION NOT NULL,
    p25                          DOUBLE PRECISION NOT NULL,
    p50                          DOUBLE PRECISION NOT NULL,
    p75                          DOUBLE PRECISION NOT NULL,
    p90                          DOUBLE PRECISION NOT NULL,
    p95                          DOUBLE PRECISION NOT NULL,
    p99                          DOUBLE PRECISION NOT NULL,

    -- Nullable throughout, and that is the design. NULL means "no target was
    -- set", which is a different statement from "the probability is zero", and
    -- a NOT NULL DEFAULT 0 here would turn the first into the second on every
    -- screen that reads it.
    probability_of_target        DOUBLE PRECISION
        CHECK (probability_of_target BETWEEN 0 AND 1),
    probability_of_loss          DOUBLE PRECISION NOT NULL
        CHECK (probability_of_loss BETWEEN 0 AND 1),
    probability_below_threshold  DOUBLE PRECISION
        CHECK (probability_below_threshold BETWEEN 0 AND 1),
    probability_above_threshold  DOUBLE PRECISION
        CHECK (probability_above_threshold BETWEEN 0 AND 1),

    -- The 95% interval for the *mean*: this run's sampling error, not the
    -- spread of the outcomes. The column names say which, because the two are
    -- indistinguishable once they are numbers on a screen.
    mean_ci_low                  DOUBLE PRECISION,
    mean_ci_high                 DOUBLE PRECISION,
    relative_standard_error      DOUBLE PRECISION,

    PRIMARY KEY (run_id, output_id)
);

CREATE TABLE simulation_sensitivity (
    run_id                UUID    NOT NULL REFERENCES simulation_runs(id) ON DELETE CASCADE,
    company_id            UUID    NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    output_id             TEXT    NOT NULL,
    input_id              TEXT    NOT NULL,
    input_name            TEXT    NOT NULL,
    -- NULL where no correlation exists — a constant input, or an input this
    -- output never reads. Zero would read as "measured, and it does not
    -- matter"; NULL says "this run cannot tell you".
    correlation           DOUBLE PRECISION CHECK (correlation BETWEEN -1 AND 1),
    rank_correlation      DOUBLE PRECISION CHECK (rank_correlation BETWEEN -1 AND 1),
    variance_contribution DOUBLE PRECISION NOT NULL
        CHECK (variance_contribution BETWEEN 0 AND 1),
    influence_rank        INT     NOT NULL,
    referenced            BOOLEAN NOT NULL,
    sample_size           INT     NOT NULL,
    PRIMARY KEY (run_id, output_id, input_id)
);

CREATE TABLE simulation_convergence (
    run_id     UUID    NOT NULL REFERENCES simulation_runs(id) ON DELETE CASCADE,
    company_id UUID    NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    output_id  TEXT    NOT NULL,
    iterations INT     NOT NULL,
    mean       DOUBLE PRECISION NOT NULL,
    median     DOUBLE PRECISION NOT NULL,
    p10        DOUBLE PRECISION NOT NULL,
    p90        DOUBLE PRECISION NOT NULL,
    -- Repeated on every checkpoint row so the series and its verdict come back
    -- in one query. Denormalised deliberately, and it cannot drift: both are
    -- written once, in one transaction, and never updated.
    stable     BOOLEAN NOT NULL,
    largest_relative_change DOUBLE PRECISION,
    warning    TEXT,
    PRIMARY KEY (run_id, output_id, iterations)
);

-- The visualisation payload of section 16: histogram, density curve and a
-- sampled cumulative distribution. About four kilobytes per output regardless
-- of whether the run was a thousand iterations or ten million, which is the
-- whole reason it is separated from the samples it was computed from.
CREATE TABLE simulation_shapes (
    run_id     UUID  NOT NULL REFERENCES simulation_runs(id) ON DELETE CASCADE,
    company_id UUID  NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    output_id  TEXT  NOT NULL,
    payload    JSONB NOT NULL,
    PRIMARY KEY (run_id, output_id)
);

-- ---------------------------------------------------------------------------
-- The job queue learns a new kind of work
-- ---------------------------------------------------------------------------

ALTER TABLE jobs DROP CONSTRAINT jobs_kind_check;
ALTER TABLE jobs ADD CONSTRAINT jobs_kind_check
    CHECK (kind IN ('analysis', 'extraction', 'retention', 'simulation'));

-- ---------------------------------------------------------------------------
-- Row-level security
-- ---------------------------------------------------------------------------
--
-- Every table here carries a company_id and takes the same policy as the rest
-- of the tenant tables. Unlike `jobs` and `notifications`, none of these is a
-- queue: the worker learns which company it is acting for from the job row
-- before it touches any of them, and then opens an ordinary tenant
-- transaction. There is no cross-tenant scan to accommodate, so there is no
-- reason to weaken anything.

DO $$
DECLARE
    t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'simulations',
        'simulation_versions',
        'simulation_inputs',
        'simulation_outputs',
        'simulation_runs',
        'simulation_statistics',
        'simulation_sensitivity',
        'simulation_convergence',
        'simulation_shapes'
    ] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I USING (company_id = current_company_id()) '
            'WITH CHECK (company_id = current_company_id())', t);
    END LOOP;
END
$$;

GRANT SELECT, INSERT, UPDATE, DELETE ON
    simulations,
    simulation_versions,
    simulation_inputs,
    simulation_outputs,
    simulation_runs,
    simulation_statistics,
    simulation_sensitivity,
    simulation_convergence,
    simulation_shapes
TO skattjakt_app;

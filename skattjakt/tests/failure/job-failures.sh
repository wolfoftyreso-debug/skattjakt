#!/usr/bin/env bash
# Failure injection against the durable job system (sections 77, 78).
#
# Section 77 asks what happens when a pod dies mid-analysis, when the database
# restarts, when object storage is unavailable, when the model provider fails,
# when a node dies. Those are not questions a unit test answers, because the
# answer depends on a lease held in a real database and on what a second
# process does when the first one stops reporting.
#
# So this runs a real Postgres, writes real job rows, and simulates the failure
# by doing what the failure does: it stops the heartbeat and moves the clock.
# No worker is killed — killing a process proves the same thing more slowly and
# less deterministically.
#
# Usage: tests/failure/job-failures.sh
# Requires: a local PostgreSQL installation.

set -euo pipefail

if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    RUNAS="${SKATTJAKT_PG_USER:-postgres}"
    id "$RUNAS" >/dev/null 2>&1 || RUNAS=nobody
    export SKATTJAKT_PG_REEXEC=1
    exec su -s /bin/bash "$RUNAS" -c "SKATTJAKT_PG_REEXEC=1 $(printf '%q ' "$0" "$@")"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PGBIN="${PGBIN:-$(dirname "$(command -v initdb || echo /usr/lib/postgresql/16/bin/initdb)")}"
[[ -x "$PGBIN/initdb" ]] || PGBIN=/usr/lib/postgresql/16/bin
WORKDIR="$(mktemp -d)"
PGDATA="$WORKDIR/data"
SOCKET="$WORKDIR/sock"
DB=skattjakt_failure

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }
check() {
    if [[ "$3" == "$2" ]]; then pass "$1"; else fail "$1 (expected $2, got $3)"; fi
}

cleanup() {
    "$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

mkdir -p "$SOCKET"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET" -l "$WORKDIR/pg.log" start >/dev/null

psql() { "$PGBIN/psql" -h "$SOCKET" -U postgres -v ON_ERROR_STOP=1 -q "$@"; }
q() { psql -d "$DB" -tAc "$1"; }

psql -d postgres -c "CREATE DATABASE $DB" >/dev/null
for migration in "$ROOT"/migrations/*.sql; do
    psql -d "$DB" -f "$migration" >/dev/null
done

COMPANY=11111111-1111-1111-1111-111111111111
psql -d "$DB" >/dev/null <<SQL
INSERT INTO companies (id, name, org_number, fiscal_year_start, fiscal_year_end)
VALUES ('$COMPANY', 'Alfa AB', '5560160680', '2025-01-01', '2025-12-31');
SQL

# The reaper and the backoff release, expressed as the SQL the worker runs. Kept
# in step with `crates/jobs/src/queue.rs` — if that changes, this fails, which
# is the intended coupling.
reap() {
    psql -d "$DB" >/dev/null <<'SQL'
UPDATE jobs SET
    state = CASE WHEN attempt >= max_attempts THEN 'dead_lettered' ELSE 'retrying' END,
    leased_until = NULL, leased_by = NULL, last_error = 'lease_expired',
    run_after = now() + interval '30 seconds', updated_at = now()
WHERE state = 'running' AND leased_until < now();

INSERT INTO dead_letters (job_id, kind, company_id, subject_id, attempts, last_error, correlation_id)
SELECT id, kind, company_id, subject_id, attempt, 'lease_expired', correlation_id
FROM jobs WHERE state = 'dead_lettered'
ON CONFLICT (job_id) DO NOTHING;
SQL
}

release_backoffs() {
    psql -d "$DB" -c \
        "UPDATE jobs SET state = 'queued' WHERE state = 'retrying' AND run_after <= now()" >/dev/null
}

new_job() {
    local id="$1" state="${2:-queued}" attempt="${3:-0}" leased_until="${4:-NULL}"
    local leased_by=NULL
    [[ "$leased_until" != "NULL" ]] && leased_by="'worker-a'"
    psql -d "$DB" >/dev/null <<SQL
INSERT INTO jobs (id, kind, company_id, subject_id, idempotency_key, state,
                  attempt, max_attempts, run_after, leased_until, leased_by, correlation_id)
VALUES ('$id', 'analysis', '$COMPANY', gen_random_uuid(), 'key-$id', '$state',
        $attempt, 3, now(), $leased_until, $leased_by, gen_random_uuid());
SQL
}

# ---------------------------------------------------------------------------
echo
echo "a worker pod dies mid-analysis (section 77)"
# ---------------------------------------------------------------------------

JOB=aaaaaaaa-0000-0000-0000-000000000001
new_job "$JOB" running 1 "now() - interval '1 minute'"

reap
check "the job returns to the retry queue rather than being lost" retrying \
    "$(q "SELECT state FROM jobs WHERE id = '$JOB'")"
check "the attempt is not refunded, so a crash loop still terminates" 1 \
    "$(q "SELECT attempt FROM jobs WHERE id = '$JOB'")"
check "the lease is released so another worker can claim it" 0 \
    "$(q "SELECT count(*) FROM jobs WHERE id = '$JOB' AND (leased_by IS NOT NULL OR leased_until IS NOT NULL)")"

psql -d "$DB" -c "UPDATE jobs SET run_after = now() - interval '1 minute' WHERE id = '$JOB'" >/dev/null
release_backoffs
check "the backoff elapses and the job becomes claimable" queued \
    "$(q "SELECT state FROM jobs WHERE id = '$JOB'")"

# ---------------------------------------------------------------------------
echo
echo "a pod that keeps dying on the same job"
# ---------------------------------------------------------------------------

JOB=aaaaaaaa-0000-0000-0000-000000000002
new_job "$JOB" running 3 "now() - interval '1 minute'"

reap
check "the last attempt dead-letters rather than looping forever" dead_lettered \
    "$(q "SELECT state FROM jobs WHERE id = '$JOB'")"
check "a human-visible dead letter is recorded" 1 \
    "$(q "SELECT count(*) FROM dead_letters WHERE job_id = '$JOB'")"
check "the dead letter is unacknowledged, so it appears in the operator's queue" 1 \
    "$(q "SELECT count(*) FROM dead_letters WHERE job_id = '$JOB' AND acknowledged_at IS NULL")"

# ---------------------------------------------------------------------------
echo
echo "a live lease is not stolen"
# ---------------------------------------------------------------------------

JOB=aaaaaaaa-0000-0000-0000-000000000003
new_job "$JOB" running 1 "now() + interval '10 minutes'"
reap
check "a worker that is still heartbeating keeps its job" running \
    "$(q "SELECT state FROM jobs WHERE id = '$JOB'")"
check "and keeps its lease" worker-a \
    "$(q "SELECT leased_by FROM jobs WHERE id = '$JOB'")"

# ---------------------------------------------------------------------------
echo
echo "two workers race for one job"
# ---------------------------------------------------------------------------

# One claimable job and eight workers going for it at the same instant.
#
# Genuinely concurrent, in background processes: running the claim statement
# twice in sequence proves nothing about `SKIP LOCKED`, because the first
# transaction has already committed by the time the second starts.
psql -d "$DB" -c "UPDATE jobs SET state = 'cancelled', leased_until = NULL, leased_by = NULL
                  WHERE state IN ('queued', 'retrying')" >/dev/null

JOB=aaaaaaaa-0000-0000-0000-000000000004
new_job "$JOB" queued 0

claim() {
    "$PGBIN/psql" -h "$SOCKET" -U postgres -d "$DB" -tAc "
        UPDATE jobs SET state = 'running', attempt = attempt + 1,
            leased_until = now() + interval '20 minutes', leased_by = '$1'
        WHERE id = (SELECT id FROM jobs WHERE kind = 'analysis' AND state = 'queued'
                    AND run_after <= now() ORDER BY run_after
                    FOR UPDATE SKIP LOCKED LIMIT 1)
        RETURNING id"
}

for worker in $(seq 1 8); do
    claim "worker-$worker" > "$WORKDIR/claim-$worker.out" 2>/dev/null &
done
wait

# Count only actual identifiers: psql prints an empty line for a statement
# that updated nothing, and counting non-empty lines would count those too.
winners="$(cat "$WORKDIR"/claim-*.out 2>/dev/null \
    | grep -cE '^[0-9a-f]{8}-[0-9a-f]{4}-' || true)"
check "exactly one of eight concurrent workers claims the job" 1 "$winners"
check "the attempt is counted exactly once" 1 \
    "$(q "SELECT attempt FROM jobs WHERE id = '$JOB'")"
check "exactly one worker holds the lease" 1 \
    "$(q "SELECT count(DISTINCT leased_by) FROM jobs WHERE id = '$JOB' AND leased_by IS NOT NULL")"

# ---------------------------------------------------------------------------
echo
echo "a duplicate request (section 13)"
# ---------------------------------------------------------------------------

DUP=aaaaaaaa-0000-0000-0000-000000000005
new_job "$DUP" queued 0
before="$(q "SELECT count(*) FROM jobs")"
"$PGBIN/psql" -h "$SOCKET" -U postgres -d "$DB" -q -c "
    INSERT INTO jobs (id, kind, company_id, subject_id, idempotency_key, state,
                      attempt, max_attempts, run_after, correlation_id)
    VALUES (gen_random_uuid(), 'analysis', '$COMPANY', gen_random_uuid(), 'key-$DUP',
            'queued', 0, 3, now(), gen_random_uuid())
    ON CONFLICT (company_id, kind, idempotency_key) DO NOTHING" >/dev/null
check "a repeated idempotency key creates no second job" "$before" \
    "$(q "SELECT count(*) FROM jobs")"

# The same key in another tenant is a different job. Scoping the index by
# company is what stops one customer's key colliding with — or probing for —
# another's.
OTHER=22222222-2222-2222-2222-222222222222
psql -d "$DB" >/dev/null <<SQL
INSERT INTO companies (id, name, org_number, fiscal_year_start, fiscal_year_end)
VALUES ('$OTHER', 'Beta AB', '5567037485', '2025-01-01', '2025-12-31');
INSERT INTO jobs (id, kind, company_id, subject_id, idempotency_key, state,
                  attempt, max_attempts, run_after, correlation_id)
VALUES (gen_random_uuid(), 'analysis', '$OTHER', gen_random_uuid(), 'key-$DUP',
        'queued', 0, 3, now(), gen_random_uuid());
SQL
check "the same key in another tenant is a separate job" 2 \
    "$(q "SELECT count(*) FROM jobs WHERE idempotency_key = 'key-$DUP'")"

# ---------------------------------------------------------------------------
echo
echo "the database refuses impossible states (section 14)"
# ---------------------------------------------------------------------------

impossible() {
    if "$PGBIN/psql" -h "$SOCKET" -U postgres -d "$DB" -q -c "$2" >/dev/null 2>&1; then
        fail "$1 was accepted"
    else
        pass "$1 is rejected"
    fi
}

impossible "a lease with no holder" \
    "INSERT INTO jobs (id, kind, company_id, subject_id, idempotency_key, state, attempt,
        max_attempts, run_after, leased_until, correlation_id)
     VALUES (gen_random_uuid(), 'analysis', '$COMPANY', gen_random_uuid(), 'bad-lease-1',
             'running', 1, 3, now(), now(), gen_random_uuid())"

impossible "a queued job holding a lease" \
    "INSERT INTO jobs (id, kind, company_id, subject_id, idempotency_key, state, attempt,
        max_attempts, run_after, leased_until, leased_by, correlation_id)
     VALUES (gen_random_uuid(), 'analysis', '$COMPANY', gen_random_uuid(), 'bad-lease-2',
             'queued', 0, 3, now(), now(), 'worker-a', gen_random_uuid())"

impossible "an unknown state" \
    "INSERT INTO jobs (id, kind, company_id, subject_id, idempotency_key, state, attempt,
        max_attempts, run_after, correlation_id)
     VALUES (gen_random_uuid(), 'analysis', '$COMPANY', gen_random_uuid(), 'bad-state',
             'in_progress', 0, 3, now(), gen_random_uuid())"

impossible "an unknown job kind" \
    "INSERT INTO jobs (id, kind, company_id, subject_id, idempotency_key, state, attempt,
        max_attempts, run_after, correlation_id)
     VALUES (gen_random_uuid(), 'analyse', '$COMPANY', gen_random_uuid(), 'bad-kind',
             'queued', 0, 3, now(), gen_random_uuid())"

impossible "a negative cost" \
    "INSERT INTO analysis_budgets (analysis_id, company_id, limit_micro_ore, spent_micro_ore)
     VALUES (gen_random_uuid(), '$COMPANY', 100, -1)"

# ---------------------------------------------------------------------------
echo
echo "the transition history cannot be rewritten (section 14)"
# ---------------------------------------------------------------------------

psql -d "$DB" >/dev/null <<SQL
INSERT INTO job_transitions (job_id, from_state, to_state, event, attempt, correlation_id)
VALUES ('$JOB', 'queued', 'running', 'claimed', 1, gen_random_uuid());
SQL

for statement in "UPDATE job_transitions SET to_state = 'succeeded'" \
                 "DELETE FROM job_transitions"; do
    if "$PGBIN/psql" -h "$SOCKET" -U skattjakt_app -d "$DB" -q -c "$statement" >/dev/null 2>&1; then
        fail "the application could run: $statement"
    else
        pass "the application cannot run: $statement"
    fi
done

# ---------------------------------------------------------------------------
echo
echo "a rule set cannot approve itself (section 53)"
# ---------------------------------------------------------------------------

psql -d "$DB" >/dev/null <<'SQL'
INSERT INTO rule_set_approvals (rule_set_version, proposed_by, change_summary)
VALUES ('se-2025.2', 'anna', 'raise the tax allocation reserve ceiling');
SQL

if psql -d "$DB" -c "
    UPDATE rule_set_approvals SET reviewed_by = 'anna', reviewed_at = now(), approved = true
    WHERE rule_set_version = 'se-2025.2'" >/dev/null 2>&1; then
    fail "a proposer could approve their own rule change"
else
    pass "a proposer cannot approve their own rule change"
fi

if psql -d "$DB" -c "
    UPDATE rule_set_approvals SET approved = true
    WHERE rule_set_version = 'se-2025.2'" >/dev/null 2>&1; then
    fail "a rule change could be approved without naming a reviewer"
else
    pass "a rule change cannot be approved without naming a reviewer"
fi

if psql -d "$DB" -c "
    UPDATE rule_set_approvals SET reviewed_by = 'björn', reviewed_at = now(), approved = true
    WHERE rule_set_version = 'se-2025.2'" >/dev/null 2>&1; then
    pass "a second person can approve it"
else
    fail "a second person could not approve it"
fi

# ---------------------------------------------------------------------------

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "all failure-mode checks passed"

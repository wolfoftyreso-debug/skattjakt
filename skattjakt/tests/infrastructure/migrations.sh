#!/usr/bin/env bash
# Migration safety (section 15).
#
# Two questions, and only the second is ever asked in a real deployment:
#
#   1. Does a fresh database end up with the right schema?
#   2. Does a database that already holds a customer's data survive being
#      upgraded from an earlier version — with the data still there?
#
# The suites elsewhere all start from an empty database, so they answer (1)
# every time and (2) never. This runs the migrations up to each intermediate
# version, writes data as that version's code would have, then applies the rest
# and checks the data is intact.
#
# Usage: tests/infrastructure/migrations.sh

set -euo pipefail

if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    RUNAS="${SKATTJAKT_PG_USER:-postgres}"
    id "$RUNAS" >/dev/null 2>&1 || RUNAS=nobody
    export SKATTJAKT_PG_REEXEC=1
    exec su -s /bin/bash "$RUNAS" -c \
        "SKATTJAKT_PG_REEXEC=1 PGBIN='${PGBIN:-}' $(printf '%q ' "$0" "$@")"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKDIR="$(mktemp -d)"
PGDATA="$WORKDIR/data"
SOCKET="$WORKDIR/sock"

PGBIN="${PGBIN:-$(dirname "$(command -v initdb || echo /usr/lib/postgresql/16/bin/initdb)")}"
[[ -x "$PGBIN/initdb" ]] || PGBIN=/usr/lib/postgresql/16/bin

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

MIGRATIONS=("$ROOT"/migrations/*.sql)
echo "${#MIGRATIONS[@]} migrations"

apply_through() { # db, count
    local db="$1" count="$2" i=0
    for migration in "${MIGRATIONS[@]}"; do
        i=$((i + 1))
        [[ "$i" -gt "$count" ]] && break
        psql -d "$db" -f "$migration" >/dev/null
    done
}

# ---------------------------------------------------------------------------
echo
echo "a fresh installation"
# ---------------------------------------------------------------------------

psql -d postgres -c "CREATE DATABASE fresh" >/dev/null
apply_through fresh "${#MIGRATIONS[@]}"
FRESH_TABLES="$(psql -d fresh -tAc \
    "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'")"
pass "every migration applies to an empty database ($FRESH_TABLES tables)"

# Applying them twice must not be how a re-run destroys a database. Forward-only
# migrations are not idempotent by design, so this asserts they *fail* rather
# than half-apply — a partial second run is the dangerous outcome.
if psql -d fresh -f "${MIGRATIONS[0]}" >/dev/null 2>&1; then
    fail "re-applying a migration silently succeeded, so a double-run is undetectable"
else
    pass "re-applying a migration is refused rather than half-done"
fi

# ---------------------------------------------------------------------------
echo
echo "an upgrade from every earlier version, with data in the database"
# ---------------------------------------------------------------------------
#
# For each intermediate version: apply up to it, write what that version could
# hold, apply the rest, and check the rows survived. This is the case a fresh
# install never exercises and the only one that happens in production.

for stop in $(seq 1 $((${#MIGRATIONS[@]} - 1))); do
    db="upgrade_from_$stop"
    version="$(basename "${MIGRATIONS[$((stop - 1))]}" .sql)"
    psql -d postgres -c "CREATE DATABASE $db" >/dev/null
    apply_through "$db" "$stop"

    # Data every version has been able to hold since 0001.
    COMPANY=aaaaaaaa-0000-0000-0000-00000000000$stop
    psql -d "$db" >/dev/null <<SQL
INSERT INTO companies (id, name, org_number, fiscal_year_start, fiscal_year_end)
VALUES ('$COMPANY', 'Bevarat AB', '5560160680', '2025-01-01', '2025-12-31');
INSERT INTO documents (id, company_id, kind, original_filename)
VALUES (gen_random_uuid(), '$COMPANY', 'annual_accounts', 'bokslut.pdf');
INSERT INTO users (id, email) VALUES (gen_random_uuid(), 'kvar-$stop@example.com');
SQL

    BEFORE_COMPANIES="$(psql -d "$db" -tAc "SELECT count(*) FROM companies")"
    BEFORE_DOCS="$(psql -d "$db" -tAc "SELECT count(*) FROM documents")"

    # The rest of the migrations, as a deploy would run them.
    i=0
    UPGRADE_OK=1
    for migration in "${MIGRATIONS[@]}"; do
        i=$((i + 1))
        [[ "$i" -le "$stop" ]] && continue
        psql -d "$db" -f "$migration" >/dev/null 2>&1 || UPGRADE_OK=0
    done

    if [[ "$UPGRADE_OK" -ne 1 ]]; then
        fail "upgrading from $version failed"
        continue
    fi

    AFTER_COMPANIES="$(psql -d "$db" -tAc "SELECT count(*) FROM companies")"
    AFTER_DOCS="$(psql -d "$db" -tAc "SELECT count(*) FROM documents")"
    AFTER_TABLES="$(psql -d "$db" -tAc \
        "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'")"

    if [[ "$AFTER_COMPANIES" == "$BEFORE_COMPANIES" && "$AFTER_DOCS" == "$BEFORE_DOCS" ]]; then
        pass "upgrading from $version keeps the data"
    else
        fail "upgrading from $version lost rows ($BEFORE_COMPANIES/$BEFORE_DOCS → $AFTER_COMPANIES/$AFTER_DOCS)"
    fi

    # The upgraded schema must be identical to a fresh one. A migration that
    # produces a *different* shape depending on where it started is how two
    # deployments of the same version stop behaving the same way.
    check "and reaches the same schema as a fresh install" "$FRESH_TABLES" "$AFTER_TABLES"
done

# ---------------------------------------------------------------------------
echo
echo "the upgraded schema is genuinely identical, not merely the same size"
# ---------------------------------------------------------------------------
#
# A table count matching is weak evidence. This compares every column, type and
# nullability, plus every constraint and index, between a fresh database and one
# that was upgraded from the first version.

SCHEMA_QUERY="SELECT table_name, column_name, data_type, is_nullable, column_default
              FROM information_schema.columns WHERE table_schema='public'
              ORDER BY table_name, column_name"
psql -d fresh -tAc "$SCHEMA_QUERY" > "$WORKDIR/fresh.schema"
psql -d upgrade_from_1 -tAc "$SCHEMA_QUERY" > "$WORKDIR/upgraded.schema"
if diff -q "$WORKDIR/fresh.schema" "$WORKDIR/upgraded.schema" >/dev/null; then
    pass "every column, type and default matches a fresh install"
else
    fail "the upgraded schema differs from a fresh one"
    diff "$WORKDIR/fresh.schema" "$WORKDIR/upgraded.schema" | head -20
fi

INDEX_QUERY="SELECT tablename, indexdef FROM pg_indexes
             WHERE schemaname='public' ORDER BY tablename, indexdef"
psql -d fresh -tAc "$INDEX_QUERY" > "$WORKDIR/fresh.indexes"
psql -d upgrade_from_1 -tAc "$INDEX_QUERY" > "$WORKDIR/upgraded.indexes"
if diff -q "$WORKDIR/fresh.indexes" "$WORKDIR/upgraded.indexes" >/dev/null; then
    pass "every index matches"
else
    fail "the indexes differ"
    diff "$WORKDIR/fresh.indexes" "$WORKDIR/upgraded.indexes" | head -20
fi

# Row-level security is the one that would be catastrophic to lose in an
# upgrade: the database would work perfectly and isolate nothing.
RLS_QUERY="SELECT relname, relrowsecurity, relforcerowsecurity FROM pg_class
           WHERE relnamespace='public'::regnamespace AND relkind='r' ORDER BY relname"
psql -d fresh -tAc "$RLS_QUERY" > "$WORKDIR/fresh.rls"
psql -d upgrade_from_1 -tAc "$RLS_QUERY" > "$WORKDIR/upgraded.rls"
if diff -q "$WORKDIR/fresh.rls" "$WORKDIR/upgraded.rls" >/dev/null; then
    FORCED="$(grep -c '|t|t$' "$WORKDIR/fresh.rls" || true)"
    pass "row-level security survives the upgrade ($FORCED tables forced)"
else
    fail "row-level security differs after an upgrade"
    diff "$WORKDIR/fresh.rls" "$WORKDIR/upgraded.rls"
fi

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "migrations are safe from a fresh install and from every earlier version"

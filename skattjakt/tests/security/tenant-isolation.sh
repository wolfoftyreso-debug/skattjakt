#!/usr/bin/env bash
# Proves tenant isolation against a real Postgres cluster.
#
# Tenant isolation is the one security property that must not be taken on
# trust, so this does not mock the database: it starts a cluster, applies the
# migrations, writes two companies' data, and then tries — as the application
# role — to read across the boundary in every way the schema allows.
#
# Usage: scripts/test-tenant-isolation.sh
# Requires: a local PostgreSQL installation (initdb, pg_ctl, psql).

set -euo pipefail

# Postgres refuses to run as root. In containers that start as root — CI images,
# most notably — re-exec as an unprivileged user rather than failing.
if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    RUNAS="${SKATTJAKT_PG_USER:-postgres}"
    id "$RUNAS" >/dev/null 2>&1 || RUNAS=nobody
    export SKATTJAKT_PG_REEXEC=1
    exec su -s /bin/bash "$RUNAS" -c "SKATTJAKT_PG_REEXEC=1 $(printf '%q ' "$0" "$@")"
fi

PGBIN="${PGBIN:-$(dirname "$(command -v initdb || echo /usr/lib/postgresql/16/bin/initdb)")}"
[[ -x "$PGBIN/initdb" ]] || PGBIN=/usr/lib/postgresql/16/bin
WORKDIR="$(mktemp -d)"
PGDATA="$WORKDIR/data"
SOCKET="$WORKDIR/sock"
DB=skattjakt_test
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cleanup() {
    "$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

mkdir -p "$SOCKET"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET" -l "$WORKDIR/log" start >/dev/null

psql() { "$PGBIN/psql" -h "$SOCKET" -U postgres -v ON_ERROR_STOP=1 -q "$@"; }

psql -d postgres -c "CREATE DATABASE $DB" >/dev/null
psql -d "$DB" -f "$ROOT/migrations/0001_init.sql" >/dev/null
echo "migrations applied"

# Seed two tenants as the owner role, which is not subject to the policies.
psql -d "$DB" >/dev/null <<'SQL'
INSERT INTO companies (id, name, org_number, fiscal_year_start, fiscal_year_end) VALUES
  ('11111111-1111-1111-1111-111111111111', 'Alfa AB', '5560160680', '2025-01-01', '2025-12-31'),
  ('22222222-2222-2222-2222-222222222222', 'Beta AB', '5565040465', '2025-01-01', '2025-12-31');

INSERT INTO documents (id, company_id, kind, original_filename) VALUES
  ('aaaaaaaa-0000-0000-0000-000000000001', '11111111-1111-1111-1111-111111111111', 'annual_accounts', 'alfa-bokslut.pdf'),
  ('bbbbbbbb-0000-0000-0000-000000000001', '22222222-2222-2222-2222-222222222222', 'annual_accounts', 'beta-bokslut.pdf');

INSERT INTO document_versions (id, document_id, company_id, version, mime_type, byte_size, sha256, storage_key) VALUES
  ('aaaaaaaa-0000-0000-0000-000000000002', 'aaaaaaaa-0000-0000-0000-000000000001', '11111111-1111-1111-1111-111111111111',
   1, 'application/pdf', 1024, repeat('a', 64), 'companies/1111/doc/v1'),
  ('bbbbbbbb-0000-0000-0000-000000000002', 'bbbbbbbb-0000-0000-0000-000000000001', '22222222-2222-2222-2222-222222222222',
   1, 'application/pdf', 2048, repeat('b', 64), 'companies/2222/doc/v1');

INSERT INTO financial_facts
  (company_id, document_version_id, period_start, period_end, kind, value_ore, extraction_confidence) VALUES
  ('11111111-1111-1111-1111-111111111111', 'aaaaaaaa-0000-0000-0000-000000000002',
   '2025-01-01', '2025-12-31', 'revenue', 1250000000, 0.95),
  ('22222222-2222-2222-2222-222222222222', 'bbbbbbbb-0000-0000-0000-000000000002',
   '2025-01-01', '2025-12-31', 'revenue', 9900000000, 0.95);
SQL
echo "two tenants seeded"

fail() { echo "FAIL: $1" >&2; exit 1; }

# Every query below runs as skattjakt_app, the role the application uses.
as_alfa() {
    "$PGBIN/psql" -h "$SOCKET" -U postgres -d "$DB" -tAq -v ON_ERROR_STOP=1 -c "
        SET ROLE skattjakt_app;
        SET LOCAL skattjakt.company_id = '11111111-1111-1111-1111-111111111111';
        $1"
}
as_nobody() {
    "$PGBIN/psql" -h "$SOCKET" -U postgres -d "$DB" -tAq -v ON_ERROR_STOP=1 -c "
        SET ROLE skattjakt_app;
        $1"
}

# 1. A tenant sees its own rows.
[[ "$(as_alfa 'SELECT count(*) FROM documents;')" == "1" ]] \
    || fail "Alfa cannot see its own document"
[[ "$(as_alfa 'SELECT name FROM companies;')" == "Alfa AB" ]] \
    || fail "Alfa cannot see its own company row"

# 2. An unfiltered query returns only that tenant's rows — the case where
#    application code forgot its WHERE clause.
[[ "$(as_alfa 'SELECT count(*) FROM financial_facts;')" == "1" ]] \
    || fail "an unfiltered fact query crossed the tenant boundary"

# 3. Naming another tenant's row by primary key returns nothing.
[[ "$(as_alfa "SELECT count(*) FROM documents WHERE id = 'bbbbbbbb-0000-0000-0000-000000000001';")" == "0" ]] \
    || fail "Alfa read Beta's document by id"
[[ "$(as_alfa "SELECT count(*) FROM companies WHERE id = '22222222-2222-2222-2222-222222222222';")" == "0" ]] \
    || fail "Alfa read Beta's company row by id"

# 4. A join cannot be used to reach across the boundary.
[[ "$(as_alfa 'SELECT count(*) FROM financial_facts f JOIN document_versions v ON v.id = f.document_version_id;')" == "1" ]] \
    || fail "a join leaked rows across tenants"

# 5. No tenant set means no rows at all — failure is closed, not open.
[[ "$(as_nobody 'SELECT count(*) FROM documents;')" == "0" ]] \
    || fail "rows were visible with no tenant set"
[[ "$(as_nobody 'SELECT count(*) FROM companies;')" == "0" ]] \
    || fail "companies were visible with no tenant set"

# 6. Writing a row belonging to another tenant is refused by the WITH CHECK.
if as_alfa "INSERT INTO documents (company_id, kind, original_filename)
            VALUES ('22222222-2222-2222-2222-222222222222', 'annual_accounts', 'smuggled.pdf');" >/dev/null 2>&1; then
    fail "Alfa wrote a document into Beta's tenant"
fi

# 7. Updating another tenant's row affects nothing.
[[ "$(as_alfa "WITH updated AS (
        UPDATE documents SET original_filename = 'tampered.pdf'
        WHERE id = 'bbbbbbbb-0000-0000-0000-000000000001' RETURNING 1)
      SELECT count(*) FROM updated;")" == "0" ]] \
    || fail "Alfa updated Beta's document"

# 8. Deleting another tenant's row affects nothing.
[[ "$(as_alfa "WITH deleted AS (
        DELETE FROM financial_facts
        WHERE company_id = '22222222-2222-2222-2222-222222222222' RETURNING 1)
      SELECT count(*) FROM deleted;")" == "0" ]] \
    || fail "Alfa deleted Beta's facts"

# 9. The audit log is append-only for the application role.
as_alfa "INSERT INTO audit_events (company_id, actor, event_type)
         VALUES ('11111111-1111-1111-1111-111111111111', 'test', 'analysis.started');" >/dev/null
if as_alfa "UPDATE audit_events SET event_type = 'rewritten';" >/dev/null 2>&1; then
    fail "an audit event was rewritten"
fi
if as_alfa "DELETE FROM audit_events;" >/dev/null 2>&1; then
    fail "an audit event was deleted"
fi

# 10. Beta's data is intact after everything Alfa attempted.
BETA_FACTS="$("$PGBIN/psql" -h "$SOCKET" -U postgres -d "$DB" -tA \
    -c "SELECT count(*) FROM financial_facts WHERE company_id = '22222222-2222-2222-2222-222222222222';")"
[[ "$BETA_FACTS" == "1" ]] || fail "Beta's data was modified by Alfa"

echo "all tenant isolation checks passed"

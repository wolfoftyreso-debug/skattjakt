#!/usr/bin/env bash
# The accessibility audit, against the real pages with real data on them.
#
# Same harness as simulation-ui.sh — PostgreSQL, the API, a company, a user
# and a model — pointed at axe-core instead. It audits every state of both
# pages that a person sees, because an interface that is accessible while
# empty and inaccessible once it holds a result is not accessible.
#
# Usage: tests/e2e/accessibility.sh
# Requires: PostgreSQL, cargo, curl, node with playwright and axe-core, chromium.

set -euo pipefail

if [[ -z "${SKATTJAKT_PG_REEXEC:-}" ]] && command -v cargo >/dev/null 2>&1; then
    cargo build --quiet --bin skattjakt-api \
        --manifest-path "$(dirname "${BASH_SOURCE[0]}")/../../Cargo.toml"
fi

if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    RUNAS="${SKATTJAKT_PG_USER:-postgres}"
    id "$RUNAS" >/dev/null 2>&1 || RUNAS=nobody
    export SKATTJAKT_PG_REEXEC=1
    exec su -s /bin/bash "$RUNAS" -c \
        "SKATTJAKT_PG_REEXEC=1 PGBIN='${PGBIN:-}' PLAYWRIGHT_MODULE='${PLAYWRIGHT_MODULE:-}' AXE_SOURCE='${AXE_SOURCE:-}' \
         PLAYWRIGHT_BROWSERS_PATH='${PLAYWRIGHT_BROWSERS_PATH:-}' \
         $(printf '%q ' "$0" "$@")"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKDIR="$(mktemp -d)"
PGDATA="$WORKDIR/data"
SOCKET="$WORKDIR/sock"
PGPORT=5453
DB=skattjakt_a11y
APIPORT=18112
EMAIL="ui@example.com"
PASSWORD="bokslut kaffe cykel oktober"

PGBIN="${PGBIN:-$(dirname "$(command -v initdb || echo /usr/lib/postgresql/16/bin/initdb)")}"
[[ -x "$PGBIN/initdb" ]] || PGBIN=/usr/lib/postgresql/16/bin

cleanup() {
    [[ -n "${API_PID:-}" ]] && kill "$API_PID" 2>/dev/null
    "$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT INT TERM

mkdir -p "$SOCKET"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET -h 127.0.0.1 -p $PGPORT" \
    -l "$WORKDIR/pg.log" start >/dev/null
psql() { "$PGBIN/psql" -h "$SOCKET" -p "$PGPORT" -U postgres -v ON_ERROR_STOP=1 -q "$@"; }
psql -d postgres -c "CREATE DATABASE $DB" >/dev/null
for migration in "$ROOT"/migrations/*.sql; do psql -d "$DB" -f "$migration" >/dev/null; done
psql -d "$DB" -c "ALTER ROLE skattjakt_app LOGIN PASSWORD 'ui'" >/dev/null

ADMIN_TOKEN="admin-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
export DATABASE_URL="postgres://skattjakt_app:ui@127.0.0.1:$PGPORT/$DB"
export SKATTJAKT_ADMIN_TOKEN="$ADMIN_TOKEN"
export SKATTJAKT_BLOB_ROOT="$WORKDIR/documents"
export RUST_LOG=skattjakt=warn
# The browser talks to the API over plain HTTP on loopback, and a `Secure`
# cookie is simply not sent over that. This is the switch the code documents
# for exactly this case, and it is the only way to exercise the cookie flow
# without terminating TLS in a test.
export SKATTJAKT_INSECURE_COOKIES=1

PORT="$APIPORT" "$ROOT/target/debug/skattjakt-api" > "$WORKDIR/api.log" 2>&1 &
API_PID=$!
for _ in $(seq 1 80); do
    curl -fsS "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && break
    sleep 0.2
done
curl -fsS "http://127.0.0.1:$APIPORT/health" >/dev/null || {
    echo "the API did not start"; cat "$WORKDIR/api.log"; exit 1; }

api() {
    curl -sS -X "$1" "http://127.0.0.1:$APIPORT$2" -H "authorization: Bearer $3" \
        -H 'content-type: application/json' ${4:+-d "$4"}
}
field() { python3 -c "import json,sys;print(json.load(sys.stdin).get('$1',''))"; }

CREATED="$(api POST /v1/companies "$ADMIN_TOKEN" \
    '{"company":{"name":"Simuleringsbolaget AB","org_number":"5560160680",
      "fiscal_year":{"start":"2025-01-01","end":"2025-12-31"}}}')"
TOKEN="$(field api_token <<<"$CREATED")"

api POST /v1/users "$TOKEN" \
    "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\",\"role\":\"owner\"}" >/dev/null

api POST /v1/simulations "$TOKEN" '{
  "name": "Resultat 2026",
  "inputs": [
    {"id":"customers","name":"Antal kunder","unit":"st","source":"CRM",
     "distribution":{"kind":"normal","mean":1000,"std_dev":120}},
    {"id":"average_revenue","name":"Snittintäkt","unit":"kr","source":"Fakturering",
     "distribution":{"kind":"triangular","low":700,"mode":850,"high":1100}},
    {"id":"fixed_costs","name":"Fasta kostnader","unit":"kr","source":"Budget",
     "distribution":{"kind":"uniform","low":400000,"high":600000}}
  ],
  "outputs": [
    {"id":"revenue","name":"Intäkter","unit":"kr","expression":"customers * average_revenue"},
    {"id":"profit","name":"Resultat","unit":"kr","expression":"revenue - fixed_costs",
     "target":300000,"critical_threshold":0}
  ]}' >/dev/null

echo "api ready on :$APIPORT with a model and a user"

PLAYWRIGHT_MODULE="${PLAYWRIGHT_MODULE:-/tmp/pw/node_modules/playwright/index.js}"
if [[ ! -f "$PLAYWRIGHT_MODULE" ]]; then
    echo "playwright is not installed at $PLAYWRIGHT_MODULE"
    echo "install it with: (cd /tmp/pw && npm install playwright)"
    exit 1
fi
export PLAYWRIGHT_MODULE
export CHROMIUM_PATH="${CHROMIUM_PATH:-/opt/pw-browsers/chromium-1194/chrome-linux/chrome}"
[[ -x "$CHROMIUM_PATH" ]] || CHROMIUM_PATH="$(ls -d /opt/pw-browsers/chromium*/chrome-linux/chrome 2>/dev/null | head -1)"
export CHROMIUM_PATH

export AXE_SOURCE="${AXE_SOURCE:-/tmp/pw/node_modules/axe-core/axe.min.js}"
if [[ ! -f "$AXE_SOURCE" ]]; then
    echo "axe-core is not installed at $AXE_SOURCE"
    echo "install it with: (cd /tmp/pw && npm install axe-core)"
    exit 1
fi

node "$ROOT/tests/e2e/accessibility.mjs" \
    "http://127.0.0.1:$APIPORT" "$EMAIL" "$PASSWORD"

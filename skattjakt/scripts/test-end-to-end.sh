#!/usr/bin/env bash
# The final product test (section 40).
#
# Starts a real Postgres, applies the migrations as the owning role, runs the
# API as the *application* role — so row-level security genuinely applies — and
# walks the whole product: create a Swedish AB, give it a profile, upload a set
# of accounts, run an analysis, and check the result, the evidence, the report,
# the audit trail, reproducibility, and that another tenant can reach none of it.
#
# Usage: scripts/test-end-to-end.sh

set -euo pipefail

if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    RUNAS="${SKATTJAKT_PG_USER:-postgres}"
    id "$RUNAS" >/dev/null 2>&1 || RUNAS=nobody
    exec su -s /bin/bash "$RUNAS" -c "SKATTJAKT_PG_REEXEC=1 $(printf '%q ' "$0" "$@")"
fi

PGBIN="${PGBIN:-/usr/lib/postgresql/16/bin}"
[[ -x "$PGBIN/initdb" ]] || PGBIN="$(dirname "$(command -v initdb)")"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKDIR="$(mktemp -d)"
PGDATA="$WORKDIR/data"
SOCKET="$WORKDIR/sock"
PGPORT="${PGPORT:-55432}"
APIPORT="${APIPORT:-18080}"
DB=skattjakt_e2e
LOG="$WORKDIR/api.log"
ADMIN_TOKEN="admin-$(date +%s)-e2e"

cleanup() {
    [[ -n "${API_PID:-}" ]] && kill "$API_PID" 2>/dev/null || true
    "$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

fail() { echo "FAIL: $1" >&2; exit 1; }
step() { printf '\n== %s\n' "$1"; }

# --- database ---------------------------------------------------------------

mkdir -p "$SOCKET"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET -p $PGPORT -h 127.0.0.1" -l "$WORKDIR/pg.log" start >/dev/null
PSQL=("$PGBIN/psql" -h 127.0.0.1 -p "$PGPORT" -U postgres -v ON_ERROR_STOP=1 -tAq)

"${PSQL[@]}" -d postgres -c "CREATE DATABASE $DB" >/dev/null
step "migrations"
# Applied by the owning role. The application role deliberately cannot create tables.
for migration in "$ROOT"/migrations/*.sql; do
    "${PSQL[@]}" -d "$DB" -f "$migration" >/dev/null
    echo "  applied $(basename "$migration")"
done

# The application connects as skattjakt_app, which is subject to RLS. In
# production the same role is created by infrastructure; here it just needs a
# password to log in with.
"${PSQL[@]}" -d "$DB" -c "ALTER ROLE skattjakt_app LOGIN PASSWORD 'e2e'" >/dev/null
DATABASE_URL="postgres://skattjakt_app:e2e@127.0.0.1:$PGPORT/$DB"

# --- api --------------------------------------------------------------------

step "starting the api"
cd "$ROOT"
DATABASE_URL="$DATABASE_URL" \
SKATTJAKT_ADMIN_TOKEN="$ADMIN_TOKEN" \
SKATTJAKT_BLOB_ROOT="$WORKDIR/documents" \
PORT="$APIPORT" \
RUST_LOG=skattjakt=info \
    "$ROOT/target/debug/skattjakt-api" > "$LOG" 2>&1 &
API_PID=$!

for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && break
    sleep 0.5
done
curl -sf "http://127.0.0.1:$APIPORT/health" >/dev/null || { cat "$LOG"; fail "the API did not start"; }
echo "  up on :$APIPORT"

api() { # method path token [body]
    local method="$1" path="$2" token="$3" body="${4:-}"
    if [[ -n "$body" ]]; then
        curl -sS -X "$method" "http://127.0.0.1:$APIPORT$path" \
            -H "authorization: Bearer $token" -H 'content-type: application/json' -d "$body"
    else
        curl -sS -X "$method" "http://127.0.0.1:$APIPORT$path" -H "authorization: Bearer $token"
    fi
}
jq() { python3 -c "import json,sys;d=json.load(sys.stdin);print(eval('d'+sys.argv[1]))" "$1"; }

# --- 1-2. a Swedish AB with a profile ---------------------------------------

step "1-2. create a Swedish AB and its profile"
COMPANY_JSON='{"company":{"name":"Konsultbyrån Nord AB","org_number":"556016-0680",
  "fiscal_year":{"start":"2025-01-01","end":"2025-12-31"},
  "industry":"Konsult","employee_count":8,"owner_count":2,"in_group":false,
  "operations_outside_sweden":false,"does_development_work":false,
  "owns_premises":false,"has_vehicles":false,"owners_active_in_company":true},
  "token_label":"e2e"}'
CREATED="$(api POST /v1/companies "$ADMIN_TOKEN" "$COMPANY_JSON")"
COMPANY_A="$(echo "$CREATED" | jq "['company_id']")"
TOKEN_A="$(echo "$CREATED" | jq "['api_token']")"
[[ -n "$COMPANY_A" && -n "$TOKEN_A" ]] || { echo "$CREATED"; fail "company was not created"; }
echo "  company $COMPANY_A"

# A second tenant, for the isolation checks further down.
OTHER='{"company":{"name":"Andra Bolaget AB","org_number":"556504-0465",
  "fiscal_year":{"start":"2025-01-01","end":"2025-12-31"}},"token_label":"e2e"}'
CREATED_B="$(api POST /v1/companies "$ADMIN_TOKEN" "$OTHER")"
TOKEN_B="$(echo "$CREATED_B" | jq "['api_token']")"
[[ -n "$TOKEN_B" ]] || { echo "$CREATED_B"; fail "second company was not created"; }

PROFILE="$(api GET /v1/companies/me "$TOKEN_A")"
[[ "$(echo "$PROFILE" | jq "['name']")" == "Konsultbyrån Nord AB" ]] || fail "profile did not round-trip"

# --- 3. upload a realistic set of accounts ----------------------------------

step "3. upload a set of accounts"
STATEMENT='RESULTATRÄKNING 2025\nNettoomsättning                     4 200 000\nÖvriga externa kostnader             -650 000\nPersonalkostnader                  -2 100 000\nPensionskostnader                     -80 000\nAv- och nedskrivningar                -40 000\nRörelseresultat                     1 330 000\nSkattemässigt resultat                850 000\n\nBALANSRÄKNING\nMateriella anläggningstillgångar      180 000\nSumma tillgångar                    2 400 000\nSumma eget kapital och skulder      2 400 000\n'
UPLOAD=$(python3 - "$STATEMENT" <<'PY'
import json, sys
print(json.dumps({
    "filename": "bokslut-2025.txt",
    "mime_type": "text/plain",
    "text": sys.argv[1].replace("\\n", "\n"),
    "kind": "annual_accounts",
    "accounts_state": "preliminary",
}))
PY
)
DOC="$(api POST /v1/documents "$TOKEN_A" "$UPLOAD")"
VERSION_ID="$(echo "$DOC" | jq "['document_version_id']")"
SHA="$(echo "$DOC" | jq "['sha256']")"
[[ -n "$VERSION_ID" ]] || { echo "$DOC"; fail "document was not stored"; }
echo "  version $VERSION_ID sha ${SHA:0:12}…"

# --- 5. document ingestion --------------------------------------------------

step "5. document ingestion"
BLOBS="$(find "$WORKDIR/documents" -type f | wc -l)"
[[ "$BLOBS" -eq 1 ]] || fail "expected exactly one stored blob, found $BLOBS"
find "$WORKDIR/documents" -type f | grep -q "$COMPANY_A" || fail "the blob is not tenant-prefixed"

# --- 4. run the analysis ----------------------------------------------------

step "4. start the analysis"
STARTED="$(api POST /v1/analyses/stored "$TOKEN_A" "{\"document_version_ids\":[\"$VERSION_ID\"],\"accounts_state\":\"preliminary\"}")"
ANALYSIS="$(echo "$STARTED" | jq "['analysis_id']")"
[[ -n "$ANALYSIS" ]] || { echo "$STARTED"; fail "analysis was not created"; }
echo "  analysis $ANALYSIS accepted"

STATUS=""
for _ in $(seq 1 120); do
    RESPONSE="$(api GET "/v1/analyses/$ANALYSIS" "$TOKEN_A")"
    STATUS="$(echo "$RESPONSE" | jq "['status']")"
    [[ "$STATUS" == "succeeded" || "$STATUS" == "failed" ]] && break
    sleep 0.5
done
[[ "$STATUS" == "succeeded" ]] || { echo "$RESPONSE"; fail "analysis ended as $STATUS"; }
echo "  finished: $STATUS"

# --- 6. financial extraction ------------------------------------------------

step "6. financial extraction"
FACTS="$("${PSQL[@]}" -d "$DB" -c "SELECT count(*) FROM financial_facts")"
[[ "$FACTS" -ge 5 ]] || fail "expected extracted facts, found $FACTS"
TRACEABLE="$("${PSQL[@]}" -d "$DB" -c "SELECT count(*) FROM financial_facts WHERE source_page IS NOT NULL AND source_text IS NOT NULL")"
[[ "$TRACEABLE" == "$FACTS" ]] || fail "$((FACTS - TRACEABLE)) facts have no page or source text"
echo "  $FACTS facts, all traceable to a page and a line"

# --- 7-10. discovery, rules, calculations, falsification --------------------

step "7-10. model runs, rules, calculations"
RUNS="$(echo "$RESPONSE" | python3 -c "import json,sys;print(len(json.load(sys.stdin)['model_runs']))")"
[[ "$RUNS" -eq 2 ]] || fail "expected a discovery run and a skeptic run, found $RUNS"
TASKS="$("${PSQL[@]}" -d "$DB" -c "SELECT string_agg(DISTINCT task, ',' ORDER BY task) FROM model_runs")"
[[ "$TASKS" == "contradiction_check,opportunity_discovery" ]] || fail "unexpected model tasks: $TASKS"

CALCS="$("${PSQL[@]}" -d "$DB" -c "SELECT count(*) FROM calculations")"
[[ "$CALCS" -ge 1 ]] || fail "no calculation was recorded"
echo "  2 model runs ($TASKS), $CALCS calculation(s)"

# --- 11-14. opportunities, evidence, confidence, ranges ---------------------

step "11-14. opportunities, evidence, confidence, ranges"
OPPS="$(api GET "/v1/analyses/$ANALYSIS/opportunities" "$TOKEN_A")"
python3 - "$OPPS" <<'PY' || exit 1
import json, sys
body = json.loads(sys.argv[1])
items = body["opportunities"]
assert items, "no opportunities were returned"
for o in items:
    assert o["status"] != "identified", f"{o['title']} was presented as established"
    assert 0 <= o["confidence"]["score"] <= 100, "confidence out of range"
    assert o["impact"]["high"] >= o["impact"]["low"], "inverted impact range"
    assert o["recommended_action"].strip(), f"{o['title']} offers no next step"
    if o["rule_ids"]:
        types = [e["type"] for e in o["evidence"]]
        assert "document_value" in types, f"{o['title']} has no document value"
        assert "rule" in types, f"{o['title']} cites no rule"
    if o["impact"]["low"] != o["impact"]["high"]:
        assert o["impact"]["low"] < o["impact"]["high"]
assert body["disclaimer"].startswith("Skattjakt är ett analys- och upptäcktsverktyg")
print(f"  {len(items)} opportunities, all with evidence, all capped below 'identified'")
PY

FIRST_OPP="$(echo "$OPPS" | python3 -c "import json,sys;print(json.load(sys.stdin)['opportunities'][0]['id'])")"
SINGLE="$(api GET "/v1/opportunities/$FIRST_OPP" "$TOKEN_A")"
echo "$SINGLE" | grep -q '"evidence"' || fail "single opportunity carries no evidence"

# --- 15-16. disclaimer and report -------------------------------------------

step "15-16. report"
REPORT="$(api GET "/v1/analyses/$ANALYSIS/report" "$TOKEN_A")"
python3 - "$REPORT" <<'PY' || exit 1
import json, sys
r = json.loads(sys.argv[1])
s = r["sections"]
for key in ["summary", "start_here", "opportunities", "warnings", "missing_information",
            "economic_potential", "evidence", "next_steps", "limitations"]:
    assert key in s, f"report is missing section {key}"
assert r["disclaimer"], "report has no disclaimer"
assert s["economic_potential"]["total"]["high"] >= s["economic_potential"]["total"]["low"]
assert s["evidence"]["rules_cited"], "report cites no rules"
print(f"  all nine sections; potential {s['economic_potential']['display']}")
PY

MARKDOWN="$(curl -sS "http://127.0.0.1:$APIPORT/v1/analyses/$ANALYSIS/report?format=markdown" -H "authorization: Bearer $TOKEN_A")"
for heading in "# Din Skattjakt" "## 1. Sammanfattning" "## 9. Begränsningar"; do
    echo "$MARKDOWN" | grep -qF "$heading" || fail "markdown report is missing '$heading'"
done
echo "$MARKDOWN" | grep -q "Skattjakt är ett analys- och upptäcktsverktyg" || fail "markdown report has no disclaimer"
echo "  markdown export renders"

# --- 17. audit trail --------------------------------------------------------

step "17. audit trail"
EVENTS="$("${PSQL[@]}" -d "$DB" -c "SELECT string_agg(DISTINCT event_type, ',' ORDER BY event_type) FROM audit_events WHERE company_id = '$COMPANY_A'")"
for expected in company.created document.uploaded analysis.created analysis.completed; do
    [[ "$EVENTS" == *"$expected"* ]] || fail "audit trail is missing $expected (has: $EVENTS)"
done
echo "  $EVENTS"

# --- 18. cross-tenant access ------------------------------------------------

step "18. cross-tenant access"
[[ "$(api GET "/v1/analyses/$ANALYSIS" "$TOKEN_B" | jq "['title']" 2>/dev/null)" == "not found" ]] \
    || fail "company B could read company A's analysis"
[[ "$(api GET "/v1/opportunities/$FIRST_OPP" "$TOKEN_B" | jq "['title']" 2>/dev/null)" == "not found" ]] \
    || fail "company B could read company A's opportunity"
B_DOCS="$(api GET /v1/documents "$TOKEN_B" | python3 -c "import json,sys;print(len(json.load(sys.stdin)['documents']))")"
[[ "$B_DOCS" -eq 0 ]] || fail "company B can see $B_DOCS of company A's documents"
[[ "$(api GET /v1/companies/me "$ADMIN_TOKEN" | jq "['title']" 2>/dev/null)" == "wrong credential" ]] \
    || fail "the admin token reached company data"
echo "  analysis, opportunity and documents all unreachable; admin token grants no company data"

# --- 19. reproducibility ----------------------------------------------------

step "19. reproducibility"
SECOND="$(api POST /v1/analyses/stored "$TOKEN_A" "{\"document_version_ids\":[\"$VERSION_ID\"]}" | jq "['analysis_id']")"
for _ in $(seq 1 120); do
    S2="$(api GET "/v1/analyses/$SECOND" "$TOKEN_A" | jq "['status']")"
    [[ "$S2" == "succeeded" || "$S2" == "failed" ]] && break
    sleep 0.5
done
[[ "$S2" == "succeeded" ]] || fail "the second analysis ended as $S2"

TITLES_1="$(api GET "/v1/analyses/$ANALYSIS/opportunities" "$TOKEN_A" | python3 -c "import json,sys;print(sorted(o['title'] for o in json.load(sys.stdin)['opportunities']))")"
TITLES_2="$(api GET "/v1/analyses/$SECOND/opportunities" "$TOKEN_A" | python3 -c "import json,sys;print(sorted(o['title'] for o in json.load(sys.stdin)['opportunities']))")"
[[ "$TITLES_1" == "$TITLES_2" ]] || fail "the same input produced different findings"
RULESET="$("${PSQL[@]}" -d "$DB" -c "SELECT DISTINCT rule_set_version FROM analysis_jobs")"
echo "  identical findings on a second run, rule set $RULESET"

# --- 20. logs ---------------------------------------------------------------

step "20. logs carry no financial or secret material"
for forbidden in "4 200 000" "Nettoomsättning" "$TOKEN_A" "$ADMIN_TOKEN" "Personalkostnader"; do
    if grep -qF -- "$forbidden" "$LOG"; then
        fail "the log contains '$forbidden'"
    fi
done
echo "  no amounts, statement labels or tokens in the log"

printf '\n== all end-to-end checks passed\n'

#!/usr/bin/env bash
# The final product test (section 40).
#
# Starts a real Postgres, applies the migrations as the owning role, runs the
# API as the *application* role — so row-level security genuinely applies — and
# walks the whole product: create a Swedish AB, give it a profile, upload a set
# of accounts, run an analysis, and check the result, the evidence, the report,
# the audit trail, reproducibility, and that another tenant can reach none of it.
#
# Usage: tests/e2e/end-to-end.sh

set -euo pipefail

if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    RUNAS="${SKATTJAKT_PG_USER:-postgres}"
    id "$RUNAS" >/dev/null 2>&1 || RUNAS=nobody
    exec su -s /bin/bash "$RUNAS" -c "SKATTJAKT_PG_REEXEC=1 $(printf '%q ' "$0" "$@")"
fi

PGBIN="${PGBIN:-/usr/lib/postgresql/16/bin}"
[[ -x "$PGBIN/initdb" ]] || PGBIN="$(dirname "$(command -v initdb)")"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Whichever build is newer, never whichever profile is preferred.
source "$ROOT/tests/lib/newest-binary.sh"
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
    [[ -n "${WORKER_PID:-}" ]] && kill "$WORKER_PID" 2>/dev/null || true
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

# --- object storage ---------------------------------------------------------
#
# When SKATTJAKT_S3_ENDPOINT is set, the whole product runs against S3 instead
# of the local filesystem. `tests/integration/e2e-on-s3.sh` sets it and starts a
# MinIO; without it this is the filesystem run, unchanged.

if [[ -n "${SKATTJAKT_S3_ENDPOINT:-}" ]]; then
    step "storage backend: S3 at $SKATTJAKT_S3_ENDPOINT"
else
    step "storage backend: filesystem"
fi

# --- api --------------------------------------------------------------------

step "starting the api"
cd "$ROOT"
BIN_API="$(newest_binary skattjakt-api)"
DATABASE_URL="$DATABASE_URL" \
SKATTJAKT_ADMIN_TOKEN="$ADMIN_TOKEN" \
SKATTJAKT_BLOB_ROOT="$WORKDIR/documents" \
PORT="$APIPORT" \
RUST_LOG=skattjakt=info \
    "$BIN_API" > "$LOG" 2>&1 &
API_PID=$!

for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && break
    sleep 0.5
done
curl -sf "http://127.0.0.1:$APIPORT/health" >/dev/null || { cat "$LOG"; fail "the API did not start"; }
echo "  up on :$APIPORT"

# --- worker -----------------------------------------------------------------
#
# A second process, because that is how it is deployed. The API enqueues and
# the worker runs; testing the API alone would leave the analysis queued
# forever and would not exercise the path that actually runs in production.

step "starting the analysis worker"
BIN_ANALYSIS_WORKER="$(newest_binary skattjakt-analysis-worker)"
DATABASE_URL="$DATABASE_URL" \
SKATTJAKT_BLOB_ROOT="$WORKDIR/documents" \
HOSTNAME=e2e-worker \
RUST_LOG=skattjakt=info \
    "$BIN_ANALYSIS_WORKER" > "$WORKDIR/worker.log" 2>&1 &
WORKER_PID=$!

# The worker has no HTTP surface, so readiness is "it did not exit".
sleep 1
kill -0 "$WORKER_PID" 2>/dev/null || { cat "$WORKDIR/worker.log"; fail "the worker did not start"; }
echo "  worker $WORKER_PID"

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

# --- 4b. the upload-ticket flow, which is how a phone uploads ---------------

step "4b. upload ticket"
TICKET="$(api POST /v1/documents/tickets "$TOKEN_A" \
    '{"filename":"skannat-bokslut.txt","mime_type":"text/plain","size":42}')"
TICKET_ID="$(echo "$TICKET" | jq "['ticket_id']")"
UPLOAD_URL="$(echo "$TICKET" | jq "['upload_url']")"
UPLOAD_METHOD="$(echo "$TICKET" | jq "['upload_method']")"
[[ -n "$TICKET_ID" ]] || { echo "$TICKET"; fail "no ticket was issued"; }
echo "  ticket $TICKET_ID via $UPLOAD_METHOD"

TICKET_BODY='Nettoomsättning  1 000 000'
if [[ "$UPLOAD_METHOD" == "direct" ]]; then
    # Straight to object storage, with no credential — the presigned URL is
    # self-contained. This is the path a phone takes.
    CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary "$TICKET_BODY" "$UPLOAD_URL")"
    [[ "$CODE" == "200" ]] || fail "the presigned upload was refused (HTTP $CODE)"
else
    CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
        -H "authorization: Bearer $TOKEN_A" --data-binary "$TICKET_BODY" \
        "http://127.0.0.1:$APIPORT$UPLOAD_URL")"
    [[ "$CODE" == "204" ]] || fail "the proxied upload was refused (HTTP $CODE)"
fi

# The declared size was 42 and the body is not 42 bytes, so completion must
# refuse it: a ticket redeemable for more bytes than it declared is a ticket
# with no size limit.
MISMATCH="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "authorization: Bearer $TOKEN_A" -H 'content-type: application/json' \
    "http://127.0.0.1:$APIPORT/v1/documents/tickets/$TICKET_ID/complete" -d '{}')"
[[ "$MISMATCH" == "422" ]] \
    || fail "a size mismatch was accepted (HTTP $MISMATCH)"
echo "  a size mismatch is refused"

# A ticket is single-use, so the rejected one cannot be retried.
AGAIN="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "authorization: Bearer $TOKEN_A" -H 'content-type: application/json' \
    "http://127.0.0.1:$APIPORT/v1/documents/tickets/$TICKET_ID/complete" -d '{}')"
[[ "$AGAIN" == "404" ]] || fail "a used ticket was accepted again (HTTP $AGAIN)"
echo "  a used ticket cannot be redeemed twice"

# Now a correct one, end to end.
# Bytes, not characters. `${#var}` counts characters, and "Nettoomsättning"
# has an ä — so a character count is one short and the upload is refused. That
# is the trap the API's error message now names explicitly.
BODY_SIZE="$(printf '%s' "$TICKET_BODY" | wc -c)"
TICKET2="$(api POST /v1/documents/tickets "$TOKEN_A" \
    "{\"filename\":\"bokslut-via-ticket.txt\",\"mime_type\":\"text/plain\",\"size\":$BODY_SIZE}")"
TICKET2_ID="$(echo "$TICKET2" | jq "['ticket_id']")"
URL2="$(echo "$TICKET2" | jq "['upload_url']")"
if [[ "$(echo "$TICKET2" | jq "['upload_method']")" == "direct" ]]; then
    curl -s -o /dev/null -X PUT --data-binary "$TICKET_BODY" "$URL2"
else
    curl -s -o /dev/null -X PUT -H "authorization: Bearer $TOKEN_A" \
        --data-binary "$TICKET_BODY" "http://127.0.0.1:$APIPORT$URL2"
fi
COMPLETED="$(api POST "/v1/documents/tickets/$TICKET2_ID/complete" "$TOKEN_A" '{}')"
TICKET_VERSION="$(echo "$COMPLETED" | jq "['document_version_id']")"
[[ -n "$TICKET_VERSION" ]] || { echo "$COMPLETED"; fail "the ticket did not produce a document"; }
echo "  ticket upload produced document version $TICKET_VERSION"

# Another tenant must not be able to redeem it.
CROSS="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "authorization: Bearer $TOKEN_B" -H 'content-type: application/json' \
    "http://127.0.0.1:$APIPORT/v1/documents/tickets/$TICKET2_ID/complete" -d '{}')"
[[ "$CROSS" == "404" ]] || fail "another tenant redeemed the ticket (HTTP $CROSS)"
echo "  another tenant cannot redeem it"

# --- 5. document ingestion --------------------------------------------------

step "5. document ingestion"
if [[ -n "${SKATTJAKT_S3_ENDPOINT:-}" ]]; then
    # On S3 the assertion is the same property, asked of the object store: one
    # object, under this company's prefix.
    OBJECTS="$(curl -s "$SKATTJAKT_S3_ENDPOINT/$SKATTJAKT_S3_BUCKET?list-type=2&prefix=companies/$COMPANY_A/" \
        --user "$SKATTJAKT_S3_ACCESS_KEY:$SKATTJAKT_S3_SECRET_KEY" \
        --aws-sigv4 "aws:amz:${SKATTJAKT_S3_REGION:-us-east-1}:s3" \
        | grep -c '<Key>' || true)"
    [[ "$OBJECTS" -eq 1 ]] || fail "expected exactly one stored object, found $OBJECTS"
    echo "  one object under companies/$COMPANY_A/"
else
    BLOBS="$(find "$WORKDIR/documents" -type f | wc -l)"
    [[ "$BLOBS" -eq 1 ]] || fail "expected exactly one stored blob, found $BLOBS"
    find "$WORKDIR/documents" -type f | grep -q "$COMPANY_A" || fail "the blob is not tenant-prefixed"
fi

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
        # Every rule the customer is shown names the paragraphs it rests on and
        # says how far each has been checked. A citation with no state reads as
        # "somebody looked this up", which is the one thing it must not imply
        # while every source in the registry is still unretrieved.
        for item in o["evidence"]:
            if item["type"] != "rule":
                continue
            citations = item.get("citations") or []
            assert citations, f"{o['title']} rests on a rule with no citations"
            for c in citations:
                assert c["reference"].strip(), "a citation has no reference"
                assert c["claim"].strip(), "a citation states no claim"
                assert c["state"] in {"unretrieved", "unreachable", "mismatch", "verified"}, \
                    f"unknown citation state {c['state']}"
                if c["state"] in {"unretrieved", "unreachable"}:
                    assert not c.get("retrieved_at"), \
                        "an unfetched citation carries a retrieval timestamp"
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
for rule in s["evidence"]["rules_cited"]:
    assert rule["source_state"] in {"unretrieved", "unreachable", "mismatch", "verified"}, \
        f"{rule['title']} reports source state {rule['source_state']}"
ep = s["economic_potential"]
assert ep["deferred"]["high"] >= ep["deferred"]["low"]

# Uppskjuten skatt får aldrig räknas in i det belopp en läsare tar för pengar
# att hämta. Ett fynd som är ett uppskov ska synas på egen rad med sitt belopp,
# och rubriksumman ska sakna det.
deferring = [o for o in s["opportunities"] if o.get("effect") == "deferral"]
if deferring:
    assert ep["deferred"]["high"] > 0, \
        "en periodiseringsfond hittades men det uppskjutna beloppet är noll"
    assert ep["deferred_note"], "det uppskjutna beloppet saknar förklaring"
    for o in deferring:
        assert o["impact"]["high"] <= ep["deferred"]["high"], \
            f"{o['title']!r} räknas inte in i det uppskjutna beloppet"
print(f"  all nine sections; lägre skatt {ep['display']}, "
      f"uppskjuten {ep['deferred_display']} ({len(deferring)} uppskov)")
PY

# What a customer should go and fetch next, gathered in one place.
#
# This section was empty in exactly the runs that worked: a complete profile and
# readable documents leave nothing in the two sources it used to draw from,
# while every finding that fired carries two or three documents a person could
# actually go and get. Asserted here against a real analysis because that is the
# only place the emptiness was visible.
python3 - "$REPORT" <<'MISSING' || exit 1
import json, sys
s = json.loads(sys.argv[1])["sections"]

requested = {item for o in s["opportunities"] for item in o["missing_information"]}
assert requested, "no finding asked for anything, so this proves nothing"
assert s["missing_information"], \
    f"{len(requested)} requests inside the findings, and the section that " \
    "gathers them is empty"

described = {m["description"] for m in s["missing_information"]}
missing_from_summary = requested - described
assert not missing_from_summary, \
    f"asked for by a finding and absent from the summary: {sorted(missing_from_summary)[:3]}"

for item in s["missing_information"]:
    assert item["unlocks"].strip(), f"{item['description']!r} says nothing about why"
print(f"  {len(s['missing_information'])} things that would make it better, all explained")
MISSING

MARKDOWN="$(curl -sS "http://127.0.0.1:$APIPORT/v1/analyses/$ANALYSIS/report?format=markdown" -H "authorization: Bearer $TOKEN_A")"
for heading in "# Din Skattjakt" "## 1. Sammanfattning" "## 9. Begränsningar"; do
    echo "$MARKDOWN" | grep -qF "$heading" || fail "markdown report is missing '$heading'"
done
echo "$MARKDOWN" | grep -q "Skattjakt är ett analys- och upptäcktsverktyg" || fail "markdown report has no disclaimer"
# The version a customer prints and hands to their accountant has to carry the
# same caveat the JSON does, or the caveat only exists where nobody reads it.
echo "$MARKDOWN" | grep -q "källa ej kontrollerad" \
    || fail "the markdown report does not say its sources are unchecked"
echo "  markdown export renders"

# --- 16B. what each of the three products actually delivers -----------------
#
# Three presentation layers over one engine, sold at two prices. Nothing had
# ever checked what a buyer gets for the difference — only that the field
# exists. This runs the same finished analysis through all three and compares.
#
# Reachable here because this deployment does not require payment: with payments
# on, the layer is fixed at redemption and asking for another is a 403. That is
# the point of the gate, and it is asserted in `payments.sh`.

step "16B. the three presentation layers"
REPORT_PRIVATE="$(api GET "/v1/analyses/$ANALYSIS/report?audience=private" "$TOKEN_A")"
REPORT_COMPANY="$(api GET "/v1/analyses/$ANALYSIS/report?audience=company" "$TOKEN_A")"
REPORT_ACCOUNTANT="$(api GET "/v1/analyses/$ANALYSIS/report?audience=accountant" "$TOKEN_A")"
python3 - "$REPORT_PRIVATE" "$REPORT_COMPANY" "$REPORT_ACCOUNTANT" <<'LAYERS' || exit 1
import json, sys

private, company, accountant = (json.loads(a) for a in sys.argv[1:4])

# The 69-kronor product has to contain something the 29-kronor one does not, or
# the price difference is for a field name.
review = accountant["sections"].get("control_review")
assert review, "the accountant layer carried no control review"
assert private["sections"].get("control_review") is None, \
    "the private layer carried the accountant's control review"
assert company["sections"].get("control_review") is None, \
    "the company layer carried the accountant's control review"

# And it has to be populated, not merely present. Four bands; a real analysis
# with findings must land something in at least one of them, and the talking
# points — the part a firm is actually buying — must carry amounts.
bands = {k: len(v) for k, v in review.items()}
assert sum(bands.values()) > 0, f"the control review is empty: {bands}"

# The band that says "check this before filing" must not be empty while findings
# are unsettled. Every finding this build can produce is capped at `verify` —
# no legal source has been retrieved — so an empty must-check would mean six
# improvements each resting on an unverified rule, which is the failure the
# source-state ladder exists to prevent.
unsettled = [o for o in accountant["sections"]["opportunities"]
             if o["status"] != "identified"]
if unsettled:
    assert review["must_check"], \
        f"{len(unsettled)} findings the engine could not settle, and nothing to check"
for item in review["possible_improvement"]:
    assert item["status"] == "identified", \
        f"{item['title']} is offered as an improvement with status {item['status']}"
for point in review["worth_raising"]:
    assert point["impact_display"], "a talking point with no amount"
    assert point["impact"]["high"] >= point["impact"]["low"]
statuses = {}
for h in accountant["sections"]["opportunities"]:
    statuses[h["status"]] = statuses.get(h["status"], 0) + 1
print(f"  control review: {bands}; finding statuses: {statuses}")

# Everything else is the same report. Stated as an assertion rather than left
# implied, because the API contract used to describe `private` as "written for
# someone reading about their own affairs" — a difference that does not exist in
# the code. Either this fails one day because somebody built it, or it keeps the
# documentation honest.
# The report names the layer it was built for, so a saved file can be told
# apart from another. That label and the control review are the only two things
# allowed to differ.
assert (private["sections"]["audience"], company["sections"]["audience"],
        accountant["sections"]["audience"]) == ("private", "company", "accountant"), \
    "a report does not name its own layer"

def body(report):
    sections = dict(report["sections"])
    sections.pop("control_review", None)
    sections.pop("audience", None)
    return {**report, "sections": sections}

assert body(private) == body(company), \
    "the private and company layers differ, which nothing documents"
assert body(company) == body(accountant), \
    "the accountant layer changes more than the control review"
print("  the three layers differ by the control review and nothing else")
LAYERS

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

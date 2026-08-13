#!/usr/bin/env bash
# The payment gate, against a real database and a real API.
#
# What this proves that a unit test cannot
# ========================================
#
# `crates/payments` tests the settlement decision against structs, and that
# covers the three money checks. What it cannot cover is the part that only
# exists when Postgres and HTTP are both involved:
#
#   * that an analysis genuinely cannot be started without a paid order;
#   * that one paid order buys exactly one analysis, **under a race** — the
#     constraint is a conditional UPDATE, and a conditional UPDATE is only
#     correct if the database says so;
#   * that an order belonging to one tenant is invisible to another;
#   * that the callback endpoint, which is unauthenticated, cannot be used to
#     mark anything paid;
#   * that a deployment with payments switched off does not accidentally
#     require them, and one with them on does not accidentally skip them.
#
# Swish itself is not reachable from here and would not be used if it were —
# nobody should run a test suite against a payment scheme. The provider is
# therefore unconfigured, which is exactly the state that must refuse rather
# than wave things through.
#
# Usage: tests/integration/payments.sh

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
PGPORT="${PGPORT:-55450}"
APIPORT="${APIPORT:-18100}"
DB=skattjakt_payments
ADMIN_TOKEN="admin-payments-suite"

# Whichever build is newer, not whichever profile is preferred. A release binary
# left over from before the change under test passes the health check and then
# fails in ways that read as product bugs — this has now cost two debugging
# sessions, so the rule lives in every suite that picks a binary.
API="$ROOT/target/release/skattjakt-api"
[[ -x "$API" ]] || API="$ROOT/target/debug/skattjakt-api"
DEBUG_API="$ROOT/target/debug/skattjakt-api"
[[ -x "$DEBUG_API" && "$DEBUG_API" -nt "$API" ]] && API="$DEBUG_API"

PGBIN="${PGBIN:-$(dirname "$(command -v initdb || echo /usr/lib/postgresql/16/bin/initdb)")}"
[[ -x "$PGBIN/initdb" ]] || PGBIN=/usr/lib/postgresql/16/bin

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }
check() { if [[ "$3" == "$2" ]]; then pass "$1"; else fail "$1 (expected $2, got $3)"; fi }

api_pid=""
cleanup() {
    [[ -n "$api_pid" ]] && kill "$api_pid" 2>/dev/null || true
    "$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

[[ -x "$API" ]] || { echo "build the API first: cargo build" >&2; exit 1; }

mkdir -p "$SOCKET"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET -p $PGPORT -h 127.0.0.1" -l "$WORKDIR/pg.log" start >/dev/null
PSQL=("$PGBIN/psql" -h 127.0.0.1 -p "$PGPORT" -U postgres -v ON_ERROR_STOP=1 -tAq)

"${PSQL[@]}" -d postgres -c "CREATE DATABASE $DB" >/dev/null
for migration in "$ROOT"/migrations/*.sql; do
    "${PSQL[@]}" -d "$DB" -f "$migration" >/dev/null
done
"${PSQL[@]}" -d "$DB" -c "ALTER ROLE skattjakt_app LOGIN PASSWORD 'pay'" >/dev/null
DATABASE_URL="postgres://skattjakt_app:pay@127.0.0.1:$PGPORT/$DB"

echo
echo "the schema refuses states that cannot be true"

# An order marked consumed with nothing to show for it would be a customer
# charged for nothing, invisibly. The database refuses to store one.
#
# A company of its own, with an organisationsnummer no later step uses, so the
# constraint checks cannot collide with the tenants the API creates.
FIXTURE=11111111-1111-1111-1111-111111111111
"${PSQL[@]}" -d "$DB" -c "INSERT INTO companies (id, name, org_number, fiscal_year_start, fiscal_year_end)
    VALUES ('$FIXTURE', 'Constraint Fixture AB', '5566778899', '2025-01-01', '2025-12-31')" >/dev/null

if "${PSQL[@]}" -d "$DB" -c "INSERT INTO orders (company_id, product, amount_ore, state, paid_at)
    VALUES ('$FIXTURE', 'company_analysis', 6900, 'consumed', now())" >/dev/null 2>&1; then
    fail "an order was marked consumed with no analysis behind it"
else
    pass "a consumed order must name what it bought"
fi

if "${PSQL[@]}" -d "$DB" -c "INSERT INTO orders (company_id, product, amount_ore, state)
    VALUES ('$FIXTURE', 'company_analysis', 6900, 'paid')" >/dev/null 2>&1; then
    fail "a paid order was accepted with no payment time"
else
    pass "a paid order must carry when it was paid"
fi

if "${PSQL[@]}" -d "$DB" -c "INSERT INTO orders (company_id, product, amount_ore)
    VALUES ('$FIXTURE', 'free_lunch', 0)" >/dev/null 2>&1; then
    fail "an unpriced product was accepted"
else
    pass "an order must name a product this build sells, at a price above zero"
fi

# --- the API, with payments switched off ------------------------------------

start_api() { # extra env
    [[ -n "$api_pid" ]] && kill "$api_pid" 2>/dev/null || true
    sleep 0.3
    env DATABASE_URL="$DATABASE_URL" SKATTJAKT_ADMIN_TOKEN="$ADMIN_TOKEN" \
        SKATTJAKT_BLOB_ROOT="$WORKDIR/blobs" PORT="$APIPORT" "$@" \
        "$API" >>"$WORKDIR/api.log" 2>&1 &
    api_pid=$!
    for _ in $(seq 1 60); do
        curl -sf "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    return 1
}

api() { # method path token [body]
    if [[ -n "${4:-}" ]]; then
        curl -sS --max-time 30 -X "$1" "http://127.0.0.1:$APIPORT$2" -H "authorization: Bearer $3" \
            -H "content-type: application/json" -d "$4"
    else
        curl -sS --max-time 30 -X "$1" "http://127.0.0.1:$APIPORT$2" -H "authorization: Bearer $3"
    fi
}
code() { # method path token [body]
    if [[ -n "${4:-}" ]]; then
        curl -sS --max-time 30 -o /dev/null -w '%{http_code}' -X "$1" "http://127.0.0.1:$APIPORT$2" \
            -H "authorization: Bearer $3" -H "content-type: application/json" -d "$4"
    else
        curl -sS --max-time 30 -o /dev/null -w '%{http_code}' -X "$1" "http://127.0.0.1:$APIPORT$2" \
            -H "authorization: Bearer $3"
    fi
}

echo
echo "with payments switched off"
start_api || { tail -5 "$WORKDIR/api.log"; fail "the API did not start"; exit 1; }

CREATED="$(api POST /v1/companies "$ADMIN_TOKEN" '{"company":{"name":"Betalbolaget AB",
  "org_number":"556016-0680","fiscal_year":{"start":"2025-01-01","end":"2025-12-31"}},
  "token_label":"payments"}')"
TOKEN_A="$(echo "$CREATED" | python3 -c 'import json,sys;print(json.load(sys.stdin)["api_token"])')"
OTHER="$(api POST /v1/companies "$ADMIN_TOKEN" '{"company":{"name":"Andra Bolaget AB",
  "org_number":"556504-0465","fiscal_year":{"start":"2025-01-01","end":"2025-12-31"}},
  "token_label":"payments"}')"
TOKEN_B="$(echo "$OTHER" | python3 -c 'import json,sys;print(json.load(sys.stdin)["api_token"])')"
[[ -n "$TOKEN_A" && -n "$TOKEN_B" ]] && pass "two tenants" || fail "two tenants"

# Orders need a provider. Without one the route refuses rather than pretending.
check "an order cannot be created with no provider" 503 "$(code POST /v1/orders "$TOKEN_A" '{"product":"company_analysis"}')"

DOC="$(api POST /v1/documents "$TOKEN_A" '{"filename":"b.txt","mime_type":"text/plain",
  "text":"RESULTATRÄKNING\nNettoomsättning 5 000 000\nSkattemässigt resultat 900 000\n",
  "kind":"annual_accounts"}')"
DV="$(echo "$DOC" | python3 -c 'import json,sys;print(json.load(sys.stdin)["document_version_id"])')"
check "an analysis runs free when payment is not required" 202 \
    "$(code POST /v1/analyses/stored "$TOKEN_A" "{\"document_version_ids\":[\"$DV\"],\"accounts_state\":\"preliminary\"}")"

echo
echo "with payments required"
# Required but no provider is a refusal to start: a deployment that takes
# orders it can never collect on is worse than one that will not boot.
if env DATABASE_URL="$DATABASE_URL" SKATTJAKT_PAYMENTS_REQUIRED=1 \
       SKATTJAKT_BLOB_ROOT="$WORKDIR/blobs" PORT="$((APIPORT + 1))" \
       "$API" >"$WORKDIR/badstart.log" 2>&1; then
    fail "the API started with payments required and no provider"
else
    grep -q "no payment provider is configured" "$WORKDIR/badstart.log" \
        && pass "required-with-no-provider is a refusal to start, with the reason" \
        || fail "it refused for the wrong reason: $(tail -2 "$WORKDIR/badstart.log")"
fi

# A provider that exists but cannot reach Swish is enough to exercise the gate:
# the gate is about orders, not about the network.
CERT="$WORKDIR/cert.pem"
python3 - "$CERT" <<'PY'
import subprocess, sys
# A self-signed pair, only ever handed to the TLS stack for loading. Nothing in
# this suite talks to Swish.
subprocess.run([
    "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
    "-keyout", sys.argv[1], "-out", sys.argv[1] + ".crt",
    "-days", "1", "-subj", "/CN=payments-suite",
], check=True, capture_output=True)
open(sys.argv[1], "a").write(open(sys.argv[1] + ".crt").read())
PY

# A provider is necessary but not sufficient. Taking money obliges the shop to
# publish who is taking it — the price, the terms, the address, the right to
# cancel — and those pages cannot render without merchant details. So a
# deployment configured to charge but not to identify itself is also a refusal
# to start, for the same reason as the one above: it would sell under a name
# nobody can read.
if env DATABASE_URL="$DATABASE_URL" SKATTJAKT_PAYMENTS_REQUIRED=1 \
       SKATTJAKT_SWISH_PAYEE_ALIAS=1231234567 \
       SKATTJAKT_SWISH_CLIENT_PEM="$CERT" \
       SKATTJAKT_SWISH_CA_PEM="$CERT.crt" \
       SKATTJAKT_SWISH_CALLBACK_URL="https://example.test/v1/payments/swish/callback" \
       SKATTJAKT_BLOB_ROOT="$WORKDIR/blobs" PORT="$((APIPORT + 1))" \
       "$API" >"$WORKDIR/nomerchant.log" 2>&1; then
    fail "the API started ready to charge with no merchant published"
else
    grep -q "SKATTJAKT_MERCHANT_NAME" "$WORKDIR/nomerchant.log" \
        && pass "charging without publishing the shop pages is a refusal to start" \
        || fail "it refused for the wrong reason: $(tail -2 "$WORKDIR/nomerchant.log")"
fi

start_api SKATTJAKT_PAYMENTS_REQUIRED=1 \
    SKATTJAKT_SWISH_PAYEE_ALIAS=1231234567 \
    SKATTJAKT_SWISH_CLIENT_PEM="$CERT" \
    SKATTJAKT_SWISH_CA_PEM="$CERT.crt" \
    SKATTJAKT_SWISH_CALLBACK_URL="https://example.test/v1/payments/swish/callback" \
    SKATTJAKT_MERCHANT_NAME="Skattjakt Sverige AB" \
    SKATTJAKT_MERCHANT_ORG_NUMBER="559999-1234" \
    SKATTJAKT_MERCHANT_ADDRESS="Exempelgatan 1, 111 22 Stockholm" \
    SKATTJAKT_MERCHANT_EMAIL="hej@skattjakt.se" \
    SKATTJAKT_MERCHANT_VAT_REGISTERED=1 \
    || { tail -5 "$WORKDIR/api.log"; fail "the API did not start with a provider"; exit 1; }
pass "the API starts with a configured provider"

# The price a customer is charged must be the price the shop publishes. These
# are two different code paths — the product table and the price page — and
# nothing but this check stops them drifting apart.
PRICE_PAGE="$(curl -sS --max-time 20 "http://127.0.0.1:$APIPORT/priser")"
grep -qF "69,00 kr" <<<"$PRICE_PAGE" \
    && pass "the published price matches the one the checkout charges" \
    || fail "the price page and the checkout disagree"

# Nothing may be sold that this build cannot deliver. Privatanalys has a price,
# a page and a payment message, and the shipped rule set has no
# private-individual rules — so an order for it would take 29 kronor for a
# report with nothing in it, and the customer could not tell "we found nothing"
# from "we looked at nothing".
check "a product with no rules behind it cannot be ordered" 503 \
    "$(code POST /v1/orders "$TOKEN_A" '{"product":"private_analysis"}')"

check "an analysis with no order is refused" 402 \
    "$(code POST /v1/analyses/stored "$TOKEN_A" "{\"document_version_ids\":[\"$DV\"],\"accounts_state\":\"preliminary\"}")"

check "an analysis naming an order that does not exist is refused" 402 \
    "$(code POST /v1/analyses/stored "$TOKEN_A" "{\"document_version_ids\":[\"$DV\"],\"accounts_state\":\"preliminary\",\"order_id\":\"99999999-9999-9999-9999-999999999999\"}")"

echo
echo "the callback grants nothing"
# Unauthenticated on purpose, and it must be unable to do anything. A forged
# callback naming a real order must not make it paid.
ORDER_ID="$("${PSQL[@]}" -d "$DB" -c "
    INSERT INTO orders (company_id, product, amount_ore, state)
    SELECT id, 'company_analysis', 6900, 'awaiting_payment' FROM companies
    WHERE name = 'Betalbolaget AB' RETURNING id")"
"${PSQL[@]}" -d "$DB" -c "
    INSERT INTO payments (order_id, company_id, provider, provider_reference, amount_ore)
    SELECT '$ORDER_ID', company_id, 'swish', 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA', 6900
    FROM orders WHERE id = '$ORDER_ID'" >/dev/null

FORGED="$(python3 -c "
import json,sys
print(json.dumps({'id':'X','status':'PAID','amount':69.0,'currency':'SEK',
                  'payeePaymentReference':'$(echo "$ORDER_ID" | tr -d '-')'}))")"
CB="$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' -X POST \
    "http://127.0.0.1:$APIPORT/v1/payments/swish/callback" \
    -H "content-type: application/json" -d "$FORGED")"
check "the callback answers 200 so Swish stops retrying" 200 "$CB"

STATE="$("${PSQL[@]}" -d "$DB" -c "SELECT state FROM orders WHERE id = '$ORDER_ID'")"
check "a forged callback did not make the order paid" awaiting_payment "$STATE"

# And an unknown reference must be a no-op rather than an error that reveals
# whether the reference exists.
CB="$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' -X POST \
    "http://127.0.0.1:$APIPORT/v1/payments/swish/callback" \
    -H "content-type: application/json" \
    -d '{"status":"PAID","amount":69.0,"currency":"SEK","payeePaymentReference":"deadbeef"}')"
check "an unknown reference is answered the same way" 200 "$CB"

CB="$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' -X POST \
    "http://127.0.0.1:$APIPORT/v1/payments/swish/callback" \
    -H "content-type: application/json" -d 'not json at all')"
check "rubbish at the callback is answered the same way" 200 "$CB"

echo
echo "one paid order buys exactly one analysis"
# Marked paid directly in the database, because there is no Swish here to pay
# it. That is the only shortcut in this suite, and it is on the *input* to the
# gate rather than on the gate.
"${PSQL[@]}" -d "$DB" -c "UPDATE orders SET state='paid', paid_at=now() WHERE id='$ORDER_ID'" >/dev/null

FIRST="$(code POST /v1/analyses/stored "$TOKEN_A" \
    "{\"document_version_ids\":[\"$DV\"],\"accounts_state\":\"preliminary\",\"order_id\":\"$ORDER_ID\"}")"
check "the first analysis is accepted" 202 "$FIRST"

# Asking again with an order that is already spent must not sell a second
# analysis — and must not refuse either. The customer whose request timed out
# and who pressed the button again has already paid; 402 would take the money
# and answer "that order cannot be used". They are shown what they bought.
SECOND="$(api POST /v1/analyses/stored "$TOKEN_A" \
    "{\"document_version_ids\":[\"$DV\"],\"accounts_state\":\"preliminary\",\"order_id\":\"$ORDER_ID\"}")"
SECOND_ID="$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("analysis_id",""))' <<<"$SECOND")"
BOUGHT_BY_ORDER="$("${PSQL[@]}" -d "$DB" -c "SELECT analysis_id FROM orders WHERE id='$ORDER_ID'")"
check "asking twice shows the analysis the order bought" "$BOUGHT_BY_ORDER" "$SECOND_ID"
COUNT_FOR_ORDER="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT count(*) FROM analysis_jobs WHERE id='$BOUGHT_BY_ORDER'")"
check "and exactly one analysis exists for it" 1 "$COUNT_FOR_ORDER"

CONSUMED="$("${PSQL[@]}" -d "$DB" -c "SELECT state FROM orders WHERE id='$ORDER_ID'")"
check "the order is consumed and names its analysis" consumed "$CONSUMED"
NAMED="$("${PSQL[@]}" -d "$DB" -c "SELECT (analysis_id IS NOT NULL) FROM orders WHERE id='$ORDER_ID'")"
check "and the analysis it bought is recorded" t "$NAMED"

echo
echo "the choice the terms page promises is actually offered"
# `/villkor` and `/angerratt` have always said the buyer picks between starting
# at once and waiting out the fourteen days. Until this existed the checkout
# offered neither, which made a purchase term a description of something that
# did not exist — the one kind of drift a customer can rely on to their cost.

# Immediate delivery without the acknowledgement is refused rather than quietly
# downgraded to the safe option: a buyer put in a two-week queue they did not
# ask for finds out a fortnight later.
check "immediate delivery without consent is refused" 400 \
    "$(code POST /v1/orders "$TOKEN_A" \
        '{"product":"company_analysis","delivery":"immediate"}')"
check "an unknown delivery choice is refused" 400 \
    "$(code POST /v1/orders "$TOKEN_A" \
        '{"product":"company_analysis","delivery":"whenever"}')"

# Silence is not consent. An order with no choice keeps the right to cancel.
DEFAULTED_ORDER="$("${PSQL[@]}" -d "$DB" -c "
    INSERT INTO orders (company_id, product, amount_ore, state)
    SELECT id, 'company_analysis', 6900, 'created' FROM companies
    WHERE name = 'Betalbolaget AB' RETURNING id")"
DEFAULT_CHOICE="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT delivery_choice FROM orders WHERE id='$DEFAULTED_ORDER'")"
check "an order that says nothing keeps the right to cancel" \
    after_cancellation_period "$DEFAULT_CHOICE"

# The database refuses the two states that would misrepresent what the buyer
# agreed to: consent without the choice, and the choice without consent.
if "${PSQL[@]}" -d "$DB" -c "UPDATE orders SET delivery_choice='immediate'
    WHERE id='$DEFAULTED_ORDER'" >/dev/null 2>&1; then
    fail "an order claimed immediate delivery with no consent recorded"
else
    pass "immediate delivery cannot be claimed without a recorded consent"
fi
if "${PSQL[@]}" -d "$DB" -c "UPDATE orders SET consent_at=now()
    WHERE id='$DEFAULTED_ORDER'" >/dev/null 2>&1; then
    fail "a consent was recorded against an order that did not choose immediate"
else
    pass "a consent cannot be recorded without the choice it belongs to"
fi

# The waiting period is real: a paid order that keeps its right to cancel does
# not buy an analysis yet.
WAITING="$("${PSQL[@]}" -d "$DB" -c "
    INSERT INTO orders (company_id, product, amount_ore, state, paid_at, deliverable_from)
    SELECT id, 'company_analysis', 6900, 'paid', now(), now() + interval '14 days'
    FROM companies WHERE name = 'Betalbolaget AB' RETURNING id")"
check "a paid order still inside the cancellation period does not deliver yet" 409 \
    "$(code POST /v1/analyses/stored "$TOKEN_A" \
        "{\"document_version_ids\":[\"$DV\"],\"accounts_state\":\"preliminary\",\"order_id\":\"$WAITING\"}")"

# And the buyer can actually exercise the right the page promises them.
CANCELLED="$(api POST "/v1/orders/$WAITING/cancel" "$TOKEN_A" '{}')"
python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d["refund"]["owed"] else 1)' \
    <<<"$CANCELLED" && pass "cancelling says a refund is owed" \
    || fail "cancelling did not record a refund"
CANCELLED_STATE="$("${PSQL[@]}" -d "$DB" -c "SELECT state FROM orders WHERE id='$WAITING'")"
check "and the order is marked refund_owed" refund_owed "$CANCELLED_STATE"
check "cancelling twice is refused" 409 \
    "$(code POST "/v1/orders/$WAITING/cancel" "$TOKEN_A" '{}')"

# A buyer who consented has no right left to exercise.
CONSENTED="$("${PSQL[@]}" -d "$DB" -c "
    INSERT INTO orders (company_id, product, amount_ore, state, paid_at,
                        delivery_choice, consent_at, consent_wording_version)
    SELECT id, 'company_analysis', 6900, 'paid', now(), 'immediate', now(), '2026.1'
    FROM companies WHERE name = 'Betalbolaget AB' RETURNING id")"
check "an order bought with consent cannot be cancelled" 409 \
    "$(code POST "/v1/orders/$CONSENTED/cancel" "$TOKEN_A" '{}')"
check "and it delivers immediately" 202 \
    "$(code POST /v1/analyses/stored "$TOKEN_A" \
        "{\"document_version_ids\":[\"$DV\"],\"accounts_state\":\"preliminary\",\"order_id\":\"$CONSENTED\"}")"

echo
echo "what was bought is what is served"
# The hole this closes: the gate checked *that* an order was paid and never
# *what it was for*, while the report chose its presentation layer from a query
# parameter the client sets. So 29 kronor of Privatanalys bought the 69-kronor
# Kontroll report, for anyone who read the API documentation.
PRIVATE_ORDER="$("${PSQL[@]}" -d "$DB" -c "
    INSERT INTO orders (company_id, product, amount_ore, state)
    SELECT id, 'private_analysis', 2900, 'awaiting_payment' FROM companies
    WHERE name = 'Betalbolaget AB' RETURNING id")"
"${PSQL[@]}" -d "$DB" -c "UPDATE orders SET state='paid', paid_at=now() WHERE id='$PRIVATE_ORDER'" >/dev/null

BOUGHT="$(api POST /v1/analyses/stored "$TOKEN_A" \
    "{\"document_version_ids\":[\"$DV\"],\"accounts_state\":\"preliminary\",\"order_id\":\"$PRIVATE_ORDER\"}")"
BOUGHT_ID="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["analysis_id"])' <<<"$BOUGHT")"

STAMP="$("${PSQL[@]}" -d "$DB" -c "SELECT audience FROM analysis_jobs WHERE id='$BOUGHT_ID'")"
check "redeeming a private order stamps the analysis private" private "$STAMP"

# The report needs a finished analysis, and no worker runs here. The result is
# therefore written directly — a shortcut on the *input* to the entitlement
# check, never on the check.
"${PSQL[@]}" -d "$DB" -c "
    UPDATE analysis_jobs SET status='succeeded', stage='done', finished_at=now(),
    result = jsonb_build_object(
        'analysis_id', id::text, 'company_id', company_id::text,
        'summary', jsonb_build_object(
            'identified_opportunities', 0, 'high_priority_count', 0,
            'needs_investigation_count', 0, 'missing_information_count', 0,
            'warnings_count', 0,
            'estimated_total', jsonb_build_object('low', 0, 'high', 0),
            'found_nothing', true),
        'opportunities', '[]'::jsonb, 'warnings', '[]'::jsonb,
        'missing_information', '[]'::jsonb, 'covered_areas', '[]'::jsonb,
        'cleared', '[]'::jsonb, 'recommended_actions', '[]'::jsonb,
        'limitations', '[]'::jsonb, 'rejected', '[]'::jsonb,
        'disclaimer', 'Preliminart.', 'generated_at', to_char(now() AT TIME ZONE 'UTC',
            'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'))
    WHERE id='$BOUGHT_ID'" >/dev/null

check "the layer it was bought as is served" 200 \
    "$(code GET "/v1/analyses/$BOUGHT_ID/report?audience=private" "$TOKEN_A")"
check "asking for the 69 kr review after paying 29 kr is refused" 403 \
    "$(code GET "/v1/analyses/$BOUGHT_ID/report?audience=accountant" "$TOKEN_A")"
check "and so is the company view" 403 \
    "$(code GET "/v1/analyses/$BOUGHT_ID/report?audience=company" "$TOKEN_A")"

# Omitting the parameter must not be the way round it. The default used to be
# `company`; on a bought analysis there is no default but the entitlement.
DEFAULTED="$(api GET "/v1/analyses/$BOUGHT_ID/report" "$TOKEN_A")"
python3 -c "
import json, sys
r = json.load(sys.stdin)
sys.exit(0 if r.get('audience') == 'private' or 'control_review' not in json.dumps(r) else 1)
" <<<"$DEFAULTED" && pass "omitting the parameter serves what was bought" \
    || fail "omitting the parameter did not serve the bought layer"

# And the control section — the thing the 69 kr actually buys — is not in it.
grep -q "control_review" <<<"$DEFAULTED" \
    && fail "the private report carried the accountant control review" \
    || pass "the control review is not in a report nobody bought it in"

echo
echo "under a race"
# The part a sequential test cannot reach. Ten requests against one paid order,
# started together: exactly one must win. A check-then-act in the handler
# passes every test above and fails this one.
# Built with the same two statements the earlier order used, which are known to
# work here: insert awaiting_payment, then mark it paid.
RACE_ORDER="$("${PSQL[@]}" -d "$DB" -c "
    INSERT INTO orders (company_id, product, amount_ore, state)
    SELECT id, 'company_analysis', 6900, 'awaiting_payment' FROM companies
    WHERE name = 'Betalbolaget AB' RETURNING id")"
"${PSQL[@]}" -d "$DB" -c "UPDATE orders SET state='paid', paid_at=now() WHERE id='$RACE_ORDER'" >/dev/null

RESULTS="$WORKDIR/race"
# The racers' pids are collected and waited on individually. A bare `wait`
# would also wait for the API this script started in the background, which
# never exits — the suite hangs rather than failing, which is the worst way for
# a test to be wrong.
RACERS=()
for i in $(seq 1 10); do
    (
        api POST /v1/analyses/stored "$TOKEN_A" \
            "{\"document_version_ids\":[\"$DV\"],\"accounts_state\":\"preliminary\",\"order_id\":\"$RACE_ORDER\"}" \
            > "$WORKDIR/race.$i"
    ) &
    RACERS+=("$!")
done
# Tolerant of a racer exiting non-zero: what matters is what each one recorded,
# and `set -e` would otherwise kill the suite silently.
wait "${RACERS[@]}" || true
for i in $(seq 1 10); do
    python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("analysis_id", "none"))
except Exception: print("unreadable")' < "$WORKDIR/race.$i" 2>/dev/null || echo "missing"
done > "$RESULTS"

# The invariant is not "nine were refused" — since a spent order now answers
# with the analysis it bought, the losers are told the truth rather than turned
# away. The invariant is that the order bought exactly one analysis and every
# racer names it. That is the property double-spending would break; the status
# codes were only ever a proxy for it.
DISTINCT="$(sort -u "$RESULTS" | grep -cv '^$' || true)"
check "all ten requests name one and the same analysis" 1 "$DISTINCT"
RACE_BOUGHT="$("${PSQL[@]}" -d "$DB" -c "SELECT analysis_id FROM orders WHERE id='$RACE_ORDER'")"
check "and it is the analysis the order records" "$RACE_BOUGHT" "$(head -1 "$RESULTS")"
RACE_ROWS="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT count(*) FROM orders WHERE id='$RACE_ORDER' AND state='consumed'")"
check "the order was spent exactly once" 1 "$RACE_ROWS"

echo
echo "an order belongs to one tenant"
check "another tenant cannot see the order" 404 "$(code GET "/v1/orders/$RACE_ORDER" "$TOKEN_B")"
# With a document of its own, so the only cross-tenant thing in the request is
# the order. Reusing tenant A's document made this pass for the wrong reason:
# the request died on the unknown document before the payment gate was reached.
DOC_B="$(api POST /v1/documents "$TOKEN_B" '{"filename":"b.txt","mime_type":"text/plain",
  "text":"RESULTATRÄKNING\nNettoomsättning 4 000 000\nSkattemässigt resultat 700 000\n",
  "kind":"annual_accounts"}')"
DV_B="$(echo "$DOC_B" | python3 -c 'import json,sys;print(json.load(sys.stdin)["document_version_id"])')"
check "and cannot spend it, with a document of its own" 402 \
    "$(code POST /v1/analyses/stored "$TOKEN_B" "{\"document_version_ids\":[\"$DV_B\"],\"accounts_state\":\"preliminary\",\"order_id\":\"$RACE_ORDER\"}")"
check "its owner can see it" 200 "$(code GET "/v1/orders/$RACE_ORDER" "$TOKEN_A")"

echo
echo "money is never invented"
NEGATIVE="$("${PSQL[@]}" -d "$DB" -c "SELECT count(*) FROM orders WHERE amount_ore <= 0")"
check "no order is free" 0 "$NEGATIVE"
MISMATCHED="$("${PSQL[@]}" -d "$DB" -c "
    SELECT count(*) FROM orders o JOIN payments p ON p.order_id = o.id
    WHERE p.amount_ore <> o.amount_ore")"
check "no payment asks for a different amount than its order" 0 "$MISMATCHED"

printf '\npassed %d, failed %d\n' "$passed" "$failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "the payment gate holds against a real database and a real API"

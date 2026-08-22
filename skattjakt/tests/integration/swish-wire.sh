#!/usr/bin/env bash
# The Swish wire format, spoken over real mutual TLS.
#
# What this establishes that nothing else could
# =============================================
#
# `SKATTJAKT_PAYMENTS.md` §9 has said since the crate was written that the wire
# format — the URL shape, the header the token arrives in, the field names, the
# status strings — was written against the documented v2 Commerce API and had
# never been exercised. Every other suite stops where the client would speak to
# Swish. This one lets it speak.
#
# The stand-in is `swish-stub.py`, the same move the suites already make for
# object storage: MinIO is not S3, and running against it still catches what
# would otherwise be caught in production. What is real here:
#
#   * a mutual-TLS handshake against a server that requires a client
#     certificate, which is the whole of Swish's authentication;
#   * certificate and key loaded from files by the product's own code;
#   * `PUT /api/v2/paymentrequests/{32 uppercase hex}` with the documented body;
#   * the token arriving in the `paymentrequesttoken` header;
#   * `GET` returning a payment whose amount and reference settlement checks;
#   * the callback being worth nothing, and the lookup being worth everything.
#
# What it cannot establish is that Swish agrees with the documentation. That
# needs the bank's test host and a real certificate. What it does establish is
# that the client sends what the specification says — which turns a spec read
# once into a spec asserted on every run.
#
# Usage: tests/integration/swish-wire.sh
# Requires: PostgreSQL, cargo, curl, python3, openssl.

set -euo pipefail

if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    RUNAS="${SKATTJAKT_PG_USER:-postgres}"
    id "$RUNAS" >/dev/null 2>&1 || RUNAS=nobody
    export SKATTJAKT_PG_REEXEC=1
    exec su -s /bin/bash "$RUNAS" -c \
        "SKATTJAKT_PG_REEXEC=1 PGBIN='${PGBIN:-}' $(printf '%q ' "$0" "$@")"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Whichever build is newer, never whichever profile is preferred.
source "$ROOT/tests/lib/newest-binary.sh"
WORKDIR="$(mktemp -d)"
PGDATA="$WORKDIR/data"
SOCKET="$WORKDIR/sock"
PGPORT="${PGPORT:-55452}"
APIPORT="${APIPORT:-18120}"
SWISHPORT="${SWISHPORT:-18121}"
DB=skattjakt_swish
ADMIN_TOKEN="admin-swish-suite"

API="$(newest_binary skattjakt-api)"

WORKER="$(newest_binary skattjakt-analysis-worker)"

PGBIN="${PGBIN:-$(dirname "$(command -v initdb || echo /usr/lib/postgresql/16/bin/initdb)")}"
[[ -x "$PGBIN/initdb" ]] || PGBIN=/usr/lib/postgresql/16/bin

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }
check() { if [[ "$3" == "$2" ]]; then pass "$1"; else fail "$1 (expected $2, got $3)"; fi }

api_pid=""
stub_pid=""
worker_pid=""
cleanup() {
    [[ -n "$api_pid" ]] && kill "$api_pid" 2>/dev/null || true
    [[ -n "$worker_pid" ]] && kill "$worker_pid" 2>/dev/null || true
    [[ -n "$stub_pid" ]] && kill "$stub_pid" 2>/dev/null || true
    "$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT INT TERM

[[ -x "$API" ]] || { echo "build the API first: cargo build" >&2; exit 1; }
[[ -x "$WORKER" ]] || { echo "build the worker first: cargo build" >&2; exit 1; }

# --- certificates -----------------------------------------------------------
#
# One CA signs both sides, which is how the real thing is shaped too: Swish
# signs the merchant's certificate, and the merchant trusts Swish's CA. Here one
# CA plays both parts because the point is the handshake, not the hierarchy.
CERTS="$WORKDIR/certs"
mkdir -p "$CERTS"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -keyout "$CERTS/ca.key" -out "$CERTS/ca.pem" \
    -subj "/CN=Swish Stub CA" >/dev/null 2>&1

make_cert() { # name subject purpose [san]
    openssl req -newkey rsa:2048 -nodes \
        -keyout "$CERTS/$1.key" -out "$CERTS/$1.csr" -subj "$2" >/dev/null 2>&1
    # Always an extension file, even when there is nothing to put in it beyond
    # the purpose. `openssl x509 -req` with no extensions emits a **version 1**
    # certificate, and rustls refuses those outright:
    #
    #     invalid peer certificate: Other(OtherError(UnsupportedCertVersion))
    #
    # Swish issues v3 certificates, so this is a fixture problem rather than a
    # product one — but it is the kind that reads as "mutual TLS is broken"
    # until somebody looks at the version byte.
    {
        printf 'basicConstraints=CA:FALSE\n'
        printf 'extendedKeyUsage=%s\n' "$3"
        [[ -n "${4:-}" ]] && printf 'subjectAltName=%s\n' "$4"
    } > "$CERTS/$1.ext"
    openssl x509 -req -in "$CERTS/$1.csr" -CA "$CERTS/ca.pem" -CAkey "$CERTS/ca.key" \
        -CAcreateserial -days 1 -extfile "$CERTS/$1.ext" \
        -out "$CERTS/$1.crt" >/dev/null 2>&1
    # Key first, then the certificate: that is the order reqwest's rustls
    # backend expects, and getting it the other way round fails at
    # `Client::builder().build()` with nothing but "builder error" to go on.
    cat "$CERTS/$1.key" "$CERTS/$1.crt" > "$CERTS/$1.pem"
}

make_cert server "/CN=127.0.0.1" serverAuth "IP:127.0.0.1,DNS:localhost"
make_cert client "/CN=1231234567" clientAuth

python3 "$ROOT/tests/integration/swish-stub.py" "$SWISHPORT" "$CERTS" \
    > "$WORKDIR/stub.log" 2>&1 &
stub_pid=$!
for _ in $(seq 1 40); do
    grep -q "swish stub on" "$WORKDIR/stub.log" 2>/dev/null && break
    sleep 0.25
done
grep -q "swish stub on" "$WORKDIR/stub.log" || {
    echo "the stub did not start"; cat "$WORKDIR/stub.log"; exit 1; }

# The handshake, before anything else depends on it. A client with no
# certificate must be refused — that is Swish's entire authentication, and a
# stub that let it through would make every later assertion meaningless.
if curl -sS --max-time 5 --cacert "$CERTS/ca.pem" \
        "https://127.0.0.1:$SWISHPORT/api/v2/paymentrequests/X" >/dev/null 2>&1; then
    fail "the stub accepted a client with no certificate"
else
    pass "mutual TLS is enforced: no certificate, no conversation"
fi

# --- database and API -------------------------------------------------------
mkdir -p "$SOCKET"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET -p $PGPORT -h 127.0.0.1" \
    -l "$WORKDIR/pg.log" start >/dev/null
PSQL=("$PGBIN/psql" -h 127.0.0.1 -p "$PGPORT" -U postgres -v ON_ERROR_STOP=1 -tAq)
"${PSQL[@]}" -d postgres -c "CREATE DATABASE $DB" >/dev/null
for migration in "$ROOT"/migrations/*.sql; do
    "${PSQL[@]}" -d "$DB" -f "$migration" >/dev/null
done
"${PSQL[@]}" -d "$DB" -c "ALTER ROLE skattjakt_app LOGIN PASSWORD 'sw'" >/dev/null

env DATABASE_URL="postgres://skattjakt_app:sw@127.0.0.1:$PGPORT/$DB" \
    SKATTJAKT_ADMIN_TOKEN="$ADMIN_TOKEN" \
    SKATTJAKT_BLOB_ROOT="$WORKDIR/blobs" \
    PORT="$APIPORT" \
    SKATTJAKT_SWISH_PAYEE_ALIAS=1231234567 \
    SKATTJAKT_SWISH_BASE_URL="https://127.0.0.1:$SWISHPORT" \
    SKATTJAKT_SWISH_CLIENT_PEM="$CERTS/client.pem" \
    SKATTJAKT_SWISH_CA_PEM="$CERTS/ca.pem" \
    SKATTJAKT_SWISH_CALLBACK_URL="https://example.test/v1/payments/swish/callback" \
    SKATTJAKT_PAYMENTS_REQUIRED=1 \
    SKATTJAKT_MERCHANT_NAME="Skattjakt Sverige AB" \
    SKATTJAKT_MERCHANT_ORG_NUMBER="559999-1234" \
    SKATTJAKT_MERCHANT_ADDRESS="Exempelgatan 1, 111 22 Stockholm" \
    SKATTJAKT_MERCHANT_EMAIL="hej@skattjakt.se" \
    SKATTJAKT_MERCHANT_VAT_REGISTERED=1 \
    "$API" > "$WORKDIR/api.log" 2>&1 &
api_pid=$!
for _ in $(seq 1 80); do
    curl -sf "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && break
    sleep 0.25
done
curl -sf "http://127.0.0.1:$APIPORT/health" >/dev/null || {
    echo "the API did not start"; tail -10 "$WORKDIR/api.log"; exit 1; }

api() { curl -sS --max-time 30 -X "$1" "http://127.0.0.1:$APIPORT$2" \
    -H "authorization: Bearer $3" -H 'content-type: application/json' ${4:+-d "$4"}; }
field() { python3 -c "import json,sys;print(json.load(sys.stdin).get('$1',''))"; }
stub() { curl -sS --max-time 10 --cacert "$CERTS/ca.pem" \
    --cert "$CERTS/client.crt" --key "$CERTS/client.key" "$@"; }

CREATED="$(api POST /v1/companies "$ADMIN_TOKEN" '{"company":{"name":"Swishbolaget AB",
  "org_number":"556016-0680","fiscal_year":{"start":"2025-01-01","end":"2025-12-31"}}}')"
TOKEN="$(field api_token <<<"$CREATED")"
[[ -n "$TOKEN" ]] || { echo "no company token"; tail -5 "$WORKDIR/api.log"; exit 1; }

# Uploaded before the gate is tested against it: a request naming a document
# that does not exist dies on the document, and would pass the payment check for
# entirely the wrong reason.
DOC="$(api POST /v1/documents "$TOKEN" '{"filename":"b.txt","mime_type":"text/plain",
  "text":"RESULTATRÄKNING\nNettoomsättning 5 000 000\nSkattemässigt resultat 900 000\n",
  "kind":"annual_accounts"}')"
DV="$(field document_version_id <<<"$DOC")"
[[ -n "$DV" ]] || { echo "no document: $DOC"; exit 1; }

echo
echo "creating a payment, over mutual TLS"
ORDER="$(api POST /v1/orders "$TOKEN" \
    '{"product":"company_analysis","delivery":"immediate",
      "accepts_loss_of_cancellation_right":true}')"
ORDER_ID="$(field order_id <<<"$ORDER")"
SWISH_TOKEN="$(field swish_token <<<"$ORDER")"

[[ -n "$ORDER_ID" ]] && pass "the order was created" \
    || { fail "no order: $ORDER"; tail -5 "$WORKDIR/api.log"; }
[[ -n "$SWISH_TOKEN" ]] && pass "the token came back in the paymentrequesttoken header" \
    || fail "no swish token — the header name or the response shape is wrong"
check "the order is awaiting the payer" awaiting_payment "$(field state <<<"$ORDER")"

echo
echo "what the client actually sent"
# The half of a wire format a stub would otherwise never check: not what came
# back, but what went out.
SEEN="$(stub -X POST "https://127.0.0.1:$SWISHPORT/_seen")"
python3 - "$SEEN" <<'PY' && pass "the request matches the documented v2 shape" \
    || fail "the request body does not match the documented v2 shape"
import json, sys
seen = json.loads(sys.argv[1])
puts = [s for s in seen if s["method"] == "PUT"]
assert len(puts) == 1, f"expected one PUT, saw {len(puts)}"
put = puts[0]
body = put["body"]
# The instruction id: 32 uppercase hex, in the URL rather than the body.
assert len(put["instruction"]) == 32, put["instruction"]
assert put["instruction"] == put["instruction"].upper()
# camelCase, and every field the specification requires.
for key in ("payeeAlias", "amount", "currency", "callbackUrl", "message",
            "payeePaymentReference"):
    assert key in body, f"missing {key}"
assert body["payeeAlias"] == "1231234567", body["payeeAlias"]
assert body["currency"] == "SEK", body["currency"]
# The amount is a decimal string, not öre and not a float.
assert body["amount"] == "69.00", body["amount"]
assert body["callbackUrl"].startswith("https://"), body["callbackUrl"]
PY

PAYMENT_REF="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT provider_reference FROM payments WHERE order_id='$ORDER_ID'")"
check "the payment records the instruction the client sent" 32 "${#PAYMENT_REF}"

echo
echo "settlement can see the payment it settles"
# The check that would have caught the defect this suite found. `payments` is
# FORCE RLS keyed on the current company, and both the callback's lookup and the
# reconciliation sweep run before a company is known. Read directly they matched
# `company_id = NULL` and returned nothing, always — so a customer could pay and
# the order would sit at awaiting_payment forever.
#
# Asserted as the **application role**, because the suite's own psql is a
# superuser and bypasses RLS: checking this as postgres would pass whether or
# not the hole was open, which is exactly how it stayed open.
APP_PSQL=("$PGBIN/psql" "postgres://skattjakt_app:sw@127.0.0.1:$PGPORT/$DB" -v ON_ERROR_STOP=1 -tAq)
FOUND="$("${APP_PSQL[@]}" -c \
    "SELECT company_for_payment_reference('swish', '$PAYMENT_REF') IS NOT NULL")"
check "the callback's lookup finds the payment under RLS" t "$FOUND"
SWEEPABLE="$("${APP_PSQL[@]}" -c \
    "SELECT count(*) >= 1 FROM unsettled_payments(interval '0 seconds', 10)")"
check "and the reconciliation sweep sees something to sweep" t "$SWEEPABLE"

echo
echo "the callback is worth nothing"
# The position the whole crate is built on, now exercised against a payment that
# genuinely exists at the provider: a callback saying PAID, while the provider
# still says CREATED, must move nothing.
curl -sS -o /dev/null -X POST "http://127.0.0.1:$APIPORT/v1/payments/swish/callback" \
    -H 'content-type: application/json' \
    -d "{\"id\":\"$PAYMENT_REF\",\"payeePaymentReference\":\"${ORDER_ID//-/}\",
         \"status\":\"PAID\",\"amount\":69.00,\"currency\":\"SEK\"}"
sleep 1
STATE="$("${PSQL[@]}" -d "$DB" -c "SELECT state FROM orders WHERE id='$ORDER_ID'")"
check "a callback claiming PAID did not make it paid" awaiting_payment "$STATE"

echo
echo "the lookup is worth everything"
stub -X POST "https://127.0.0.1:$SWISHPORT/_control/$PAYMENT_REF/PAID" >/dev/null
curl -sS -o /dev/null -X POST "http://127.0.0.1:$APIPORT/v1/payments/swish/callback" \
    -H 'content-type: application/json' \
    -d "{\"id\":\"$PAYMENT_REF\",\"status\":\"PAID\"}"
for _ in $(seq 1 40); do
    STATE="$("${PSQL[@]}" -d "$DB" -c "SELECT state FROM orders WHERE id='$ORDER_ID'")"
    [[ "$STATE" == "paid" ]] && break
    sleep 0.25
done
check "once the provider says PAID, the order is paid" paid "$STATE"

PAID_AT="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT (paid_at IS NOT NULL) FROM orders WHERE id='$ORDER_ID'")"
check "and it records when" t "$PAID_AT"

echo
echo "a declined payment"
DECLINED_ORDER="$(api POST /v1/orders "$TOKEN" \
    '{"product":"control_review","delivery":"immediate",
      "accepts_loss_of_cancellation_right":true}')"
DECLINED_ID="$(field order_id <<<"$DECLINED_ORDER")"
DECLINED_REF="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT provider_reference FROM payments WHERE order_id='$DECLINED_ID'")"
stub -X POST "https://127.0.0.1:$SWISHPORT/_control/$DECLINED_REF/DECLINED" >/dev/null
curl -sS -o /dev/null -X POST "http://127.0.0.1:$APIPORT/v1/payments/swish/callback" \
    -H 'content-type: application/json' -d "{\"id\":\"$DECLINED_REF\"}"
for _ in $(seq 1 40); do
    STATE="$("${PSQL[@]}" -d "$DB" -c "SELECT state FROM orders WHERE id='$DECLINED_ID'")"
    [[ "$STATE" == "declined" ]] && break
    sleep 0.25
done
check "a declined payment declines the order" declined "$STATE"
check "and it cannot buy an analysis" 402 \
    "$(curl -sS -o /dev/null -w '%{http_code}' --max-time 30 \
        -X POST "http://127.0.0.1:$APIPORT/v1/analyses/stored" \
        -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
        -d "{\"document_version_ids\":[\"$DV\"],
             \"accounts_state\":\"preliminary\",\"order_id\":\"$DECLINED_ID\"}")"

# A callback that names nothing must be counted as such, not as accepted. The
# broken callback this suite found looked perfectly healthy on the dashboard
# precisely because every unresolvable one was recorded as a success.
curl -sS -o /dev/null -X POST "http://127.0.0.1:$APIPORT/v1/payments/swish/callback" \
    -H 'content-type: application/json' \
    -d '{"id":"FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF","status":"PAID"}'
METRICS="$(curl -sS "http://127.0.0.1:$APIPORT/metrics")"
grep -q 'payment_callbacks_total{outcome="unknown"} *[1-9]' <<<"$METRICS" \
    && pass "a callback naming nothing is counted as unknown, not accepted" \
    || fail "an unresolvable callback was not counted apart from a real one"
grep -q 'payment_callbacks_total{outcome="accepted"} *[1-9]' <<<"$METRICS" \
    && pass "and a real one is still counted as accepted" \
    || fail "no callback was counted as accepted"

echo
echo "when the callback never arrives at all"
# The sweep is the guarantee and the callback is the optimisation — that claim
# is the reason the callback endpoint needs no authentication, and until now
# nothing had ever run it. This payment gets no callback of any kind.
#
# The payment is backdated past the grace rather than the test waiting thirty
# seconds for it: a shortcut on the *age of the input*, not on the mechanism,
# which is the same shape as marking an order paid directly.
SWEPT_ORDER="$(api POST /v1/orders "$TOKEN" \
    '{"product":"control_review","delivery":"immediate",
      "accepts_loss_of_cancellation_right":true}')"
SWEPT_ID="$(field order_id <<<"$SWEPT_ORDER")"
SWEPT_REF="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT provider_reference FROM payments WHERE order_id='$SWEPT_ID'")"
"${PSQL[@]}" -d "$DB" -c \
    "UPDATE payments SET created_at = now() - interval '5 minutes' WHERE order_id='$SWEPT_ID'" >/dev/null
stub -X POST "https://127.0.0.1:$SWISHPORT/_control/$SWEPT_REF/PAID" >/dev/null

# The worker inherits the same Swish configuration; it is the process that owns
# the sweep. `tokio::time::interval` fires its first tick immediately, so this
# does not wait out a cycle.
env DATABASE_URL="postgres://skattjakt_app:sw@127.0.0.1:$PGPORT/$DB" \
    SKATTJAKT_BLOB_ROOT="$WORKDIR/blobs" \
    SKATTJAKT_SWISH_PAYEE_ALIAS=1231234567 \
    SKATTJAKT_SWISH_BASE_URL="https://127.0.0.1:$SWISHPORT" \
    SKATTJAKT_SWISH_CLIENT_PEM="$CERTS/client.pem" \
    SKATTJAKT_SWISH_CA_PEM="$CERTS/ca.pem" \
    SKATTJAKT_SWISH_CALLBACK_URL="https://example.test/v1/payments/swish/callback" \
    HOSTNAME=swish-worker-1 \
    "$WORKER" > "$WORKDIR/worker.log" 2>&1 &
worker_pid=$!
for _ in $(seq 1 80); do
    STATE="$("${PSQL[@]}" -d "$DB" -c "SELECT state FROM orders WHERE id='$SWEPT_ID'")"
    [[ "$STATE" == "paid" ]] && break
    sleep 0.5
done
check "the sweep settles a payment nobody was told about" paid "$STATE"

LOOKUPS="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT lookups >= 1 FROM payments WHERE order_id='$SWEPT_ID'")"
check "and it settled by asking, not by assuming" t "$LOOKUPS"

echo
echo "the paid order buys its analysis"
STARTED="$(api POST /v1/analyses/stored "$TOKEN" \
    "{\"document_version_ids\":[\"$DV\"],\"accounts_state\":\"preliminary\",
      \"order_id\":\"$ORDER_ID\"}")"
ANALYSIS="$(field analysis_id <<<"$STARTED")"
[[ -n "$ANALYSIS" ]] && pass "the analysis was accepted against the paid order" \
    || fail "the paid order did not buy an analysis: $STARTED"

AUDIENCE="$("${PSQL[@]}" -d "$DB" -c "SELECT audience FROM analysis_jobs WHERE id='$ANALYSIS'")"
check "and it is stamped with what was bought" company "$AUDIENCE"

echo
echo "nothing secret reached the logs"
# The certificate is the whole of the credential. A log line carrying it, or the
# token that starts a payment, would be the one leak that matters here.
if grep -qE "BEGIN (RSA )?PRIVATE KEY|$SWISH_TOKEN" "$WORKDIR/api.log"; then
    fail "a private key or a payment token reached the log"
else
    pass "no key material or payment token in the log"
fi

printf '\npassed %d, failed %d\n' "$passed" "$failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "the Swish wire format works over real mutual TLS"

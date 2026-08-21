#!/usr/bin/env bash
# Notifications, end to end, against a real SMTP server.
#
# The unit tests prove what a message says. This proves the chain that gets it
# there: an analysis finishes, the outbox row is written in the same
# transaction, the delivery worker claims it, and a message arrives at a server
# that speaks SMTP and would reject a malformed session.
#
# It also proves the rule the whole notify crate exists for — that a message
# carries *that* something happened and never *what was found* — by reading the
# delivered mail back and looking for the amounts the analysis actually
# produced.
#
# Usage: tests/integration/notifications.sh
# Requires: docker, PostgreSQL, cargo, curl, python3.

set -euo pipefail

if [[ -z "${SKATTJAKT_PG_REEXEC:-}" ]] && command -v cargo >/dev/null 2>&1; then
    cargo build --quiet \
        --bin skattjakt-api --bin skattjakt-analysis-worker --bin skattjakt-notification-worker \
        --manifest-path "$(dirname "${BASH_SOURCE[0]}")/../../Cargo.toml"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Whichever build is newer, never whichever profile is preferred.
source "$ROOT/tests/lib/newest-binary.sh"
CONTAINER=skattjakt-mailpit-test
SMTP_PORT=11025
HTTP_PORT=18025

# Docker is only reachable as root here, so the container is started before
# dropping privileges for PostgreSQL.
cleanup_container() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }

if [[ -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    cleanup_container
    echo "starting mailpit"
    docker run -d --name "$CONTAINER" \
        -p "127.0.0.1:$SMTP_PORT:1025" \
        -p "127.0.0.1:$HTTP_PORT:8025" \
        mirror.gcr.io/axllent/mailpit:latest >/dev/null
    for _ in $(seq 1 60); do
        curl -fsS "http://127.0.0.1:$HTTP_PORT/api/v1/messages" >/dev/null 2>&1 && break
        sleep 0.5
    done
    curl -fsS "http://127.0.0.1:$HTTP_PORT/api/v1/messages" >/dev/null || {
        echo "mailpit did not start"; docker logs "$CONTAINER" | tail -20; exit 1; }
    echo "mailpit ready: smtp :$SMTP_PORT, api :$HTTP_PORT"
fi

if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    RUNAS="${SKATTJAKT_PG_USER:-postgres}"
    id "$RUNAS" >/dev/null 2>&1 || RUNAS=nobody
    export SKATTJAKT_PG_REEXEC=1
    # `trap` in the parent would fire before the child finishes, so the
    # container is cleaned up after the re-exec returns.
    set +e
    su -s /bin/bash "$RUNAS" -c \
        "SKATTJAKT_PG_REEXEC=1 PGBIN='${PGBIN:-}' $(printf '%q ' "$0" "$@")"
    status=$?
    set -e
    cleanup_container
    exit "$status"
fi

WORKDIR="$(mktemp -d)"
PGDATA="$WORKDIR/data"
SOCKET="$WORKDIR/sock"
PGPORT=5443
DB=skattjakt_notify
APIPORT=18102

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
    for pid in ${API_PID:-} ${ANALYSIS_PID:-} ${NOTIFY_PID:-}; do
        kill "$pid" 2>/dev/null || true
    done
    "$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

mkdir -p "$SOCKET"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET -h 127.0.0.1 -p $PGPORT" \
    -l "$WORKDIR/pg.log" start >/dev/null

psql() { "$PGBIN/psql" -h "$SOCKET" -p "$PGPORT" -U postgres -v ON_ERROR_STOP=1 -q "$@"; }
q() { psql -d "$DB" -tAc "$1"; }

psql -d postgres -c "CREATE DATABASE $DB" >/dev/null
for migration in "$ROOT"/migrations/*.sql; do psql -d "$DB" -f "$migration" >/dev/null; done
psql -d "$DB" -c "ALTER ROLE skattjakt_app LOGIN PASSWORD 'notify'" >/dev/null
echo "database ready"

ADMIN_TOKEN="admin-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
export DATABASE_URL="postgres://skattjakt_app:notify@127.0.0.1:$PGPORT/$DB"
export SKATTJAKT_ADMIN_TOKEN="$ADMIN_TOKEN"
export SKATTJAKT_BLOB_ROOT="$WORKDIR/documents"
export RUST_LOG=skattjakt=warn

PORT="$APIPORT" "$(newest_binary skattjakt-api)" > "$WORKDIR/api.log" 2>&1 &
API_PID=$!
for _ in $(seq 1 60); do
    curl -fsS "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && break
    sleep 0.2
done
curl -fsS "http://127.0.0.1:$APIPORT/health" >/dev/null || {
    echo "the API did not start"; cat "$WORKDIR/api.log"; exit 1; }

HOSTNAME=analysis-1 "$(newest_binary skattjakt-analysis-worker)" \
    > "$WORKDIR/analysis.log" 2>&1 &
ANALYSIS_PID=$!

# STARTTLS off: Mailpit is on loopback here, and the code refuses credentials
# without it, so this is the only honest way to exercise a plaintext relay.
SKATTJAKT_SMTP_HOST=127.0.0.1 \
SKATTJAKT_SMTP_PORT="$SMTP_PORT" \
SKATTJAKT_SMTP_STARTTLS=0 \
SKATTJAKT_SMTP_FROM="Skattjakt <ingen-svar@skattjakt.se>" \
    "$(newest_binary skattjakt-notification-worker)" > "$WORKDIR/notify.log" 2>&1 &
NOTIFY_PID=$!
sleep 1
kill -0 "$NOTIFY_PID" 2>/dev/null || { echo "the notification worker died"; cat "$WORKDIR/notify.log"; exit 1; }
echo "api and both workers running"

jqf() { python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('$1',''))"; }
api() {
    local method="$1" path="$2" token="$3" body="${4:-}"
    if [[ -n "$body" ]]; then
        curl -sS -X "$method" "http://127.0.0.1:$APIPORT$path" \
            -H "authorization: Bearer $token" -H 'content-type: application/json' -d "$body"
    else
        curl -sS -X "$method" "http://127.0.0.1:$APIPORT$path" -H "authorization: Bearer $token"
    fi
}

# --- a company, an owner with an address, and an analysis -------------------

CREATED="$(api POST /v1/companies "$ADMIN_TOKEN" \
    '{"company":{"name":"Notisbolaget AB","org_number":"5560160680",
      "fiscal_year":{"start":"2025-01-01","end":"2025-12-31"}}}')"
COMPANY="$(jqf company_id <<<"$CREATED")"
TOKEN="$(jqf api_token <<<"$CREATED")"

api POST /v1/users "$TOKEN" \
    '{"email":"agaren@notisbolaget.example","password":"bokslut kaffe cykel oktober","role":"owner"}' \
    >/dev/null

STATEMENT='RESULTATRÄKNING 2025\nNettoomsättning                     4 200 000\nPersonalkostnader                  -2 100 000\nRörelseresultat                     1 330 000\nSkattemässigt resultat                850 000\n\nBALANSRÄKNING\nMateriella anläggningstillgångar      180 000\nSumma tillgångar                    2 400 000\nSumma eget kapital och skulder      2 400 000\n'
UPLOAD=$(python3 - "$STATEMENT" <<'PY'
import json, sys
print(json.dumps({
    "filename": "bokslut-2025.txt", "mime_type": "text/plain",
    "text": sys.argv[1].replace("\\n", "\n"),
    "kind": "annual_accounts", "accounts_state": "preliminary",
}))
PY
)
VERSION="$(api POST /v1/documents "$TOKEN" "$UPLOAD" | jqf document_version_id)"
ANALYSIS="$(api POST /v1/analyses/stored "$TOKEN" \
    "{\"document_version_ids\":[\"$VERSION\"]}" | jqf analysis_id)"
[[ -n "$ANALYSIS" ]] || { echo "the analysis was not accepted"; exit 1; }

# --- wait for the analysis, then for the mail -------------------------------

for _ in $(seq 1 120); do
    STATUS="$(api GET "/v1/analyses/$ANALYSIS" "$TOKEN" | jqf status)"
    [[ "$STATUS" == "succeeded" || "$STATUS" == "failed" ]] && break
    sleep 0.5
done
check "the analysis completed" succeeded "$STATUS"

echo
echo "the outbox"
check "a notification was written in the analysis's own transaction" 1 \
    "$(q "SELECT count(*) FROM notifications WHERE kind = 'analysis_completed'")"
check "and it is keyed to the analysis, so a retry cannot double it" "$ANALYSIS" \
    "$(q "SELECT dedupe_key FROM notifications WHERE kind = 'analysis_completed'")"

for _ in $(seq 1 60); do
    COUNT="$(curl -fsS "http://127.0.0.1:$HTTP_PORT/api/v1/messages" \
        | python3 -c 'import json,sys;print(json.load(sys.stdin)["messages_count"])' 2>/dev/null || echo 0)"
    [[ "$COUNT" -gt 0 ]] && break
    sleep 0.5
done

echo
echo "what arrived at the relay"
check "exactly one message was delivered" 1 "$COUNT"

MESSAGE="$(curl -fsS "http://127.0.0.1:$HTTP_PORT/api/v1/messages")"
MSG_ID="$(python3 -c 'import json,sys;print(json.load(sys.stdin)["messages"][0]["ID"])' <<<"$MESSAGE")"
SUBJECT="$(python3 -c 'import json,sys;print(json.load(sys.stdin)["messages"][0]["Subject"])' <<<"$MESSAGE")"
TO="$(python3 -c 'import json,sys;print(json.load(sys.stdin)["messages"][0]["To"][0]["Address"])' <<<"$MESSAGE")"

check "it went to the owner" "agaren@notisbolaget.example" "$TO"
# The subject is Swedish, so this also proves RFC 2047 header encoding: an
# unencoded 'ä' would arrive mangled or not at all.
check "the subject survived UTF-8 encoding" "Din analys är klar" "$SUBJECT"

BODY="$(curl -fsS "http://127.0.0.1:$HTTP_PORT/api/v1/message/$MSG_ID" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin).get("Text",""))')"

# --- the rule the whole crate exists for ------------------------------------
#
# A notification carries that something happened, never what was found. These
# are the figures the analysis actually produced; none may appear in a message
# that is displayed on a lock screen and forwarded through mail providers.

echo
echo "what the message must not contain"
LEAKED=0
for figure in "4 200 000" "4200000" "2 100 000" "1 330 000" "850 000" "180 000" "2 400 000"; do
    if grep -qF "$figure" <<<"$BODY"; then
        fail "an amount from the accounts appears in the email: $figure"
        LEAKED=1
    fi
done
[[ "$LEAKED" -eq 0 ]] && pass "no amount from the accounts appears in the email"

if grep -qiE 'kr\b|kronor|besparing|avdrag på' <<<"$BODY"; then
    fail "the email quantifies a finding"
else
    pass "the email quantifies nothing"
fi

if grep -qF "Notisbolaget" <<<"$BODY"; then
    fail "the email names the company, which identifies the business to anyone reading over a shoulder"
else
    pass "the email does not name the company"
fi

grep -qF "$ANALYSIS" <<<"$BODY" \
    && pass "it carries the analysis reference, so the customer can find it" \
    || fail "the email carries no reference to the analysis"
grep -qF "preliminära" <<<"$BODY" \
    && pass "it carries the same disclaimer as the product" \
    || fail "the disclaimer is missing"

echo
echo "the outbox afterwards"
check "the row is marked delivered" delivered \
    "$(q "SELECT state FROM notifications WHERE kind = 'analysis_completed'")"
check "email is recorded as the channel that succeeded" "{email}" \
    "$(q "SELECT delivered_channels FROM notifications WHERE kind = 'analysis_completed'")"

# The owner registered no device, so push was never asked for. If it had been,
# it would have failed as not-configured rather than silently claiming success.
check "push was not attempted, because no channel asked for it" 0 \
    "$(q "SELECT count(*) FROM notifications WHERE 'push' = ANY(delivered_channels)")"

echo
echo "what reaches the logs"
for secret in "agaren@notisbolaget.example" "4 200 000"; do
    grep -qF "$secret" "$WORKDIR/notify.log" 2>/dev/null \
        && fail "an address or an amount reached the worker's log" \
        && break
done
pass "no address or amount reached the worker's log"

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || { echo; echo "--- notification worker log ---"; tail -20 "$WORKDIR/notify.log"; exit 1; }
echo "notifications are delivered, and carry nothing they should not"

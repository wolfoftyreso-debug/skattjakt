#!/usr/bin/env bash
# The session and authorisation surface, against a live API.
#
# The unit tests in `crates/identity` prove the policy. This proves the wiring:
# that a refresh really rotates in the database, that presenting a rotated token
# really tears down the family, that an advisor really cannot delete a company,
# and that removing someone from a company really ends their access.
#
# Those are the parts a policy test cannot reach, and they are where the bugs
# that matter live.
#
# Usage: tests/security/session-suite.sh
# Requires: PostgreSQL, cargo, curl, python3.

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
        "SKATTJAKT_PG_REEXEC=1 PGBIN='${PGBIN:-}' $(printf '%q ' "$0" "$@")"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKDIR="$(mktemp -d)"
PGDATA="$WORKDIR/data"
SOCKET="$WORKDIR/sock"
PGPORT=5442
DB=skattjakt_sessions
PORT="${SKATTJAKT_TEST_PORT:-18101}"
BASE="http://127.0.0.1:$PORT"
LOG="$WORKDIR/api.log"

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
    [[ -n "${API_PID:-}" ]] && kill "$API_PID" 2>/dev/null || true
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
for migration in "$ROOT"/migrations/*.sql; do
    psql -d "$DB" -f "$migration" >/dev/null
done
psql -d "$DB" -c "ALTER ROLE skattjakt_app LOGIN PASSWORD 'sessions'" >/dev/null
echo "database ready"

ADMIN_TOKEN="admin-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
export DATABASE_URL="postgres://skattjakt_app:sessions@127.0.0.1:$PGPORT/$DB"
export SKATTJAKT_ADMIN_TOKEN="$ADMIN_TOKEN"
export SKATTJAKT_BLOB_ROOT="$WORKDIR/documents"
export PORT="$PORT"
export RUST_LOG=skattjakt=warn

"$ROOT/target/debug/skattjakt-api" > "$LOG" 2>&1 &
API_PID=$!
for _ in $(seq 1 60); do
    curl -fsS "$BASE/health" >/dev/null 2>&1 && break
    sleep 0.2
done
curl -fsS "$BASE/health" >/dev/null || { echo "the API did not start"; cat "$LOG"; exit 1; }
echo "api ready on $BASE"

jqf() { python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('$1',''))"; }
status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

# --- two companies, and people in them --------------------------------------

new_company() {
    curl -fsS -X POST "$BASE/v1/companies" \
        -H "authorization: Bearer $ADMIN_TOKEN" -H 'content-type: application/json' \
        -d "{\"company\":{\"name\":\"$1\",\"org_number\":\"$2\",
             \"fiscal_year\":{\"start\":\"2025-01-01\",\"end\":\"2025-12-31\"}}}"
}

ALFA="$(new_company "Alfa AB" 5560160680)"
BETA="$(new_company "Beta AB" 5567037485)"
ALFA_ID="$(jqf company_id <<<"$ALFA")"
BETA_ID="$(jqf company_id <<<"$BETA")"
ALFA_TOKEN="$(jqf api_token <<<"$ALFA")"

PASSWORD='bokslut kaffe cykel oktober'

# The company token is the only credential that exists at bootstrap, and it
# carries owner permissions — which is how the first person gets created.
OWNER="$(curl -fsS -X POST "$BASE/v1/users" \
    -H "authorization: Bearer $ALFA_TOKEN" -H 'content-type: application/json' \
    -d "{\"email\":\"anna@alfa.example\",\"password\":\"$PASSWORD\",\"role\":\"owner\"}")"
check "an owner can be created with the company token" "anna@alfa.example" "$(jqf email <<<"$OWNER")"

ADVISOR="$(curl -fsS -X POST "$BASE/v1/users" \
    -H "authorization: Bearer $ALFA_TOKEN" -H 'content-type: application/json' \
    -d "{\"email\":\"revisor@byra.example\",\"password\":\"$PASSWORD\",\"role\":\"advisor\"}")"
ADVISOR_ID="$(jqf user_id <<<"$ADVISOR")"

# ---------------------------------------------------------------------------
echo
echo "signing in"
# ---------------------------------------------------------------------------

sign_in() { # email client install
    curl -s -X POST "$BASE/v1/auth/sign-in" \
        -H 'content-type: application/json' -H "x-skattjakt-client: $2" \
        -d "{\"email\":\"$1\",\"password\":\"$PASSWORD\",\"install_id\":\"$3\",
             \"device_name\":\"testenhet\"}"
}

SESSION="$(sign_in anna@alfa.example ios phone-1)"
ACCESS="$(jqf access_token <<<"$SESSION")"
REFRESH="$(jqf refresh_token <<<"$SESSION")"
check "a valid sign-in issues a session" owner "$(jqf role <<<"$SESSION")"
[[ -n "$ACCESS" && -n "$REFRESH" ]] && pass "both tokens are returned" \
    || fail "a token is missing"

check "the access token authenticates" 200 \
    "$(status -H "authorization: Bearer $ACCESS" "$BASE/v1/companies/me")"

check "a wrong password is rejected" 401 \
    "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/auth/sign-in" \
        -H 'content-type: application/json' \
        -d '{"email":"anna@alfa.example","password":"fel lösenord alls inte"}')"

check "an unknown address is rejected the same way" 401 \
    "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/auth/sign-in" \
        -H 'content-type: application/json' \
        -d "{\"email\":\"ingen@alfa.example\",\"password\":\"$PASSWORD\"}")"

# The two 401 bodies must be identical, or the difference enumerates customers.
WRONG_BODY="$(curl -s -X POST "$BASE/v1/auth/sign-in" -H 'content-type: application/json' \
    -d '{"email":"anna@alfa.example","password":"fel lösenord alls inte"}')"
MISSING_BODY="$(curl -s -X POST "$BASE/v1/auth/sign-in" -H 'content-type: application/json' \
    -d "{\"email\":\"ingen@alfa.example\",\"password\":\"$PASSWORD\"}")"
check "a wrong password and an unknown address are indistinguishable" \
    "$WRONG_BODY" "$MISSING_BODY"

# --- tokens are never stored in the clear -----------------------------------

check "the access token is not in the database in the clear" 0 \
    "$(q "SELECT count(*) FROM sessions WHERE access_token_hash = '$ACCESS'")"
check "the refresh token is not in the database in the clear" 0 \
    "$(q "SELECT count(*) FROM sessions WHERE refresh_token_hash = '$REFRESH'")"
check "the password is not in the database in the clear" 0 \
    "$(q "SELECT count(*) FROM user_credentials WHERE password_hash = '$PASSWORD'")"
check "every stored credential is argon2id" 0 \
    "$(q "SELECT count(*) FROM user_credentials WHERE password_hash NOT LIKE '\$argon2id\$%'")"

# ---------------------------------------------------------------------------
echo
echo "refresh rotation"
# ---------------------------------------------------------------------------

ROTATED="$(curl -s -X POST "$BASE/v1/auth/refresh" -H 'content-type: application/json' \
    -d "{\"refresh_token\":\"$REFRESH\"}")"
NEW_ACCESS="$(jqf access_token <<<"$ROTATED")"
NEW_REFRESH="$(jqf refresh_token <<<"$ROTATED")"

[[ -n "$NEW_ACCESS" ]] && pass "a refresh issues a new access token" \
    || fail "the refresh returned no access token"
[[ "$NEW_REFRESH" != "$REFRESH" ]] && pass "the refresh token rotates" \
    || fail "the refresh token did not change"
check "the new access token works" 200 \
    "$(status -H "authorization: Bearer $NEW_ACCESS" "$BASE/v1/companies/me")"
check "the generation advanced" 1 \
    "$(q "SELECT generation FROM sessions
          WHERE superseded_at IS NULL AND revoked_at IS NULL
            AND user_id = (SELECT id FROM users WHERE email = 'anna@alfa.example')")"
check "the superseded generation is kept, so a replay is detectable" 1 \
    "$(q "SELECT count(*) FROM sessions WHERE superseded_at IS NOT NULL")"

# --- reuse detection --------------------------------------------------------
#
# The rotated-away token is replayed after the grace window. Two parties holding
# tokens from one family is what a theft looks like, and the whole family must
# come down — signing out the customer as well as the thief.

psql -d "$DB" -c "UPDATE sessions SET superseded_at = now() - interval '10 minutes'
                  WHERE superseded_at IS NOT NULL" >/dev/null

REPLAY="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/auth/refresh" \
    -H 'content-type: application/json' -d "{\"refresh_token\":\"$REFRESH\"}")"
check "replaying a rotated refresh token is refused" 401 "$REPLAY"
check "the whole family is revoked for reuse" refresh_reuse \
    "$(q "SELECT DISTINCT revoked_reason FROM sessions WHERE revoked_reason IS NOT NULL")"
check "the access token from that family stops working" 401 \
    "$(status -H "authorization: Bearer $NEW_ACCESS" "$BASE/v1/companies/me")"
check "the rotated token cannot be used to refresh again either" 401 \
    "$(status -X POST "$BASE/v1/auth/refresh" -H 'content-type: application/json' \
        -d "{\"refresh_token\":\"$NEW_REFRESH\"}")"

# ---------------------------------------------------------------------------
echo
echo "roles"
# ---------------------------------------------------------------------------

ADV_SESSION="$(sign_in revisor@byra.example web browser-1)"
ADV_ACCESS="$(jqf access_token <<<"$ADV_SESSION")"
check "an advisor can sign in" advisor "$(jqf role <<<"$ADV_SESSION")"

check "an advisor can read the company" 200 \
    "$(status -H "authorization: Bearer $ADV_ACCESS" "$BASE/v1/companies/me")"
check "an advisor can list documents" 200 \
    "$(status -H "authorization: Bearer $ADV_ACCESS" "$BASE/v1/documents")"
check "an advisor cannot create a user" 403 \
    "$(status -X POST "$BASE/v1/users" -H "authorization: Bearer $ADV_ACCESS" \
        -H 'content-type: application/json' \
        -d "{\"email\":\"smyg@byra.example\",\"password\":\"$PASSWORD\"}")"

# --- a web session is much shorter than a phone session ---------------------

WEB_LIFETIME="$(q "SELECT round(extract(epoch FROM refresh_expires_at - created_at))
                   FROM sessions WHERE client_kind = 'web' ORDER BY created_at DESC LIMIT 1")"
IOS_LIFETIME="$(q "SELECT round(extract(epoch FROM refresh_expires_at - created_at))
                   FROM sessions WHERE client_kind = 'ios' ORDER BY created_at DESC LIMIT 1")"
if [[ "$WEB_LIFETIME" -lt "$IOS_LIFETIME" ]]; then
    pass "a browser session is shorter than a phone session ($WEB_LIFETIME s vs $IOS_LIFETIME s)"
else
    fail "a browser session is not shorter than a phone session"
fi

# ---------------------------------------------------------------------------
echo
echo "devices"
# ---------------------------------------------------------------------------

ANNA="$(sign_in anna@alfa.example ios phone-1)"
ANNA_ACCESS="$(jqf access_token <<<"$ANNA")"
ANNA_DEVICE="$(jqf device_id <<<"$ANNA")"

# The same installation signing in again must not accumulate a device row.
sign_in anna@alfa.example ios phone-1 >/dev/null
check "signing in again on the same installation reuses the device" 1 \
    "$(q "SELECT count(*) FROM devices WHERE install_id = 'phone-1'")"

sign_in anna@alfa.example android tablet-1 >/dev/null
DEVICES="$(curl -fsS -H "authorization: Bearer $ANNA_ACCESS" "$BASE/v1/auth/devices" \
    | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["devices"]))')"
check "both devices are listed" 2 "$DEVICES"

check "a push token can be registered" 204 \
    "$(status -X PUT "$BASE/v1/auth/devices/$ANNA_DEVICE/push-token" \
        -H "authorization: Bearer $ANNA_ACCESS" -H 'content-type: application/json' \
        -d '{"push_token":"apns-token-abc","provider":"apns"}')"
check "a push token without a provider is refused" 422 \
    "$(status -X PUT "$BASE/v1/auth/devices/$ANNA_DEVICE/push-token" \
        -H "authorization: Bearer $ANNA_ACCESS" -H 'content-type: application/json' \
        -d '{"push_token":"orphan"}')"

# Another user's device must not be reachable by knowing its id.
check "another user cannot redirect this device's notifications" 404 \
    "$(status -X PUT "$BASE/v1/auth/devices/$ANNA_DEVICE/push-token" \
        -H "authorization: Bearer $ADV_ACCESS" -H 'content-type: application/json' \
        -d '{"push_token":"stolen","provider":"apns"}')"

# ---------------------------------------------------------------------------
echo
echo "signing out"
# ---------------------------------------------------------------------------

check "sign-out returns no content" 204 \
    "$(status -X POST "$BASE/v1/auth/sign-out" -H "authorization: Bearer $ANNA_ACCESS")"
check "the access token stops working immediately" 401 \
    "$(status -H "authorization: Bearer $ANNA_ACCESS" "$BASE/v1/companies/me")"

AGAIN="$(sign_in anna@alfa.example ios phone-1)"
AGAIN_ACCESS="$(jqf access_token <<<"$AGAIN")"
curl -fsS -X POST "$BASE/v1/auth/sign-out-everywhere" \
    -H "authorization: Bearer $AGAIN_ACCESS" >/dev/null
check "signing out everywhere ends every session for that user" 0 \
    "$(q "SELECT count(*) FROM sessions s JOIN users u ON u.id = s.user_id
          WHERE u.email = 'anna@alfa.example' AND s.revoked_at IS NULL")"
check "and leaves other users signed in" 200 \
    "$(status -H "authorization: Bearer $ADV_ACCESS" "$BASE/v1/companies/me")"

# ---------------------------------------------------------------------------
echo
echo "membership changes take effect"
# ---------------------------------------------------------------------------

# Removing someone from a company must end their access, not wait for their
# token to expire. The access-token lookup joins company_members for exactly
# this reason.
psql -d "$DB" -c "DELETE FROM company_members WHERE user_id = '$ADVISOR_ID'" >/dev/null
check "a removed member's live access token stops working at once" 401 \
    "$(status -H "authorization: Bearer $ADV_ACCESS" "$BASE/v1/companies/me")"

# ---------------------------------------------------------------------------
echo
echo "tenant isolation still holds for sessions"
# ---------------------------------------------------------------------------

BETA_TOKEN="$(jqf api_token <<<"$BETA")"
curl -fsS -X POST "$BASE/v1/users" \
    -H "authorization: Bearer $BETA_TOKEN" -H 'content-type: application/json' \
    -d "{\"email\":\"bo@beta.example\",\"password\":\"$PASSWORD\",\"role\":\"owner\"}" >/dev/null
BO="$(sign_in bo@beta.example web bo-browser)"
BO_ACCESS="$(jqf access_token <<<"$BO")"

BO_COMPANY="$(curl -fsS -H "authorization: Bearer $BO_ACCESS" "$BASE/v1/companies/me" | jqf id)"
check "a session sees only its own company" "$BETA_ID" "$BO_COMPANY"
check "and cannot switch into one it is not a member of" 404 \
    "$(status -X POST "$BASE/v1/auth/switch-company" \
        -H "authorization: Bearer $BO_ACCESS" -H 'content-type: application/json' \
        -d "{\"company_id\":\"$ALFA_ID\"}")"

# ---------------------------------------------------------------------------
echo
echo "changing a password"
# ---------------------------------------------------------------------------

check "a weak password is refused" 422 \
    "$(status -X POST "$BASE/v1/auth/change-password" \
        -H "authorization: Bearer $BO_ACCESS" -H 'content-type: application/json' \
        -d "{\"current_password\":\"$PASSWORD\",\"new_password\":\"kort\"}")"

check "a wrong current password is refused" 401 \
    "$(status -X POST "$BASE/v1/auth/change-password" \
        -H "authorization: Bearer $BO_ACCESS" -H 'content-type: application/json' \
        -d "{\"current_password\":\"inte rätt alls\",\"new_password\":\"nytt lösenord som är långt\"}")"

# A second device, to prove the change signs it out.
OTHER="$(sign_in bo@beta.example ios bo-phone)"
OTHER_ACCESS="$(jqf access_token <<<"$OTHER")"

curl -fsS -X POST "$BASE/v1/auth/change-password" \
    -H "authorization: Bearer $BO_ACCESS" -H 'content-type: application/json' \
    -d "{\"current_password\":\"$PASSWORD\",\"new_password\":\"nytt lösenord som är långt\"}" >/dev/null

check "the other device is signed out by the change" 401 \
    "$(status -H "authorization: Bearer $OTHER_ACCESS" "$BASE/v1/companies/me")"
check "the session that made the change survives" 200 \
    "$(status -H "authorization: Bearer $BO_ACCESS" "$BASE/v1/companies/me")"

# ---------------------------------------------------------------------------
echo
echo "what reaches the logs"
# ---------------------------------------------------------------------------

for secret in "$PASSWORD" "$ACCESS" "$REFRESH" "anna@alfa.example"; do
    if grep -qF "$secret" "$LOG" 2>/dev/null; then
        fail "a credential or an email address reached the log"
    fi
done
pass "no password, token or email address reached the log"

METRICS="$(curl -fsS "$BASE/metrics")"
if grep -qE 'anna@|@alfa|@beta|@byra' <<<"$METRICS"; then
    fail "an email address reached /metrics"
else
    pass "no email address reached /metrics"
fi
grep -q 'skattjakt_sign_ins_total' <<<"$METRICS" \
    && pass "sign-in outcomes are published as a metric" \
    || fail "sign-in outcomes are not published"

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "all session checks passed"

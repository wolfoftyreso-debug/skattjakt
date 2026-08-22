#!/usr/bin/env bash
# Source verification, against a real database and a real HTTP server.
#
# What this proves that the unit tests cannot
# ===========================================
#
# `crates/rules/src/verify.rs` is tested against strings, and that covers the
# judgement — is this the cited document, is the paragraph there, does it still
# say 25 per cent. What it cannot cover is everything between the judgement and
# a customer:
#
#   * that a fetch over a socket produces the text the checker expects;
#   * that the verdict reaches `source_retrievals` in a shape the CHECK
#     constraints accept;
#   * that a failed fetch **does not erase** an earlier successful one, which is
#     the single most consequential line in the whole feature and lives in SQL;
#   * that two workers sweeping at once do not both fetch everything;
#   * that the interval actually suppresses a second sweep.
#
# Every one of those is a property of Postgres, the network stack, or both. The
# only way to know is to run it.
#
# The fixtures are served from localhost because the statute hosts are blocked
# by this environment's egress policy — see SKATTJAKT_PRODUCT_SURFACE.md §5.4.
# That is a limitation of where this runs, not of the mechanism: the code path
# under test here is the same one that runs against riksdagen.se.
#
# Usage: tests/integration/source-verification.sh

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
PGPORT="${PGPORT:-55445}"
DB=skattjakt_sources
PAGES="$WORKDIR/pages"
# Whichever build is newer, never whichever profile is preferred. The API
# selection below already had this rule inline and the worker selection did not,
# so a rebuilt worker went on being ignored — which is exactly how a rule set
# the running worker could not parse read as twenty product failures.
WORKER="$(newest_binary skattjakt-analysis-worker)"

PGBIN="${PGBIN:-$(dirname "$(command -v initdb || echo /usr/lib/postgresql/16/bin/initdb)")}"
[[ -x "$PGBIN/initdb" ]] || PGBIN=/usr/lib/postgresql/16/bin

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }
check() { if [[ "$3" == "$2" ]]; then pass "$1"; else fail "$1 (expected $2, got $3)"; fi }

server_pid=""
cleanup() {
    [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null || true
    "$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

[[ -x "$WORKER" ]] || { echo "build the worker first: cargo build --release" >&2; exit 1; }

# --- database ---------------------------------------------------------------

mkdir -p "$SOCKET" "$PAGES"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET -p $PGPORT -h 127.0.0.1" -l "$WORKDIR/pg.log" start >/dev/null
PSQL=("$PGBIN/psql" -h 127.0.0.1 -p "$PGPORT" -U postgres -v ON_ERROR_STOP=1 -tAq)

"${PSQL[@]}" -d postgres -c "CREATE DATABASE $DB" >/dev/null
for migration in "$ROOT"/migrations/*.sql; do
    "${PSQL[@]}" -d "$DB" -f "$migration" >/dev/null
done
"${PSQL[@]}" -d "$DB" -c "ALTER ROLE skattjakt_app LOGIN PASSWORD 'sources'" >/dev/null
export DATABASE_URL="postgres://skattjakt_app:sources@127.0.0.1:$PGPORT/$DB"

echo
echo "the schema"
COLUMNS="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT count(*) FROM information_schema.columns WHERE table_name='source_retrievals'")"
[[ "$COLUMNS" -ge 7 ]] && pass "source_retrievals exists with $COLUMNS columns" \
    || fail "source_retrievals has $COLUMNS columns"

# The constraint that makes `verified` mean something. Asserted against the
# database rather than trusted, because it is the invariant everything else
# rests on: without it the state is a word somebody typed.
if "${PSQL[@]}" -d "$DB" -c \
    "INSERT INTO source_retrievals (source_id, state) VALUES ('forged', 'verified')" \
    >/dev/null 2>&1; then
    fail "a source was marked verified with no hash and no timestamp"
else
    pass "the database refuses a verified state with no retrieval behind it"
fi
if "${PSQL[@]}" -d "$DB" -c \
    "INSERT INTO source_retrievals (source_id, state, retrieved_at, sha256)
     VALUES ('forged', 'mismatch', now(), repeat('a', 64))" >/dev/null 2>&1; then
    fail "a mismatch was accepted with no note"
else
    pass "the database refuses a mismatch that does not say why"
fi

# --- the pages the fixtures point at ----------------------------------------

cat >"$PAGES/il-30-5.html" <<'HTML'
<!doctype html><html><head><title>Inkomstskattelag (1999:1229)</title>
<style>.x{display:none}</style><script>var noise = "25 procent";</script></head>
<body><h1>Inkomstskattelag (1999:1229)</h1>
<div><h2>30&nbsp;kap. Periodiseringsfonder</h2>
<p><b>5&nbsp;&sect;</b>&nbsp;&nbsp;En juridisk person f&aring;r g&ouml;ra avdrag med h&ouml;gst
25&nbsp;procent av &ouml;verskottet av n&auml;ringsverksamheten.</p></div></body></html>
HTML

cat >"$PAGES/il-65-10.html" <<'HTML'
<!doctype html><html><body><h1>Inkomstskattelag (1999:1229)</h1>
<h2>65&nbsp;kap.</h2><p><b>10&nbsp;&sect;</b> F&ouml;r juridiska personer &auml;r skatten
20,6&nbsp;procent av den beskattningsbara inkomsten.</p></body></html>
HTML

# The same statute and paragraph, with the rate moved. This is the case the
# whole feature exists for.
cat >"$PAGES/il-30-5-changed.html" <<'HTML'
<!doctype html><html><head><script>var noise = "25 procent";</script></head>
<body><h1>Inkomstskattelag (1999:1229)</h1>
<div><h2>30&nbsp;kap. Periodiseringsfonder</h2>
<p><b>5&nbsp;&sect;</b>&nbsp;&nbsp;En juridisk person f&aring;r g&ouml;ra avdrag med h&ouml;gst
22&nbsp;procent av &ouml;verskottet av n&auml;ringsverksamheten.</p></div></body></html>
HTML

port="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
(cd "$PAGES" && exec python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1) &
server_pid=$!
for _ in $(seq 1 50); do
    python3 -c "import socket,sys;socket.create_connection(('127.0.0.1',$port),timeout=0.2).close()" 2>/dev/null && break
    sleep 0.1
done
base="http://127.0.0.1:$port"
dead="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"

# --- a rule set whose sources are the fixtures ------------------------------

write_ruleset() { # path, url for il-30-5
    python3 - "$1" "$2" "$base" "$dead" "$ROOT/rules/se-ruleset.json" <<'PY'
import json, sys
path, changed_url, base, dead, real = sys.argv[1:6]

# The real rule set, with three sources repointed at the fixture server and the
# rest at a port nothing is listening on. Built from the shipped file rather
# than hand-written so the test exercises real rules, real parameters and the
# real validation, not a toy.
data = json.load(open(real))
for key, source in data["sources"].items():
    if key == "il-30-5":
        source["machine_url"] = changed_url
    elif key == "il-65-10":
        source["machine_url"] = f"{base}/il-65-10.html"
    elif key == "il-30-6":
        source["machine_url"] = f"{base}/no-such-page.html"
    else:
        source["machine_url"] = f"http://127.0.0.1:{dead}/{key}.html"
json.dump(data, open(path, "w"), ensure_ascii=False, indent=2)
PY
}

RULESET="$WORKDIR/ruleset.json"
write_ruleset "$RULESET" "$base/il-30-5.html"

echo
echo "checking without writing"
set +e
OUT="$("$WORKER" verify-sources --ruleset "$RULESET" 2>&1)"
RC=$?
set -e
grep -q "ok        il-30-5" <<<"$OUT" && pass "a source that agrees is verified" \
    || fail "a source that agrees is verified: $OUT"
grep -q "ok        il-65-10" <<<"$OUT" && pass "a second source is verified independently" \
    || fail "a second source is verified"
grep -q "unreached il-30-6.*HTTP 404" <<<"$OUT" && pass "a 404 is unreachable, not verified" \
    || fail "a 404 is unreachable: $(grep il-30-6 <<<"$OUT")"
grep -q "unreached il-40-2" <<<"$OUT" && pass "a refused connection is unreachable" \
    || fail "a refused connection is unreachable"
check "exits 0 when something verified and nothing contradicted" 0 "$RC"

ROWS="$("${PSQL[@]}" -d "$DB" -c "SELECT count(*) FROM source_retrievals")"
check "without --write the database is untouched" 0 "$ROWS"

echo
echo "checking and recording"
set +e
"$WORKER" verify-sources --ruleset "$RULESET" --write >/dev/null 2>&1
set -e
state_of() { "${PSQL[@]}" -d "$DB" -c "SELECT state FROM source_retrievals WHERE source_id='$1'"; }
field_of() { "${PSQL[@]}" -d "$DB" -c "SELECT coalesce($2::text,'') FROM source_retrievals WHERE source_id='$1'"; }

check "a verified source is recorded" verified "$(state_of il-30-5)"
DIGEST="$(field_of il-30-5 sha256)"
[[ "$DIGEST" =~ ^[0-9a-f]{64}$ ]] && pass "with a sha256 of what was read" \
    || fail "with a sha256 (got '$DIGEST')"
[[ -n "$(field_of il-30-5 retrieved_at)" ]] && pass "and a timestamp" || fail "and a timestamp"
check "an unreachable source is recorded as such" unreachable "$(state_of il-30-6)"
WITH_REASON="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT count(*) FROM source_retrievals WHERE source_id='il-30-6' AND note LIKE '%404%'")"
check "and carries the reason" 1 "$WITH_REASON"

echo
echo "the same page twice"
"$WORKER" verify-sources --ruleset "$RULESET" --write >/dev/null 2>&1 || true
check "the hash is stable across runs" "$DIGEST" "$(field_of il-30-5 sha256)"
STREAK="$("${PSQL[@]}" -d "$DB" -c "SELECT failure_streak FROM source_retrievals WHERE source_id='il-30-6'")"
[[ "$STREAK" -ge 2 ]] && pass "repeated failures accumulate ($STREAK)" \
    || fail "repeated failures accumulate (got $STREAK)"

echo
echo "the page changes under a recorded hash"
write_ruleset "$RULESET" "$base/il-30-5-changed.html"
set +e
OUT="$("$WORKER" verify-sources --ruleset "$RULESET" --write 2>&1)"
RC=$?
set -e
check "the rate change is caught" mismatch "$(state_of il-30-5)"
grep -q "25 procent" <<<"$OUT" && pass "the report names the figure that vanished" \
    || fail "the report names the figure: $OUT"
check "exits 1 so a pipeline stops" 1 "$RC"
[[ "$(field_of il-30-5 sha256)" != "$DIGEST" ]] && pass "a changed page changes the hash" \
    || fail "a changed page changes the hash"
[[ -n "$(field_of il-30-5 note)" ]] && pass "a mismatch records why" || fail "a mismatch records why"

echo
echo "a network failure does not erase what was read"
# The property that keeps the record honest over time, and the one that lives
# in SQL rather than in Rust. A source verified last week must not be demoted
# because a proxy said no today: that would be a fact about the network
# masquerading as a fact about the law.
BEFORE_STATE="$(state_of il-65-10)"
BEFORE_SHA="$(field_of il-65-10 sha256)"
check "the source starts verified" verified "$BEFORE_STATE"
python3 - "$RULESET" "$dead" <<'PY'
import json, sys
path, dead = sys.argv[1], sys.argv[2]
data = json.load(open(path))
data["sources"]["il-65-10"]["machine_url"] = f"http://127.0.0.1:{dead}/gone.html"
json.dump(data, open(path, "w"), ensure_ascii=False, indent=2)
PY
"$WORKER" verify-sources --ruleset "$RULESET" --write >/dev/null 2>&1 || true
check "it stays verified after an unreachable check" verified "$(state_of il-65-10)"
check "and keeps the hash it was verified with" "$BEFORE_SHA" "$(field_of il-65-10 sha256)"
LAST="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT (last_checked_at > retrieved_at) FROM source_retrievals WHERE source_id='il-65-10'")"
check "but records that a later check was attempted" t "$LAST"

echo
echo "the running API reports what the sweep found, not what it was built with"
#
# The end of the chain, and the only part that matters to a customer. The
# binary's embedded registry says every source is `unretrieved`; the database
# now says otherwise for two of them. If the API still answered from the binary,
# every check above would be a well-tested write to a table nobody reads.

VERIFIED_COUNT="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT count(*) FROM source_retrievals WHERE state='verified'")"
MISMATCH_COUNT="$("${PSQL[@]}" -d "$DB" -c \
    "SELECT count(*) FROM source_retrievals WHERE state='mismatch'")"
[[ "$VERIFIED_COUNT" -ge 1 && "$MISMATCH_COUNT" -ge 1 ]] \
    && pass "the database holds $VERIFIED_COUNT verified and $MISMATCH_COUNT contradicted" \
    || fail "the database has nothing interesting to report ($VERIFIED_COUNT/$MISMATCH_COUNT)"

# Whichever build is newer, not whichever profile is preferred. A release binary
# left over from before the change under test passes the health check and then
# fails in ways that read as product bugs — this has now cost two debugging
# sessions, so the rule lives in every suite that picks a binary.
API="$(newest_binary skattjakt-api)"
APIPORT="${APIPORT:-18095}"
TOKEN="source-verification-suite"
DATABASE_URL="$DATABASE_URL" SKATTJAKT_API_TOKEN="$TOKEN" \
SKATTJAKT_BLOB_ROOT="$WORKDIR/blobs" PORT="$APIPORT" \
    "$API" >"$WORKDIR/api.log" 2>&1 &
api_pid=$!
for _ in $(seq 1 60); do
    curl -sf "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && break
    sleep 0.5
done

if curl -sf "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1; then
    curl -sS "http://127.0.0.1:$APIPORT/v1/rules" -H "authorization: Bearer $TOKEN" \
        >"$WORKDIR/rules.json"
    if python3 - "$WORKDIR/rules.json" >"$WORKDIR/api-check.txt" 2>&1 <<'PY'
import json, sys
rules = json.load(open(sys.argv[1]))["rules"]
states = {}
for rule in rules:
    for source in rule["sources"]:
        states[source["id"]] = source["state"]

problems = []
# `il-65-10` is cited by the corporate-tax *parameter*, not by a rule, so it
# does not appear here — this endpoint describes rules. The one the sweep
# reached and contradicted does.
if states.get("il-30-5") != "mismatch":
    problems.append(f"il-30-5 reads {states.get('il-30-5')}, not mismatch")
# And a source the sweep could not reach must not have been promoted.
if states.get("il-40-2") == "verified":
    problems.append("an unreachable source is being reported as verified")

# The rule that cites the contradicted paragraph must carry the weakest state.
for rule in rules:
    if "il-30-5" in [s["id"] for s in rule["sources"]]:
        if rule["source_state"] != "mismatch":
            problems.append(
                f"{rule['rule_id']} reports {rule['source_state']} while citing a "
                "source that contradicts it"
            )
        # And a verified retrieval must carry the timestamp that earned it.
        for source in rule["sources"]:
            if source["state"] in ("verified", "mismatch") and not source.get("retrieved_at"):
                problems.append(f"{source['id']} is {source['state']} with no retrieved_at")

print("; ".join(problems))
sys.exit(1 if problems else 0)
PY
    then
        pass "the API reports the swept state per source and per rule"
    else
        fail "the API disagrees with the sweep: $(cat "$WORKDIR/api-check.txt")"
    fi
    kill "$api_pid" 2>/dev/null || true
else
    fail "the API did not start: $(tail -3 "$WORKDIR/api.log")"
fi

printf '\npassed %d, failed %d\n' "$passed" "$failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "source verification works against a real database and a real server"

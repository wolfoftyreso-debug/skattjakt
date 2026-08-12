#!/usr/bin/env bash
# The Monte Carlo chain, end to end, against a real API, a real database and a
# real worker.
#
# The unit tests prove the mathematics. This proves the chain the specification
# asks for in section 26 and nothing shorter:
#
#   input → distribution → sample → simulation → calculation → output →
#   statistics → sensitivity → convergence → visualisation → persistence → audit
#
# and the properties that only exist once there is more than one process: that a
# queued run survives being handed to a worker, that cancellation reaches it,
# that a seed reproduces a result across two separate runs on two separate
# machines' worth of state, and that one tenant cannot see another's models.
#
# Usage: tests/integration/simulations.sh
# Requires: PostgreSQL, cargo, curl, python3.

set -euo pipefail

if [[ -z "${SKATTJAKT_PG_REEXEC:-}" ]] && command -v cargo >/dev/null 2>&1; then
    cargo build --quiet --bin skattjakt-api --bin skattjakt-analysis-worker \
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
PGPORT=5449
DB=skattjakt_sim
APIPORT=18108

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
    for pid in ${API_PID:-} ${WORKER_PID:-}; do kill "$pid" 2>/dev/null || true; done
    "$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
# INT and TERM as well as EXIT. Trapping EXIT alone leaves a PostgreSQL server
# and two binaries running when the suite is interrupted, and the next run then
# fails on a port that is already bound — which reads as a broken test rather
# than as a leftover.
trap cleanup EXIT INT TERM

mkdir -p "$SOCKET"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET -h 127.0.0.1 -p $PGPORT" \
    -l "$WORKDIR/pg.log" start >/dev/null

psql() { "$PGBIN/psql" -h "$SOCKET" -p "$PGPORT" -U postgres -v ON_ERROR_STOP=1 -q "$@"; }
q() { psql -d "$DB" -tAc "$1"; }

psql -d postgres -c "CREATE DATABASE $DB" >/dev/null
for migration in "$ROOT"/migrations/*.sql; do psql -d "$DB" -f "$migration" >/dev/null; done
psql -d "$DB" -c "ALTER ROLE skattjakt_app LOGIN PASSWORD 'sim'" >/dev/null
echo "database ready"

ADMIN_TOKEN="admin-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
export DATABASE_URL="postgres://skattjakt_app:sim@127.0.0.1:$PGPORT/$DB"
export SKATTJAKT_ADMIN_TOKEN="$ADMIN_TOKEN"
export SKATTJAKT_BLOB_ROOT="$WORKDIR/documents"
export RUST_LOG=skattjakt=warn

PORT="$APIPORT" "$ROOT/target/debug/skattjakt-api" > "$WORKDIR/api.log" 2>&1 &
API_PID=$!
for _ in $(seq 1 80); do
    curl -fsS "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && break
    sleep 0.2
done
curl -fsS "http://127.0.0.1:$APIPORT/health" >/dev/null || {
    echo "the API did not start"; cat "$WORKDIR/api.log"; exit 1; }

HOSTNAME=sim-worker-1 "$ROOT/target/debug/skattjakt-analysis-worker" \
    > "$WORKDIR/worker.log" 2>&1 &
WORKER_PID=$!
sleep 1
kill -0 "$WORKER_PID" 2>/dev/null || {
    echo "the worker died"; cat "$WORKDIR/worker.log"; exit 1; }
echo "api and worker running"

api() {
    local method="$1" path="$2" token="$3" body="${4:-}"
    if [[ -n "$body" ]]; then
        curl -sS -X "$method" "http://127.0.0.1:$APIPORT$path" \
            -H "authorization: Bearer $token" -H 'content-type: application/json' -d "$body"
    else
        curl -sS -X "$method" "http://127.0.0.1:$APIPORT$path" -H "authorization: Bearer $token"
    fi
}
code() {
    local method="$1" path="$2" token="$3" body="${4:-}"
    if [[ -n "$body" ]]; then
        curl -sS -o /dev/null -w '%{http_code}' -X "$method" "http://127.0.0.1:$APIPORT$path" \
            -H "authorization: Bearer $token" -H 'content-type: application/json' -d "$body"
    else
        curl -sS -o /dev/null -w '%{http_code}' -X "$method" "http://127.0.0.1:$APIPORT$path" \
            -H "authorization: Bearer $token"
    fi
}
# Reads a value out of a JSON response by dotted path, so the tests can assert
# on the actual contract rather than on a grep of it.
get() { python3 -c "
import json,sys
d=json.load(sys.stdin)
for part in '$1'.split('.'):
    if part == '': continue
    if isinstance(d, list): d = d[int(part)]
    else: d = d.get(part)
    if d is None: print(''); raise SystemExit
print(json.dumps(d) if isinstance(d,(dict,list,bool)) else d)
"; }

new_company() {
    api POST /v1/companies "$ADMIN_TOKEN" \
        "{\"company\":{\"name\":\"$1\",\"org_number\":\"$2\",
          \"fiscal_year\":{\"start\":\"2025-01-01\",\"end\":\"2025-12-31\"}}}"
}

CREATED="$(new_company "Simuleringsbolaget AB" 5560160680)"
COMPANY="$(get company_id <<<"$CREATED")"
TOKEN="$(get api_token <<<"$CREATED")"

# The model. Eight inputs across six distribution families, three chained
# outputs, one with a branch — realistic rather than minimal, because a model
# with one normal input would not exercise the parts that break.
MODEL='{
  "name": "Resultat 2026",
  "description": "Intäkter, kostnader och resultat under osäkerhet.",
  "note": "första versionen",
  "inputs": [
    {"id":"customers","name":"Antal kunder","source":"CRM, 2025-12-31",
     "confidence":"medium","unit":"st",
     "distribution":{"kind":"normal","mean":1000,"std_dev":120}},
    {"id":"average_revenue","name":"Snittintäkt per kund","unit":"kr",
     "source":"Faktureringsunderlag 2025","confidence":"high",
     "distribution":{"kind":"triangular","low":700,"mode":850,"high":1100}},
    {"id":"churn","name":"Kundtapp","source":"Historik 2023–2025",
     "distribution":{"kind":"beta","alpha":2,"beta":18,"low":0,"high":1}},
    {"id":"fixed_costs","name":"Fasta kostnader","unit":"kr",
     "distribution":{"kind":"uniform","low":400000,"high":600000}},
    {"id":"variable_cost_rate","name":"Rörlig kostnadsandel",
     "distribution":{"kind":"lognormal","log_mean":-1.6,"log_std_dev":0.25},
     "constraints":{"max":0.9,"mode":"resample"}},
    {"id":"incidents","name":"Antal driftstörningar",
     "distribution":{"kind":"poisson","lambda":4}},
    {"id":"incident_cost","name":"Kostnad per störning","unit":"kr",
     "distribution":{"kind":"exponential","rate":0.00004}},
    {"id":"wins_contract","name":"Vinner ramavtalet",
     "distribution":{"kind":"bernoulli","p":0.35}}
  ],
  "outputs": [
    {"id":"revenue","name":"Intäkter","unit":"kr",
     "expression":"customers * (1 - churn) * average_revenue + if(wins_contract, 250000, 0)"},
    {"id":"costs","name":"Kostnader","unit":"kr",
     "expression":"fixed_costs + revenue * variable_cost_rate + incidents * incident_cost"},
    {"id":"profit","name":"Resultat","unit":"kr","expression":"revenue - costs",
     "target":100000,"target_direction":"at_least","critical_threshold":0}
  ]
}'

# ---------------------------------------------------------------------------
echo
echo "1. the model"
# ---------------------------------------------------------------------------

RESPONSE="$(api POST /v1/simulations "$TOKEN" "$MODEL")"
SIMULATION="$(get id <<<"$RESPONSE")"
SPEC_HASH="$(get spec_hash <<<"$RESPONSE")"
if [[ -n "$SIMULATION" ]]; then
    pass "a model with 8 inputs and 3 outputs is accepted"
else
    fail "the model was rejected: $RESPONSE"; echo "$RESPONSE"; exit 1
fi
check "and is fingerprinted" 64 "${#SPEC_HASH}"

CATALOGUE="$(api GET /v1/simulations/distributions "$TOKEN")"
check "the catalogue offers eleven distributions" 11 \
    "$(python3 -c "import json,sys;print(len(json.load(sys.stdin)['distributions']))" <<<"$CATALOGUE")"

BAD='{"name":"Trasig","inputs":[{"id":"x","name":"x",
      "distribution":{"kind":"triangular","low":0,"mode":50,"high":10}}],
      "outputs":[{"id":"y","name":"y","expression":"x"}]}'
STATUS="$(code POST /v1/simulations "$TOKEN" "$BAD")"
check "a triangular whose peak is outside its range is refused" 422 "$STATUS"
DETAIL="$(api POST /v1/simulations "$TOKEN" "$BAD" | get detail)"
case "$DETAIL" in
    *mode*) pass "and the message names the parameter" ;;
    *) fail "the message did not name the parameter: $DETAIL" ;;
esac

BAD_EXPR='{"name":"Trasig","inputs":[{"id":"x","name":"x",
           "distribution":{"kind":"normal","mean":1,"std_dev":1}}],
           "outputs":[{"id":"y","name":"y","expression":"x + nonexistent"}]}'
check "an expression naming an unknown input is refused" 422 \
    "$(code POST /v1/simulations "$TOKEN" "$BAD_EXPR")"

IMPOSSIBLE='{"name":"Omöjlig","inputs":[{"id":"x","name":"x",
             "distribution":{"kind":"uniform","low":0,"high":10},
             "constraints":{"min":1000}}],
             "outputs":[{"id":"y","name":"y","expression":"x"}]}'
check "a constraint no draw could satisfy is refused before any run" 422 \
    "$(code POST /v1/simulations "$TOKEN" "$IMPOSSIBLE")"

# ---------------------------------------------------------------------------
echo
echo "2. an inline run: the whole chain in one request"
# ---------------------------------------------------------------------------

RUN="$(api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" \
    '{"iterations":20000,"seed":"424242","reason":"integrationstest"}')"
STATE="$(get state <<<"$RUN")"
check "a 20 000-iteration run answers inside the request" succeeded "$STATE"
check "and says it ran inline" inline "$(get execution <<<"$RUN")"
check "and returns the seed as a string" 424242 "$(get seed <<<"$RUN")"
RUN_ID="$(get run_id <<<"$RUN")"

check "a statistics block per output" 3 \
    "$(python3 -c "import json,sys;print(len(json.load(sys.stdin)['statistics']))" <<<"$RUN")"

PROFIT=$(python3 -c "
import json,sys
d=json.load(sys.stdin)
s=[x for x in d['statistics'] if x['output_id']=='profit'][0]
print(json.dumps(s))
" <<<"$RUN")

python3 - "$PROFIT" <<'PY' && pass "the percentiles are ordered and the probabilities are probabilities" \
    || fail "the statistics are not internally consistent"
import json, sys
s = json.loads(sys.argv[1])
order = [s['p5'], s['p10'], s['p25'], s['p50'], s['p75'], s['p90'], s['p95'], s['p99']]
assert order == sorted(order), order
assert s['min'] <= s['p5'] and s['p99'] <= s['max']
assert s['median'] == s['p50']
assert 0 <= s['probability_of_target'] <= 1
assert 0 <= s['probability_of_loss'] <= 1
low, high = s['mean_confidence_interval_95']
assert low < s['mean'] < high
assert s['count'] == 20000
PY

check "the disclaimer travels with the result" 1 \
    "$(python3 -c "import json,sys;print(1 if json.load(sys.stdin).get('disclaimer') else 0)" <<<"$RUN")"

python3 - "$RUN" <<'PY' && pass "the visualisation payload is a histogram, a density curve and a CDF" \
    || fail "the visualisation payload is wrong"
import json, sys
d = json.loads(sys.argv[1])
shape = [s for s in d['shapes'] if s['output_id'] == 'profit'][0]
assert len(shape['bins']) >= 8, len(shape['bins'])
assert abs(sum(b['share'] for b in shape['bins']) - 1.0) < 1e-9
assert sum(b['count'] for b in shape['bins']) == 20000
assert len(shape['density']) == len(shape['bins'])
assert len(shape['cdf']) == 201
assert shape['cdf'][0]['probability'] == 0.0
assert shape['cdf'][-1]['probability'] == 1.0
values = [p['value'] for p in shape['cdf']]
assert values == sorted(values)
PY

python3 - "$RUN" <<'PY' && pass "sensitivity ranks every input and the shares sum to one" \
    || fail "the sensitivity report is wrong"
import json, sys
d = json.loads(sys.argv[1])
# Flat rows, exactly as GET /sensitivity returns them. The inline and stored
# paths deliberately share one shape.
rows = [s for s in d['sensitivity'] if s['output_id'] == 'profit']
assert len(rows) == 8, len(rows)
assert abs(sum(r['variance_contribution'] for r in rows) - 1.0) < 1e-9
assert sorted(r['rank'] for r in rows) == list(range(1, 9))
# Every input reaches profit, directly or through revenue and costs.
assert all(r['referenced'] for r in rows)
assert all(r['sample_size'] == 20000 for r in rows)
PY

python3 - "$RUN" <<'PY' && pass "convergence is reported from 1 000 iterations up to the full run" \
    || fail "the convergence report is wrong"
import json, sys
d = json.loads(sys.argv[1])
counts = [c['iterations'] for c in d['convergence'] if c['output_id'] == 'profit']
assert counts[0] == 1000 and counts[-1] == 20000, counts
assert counts == sorted(counts)
PY

# ---------------------------------------------------------------------------
echo
echo "3. reproducibility (section 12)"
# ---------------------------------------------------------------------------

AGAIN="$(api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" \
    '{"iterations":20000,"seed":"424242"}')"
FIRST_P50=$(python3 -c "
import json,sys
print([s for s in json.load(sys.stdin)['statistics'] if s['output_id']=='profit'][0]['p50'])" <<<"$RUN")
SECOND_P50=$(python3 -c "
import json,sys
print([s for s in json.load(sys.stdin)['statistics'] if s['output_id']=='profit'][0]['p50'])" <<<"$AGAIN")
check "the same seed reproduces the result exactly" "$FIRST_P50" "$SECOND_P50"

DIFFERENT="$(api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" \
    '{"iterations":20000,"seed":"999"}')"
OTHER_P50=$(python3 -c "
import json,sys
print([s for s in json.load(sys.stdin)['statistics'] if s['output_id']=='profit'][0]['p50'])" <<<"$DIFFERENT")
if [[ "$FIRST_P50" != "$OTHER_P50" ]]; then
    pass "a different seed gives a different result"
else
    fail "two seeds produced identical results"
fi

# The reason seeds are strings rather than numbers: above 2^53 a JSON number
# would come back changed, and a run that cannot be repeated is not reproducible.
BIG_SEED=18446744073709551615
BIG="$(api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" \
    "{\"iterations\":1000,\"seed\":\"$BIG_SEED\"}")"
check "a 64-bit seed survives the round trip" "$BIG_SEED" "$(get seed <<<"$BIG")"

OMITTED="$(api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" '{"iterations":1000}')"
DRAWN="$(get seed <<<"$OMITTED")"
if [[ -n "$DRAWN" && "$DRAWN" != "0" ]]; then
    pass "a run without a seed gets one drawn and recorded ($DRAWN)"
else
    fail "no seed was recorded for a run that did not specify one"
fi

# ---------------------------------------------------------------------------
echo
echo "4. persistence, and the audit trail (section 13)"
# ---------------------------------------------------------------------------

check "the run row is stored" succeeded \
    "$(q "SELECT state FROM simulation_runs WHERE id = '$RUN_ID'")"
check "with statistics for each output" 3 \
    "$(q "SELECT count(*) FROM simulation_statistics WHERE run_id = '$RUN_ID'")"
check "with sensitivity for each input of each output" 24 \
    "$(q "SELECT count(*) FROM simulation_sensitivity WHERE run_id = '$RUN_ID'")"
check "with a chart payload per output" 3 \
    "$(q "SELECT count(*) FROM simulation_shapes WHERE run_id = '$RUN_ID'")"
check "and convergence checkpoints" 1 \
    "$(q "SELECT CASE WHEN count(*) >= 9 THEN 1 ELSE 0 END FROM simulation_convergence WHERE run_id = '$RUN_ID'")"

# Section 16: the raw samples are never stored. Twenty thousand doubles per
# output would be 480 KB here and 240 MB at ten million iterations, and they are
# reproducible from the seed at any time.
STORED_BYTES="$(q "SELECT sum(pg_column_size(payload)) FROM simulation_shapes WHERE run_id = '$RUN_ID'")"
if [[ "$STORED_BYTES" -lt 60000 ]]; then
    pass "the stored result is $STORED_BYTES bytes, not the samples it came from"
else
    fail "the stored result is $STORED_BYTES bytes, which looks like raw samples"
fi

check "the seed is stored as a decimal string, not a bit-cast integer" 424242 \
    "$(q "SELECT seed FROM simulation_runs WHERE id = '$RUN_ID'")"

AUDIT="$(api GET "/v1/simulations/$SIMULATION/audit" "$TOKEN")"
python3 - "$AUDIT" <<'PY' && pass "the audit trail records creation, requests and completions with seed and version" \
    || fail "the audit trail is incomplete"
import json, sys
events = json.loads(sys.argv[1])['events']
kinds = {e['event_type'] for e in events}
assert 'simulation.created' in kinds, kinds
assert 'simulation.run_requested' in kinds, kinds
assert 'simulation.run_completed' in kinds, kinds
requested = [e for e in events if e['event_type'] == 'simulation.run_requested']
detail = requested[0]['detail']
for field in ('run_id', 'seed', 'iterations', 'engine_version', 'spec_hash', 'execution'):
    assert field in detail, (field, detail)
assert any(e['detail'].get('reason') == 'integrationstest' for e in requested)
assert all(e['actor'] for e in events)
PY

# ---------------------------------------------------------------------------
echo
echo "5. versioning: an old run keeps meaning what it meant"
# ---------------------------------------------------------------------------

EDITED="$(python3 -c "
import json,sys
m = json.loads(sys.stdin.read())
m['inputs'][0]['distribution']['mean'] = 1500
m['note'] = 'fler kunder'
print(json.dumps(m))
" <<<"$MODEL")"
V2="$(api POST "/v1/simulations/$SIMULATION/versions" "$TOKEN" "$EDITED")"
check "a new version is appended" 2 "$(get version <<<"$V2")"

STILL="$(api GET "/v1/simulations/$SIMULATION/statistics?run=$RUN_ID" "$TOKEN")"
STILL_P50=$(python3 -c "
import json,sys
print([s for s in json.load(sys.stdin)['statistics'] if s['output_id']=='profit'][0]['p50'])" <<<"$STILL")
check "the earlier run still reports what it reported" "$FIRST_P50" "$STILL_P50"

NEW_RUN="$(api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" \
    '{"iterations":20000,"seed":"424242"}')"
NEW_P50=$(python3 -c "
import json,sys
print([s for s in json.load(sys.stdin)['statistics'] if s['output_id']=='profit'][0]['p50'])" <<<"$NEW_RUN")
if [[ "$NEW_P50" != "$FIRST_P50" ]]; then
    pass "the same seed on a changed model gives a different result"
else
    fail "editing the model did not change the result"
fi

# The property that makes a model editable: streams are named after inputs, so
# adding one does not move the numbers the others see.
WIDER="$(python3 -c "
import json,sys
m = json.loads(sys.stdin.read())
m['inputs'].insert(0, {'id':'staff','name':'Antal anställda',
                       'distribution':{'kind':'poisson','lambda':12}})
m['note'] = 'ny indata som inget utfall läser'
print(json.dumps(m))
" <<<"$EDITED")"
api POST "/v1/simulations/$SIMULATION/versions" "$TOKEN" "$WIDER" >/dev/null
WIDER_RUN="$(api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" \
    '{"iterations":20000,"seed":"424242"}')"
WIDER_P50=$(python3 -c "
import json,sys
print([s for s in json.load(sys.stdin)['statistics'] if s['output_id']=='profit'][0]['p50'])" <<<"$WIDER_RUN")
check "adding an unrelated input leaves the other results untouched" "$NEW_P50" "$WIDER_P50"

UNUSED_CONTRIBUTION=$(python3 -c "
import json,sys
d=json.load(sys.stdin)
e=[s for s in d['sensitivity']
   if s['output_id']=='profit' and s['input_id']=='staff'][0]
print('%s|%s|%s' % (e['variance_contribution'], json.dumps(e['referenced']), json.dumps(e['correlation'])))
" <<<"$WIDER_RUN")
check "and that input is reported as having no influence" "0.0|false|null" "$UNUSED_CONTRIBUTION"

# ---------------------------------------------------------------------------
echo
echo "6. a queued run: the worker, progress and cancellation"
# ---------------------------------------------------------------------------

QUEUED="$(api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" \
    '{"iterations":2000000,"seed":"7","reason":"stor körning"}')"
check "a two-million-iteration run is queued rather than answered inline" queued \
    "$(get state <<<"$QUEUED")"
QUEUED_RUN="$(get run_id <<<"$QUEUED")"
check "and the job reaches the queue as a simulation" 1 \
    "$(q "SELECT count(*) FROM jobs WHERE kind = 'simulation' AND subject_id = '$QUEUED_RUN'")"

# Wait for the worker to finish it, watching progress on the way.
SAW_PROGRESS=0
for _ in $(seq 1 200); do
    STATUS="$(api GET "/v1/simulations/$SIMULATION/results?run=$QUEUED_RUN" "$TOKEN")"
    STATE="$(get state <<<"$STATUS")"
    DONE="$(get completed_iterations <<<"$STATUS")"
    [[ "${DONE:-0}" -gt 0 && "$STATE" == "running" ]] && SAW_PROGRESS=1
    [[ "$STATE" == "succeeded" || "$STATE" == "failed" ]] && break
    sleep 0.5
done
check "the worker completes it" succeeded "$STATE"
check "and progress was visible while it ran" 1 "$SAW_PROGRESS"

check "a finished run reports its throughput" 1 \
    "$(python3 -c "
import json,sys
d=json.load(sys.stdin)
print(1 if (d.get('iterations_per_second') or 0) > 1000 else 0)" <<<"$STATUS")"

python3 - "$STATUS" "$RUN" <<'PY' && pass "and a two-million-iteration result is the same size as a 20 000 one" \
    || fail "the payload grew with the iteration count"
import json, sys
# The property of section 16, measured rather than assumed: a hundredfold more
# iterations must not make the stored result meaningfully bigger. A fixed byte
# threshold would only be measuring how many outputs this model happens to have.
large = len(json.dumps(json.loads(sys.argv[1])['shapes']))
small = len(json.dumps(json.loads(sys.argv[2])['shapes']))
assert large < small * 1.2, (small, large)
PY

# Cancellation. Started large enough that it cannot finish before the request
# to stop it arrives.
BIG_RUN="$(api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" \
    '{"iterations":8000000,"seed":"8"}')"
BIG_ID="$(get run_id <<<"$BIG_RUN")"
for _ in $(seq 1 60); do
    [[ "$(q "SELECT state FROM simulation_runs WHERE id = '$BIG_ID'")" == "running" ]] && break
    sleep 0.25
done
CANCEL="$(api POST "/v1/simulations/$SIMULATION/cancel?run=$BIG_ID" "$TOKEN")"
check "cancelling an in-flight run is accepted" true "$(get cancelled <<<"$CANCEL")"

for _ in $(seq 1 120); do
    CANCEL_STATE="$(q "SELECT state FROM simulation_runs WHERE id = '$BIG_ID'")"
    [[ "$CANCEL_STATE" == "cancelled" ]] && break
    sleep 0.5
done
check "and the worker stops" cancelled "$CANCEL_STATE"
check "a cancelled run stores no statistics" 0 \
    "$(q "SELECT count(*) FROM simulation_statistics WHERE run_id = '$BIG_ID'")"

AGAIN_CANCEL="$(api POST "/v1/simulations/$SIMULATION/cancel?run=$BIG_ID" "$TOKEN")"
check "cancelling a finished run says nothing was cancelled" false \
    "$(get cancelled <<<"$AGAIN_CANCEL")"

# ---------------------------------------------------------------------------
echo
echo "7. the run that has no answer"
# ---------------------------------------------------------------------------

DIVIDE='{"name":"Division","inputs":[
   {"id":"divisor","name":"Nämnare",
    "distribution":{"kind":"discrete","values":[0,2],"weights":[1,1]}}],
   "outputs":[{"id":"ratio","name":"Kvot","expression":"100 / divisor"}]}'
DIV_ID="$(api POST /v1/simulations "$TOKEN" "$DIVIDE" | get id)"
DIV_RESULT="$(api POST "/v1/simulations/$DIV_ID/run" "$TOKEN" '{"iterations":1000,"seed":"1"}')"
DIV_DETAIL="$(get detail <<<"$DIV_RESULT")"
case "$DIV_DETAIL" in
    *ratio*iteration*|*ratio*) pass "an output that produces an infinity fails the run and names it" ;;
    *) fail "the divide-by-zero run did not report the output: $DIV_DETAIL" ;;
esac
check "and nothing is stored for it" failed \
    "$(q "SELECT state FROM simulation_runs WHERE simulation_id = '$DIV_ID' ORDER BY requested_at DESC LIMIT 1")"

check "an iteration count below the minimum is refused" 422 \
    "$(code POST "/v1/simulations/$SIMULATION/run" "$TOKEN" '{"iterations":5}')"
check "an iteration count above the maximum is refused" 422 \
    "$(code POST "/v1/simulations/$SIMULATION/run" "$TOKEN" '{"iterations":50000000}')"
check "a malformed seed is refused" 422 \
    "$(code POST "/v1/simulations/$SIMULATION/run" "$TOKEN" '{"iterations":1000,"seed":"kaffe"}')"

# ---------------------------------------------------------------------------
echo
echo "8. edge cases the specification names"
# ---------------------------------------------------------------------------

ZERO_VARIANCE='{"name":"Utan osäkerhet","inputs":[
  {"id":"a","name":"a","distribution":{"kind":"normal","mean":10,"std_dev":0}},
  {"id":"b","name":"b","distribution":{"kind":"uniform","low":4,"high":4}}],
  "outputs":[{"id":"product","name":"Produkt","expression":"a * b","target":40}]}'
FIXED_ID="$(api POST /v1/simulations "$TOKEN" "$ZERO_VARIANCE" | get id)"
FIXED="$(api POST "/v1/simulations/$FIXED_ID/run" "$TOKEN" '{"iterations":1000,"seed":"1"}')"
python3 - "$FIXED" <<'PY' && pass "a model with no uncertainty produces a constant, not a NaN" \
    || fail "a zero-variance model did not behave"
import json, sys
d = json.loads(sys.argv[1])
s = d['statistics'][0]
assert s['mean'] == 40.0 and s['std_dev'] == 0.0, s
assert s['p10'] == 40.0 and s['p90'] == 40.0
assert s['probability_of_target'] == 1.0
shape = d['shapes'][0]
assert len(shape['bins']) == 1
assert shape['density'] == []
rows = d['sensitivity']
assert all(r['correlation'] is None for r in rows), rows
assert all(r['variance_contribution'] == 0.0 for r in rows), rows
PY

NEGATIVE='{"name":"Negativa värden","inputs":[
  {"id":"x","name":"x","distribution":{"kind":"normal","mean":-5000,"std_dev":1000}}],
  "outputs":[{"id":"y","name":"y","expression":"x","target":0,"critical_threshold":-6000}]}'
NEG_ID="$(api POST /v1/simulations "$TOKEN" "$NEGATIVE" | get id)"
NEG="$(api POST "/v1/simulations/$NEG_ID/run" "$TOKEN" '{"iterations":20000,"seed":"3"}')"
python3 - "$NEG" <<'PY' && pass "negative outcomes are handled and the loss probability is right" \
    || fail "a negative-valued model did not behave"
import json, sys
s = json.loads(sys.argv[1])['statistics'][0]
assert s['mean'] < 0 and s['p10'] < s['p90'] < 0, s
assert s['probability_of_loss'] > 0.99
assert s['probability_of_target'] < 0.01     # target is "at least 0"
PY

HUGE='{"name":"Stora tal","inputs":[
  {"id":"x","name":"x","distribution":{"kind":"lognormal","log_mean":20,"log_std_dev":2}}],
  "outputs":[{"id":"y","name":"y","expression":"x * 1000"}]}'
HUGE_ID="$(api POST /v1/simulations "$TOKEN" "$HUGE" | get id)"
HUGE_RESULT="$(api POST "/v1/simulations/$HUGE_ID/run" "$TOKEN" '{"iterations":20000,"seed":"4"}')"
python3 - "$HUGE_RESULT" <<'PY' && pass "very large values stay finite through every statistic" \
    || fail "large values produced a non-finite statistic"
import json, math, sys
s = json.loads(sys.argv[1])['statistics'][0]
for key, value in s.items():
    if isinstance(value, float):
        assert math.isfinite(value), (key, value)
PY

TINY='{"name":"Små tal","inputs":[
  {"id":"x","name":"x","distribution":{"kind":"uniform","low":1e-9,"high":2e-9}}],
  "outputs":[{"id":"y","name":"y","expression":"x"}]}'
TINY_ID="$(api POST /v1/simulations "$TOKEN" "$TINY" | get id)"
TINY_RESULT="$(api POST "/v1/simulations/$TINY_ID/run" "$TOKEN" '{"iterations":20000,"seed":"5"}')"
python3 - "$TINY_RESULT" <<'PY' && pass "very small values keep their precision rather than collapsing to zero" \
    || fail "small values underflowed"
import json, sys
s = json.loads(sys.argv[1])['statistics'][0]
assert 0 < s['std_dev'] < 1e-9, s['std_dev']
assert s['p10'] < s['p90'], s
PY

# A run that has not finished must return no numbers rather than partial ones.
PENDING="$(api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" '{"iterations":4000000,"seed":"11"}')"
PENDING_ID="$(get run_id <<<"$PENDING")"
PARTIAL="$(api GET "/v1/simulations/$SIMULATION/results?run=$PENDING_ID" "$TOKEN")"
check "an unfinished run returns progress and no statistics" "" \
    "$(get statistics <<<"$PARTIAL")"
api POST "/v1/simulations/$SIMULATION/cancel?run=$PENDING_ID" "$TOKEN" >/dev/null

# Concurrency: three runs of the same model at once must not interfere.
CONCURRENT=()
for seed in 21 22 23; do
    api POST "/v1/simulations/$SIMULATION/run" "$TOKEN" \
        "{\"iterations\":20000,\"seed\":\"$seed\"}" > "$WORKDIR/run-$seed.json" &
    CONCURRENT+=($!)
done
# Named pids, not a bare `wait`. The API and the worker are background jobs of
# this same shell, so `wait` with no arguments waits for those too — and they
# are supposed to keep running until the suite ends. The suite hung there.
wait "${CONCURRENT[@]}"
DISTINCT=$(python3 -c "
import json
values = set()
for seed in (21, 22, 23):
    d = json.load(open('$WORKDIR/run-%d.json' % seed))
    values.add([s for s in d['statistics'] if s['output_id']=='profit'][0]['p50'])
print(len(values))
")
check "three concurrent runs each produce their own result" 3 "$DISTINCT"

# ---------------------------------------------------------------------------
echo
echo "9. security (section 19)"
# ---------------------------------------------------------------------------

OTHER="$(new_company "Grannbolaget AB" 5566778899)"
OTHER_TOKEN="$(get api_token <<<"$OTHER")"

check "another tenant cannot read the model" 404 \
    "$(code GET "/v1/simulations/$SIMULATION" "$OTHER_TOKEN")"
check "another tenant cannot read its results" 404 \
    "$(code GET "/v1/simulations/$SIMULATION/results" "$OTHER_TOKEN")"
check "another tenant cannot run it" 404 \
    "$(code POST "/v1/simulations/$SIMULATION/run" "$OTHER_TOKEN" '{"iterations":1000}')"
check "another tenant cannot cancel its runs" 404 \
    "$(code POST "/v1/simulations/$SIMULATION/cancel?run=$RUN_ID" "$OTHER_TOKEN")"
check "another tenant's list is empty of it" 0 \
    "$(api GET /v1/simulations "$OTHER_TOKEN" | python3 -c "
import json,sys
print(len([s for s in json.load(sys.stdin)['simulations'] if s['id'] == '$SIMULATION']))")"
check "no credential at all is refused" 401 \
    "$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$APIPORT/v1/simulations")"
check "the admin token, which owns no company, cannot read simulations" 403 \
    "$(code GET /v1/simulations "$ADMIN_TOKEN")"

# The advisor role may run scenarios and may not read the audit trail.
api POST /v1/users "$TOKEN" \
    '{"email":"radgivaren@example.com","password":"bokslut kaffe cykel oktober","role":"advisor"}' \
    >/dev/null
ADVISOR_SESSION="$(curl -sS -X POST "http://127.0.0.1:$APIPORT/v1/auth/sign-in" \
    -H 'content-type: application/json' -H 'x-skattjakt-client: ios' \
    -d '{"email":"radgivaren@example.com","password":"bokslut kaffe cykel oktober","install_id":"11111111-1111-1111-1111-111111111111"}')"
ADVISOR_TOKEN="$(get access_token <<<"$ADVISOR_SESSION")"
if [[ -n "$ADVISOR_TOKEN" ]]; then
    check "an advisor may run a simulation" 200 \
        "$(code POST "/v1/simulations/$SIMULATION/run" "$ADVISOR_TOKEN" '{"iterations":1000}')"
    check "and may not read its audit trail" 403 \
        "$(code GET "/v1/simulations/$SIMULATION/audit" "$ADVISOR_TOKEN")"
else
    fail "the advisor could not sign in"
fi

# An oversized request must be refused by the body limit rather than parsed.
BIG_BODY="$(python3 -c "
import json
inputs = [{'id': f'x{i}', 'name': 'x', 'distribution': {'kind':'normal','mean':0,'std_dev':1}}
          for i in range(200)]
print(json.dumps({'name':'För många','inputs':inputs,
                  'outputs':[{'id':'y','name':'y','expression':'x0'}]}))")"
check "a model with more inputs than the engine accepts is refused" 422 \
    "$(code POST /v1/simulations "$TOKEN" "$BIG_BODY")"

# ---------------------------------------------------------------------------
echo
echo "10. observability (section 20)"
# ---------------------------------------------------------------------------

METRICS="$(curl -sS "http://127.0.0.1:$APIPORT/metrics")"
for metric in skattjakt_simulations_started_total skattjakt_simulations_finished_total \
              skattjakt_simulation_duration_ms skattjakt_simulation_iterations_total; do
    if grep -q "^$metric" <<<"$METRICS"; then
        pass "$metric is exported"
    else
        fail "$metric is missing from /metrics"
    fi
done

if grep -q 'skattjakt_simulations_started_total{execution="queued"}' <<<"$METRICS"; then
    pass "the execution mode is a label, so inline and queued are distinguishable"
else
    fail "the execution label is missing"
fi

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "the whole Monte Carlo chain works: input → distribution → sample → \
calculation → statistics → sensitivity → convergence → visualisation → \
persistence → audit"

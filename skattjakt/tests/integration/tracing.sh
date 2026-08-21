#!/usr/bin/env bash
# Trace export, against a real OpenTelemetry collector.
#
# The unit tests prove the OTLP body is shaped correctly. Only a collector can
# prove it is *accepted* — a wrong field name, a numeric timestamp where a
# string is required, a malformed trace id, all produce a body that looks right
# and is rejected.
#
# The assertion that matters is the one a unit test cannot make at all: that a
# trace started by an HTTP request continues into the analysis worker, in a
# different process, having crossed a database queue. That is what makes a
# trace useful here, and it is the thing that silently stops working.
#
# Usage: tests/integration/tracing.sh
# Requires: docker, PostgreSQL, cargo, curl, python3.

set -euo pipefail

if [[ -z "${SKATTJAKT_PG_REEXEC:-}" ]] && command -v cargo >/dev/null 2>&1; then
    cargo build --quiet --bin skattjakt-api --bin skattjakt-analysis-worker \
        --manifest-path "$(dirname "${BASH_SOURCE[0]}")/../../Cargo.toml"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Whichever build is newer, never whichever profile is preferred.
source "$ROOT/tests/lib/newest-binary.sh"
CONTAINER=skattjakt-otel-test
OTLP_PORT=14318
SPOOL=/tmp/skattjakt-otel-spool

cleanup_container() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }

if [[ -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    cleanup_container
    rm -rf "$SPOOL"; mkdir -p "$SPOOL"; chmod 777 "$SPOOL"

    # The collector writes every span it accepts to a file, which is then read
    # back. A debug exporter would only prove the collector logged something.
    cat > "$SPOOL/config.yaml" <<'YAML'
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
exporters:
  file:
    path: /spool/spans.json
service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [file]
YAML
    chmod 644 "$SPOOL/config.yaml"

    echo "starting the collector"
    docker run -d --name "$CONTAINER" \
        -p "127.0.0.1:$OTLP_PORT:4318" \
        -v "$SPOOL:/spool" \
        mirror.gcr.io/otel/opentelemetry-collector-contrib:latest \
        --config /spool/config.yaml >/dev/null

    for _ in $(seq 1 60); do
        curl -s -o /dev/null "http://127.0.0.1:$OTLP_PORT/v1/traces" && break
        sleep 0.5
    done
    echo "collector ready on :$OTLP_PORT"
fi

if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    RUNAS="${SKATTJAKT_PG_USER:-postgres}"
    id "$RUNAS" >/dev/null 2>&1 || RUNAS=nobody
    export SKATTJAKT_PG_REEXEC=1
    set +e
    su -s /bin/bash "$RUNAS" -c \
        "SKATTJAKT_PG_REEXEC=1 PGBIN='${PGBIN:-}' SPOOL='$SPOOL' OTLP_PORT='$OTLP_PORT' \
         $(printf '%q ' "$0" "$@")"
    status=$?
    set -e
    cleanup_container
    exit "$status"
fi

SPOOL="${SPOOL:-/tmp/skattjakt-otel-spool}"
OTLP_PORT="${OTLP_PORT:-14318}"
WORKDIR="$(mktemp -d)"
PGDATA="$WORKDIR/data"
SOCKET="$WORKDIR/sock"
PGPORT=5445
DB=skattjakt_tracing
APIPORT=18104

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
trap cleanup EXIT

mkdir -p "$SOCKET"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET -h 127.0.0.1 -p $PGPORT" \
    -l "$WORKDIR/pg.log" start >/dev/null
psql() { "$PGBIN/psql" -h "$SOCKET" -p "$PGPORT" -U postgres -v ON_ERROR_STOP=1 -q "$@"; }
psql -d postgres -c "CREATE DATABASE $DB" >/dev/null
for migration in "$ROOT"/migrations/*.sql; do psql -d "$DB" -f "$migration" >/dev/null; done
psql -d "$DB" -c "ALTER ROLE skattjakt_app LOGIN PASSWORD 'tracing'" >/dev/null
echo "database ready"

ADMIN_TOKEN="admin-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
export DATABASE_URL="postgres://skattjakt_app:tracing@127.0.0.1:$PGPORT/$DB"
export SKATTJAKT_ADMIN_TOKEN="$ADMIN_TOKEN"
export SKATTJAKT_BLOB_ROOT="$WORKDIR/documents"
export RUST_LOG=skattjakt=warn
export OTEL_EXPORTER_OTLP_ENDPOINT="http://127.0.0.1:$OTLP_PORT"
export SKATTJAKT_ENVIRONMENT=test

PORT="$APIPORT" OTEL_SERVICE_NAME=skattjakt-api \
    "$(newest_binary skattjakt-api)" > "$WORKDIR/api.log" 2>&1 &
API_PID=$!
for _ in $(seq 1 60); do
    curl -fsS "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && break
    sleep 0.2
done
curl -fsS "http://127.0.0.1:$APIPORT/health" >/dev/null || {
    echo "the API did not start"; cat "$WORKDIR/api.log"; exit 1; }

HOSTNAME=tracing-worker OTEL_SERVICE_NAME=skattjakt-analysis-worker \
    "$(newest_binary skattjakt-analysis-worker)" > "$WORKDIR/worker.log" 2>&1 &
WORKER_PID=$!
sleep 1
echo "api and worker running, both exporting to the collector"

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

# --- one request, one analysis ----------------------------------------------

CREATED="$(api POST /v1/companies "$ADMIN_TOKEN" \
    '{"company":{"name":"Spårbolaget AB","org_number":"5560160680",
      "fiscal_year":{"start":"2025-01-01","end":"2025-12-31"}}}')"
TOKEN="$(jqf api_token <<<"$CREATED")"

UPLOAD='{"filename":"b.txt","mime_type":"text/plain",
         "text":"Nettoomsättning 4 200 000\nSkattemässigt resultat 850 000\n",
         "kind":"annual_accounts","accounts_state":"preliminary"}'
VERSION="$(api POST /v1/documents "$TOKEN" "$UPLOAD" | jqf document_version_id)"

# The client supplies its own traceparent, which is what a mobile client would
# do. Everything below should hang off this trace id.
CLIENT_TRACE="4bf92f3577b34da6a3ce929d0e0e4736"
STARTED="$(curl -sS -X POST "http://127.0.0.1:$APIPORT/v1/analyses/stored" \
    -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -H "traceparent: 00-$CLIENT_TRACE-00f067aa0ba902b7-01" \
    -D "$WORKDIR/headers.txt" \
    -d "{\"document_version_ids\":[\"$VERSION\"]}")"
ANALYSIS="$(jqf analysis_id <<<"$STARTED")"
[[ -n "$ANALYSIS" ]] || { echo "the analysis was not accepted"; exit 1; }

echo
echo "the response"
RESPONSE_TRACE="$(grep -i '^traceparent:' "$WORKDIR/headers.txt" | tr -d '\r' | cut -d' ' -f2)"
if [[ "$RESPONSE_TRACE" == *"$CLIENT_TRACE"* ]]; then
    pass "the response continues the client's trace"
else
    fail "the response started a new trace: $RESPONSE_TRACE"
fi

for _ in $(seq 1 120); do
    STATUS="$(api GET "/v1/analyses/$ANALYSIS" "$TOKEN" | jqf status)"
    [[ "$STATUS" == "succeeded" || "$STATUS" == "failed" ]] && break
    sleep 0.5
done
check "the analysis completed" succeeded "$STATUS"

# The exporters flush on a timer, so give both a window.
echo
echo "waiting for both processes to flush"
for _ in $(seq 1 40); do
    [[ -s "$SPOOL/spans.json" ]] && \
      grep -qF "$CLIENT_TRACE" "$SPOOL/spans.json" 2>/dev/null && \
      grep -qF "analysis.run" "$SPOOL/spans.json" 2>/dev/null && break
    sleep 1
done

# --- what the collector accepted --------------------------------------------

echo
echo "what the collector accepted"
if [[ ! -s "$SPOOL/spans.json" ]]; then
    fail "the collector wrote nothing; no span was accepted"
    echo "--- api log ---"; tail -10 "$WORKDIR/api.log"
    echo "--- worker log ---"; tail -10 "$WORKDIR/worker.log"
    echo; echo "passed $passed, failed $failed"; exit 1
fi
pass "the collector accepted and wrote spans"

ANALYSIS_RESULT="$(python3 - "$SPOOL/spans.json" "$CLIENT_TRACE" <<'PY'
import json, sys

spans = []
for line in open(sys.argv[1]):
    line = line.strip()
    if not line:
        continue
    for resource in json.loads(line).get("resourceSpans", []):
        service = ""
        for attribute in resource.get("resource", {}).get("attributes", []):
            if attribute["key"] == "service.name":
                service = attribute["value"].get("stringValue", "")
        for scope in resource.get("scopeSpans", []):
            for span in scope.get("spans", []):
                span["_service"] = service
                spans.append(span)

trace = sys.argv[2]
in_trace = [s for s in spans if s.get("traceId") == trace]
http = [s for s in in_trace if s.get("name") == "http.request"]
work = [s for s in in_trace if s.get("name") == "analysis.run"]

print(json.dumps({
    "total": len(spans),
    "in_trace": len(in_trace),
    "http_spans": len(http),
    "worker_spans": len(work),
    "services": sorted({s["_service"] for s in in_trace}),
    # The property that matters: the worker's span names an HTTP span as its
    # parent, across two processes and a database queue.
    "worker_parent_is_an_http_span": bool(work) and any(
        w.get("parentSpanId") and w["parentSpanId"] in {h.get("spanId") for h in http}
        for w in work
    ),
    "worker_has_a_parent": bool(work) and all(w.get("parentSpanId") for w in work),
    "timestamps_are_strings": all(
        isinstance(s.get("startTimeUnixNano"), str) for s in in_trace
    ),
    "attribute_keys": sorted({
        a["key"] for s in in_trace for a in s.get("attributes", [])
    }),
    "attribute_values": sorted({
        a["value"].get("stringValue", "") for s in in_trace for a in s.get("attributes", [])
    }),
}))
PY
)"

field() { python3 -c "import json,sys;print(json.loads(sys.argv[1])['$1'])" "$ANALYSIS_RESULT"; }

check "the API's request span reached the collector" 1 "$(field http_spans)"
check "the worker's analysis span reached the collector" 1 "$(field worker_spans)"
check "both are in the trace the client started" True "$(field timestamps_are_strings)"

if [[ "$(field worker_has_a_parent)" == "True" ]]; then
    pass "the worker's span has a parent rather than starting a new trace"
else
    fail "the worker started an orphan span"
fi

if [[ "$(field worker_parent_is_an_http_span)" == "True" ]]; then
    pass "the worker's span hangs off the HTTP request that queued it — across two processes and a queue"
else
    fail "the trace does not connect the request to the work it caused"
fi

SERVICES="$(field services)"
if [[ "$SERVICES" == *"skattjakt-api"* && "$SERVICES" == *"skattjakt-analysis-worker"* ]]; then
    pass "both services are named in the trace: $SERVICES"
else
    fail "a service is missing from the trace: $SERVICES"
fi

# --- what a span must not carry ---------------------------------------------
#
# A collector is shared, retained differently, and read by more people than the
# database is. The classification rules that govern logs and metric labels
# govern this too.

echo
echo "what a span must not carry"
VALUES="$(field attribute_values)"
KEYS="$(field attribute_keys)"

LEAKED=0
for forbidden in "Spårbolaget" "5560160680" "4 200 000" "4200000" "850 000" "$ANALYSIS"; do
    if grep -qF "$forbidden" <<<"$VALUES"; then
        fail "a span attribute carries something confidential: $forbidden"
        LEAKED=1
    fi
done
[[ "$LEAKED" -eq 0 ]] && pass "no company name, org number, amount or subject id in any attribute"

if grep -q 'company_id' <<<"$KEYS"; then
    fail "a span attribute is keyed on the company"
else
    pass "no attribute is keyed on the company"
fi

if grep -q 'correlation_id' <<<"$KEYS"; then
    pass "the correlation id is carried, which identifies the work without identifying the customer"
else
    fail "the correlation id is missing, so a span cannot be tied back to a log line"
fi

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "traces leave the process, and connect a request to the work it caused"

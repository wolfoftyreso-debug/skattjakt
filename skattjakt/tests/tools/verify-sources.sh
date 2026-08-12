#!/usr/bin/env bash
# Tests the source verifier against pages whose contents we control.
#
# Why this file exists
# ====================
#
# `tools/verify-sources.py` is the thing that decides whether a rule rests on a
# law somebody has actually read. Every source in the shipped set is currently
# `unretrieved`, because the build environment's proxy blocks the Swedish
# statute databases — so in this environment the verifier has never once
# returned `verified`, and its checking logic has never once run.
#
# Untested verification machinery is worse than none: it produces a green line
# that nobody re-examines. A verifier that silently passes everything and a
# verifier that works look identical from the outside until the day the law
# changes and the first one says nothing.
#
# So this serves fixture pages over localhost and asserts the verifier reaches
# the right verdict on each: a page that says what the rule set claims, a page
# missing the operative figure, a page for the wrong statute, a page without
# the cited paragraph, a URL that 404s, a host that is not listening. It also
# asserts the two properties that matter more than any single verdict:
#
#   * `--write` never invents a `verified` state, and
#   * a failed fetch today does not erase a successful retrieval from before.
#
# Usage: tests/tools/verify-sources.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKDIR="$(mktemp -d)"
PAGES="$WORKDIR/pages"

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }

server_pid=""
cleanup() {
    [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

mkdir -p "$PAGES"

# --- the fixture pages ------------------------------------------------------
#
# Written the way the real ones are: markup around the text, non-breaking
# spaces inside figures, `§§` for a range, entities for the Swedish letters.
# If the verifier only works on clean text it does not work.

cat >"$PAGES/il-30-5.html" <<'HTML'
<!doctype html><html><head><title>Inkomstskattelag (1999:1229)</title>
<style>.x{display:none}</style><script>var noise = "25 procent";</script></head>
<body>
<h1>Inkomstskattelag (1999:1229)</h1>
<div class="chapter"><h2>30&nbsp;kap. Periodiseringsfonder</h2>
<p><b>5&nbsp;&sect;</b>&nbsp;&nbsp;En juridisk person f&aring;r g&ouml;ra avdrag med h&ouml;gst
25&nbsp;procent av &ouml;verskottet av n&auml;ringsverksamheten.</p>
<p><b>6&nbsp;&sect;</b>&nbsp;&nbsp;Enskilda n&auml;ringsidkare f&aring;r g&ouml;ra avdrag med
h&ouml;gst 30&nbsp;procent.</p>
</div></body></html>
HTML

# Same statute, same paragraph, but the rate has moved. This is the case the
# whole program exists for: the law changed and the rule set did not. Note the
# figure the rule set expects still appears on the page — inside a script — so
# a verifier that searched the raw HTML would call this one fine.
cat >"$PAGES/il-30-5-changed.html" <<'HTML'
<!doctype html><html><head><title>Inkomstskattelag (1999:1229)</title>
<style>.x{display:none}</style><script>var noise = "25 procent";</script></head>
<body>
<h1>Inkomstskattelag (1999:1229)</h1>
<div class="chapter"><h2>30&nbsp;kap. Periodiseringsfonder</h2>
<p><b>5&nbsp;&sect;</b>&nbsp;&nbsp;En juridisk person f&aring;r g&ouml;ra avdrag med h&ouml;gst
22&nbsp;procent av &ouml;verskottet av n&auml;ringsverksamheten.</p>
<p><b>6&nbsp;&sect;</b>&nbsp;&nbsp;Enskilda n&auml;ringsidkare f&aring;r g&ouml;ra avdrag med
h&ouml;gst 30&nbsp;procent.</p>
</div></body></html>
HTML

# The right paragraph number in the wrong statute.
cat >"$PAGES/wrong-statute.html" <<'HTML'
<!doctype html><html><body><h1>Mervärdesskattelag (2023:200)</h1>
<p><b>5&nbsp;&sect;</b> Något helt annat.</p></body></html>
HTML

# The right statute, but the cited paragraph is not in the retrieved extract —
# what a paywall stub or a partial fetch looks like.
cat >"$PAGES/truncated.html" <<'HTML'
<!doctype html><html><body><h1>Inkomstskattelag (1999:1229)</h1>
<p>Innehållsförteckning. 30 kap. Periodiseringsfonder ...</p>
<p>För att läsa lagtexten, logga in.</p></body></html>
HTML

# The `script`/`style` stripping has to be real: this page's only occurrence of
# the claimed figure is inside a script, so a verifier that does not strip them
# passes a page that a reader would say does not contain it.
cat >"$PAGES/only-in-script.html" <<'HTML'
<!doctype html><html><head><script>
var text = "30 kap. 5 § ... 25 procent av överskottet";
</script></head><body><h1>Inkomstskattelag (1999:1229)</h1>
<p><b>30 kap. 5 §</b> Texten kunde inte laddas.</p></body></html>
HTML

port="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
(cd "$PAGES" && exec python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1) &
server_pid=$!

for _ in $(seq 1 50); do
    if python3 - "$port" <<'PY' 2>/dev/null; then break; fi
import socket, sys
socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.2).close()
PY
    sleep 0.1
done

base="http://127.0.0.1:$port"
# A port nothing is listening on, for the unreachable case. Bound and released:
# the kernel will not have handed it to anything else in the next second.
dead="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"

# --- the rule sets ----------------------------------------------------------

source_entry() { # id, page-or-url, locator, must_contain-json, [document]
    python3 - "$1" "$2" "$3" "$4" "${5-1999:1229}" <<'PY'
import json, sys
key, url, locator, must, document = sys.argv[1:6]
print(json.dumps({key: {
    "authority": "Regeringskansliet",
    "collection": "SFS",
    "document": document,
    "title": "Inkomstskattelag",
    "locator": locator,
    "url": url,
    "machine_url": url,
    "asserted_claim": "the claim under test",
    "must_contain": json.loads(must),
    "retrieval": {"state": "unretrieved", "at": None, "sha256": None, "note": None},
}})[1:-1])
PY
}

write_ruleset() { # path, entries...
    local path="$1"; shift
    { printf '{ "version": "test", "sources": {'
      local first=1
      for entry in "$@"; do
          [[ $first -eq 1 ]] || printf ','
          first=0
          printf '%s' "$entry"
      done
      printf '} }\n'
    } >"$path"
}

run() { # ruleset, [--write] -> stdout, and sets RC
    set +e
    OUTPUT="$(python3 "$ROOT/tools/verify-sources.py" "$@" 2>&1)"
    RC=$?
    set -e
}

state_of() { # ruleset, key
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sources"][sys.argv[2]]["retrieval"]["state"])' "$1" "$2"
}
field_of() { # ruleset, key, field
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sources"][sys.argv[2]]["retrieval"][sys.argv[3]] or "")' "$1" "$2" "$3"
}

echo
echo "a source that says what the rule set claims"

write_ruleset "$WORKDIR/good.json" \
    "$(source_entry ok "$base/il-30-5.html" "30 kap. 5 §" '["25 procent", "överskottet av näringsverksamheten"]')"
run "$WORKDIR/good.json"
if grep -q "  ok        ok " <<<"$OUTPUT"; then pass "is reported verified"; else fail "is reported verified: $OUTPUT"; fi
if [[ "$RC" == 0 ]]; then pass "exits 0"; else fail "exits 0 (got $RC)"; fi

echo
echo "a source that contradicts it"

write_ruleset "$WORKDIR/changed.json" \
    "$(source_entry rate "$base/il-30-5-changed.html" "30 kap. 5 §" '["25 procent"]')"
run "$WORKDIR/changed.json"
if grep -q "MISMATCH  rate" <<<"$OUTPUT"; then pass "a changed rate is caught"; else fail "a changed rate is caught: $OUTPUT"; fi
if grep -q "25 procent" <<<"$OUTPUT"; then pass "the report names what is missing"; else fail "the report names what is missing"; fi
if [[ "$RC" == 1 ]]; then pass "exits 1 so a pipeline stops"; else fail "exits 1 (got $RC)"; fi

write_ruleset "$WORKDIR/wrong.json" \
    "$(source_entry statute "$base/wrong-statute.html" "30 kap. 5 §" '[]')"
run "$WORKDIR/wrong.json"
if grep -q "does not mention SFS 1999:1229" <<<"$OUTPUT"; then pass "the wrong statute is caught"; else fail "the wrong statute is caught: $OUTPUT"; fi

write_ruleset "$WORKDIR/truncated.json" \
    "$(source_entry paragraph "$base/truncated.html" "30 kap. 5 §" '[]')"
run "$WORKDIR/truncated.json"
if grep -q "could not find 30 kap. 5 §" <<<"$OUTPUT"; then pass "a missing paragraph is caught"; else fail "a missing paragraph is caught: $OUTPUT"; fi

write_ruleset "$WORKDIR/script.json" \
    "$(source_entry scripted "$base/only-in-script.html" "30 kap. 5 §" '["25 procent"]')"
run "$WORKDIR/script.json"
if grep -q "MISMATCH  scripted" <<<"$OUTPUT"; then pass "text inside a script does not count"; else fail "text inside a script does not count: $OUTPUT"; fi

echo
echo "a source that cannot be retrieved"

write_ruleset "$WORKDIR/missing.json" \
    "$(source_entry gone "$base/no-such-page.html" "30 kap. 5 §" '[]')"
run "$WORKDIR/missing.json"
if grep -q "unreached gone.*HTTP 404" <<<"$OUTPUT"; then pass "a 404 is unreachable, not verified"; else fail "a 404 is unreachable: $OUTPUT"; fi
if [[ "$RC" == 2 ]]; then pass "nothing retrieved exits 2"; else fail "nothing retrieved exits 2 (got $RC)"; fi

write_ruleset "$WORKDIR/refused.json" \
    "$(source_entry offline "http://127.0.0.1:$dead/il-30-5.html" "30 kap. 5 §" '[]')"
run "$WORKDIR/refused.json"
if grep -q "unreached offline" <<<"$OUTPUT"; then pass "a refused connection is unreachable"; else fail "a refused connection is unreachable: $OUTPUT"; fi

echo
echo "formatting is not substance"

# The published text sets these with non-breaking spaces and a chapter heading
# separated from the paragraph by intervening markup. A verifier that fails
# here fails on typography and gets switched off.
write_ruleset "$WORKDIR/format.json" \
    "$(source_entry spaced "$base/il-30-5.html" "30 kap. 5 §" '["25 procent av överskottet"]')"
run "$WORKDIR/format.json"
if grep -q "  ok        spaced" <<<"$OUTPUT"; then pass "non-breaking spaces fold"; else fail "non-breaking spaces fold: $OUTPUT"; fi

write_ruleset "$WORKDIR/case.json" \
    "$(source_entry cased "$base/il-30-5.html" "30 kap. 5 §" '["JURIDISK PERSON"]')"
run "$WORKDIR/case.json"
if grep -q "  ok        cased" <<<"$OUTPUT"; then pass "case folds"; else fail "case folds: $OUTPUT"; fi

echo
echo "what --write may and may not do"

write_ruleset "$WORKDIR/write-good.json" \
    "$(source_entry ok "$base/il-30-5.html" "30 kap. 5 §" '["25 procent"]')"
run "$WORKDIR/write-good.json" --write
if [[ "$(state_of "$WORKDIR/write-good.json" ok)" == verified ]]; then pass "a verified source is recorded"; else fail "a verified source is recorded"; fi
digest="$(field_of "$WORKDIR/write-good.json" ok sha256)"
if [[ "$digest" =~ ^[0-9a-f]{64}$ ]]; then pass "with a sha256 of what was read"; else fail "with a sha256 (got '$digest')"; fi
if [[ -n "$(field_of "$WORKDIR/write-good.json" ok at)" ]]; then pass "and a timestamp"; else fail "and a timestamp"; fi

# The same page fetched twice must hash the same, or the record is noise.
run "$WORKDIR/write-good.json" --write
if [[ "$(field_of "$WORKDIR/write-good.json" ok sha256)" == "$digest" ]]; then pass "the hash is stable across runs"; else fail "the hash is stable across runs"; fi

# A page that changed under a recorded hash must change the hash. Without this
# the digest is decoration.
cp "$PAGES/il-30-5-changed.html" "$PAGES/il-30-5.html"
run "$WORKDIR/write-good.json" --write
if [[ "$(field_of "$WORKDIR/write-good.json" ok sha256)" != "$digest" ]]; then
    pass "a changed page changes the hash"
else
    fail "a changed page changes the hash"
fi

write_ruleset "$WORKDIR/write-bad.json" \
    "$(source_entry gone "$base/no-such-page.html" "30 kap. 5 §" '[]')"
run "$WORKDIR/write-bad.json" --write
if [[ "$(state_of "$WORKDIR/write-bad.json" gone)" == unretrieved ]]; then
    pass "an unreachable source is never written verified"
else
    fail "an unreachable source is never written verified"
fi

# The property that keeps the record honest over time: today's proxy failure is
# not evidence about the law, so it must not erase what was read last week.
python3 - "$WORKDIR/write-bad.json" <<'PY'
import json, sys
path = sys.argv[1]
data = json.load(open(path))
data["sources"]["gone"]["retrieval"] = {
    "state": "verified", "at": "2026-01-01T00:00:00+00:00",
    "sha256": "f" * 64, "note": None,
}
json.dump(data, open(path, "w"), ensure_ascii=False, indent=2)
PY
run "$WORKDIR/write-bad.json" --write
if [[ "$(state_of "$WORKDIR/write-bad.json" gone)" == verified &&
      "$(field_of "$WORKDIR/write-bad.json" gone sha256)" == "$(printf 'f%.0s' {1..64})" ]]; then
    pass "an earlier retrieval survives a network failure"
else
    fail "an earlier retrieval survives a network failure"
fi

# But a source that is reachable and now contradicts the rule set must lose it.
write_ruleset "$WORKDIR/demote.json" \
    "$(source_entry rate "$base/il-30-5-changed.html" "30 kap. 5 §" '["25 procent"]')"
python3 - "$WORKDIR/demote.json" <<'PY'
import json, sys
path = sys.argv[1]
data = json.load(open(path))
data["sources"]["rate"]["retrieval"] = {
    "state": "verified", "at": "2026-01-01T00:00:00+00:00",
    "sha256": "a" * 64, "note": None,
}
json.dump(data, open(path, "w"), ensure_ascii=False, indent=2)
PY
run "$WORKDIR/demote.json" --write
if [[ "$(state_of "$WORKDIR/demote.json" rate)" == mismatch ]]; then
    pass "a source that now contradicts loses its verified state"
else
    fail "a source that now contradicts loses its verified state"
fi

echo
echo "without --write nothing on disk moves"

write_ruleset "$WORKDIR/readonly.json" \
    "$(source_entry ok "$base/il-30-5-changed.html" "30 kap. 5 §" '["22 procent"]')"
before="$(sha256sum "$WORKDIR/readonly.json" | cut -d' ' -f1)"
run "$WORKDIR/readonly.json"
if [[ "$(sha256sum "$WORKDIR/readonly.json" | cut -d' ' -f1)" == "$before" ]]; then
    pass "the rule set is untouched"
else
    fail "the rule set is untouched"
fi

echo
echo "the shipped rule set"

# Not a network check — this asserts the verifier can read the real file and
# that every source in it is shaped the way the verifier requires. A registry
# entry missing `machine_url` would otherwise sit there reporting `no url`
# forever and nobody would read the line.
missing="$(python3 - "$ROOT/rules/se-ruleset.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
bad = []
for key, source in data["sources"].items():
    if not (source.get("machine_url") or source.get("url")):
        bad.append(f"{key}: no url")
    for field in ("authority", "collection", "document", "locator", "asserted_claim"):
        if not source.get(field):
            bad.append(f"{key}: no {field}")
    if "retrieval" not in source:
        bad.append(f"{key}: no retrieval record")
print("; ".join(bad))
PY
)"
if [[ -z "$missing" ]]; then pass "every source is fetchable and fully described"; else fail "every source is fetchable: $missing"; fi

count="$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))["sources"]))' "$ROOT/rules/se-ruleset.json")"
if [[ "$count" -ge 20 ]]; then pass "$count sources in the registry"; else fail "only $count sources in the registry"; fi

printf '\npassed %d, failed %d\n' "$passed" "$failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "the source verifier reaches the right verdict on pages we control"

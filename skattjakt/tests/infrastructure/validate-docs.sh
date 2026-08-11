#!/usr/bin/env bash
# Checks that the documentation still describes the system.
#
# Documentation rots silently. These are the couplings that matter — the ones
# where a stale document actively misleads someone at 03:00 — expressed as
# assertions so they break a build instead of a person.
#
# Usage: tests/infrastructure/validate-docs.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }

# --- the eight documents the build order requires --------------------------

echo
echo "required documents"
for doc in ARCHITECTURE SECURITY THREAT_MODEL DATA_MODEL ANALYSIS_PIPELINE \
           RULE_ENGINE DEPLOYMENT RUNBOOK; do
    path="docs/SKATTJAKT_${doc}.md"
    if [[ -s "$path" ]]; then
        pass "$path ($(wc -l < "$path") lines)"
    else
        fail "$path is missing or empty"
    fi
done

# --- every alert has somewhere to send the person it woke ------------------

echo
echo "alert coverage"
if python3 - <<'PYTHON'
import sys

import yaml

alerts = set()
document = yaml.safe_load(open("infrastructure/monitoring/alerts.yaml"))
for group in document["spec"]["groups"]:
    for rule in group["rules"]:
        alerts.add(rule["alert"])

runbook = open("docs/SKATTJAKT_RUNBOOK.md").read()
missing = sorted(a for a in alerts if a not in runbook)

if missing:
    print(f"        alerts with no runbook section: {', '.join(missing)}")
    sys.exit(1)

print(f"        {len(alerts)} alerts, all present in the runbook")
PYTHON
then
    pass "every alert is named in the runbook"
else
    fail "every alert is named in the runbook"
fi

# --- the metrics the alerts query are the metrics the code emits -----------
#
# An alert on a metric nobody publishes is worse than no alert: it looks like
# coverage and fires never.

echo
echo "alerts query metrics that exist"
if python3 - <<'PYTHON'
import re
import sys

import yaml

emitted = set(
    re.findall(r'"(skattjakt_[a-z_]+)"', open("crates/telemetry/src/metrics.rs").read())
)

queried = set()
document = yaml.safe_load(open("infrastructure/monitoring/alerts.yaml"))
for group in document["spec"]["groups"]:
    for rule in group["rules"]:
        for name in re.findall(r"\bskattjakt_[a-z_]+", rule["expr"]):
            # Histograms are queried as _bucket, _sum and _count.
            queried.add(re.sub(r"_(bucket|sum|count)$", "", name))

unknown = sorted(queried - emitted)
if unknown:
    print(f"        alerts query metrics the code never emits: {', '.join(unknown)}")
    sys.exit(1)

print(f"        {len(queried)} skattjakt metrics queried, all emitted by the code")
PYTHON
then
    pass "no alert queries a metric the code does not emit"
else
    fail "no alert queries a metric the code does not emit"
fi

# --- the contract describes the routes that exist --------------------------
#
# The OpenAPI file is the contract (section 17) and the running build serves it.
# A route the contract does not describe is a surface nobody reviewed; a path
# the contract describes and the code does not serve is a promise that 404s.

echo
echo "the contract matches the routes"
if python3 - <<'PYTHON'
import re
import sys

import yaml

spec = yaml.safe_load(open("apps/api/openapi.yaml"))
documented = set(spec["paths"])

source = open("apps/api/src/lib.rs").read()
# `.route("/path", …)` — the path may sit on its own line.
routed = set(re.findall(r'\.route\(\s*"([^"]+)"', source))

# The interface and its icon are pages, not API surface.
routed -= {"/", "/favicon.svg", "/favicon.ico"}

undocumented = sorted(routed - documented)
unserved = sorted(documented - routed)

if undocumented:
    print(f"        routes the contract does not describe: {', '.join(undocumented)}")
if unserved:
    print(f"        contract paths the code does not serve: {', '.join(unserved)}")
if undocumented or unserved:
    sys.exit(1)

print(f"        {len(documented)} paths, all served and all described")
PYTHON
then
    pass "every route is in the contract and every contract path is served"
else
    fail "the contract and the routes have drifted"
fi

# --- the documents point at files that exist -------------------------------

echo
echo "referenced paths exist"
broken=0
while IFS= read -r reference; do
    [[ -e "$reference" ]] || { echo "        docs reference a missing path: $reference"; broken=$((broken + 1)); }
done < <(grep -ohE '`(crates|apps|workers|tests|infrastructure|migrations|rules|docs)/[a-zA-Z0-9_./{}-]+`' docs/*.md \
    | tr -d '`' | grep -vE '[{}]' | sort -u)

if [[ "$broken" -eq 0 ]]; then
    pass "every repository path named in the documents exists"
else
    fail "$broken referenced paths do not exist"
fi

# --- the rule set's review state matches what the documents claim ----------
#
# The documents state that nothing is presented as established while the rule
# set is unreviewed. If a rule is ever marked reviewed, that claim has to be
# revisited rather than left standing.

echo
echo "the review gate"
if python3 - <<'PYTHON'
import json
import sys

rules = json.load(open("rules/se-ruleset.json"))["rules"]
states = {r.get("review", {}).get("state") for r in rules}

docs = open("docs/SKATTJAKT_RULE_ENGINE.md").read()
claims_unreviewed = "has not been reviewed by a tax professional" in docs

if states == {"awaiting_professional_review"}:
    if not claims_unreviewed:
        print("        the rule set is unreviewed but the documents no longer say so")
        sys.exit(1)
    print(f"        all {len(rules)} rules awaiting review, and the documents say so")
elif claims_unreviewed:
    print(f"        rules now carry {states}, but the documents still say unreviewed")
    sys.exit(1)
else:
    print(f"        review states: {states}")
PYTHON
then
    pass "the documents agree with the rule set's review state"
else
    fail "the documents disagree with the rule set's review state"
fi

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "the documentation still describes the system"

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
           RULE_ENGINE DEPLOYMENT RUNBOOK \
           PRODUCT_SURFACE CLIENT_ARCHITECTURE MEMORY_ARCHITECTURE; do
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

# The interface, its stylesheet and its icon are pages, not API surface. The
# list is explicit rather than a prefix rule: a new `/v1/...` route that nobody
# documented must fail this check, and a pattern loose enough to excuse a page
# would eventually excuse an endpoint.
routed -= {
    "/", "/simulations", "/favicon.svg", "/favicon.ico",
    "/ui/app.css", "/ui/index.css", "/ui/index.js",
    "/ui/simulate.css", "/ui/simulate.js",
}

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

# --- the contract has no duplicate keys ------------------------------------
#
# YAML permits a duplicate mapping key and silently keeps the last. A second
# `Problem:` schema in this file shadowed the documented one for a while, so the
# contract described an error body with a `code` field while consumers reading
# the file saw one without. The parser will not tell you; this does.

echo
echo "the contract has no shadowed definitions"
if python3 - <<'PYTHON'
import sys

import yaml


class DuplicateKeyLoader(yaml.SafeLoader):
    pass


def no_duplicates(loader, node, deep=False):
    seen = set()
    for key_node, _ in node.value:
        key = loader.construct_object(key_node, deep=deep)
        if key in seen:
            raise ValueError(f"duplicate key: {key}")
        seen.add(key)
    return yaml.SafeLoader.construct_mapping(loader, node, deep)


DuplicateKeyLoader.add_constructor(
    yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, no_duplicates
)

try:
    yaml.load(open("apps/api/openapi.yaml"), Loader=DuplicateKeyLoader)
except ValueError as error:
    print(f"        {error}")
    sys.exit(1)

print("        no key is defined twice")
PYTHON
then
    pass "no definition in the contract shadows another"
else
    fail "the contract defines something twice"
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

# --- the surface matrix does not claim more of the cluster than it reached --
#
# This check used to assert the string "never applied", which was true until the
# manifests were applied to a real API server. The claim it guards has moved
# rather than gone away, and it moved in the direction that matters: the
# manifests are now admitted by a live cluster, and no container has ever
# started, because the build environment masks CAP_SYS_RESOURCE and the kubelet
# cannot set a pod sandbox's oom_score_adj.
#
# So the assertion is now the *narrower* claim. "Applied" must not quietly
# become "running", and anything that needs a running container — NetworkPolicy
# enforcement above all — must stay on the unverified list until something
# actually runs.

echo
echo "the surface matrix is honest about the cluster"
if grep -q "no pod started" docs/SKATTJAKT_PRODUCT_SURFACE.md; then
    pass "the matrix records that the manifests were applied but nothing ran"
else
    fail "the matrix no longer records that no pod has started"
fi

if grep -q "NetworkPolicy \*enforcement\*" docs/SKATTJAKT_PRODUCT_SURFACE.md; then
    pass "NetworkPolicy enforcement is still listed as unverified"
else
    fail "NetworkPolicy enforcement is no longer listed as unverified"
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

# --- the source registry matches what the documents claim ------------------
#
# The review gate above is now satisfiable a second way: every cited source
# verified. So the documents' claim about retrieval state is load-bearing in
# the same way the review claim is, and needs the same tie to the data.
#
# This also catches the drift the review check cannot: a rule or a figure that
# cites an id nobody put in the registry. `RuleEngine::validate` rejects that at
# startup, but the docs quote a source count, and a count that stops matching is
# how a document starts describing a system that no longer exists.

echo
echo "the source registry"
if python3 - <<'PYTHON'
import json
import re
import sys

data = json.load(open("rules/se-ruleset.json"))
registry = data["sources"]
rules = data["rules"]

problems = []

cited = {s for rule in rules for s in rule.get("sources", [])}
for constants in data["constants"]:
    cited |= {p["source"] for p in constants["parameters"].values()}
unknown = cited - set(registry)
if unknown:
    problems.append(f"cited but not in the registry: {sorted(unknown)}")
orphaned = set(registry) - cited
if orphaned:
    problems.append(f"in the registry but cited by nothing: {sorted(orphaned)}")

for rule in rules:
    if not rule.get("sources"):
        problems.append(f"{rule['rule_id']} cites no source")

states = {key: source["retrieval"]["state"] for key, source in registry.items()}
for key, state in states.items():
    if state == "verified":
        record = registry[key]["retrieval"]
        if not record.get("sha256") or not record.get("at"):
            problems.append(f"{key} claims verified without a hash and a timestamp")

# Collapsed, because the document is hard-wrapped and a claim that happens to
# straddle a line break is the same claim.
docs = re.sub(r"\s+", " ", open("docs/SKATTJAKT_RULE_ENGINE.md").read())
distinct = set(states.values())
count = len(registry)

# The headline claim in the document, and the number beside it.
claims_none_retrieved = "No source in the registry has been retrieved" in docs
if distinct == {"unretrieved"}:
    if not claims_none_retrieved:
        problems.append("nothing has been retrieved but the documents no longer say so")
    if f"all {count} sit at `unretrieved`" not in docs:
        problems.append(f"the documents do not state the registry's size as {count}")
    if f"0 verified, 0 mismatched, {count} unretrieved" not in docs:
        problems.append("the documents' current-state line does not match the registry")
elif claims_none_retrieved:
    problems.append(f"sources now carry {sorted(distinct)}, but the documents say none were retrieved")

if problems:
    for problem in problems:
        print(f"        {problem}")
    sys.exit(1)

print(f"        {count} sources, all cited, states {sorted(distinct)}, and the documents agree")
PYTHON
then
    pass "the documents agree with the source registry"
else
    fail "the documents disagree with the source registry"
fi

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "the documentation still describes the system"

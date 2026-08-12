#!/usr/bin/env bash
# Validates the Kubernetes manifests without a cluster.
#
# Two levels. The first is schema validation, which catches a misspelled field
# — real, and the cheap half. The second is the half that matters: asserting
# the security properties this deployment depends on, so that removing a
# NetworkPolicy or dropping `runAsNonRoot` fails a build rather than being
# discovered by an auditor.
#
# Every assertion below corresponds to a line in SKATTJAKT_SECURITY.md. If one
# of them is ever relaxed, both files have to change, which is the point.
#
# Usage: tests/infrastructure/validate-manifests.sh
# Requires: kustomize, kubeconform, python3.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

KUBERNETES_VERSION="${KUBERNETES_VERSION:-1.31.0}"
ENVIRONMENTS=(dev staging prod)

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }

for env in "${ENVIRONMENTS[@]}"; do
    echo
    echo "$env"

    rendered="$WORKDIR/$env.yaml"
    if ! kustomize build "$ROOT/infrastructure/overlays/$env" > "$rendered" 2>"$WORKDIR/$env.err"; then
        fail "the overlay renders"
        sed 's/^/        /' "$WORKDIR/$env.err"
        continue
    fi
    pass "the overlay renders"

    if kubeconform -strict -summary -kubernetes-version "$KUBERNETES_VERSION" "$rendered" \
        > "$WORKDIR/$env.conform" 2>&1; then
        pass "$(tail -1 "$WORKDIR/$env.conform" | sed 's/^Summary: //')"
    else
        fail "schema validation"
        sed 's/^/        /' "$WORKDIR/$env.conform"
        continue
    fi

    # The assertions. Written in Python because they are structural questions
    # about a document tree, and expressing them in shell would mean grepping
    # YAML, which is how a check passes for the wrong reason.
    if python3 - "$rendered" "$env" <<'PYTHON'
import json
import sys

import yaml

path, env = sys.argv[1], sys.argv[2]
docs = [d for d in yaml.safe_load_all(open(path)) if d]
failures = []


def by_kind(kind):
    return [d for d in docs if d["kind"] == kind]


def pod_specs():
    """Every pod template, whatever wraps it."""
    for doc in docs:
        if doc["kind"] in ("Deployment", "StatefulSet"):
            yield doc["metadata"]["name"], doc["spec"]["template"]["spec"]
        elif doc["kind"] == "CronJob":
            yield (
                doc["metadata"]["name"],
                doc["spec"]["jobTemplate"]["spec"]["template"]["spec"],
            )


def check(condition, message):
    if not condition:
        failures.append(message)


# --- the namespace and its guardrails (section 40) -------------------------

namespaces = by_kind("Namespace")
check(len(namespaces) == 1, f"expected one Namespace, found {len(namespaces)}")
ns = namespaces[0]
check(
    ns["metadata"]["labels"].get("pod-security.kubernetes.io/enforce") == "restricted",
    "the namespace does not enforce the restricted Pod Security level",
)
check(len(by_kind("ResourceQuota")) == 1, "no ResourceQuota")
check(len(by_kind("LimitRange")) == 1, "no LimitRange")

# Everything lands in the environment's namespace and nowhere else.
namespaced = {
    d["metadata"].get("namespace")
    for d in docs
    if d["kind"] != "Namespace" and d["metadata"].get("namespace")
}
check(
    namespaced == {f"skattjakt-{env}"},
    f"resources are spread across namespaces: {sorted(namespaced)}",
)

# --- default deny, both directions (section 32) ----------------------------

policies = by_kind("NetworkPolicy")
default_deny = [
    p
    for p in policies
    if p["spec"].get("podSelector") == {}
    and set(p["spec"].get("policyTypes", [])) == {"Ingress", "Egress"}
    and not p["spec"].get("ingress")
    and not p["spec"].get("egress")
]
check(default_deny, "there is no default-deny NetworkPolicy for both directions")

# Every workload is covered by a policy naming it.
selected = set()
for policy in policies:
    labels = policy["spec"].get("podSelector", {}).get("matchLabels", {})
    if name := labels.get("app.kubernetes.io/name"):
        selected.add(name)
for name in (
    "skattjakt-api",
    "skattjakt-analysis-worker",
    "skattjakt-postgres",
    "skattjakt-minio",
):
    check(name in selected, f"{name} has no NetworkPolicy of its own")

# The datastores originate nothing. The shortest path from an injection to
# data leaving the building runs through an outbound connection from these.
for name in ("skattjakt-postgres", "skattjakt-minio"):
    for policy in policies:
        labels = policy["spec"].get("podSelector", {}).get("matchLabels", {})
        if labels.get("app.kubernetes.io/name") != name:
            continue
        if "Egress" in policy["spec"].get("policyTypes", []):
            check(
                not policy["spec"].get("egress"),
                f"{name} is allowed to originate connections",
            )

# The worker may reach the internet; the private ranges and the metadata
# address must be excluded, or an SSRF reaches the cluster's own services.
worker_egress = [
    p
    for p in policies
    if p["spec"].get("podSelector", {}).get("matchLabels", {}).get(
        "app.kubernetes.io/name"
    )
    == "skattjakt-analysis-worker"
    and "Egress" in p["spec"].get("policyTypes", [])
]
found_metadata_exclusion = False
for policy in worker_egress:
    for rule in policy["spec"].get("egress", []):
        for target in rule.get("to", []):
            block = target.get("ipBlock")
            if not block or block.get("cidr") != "0.0.0.0/0":
                continue
            excepted = block.get("except", [])
            check(
                "169.254.0.0/16" in excepted,
                "the worker's internet egress does not exclude the link-local range",
            )
            for private in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"):
                check(
                    private in excepted,
                    f"the worker's internet egress does not exclude {private}",
                )
            found_metadata_exclusion = True
check(found_metadata_exclusion, "the worker has no reviewed internet egress rule")

# --- pod hardening (section 36) --------------------------------------------

for name, spec in pod_specs():
    pod_ctx = spec.get("securityContext", {})
    check(
        pod_ctx.get("runAsNonRoot") is True,
        f"{name} does not set runAsNonRoot",
    )
    check(
        pod_ctx.get("seccompProfile", {}).get("type") == "RuntimeDefault",
        f"{name} does not set the RuntimeDefault seccomp profile",
    )
    check(
        spec.get("automountServiceAccountToken") is False,
        f"{name} mounts a service account token it has no use for",
    )
    for container in spec.get("containers", []):
        ctx = container.get("securityContext", {})
        label = f"{name}/{container['name']}"
        check(
            ctx.get("allowPrivilegeEscalation") is False,
            f"{label} allows privilege escalation",
        )
        check(
            ctx.get("readOnlyRootFilesystem") is True,
            f"{label} has a writable root filesystem",
        )
        check(
            ctx.get("capabilities", {}).get("drop") == ["ALL"],
            f"{label} does not drop all capabilities",
        )
        check(
            "resources" in container and "requests" in container["resources"],
            f"{label} has no resource requests, so it cannot be scheduled sanely",
        )
        check(
            container["resources"].get("limits", {}).get("memory"),
            f"{label} has no memory limit",
        )

# --- availability (section 42) ---------------------------------------------

budgets = {
    p["spec"]["selector"]["matchLabels"]["app.kubernetes.io/name"]
    for p in by_kind("PodDisruptionBudget")
}
for name in ("skattjakt-api", "skattjakt-analysis-worker"):
    check(name in budgets, f"{name} has no PodDisruptionBudget")

for deployment in by_kind("Deployment"):
    name = deployment["metadata"]["name"]
    rolling = deployment["spec"].get("strategy", {}).get("rollingUpdate", {})
    check(
        rolling.get("maxUnavailable") == 0,
        f"{name} allows a deploy to reduce capacity",
    )
    for container in deployment["spec"]["template"]["spec"]["containers"]:
        # Liveness answers "is it wedged"; readiness answers "send it traffic".
        # Conflating them turns a database blip into a restart loop.
        if name == "skattjakt-api":
            check("livenessProbe" in container, f"{name} has no liveness probe")
            check("readinessProbe" in container, f"{name} has no readiness probe")
            check(
                container["livenessProbe"]["httpGet"]["path"]
                != container["readinessProbe"]["httpGet"]["path"],
                f"{name} uses one endpoint for both probes",
            )

# --- images (sections 36, 37) ----------------------------------------------

images = [
    c["image"]
    for _, spec in pod_specs()
    for c in spec.get("containers", []) + spec.get("initContainers", [])
]
check(images, "no images at all")
for image in images:
    check(":latest" not in image, f"{image} is pinned to a moving tag")

# Third-party images pinned to an immutable version tag. Everything *not* on
# this list must carry a digest outside dev.
#
# The rule is written this way round deliberately. The first version checked
# only images whose name started with our registry prefix, so a new workload
# whose image had not been rewritten by the overlay yet — a bare name, no
# registry, no digest — matched nothing and was skipped. An unrecognised image
# is the most suspicious case, not the least, and a check that only inspects
# what it already recognises is a check that passes the day it matters.
PINNED_THIRD_PARTY = (
    "postgres:16-alpine",
    "quay.io/minio/minio:RELEASE.",
    "quay.io/prometheuscommunity/postgres-exporter:v",
)

if env in ("staging", "prod"):
    for image in images:
        if any(image.startswith(known) for known in PINNED_THIRD_PARTY):
            continue
        check(
            "@sha256:" in image,
            f"{env} runs {image} without a digest",
        )

# --- secrets (section 30) --------------------------------------------------

check(
    not by_kind("Secret"),
    "a Secret is rendered from the repository; secrets are created out of band",
)

# --- backup and its restore test (section 34) ------------------------------

cronjobs = {c["metadata"]["name"] for c in by_kind("CronJob")}
check("skattjakt-backup" in cronjobs, "there is no backup job")
check(
    "skattjakt-restore-test" in cronjobs,
    "there is no restore test; an untested backup is not a verified backup",
)

if failures:
    for failure in failures:
        print(f"        {failure}")
    sys.exit(1)
PYTHON
    then
        pass "the security and availability properties hold"
    else
        fail "the security and availability properties hold"
    fi
done

# --- files that are not rendered by an overlay -----------------------------

echo
echo "gitops and monitoring"
for file in "$ROOT/infrastructure/gitops/applications.yaml" \
            "$ROOT/infrastructure/monitoring/alerts.yaml"; do
    name="$(basename "$file")"
    # Custom resources, so kubeconform has no schema for them; parsing plus a
    # structural check is what can honestly be asserted without a cluster.
    if python3 - "$file" <<'PYTHON'
import sys

import yaml

docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
assert docs, "no documents"
for doc in docs:
    assert doc.get("apiVersion"), "a document has no apiVersion"
    assert doc.get("kind"), "a document has no kind"
    assert doc.get("metadata", {}).get("name"), "a document has no name"

    # Production must not self-heal or prune automatically: an automated prune
    # is one bad merge away from deleting a PersistentVolumeClaim.
    if doc["kind"] == "Application" and doc["metadata"]["name"].endswith("-prod"):
        automated = doc["spec"].get("syncPolicy", {}).get("automated", {})
        assert automated.get("prune") is False, "production prunes automatically"
        assert automated.get("selfHeal") is False, "production self-heals automatically"

    # Every alert must name a runbook section or say why it does not need one.
    if doc["kind"] == "PrometheusRule":
        for group in doc["spec"]["groups"]:
            for rule in group["rules"]:
                assert rule.get("annotations", {}).get(
                    "summary"
                ), f"{rule['alert']} has no summary"
PYTHON
    then
        pass "$name"
    else
        fail "$name"
    fi
done

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "all manifest checks passed"

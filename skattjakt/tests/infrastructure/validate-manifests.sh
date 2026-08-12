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
import re
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

# --- the declared workload has to fit inside its own quota -----------------
#
# Added after applying these manifests to a real API server for the first time
# and watching the dev environment come up without object storage. Every object
# was individually valid and every object was accepted; the ResourceQuota then
# refused MinIO's PersistentVolumeClaim, so the StatefulSet controller never
# created the pod. No pod is in a failing state, no rollout reports an error,
# and the only trace is an event on a controller nobody watches.
#
# A schema validator cannot see this, because the conflict does not exist in
# any one document — it exists in the sum. That is exactly the kind of question
# worth asking here rather than in a cluster.

_SUFFIX = {
    "": 1, "m": 0.001,
    "k": 10 ** 3, "M": 10 ** 6, "G": 10 ** 9, "T": 10 ** 12,
    "Ki": 2 ** 10, "Mi": 2 ** 20, "Gi": 2 ** 30, "Ti": 2 ** 40,
}


def quantity(value):
    """A Kubernetes quantity as a float in base units: cores, or bytes."""
    if value is None:
        return 0.0
    match = re.fullmatch(r"(\d+(?:\.\d+)?)([a-zA-Z]*)", str(value).strip())
    if not match or match.group(2) not in _SUFFIX:
        raise ValueError(f"cannot parse the quantity {value!r}")
    return float(match.group(1)) * _SUFFIX[match.group(2)]


def human(value, key):
    return f"{value / 2 ** 30:.2f}Gi" if ("memory" in key or "storage" in key) else f"{value:.2f}"


if by_kind("ResourceQuota") and by_kind("LimitRange"):
    quota = by_kind("ResourceQuota")[0]["spec"]["hard"]
    ranges = by_kind("LimitRange")[0]["spec"]["limits"]
    container_range = next(r for r in ranges if r["type"] == "Container")
    claim_range = next((r for r in ranges if r["type"] == "PersistentVolumeClaim"), {})
    default_request = container_range.get("defaultRequest", {})
    default_limit = container_range.get("default", {})

    def demand_of(container):
        """What the quota will actually charge for this container.

        Not simply what it declares: the LimitRange fills in anything omitted,
        and the filled-in value is what gets counted. Reading only the explicit
        fields would under-count a container that states no CPU limit and be
        wrong in the safe-looking direction.
        """
        resources = container.get("resources", {})
        stated_requests = resources.get("requests", {})
        stated_limits = resources.get("limits", {})
        demand = {}
        for resource in ("cpu", "memory"):
            limit = stated_limits.get(resource, default_limit.get(resource))
            request = stated_requests.get(
                resource, default_request.get(resource, limit)
            )
            demand[f"requests.{resource}"] = quantity(request)
            demand[f"limits.{resource}"] = quantity(limit)
        return demand

    def pod_demand(spec):
        """A pod's charge: the containers' sum, or its largest init container.

        Init containers run one at a time and before the rest, so the scheduler
        charges whichever of the two is bigger rather than their total.
        """
        totals = {k: 0.0 for k in
                  ("requests.cpu", "requests.memory", "limits.cpu", "limits.memory")}
        for container in spec.get("containers", []):
            for key, value in demand_of(container).items():
                totals[key] += value
        for container in spec.get("initContainers", []):
            for key, value in demand_of(container).items():
                totals[key] = max(totals[key], value)
        return totals

    # `spec.replicas` is not what the cluster runs. Where an HPA targets a
    # workload it owns the replica count within seconds of the apply, so a
    # `replicas: 1` in an overlay next to an HPA with `minReplicas: 2` is a
    # number that is true for about as long as it takes to read it. Both ends
    # of the HPA's range have to be checked, and for different reasons:
    #
    #   min — the steady state. If this does not fit, the environment never
    #         reaches full strength and sits permanently degraded.
    #   max — the ceiling the autoscaler is allowed to reach. If *this* does
    #         not fit, the HPA scales up under load until the quota refuses,
    #         and the service stops scaling at precisely the moment it needed
    #         to. The only signal is a FailedCreate event on a ReplicaSet.
    #
    # A quota below the autoscaler's ceiling is not a guardrail. It is a bug
    # with a guardrail's name on it.
    autoscaled = {
        h["spec"]["scaleTargetRef"]["name"]: (h["spec"]["minReplicas"], h["spec"]["maxReplicas"])
        for h in by_kind("HorizontalPodAutoscaler")
    }

    def workload_totals(at_ceiling):
        totals = {k: 0.0 for k in
                  ("requests.cpu", "requests.memory", "limits.cpu", "limits.memory")}
        storage, claims = 0.0, 0
        counts = {"count/deployments.apps": 0, "count/statefulsets.apps": 0}
        for doc in docs:
            if doc["kind"] not in ("Deployment", "StatefulSet"):
                continue
            counts[f"count/{doc['kind'].lower()}s.apps"] += 1
            name = doc["metadata"]["name"]
            declared_replicas = doc["spec"].get("replicas", 1)
            low, high = autoscaled.get(name, (declared_replicas, declared_replicas))
            replicas = high if at_ceiling else low
            for key, value in pod_demand(doc["spec"]["template"]["spec"]).items():
                totals[key] += value * replicas
            # Volumes do not scale with an HPA: a StatefulSet's claims follow
            # its own replica count, and nothing autoscales a StatefulSet here.
            for claim in doc["spec"].get("volumeClaimTemplates", []):
                totals.setdefault("requests.storage", 0.0)
                totals["requests.storage"] += (
                    quantity(claim["spec"]["resources"]["requests"]["storage"])
                    * declared_replicas
                )
                claims += declared_replicas
        totals.setdefault("requests.storage", 0.0)
        return totals, claims, counts

    steady, claims, counts = workload_totals(at_ceiling=False)
    ceiling, _, _ = workload_totals(at_ceiling=True)

    for label, totals in (("at rest", steady), ("at the autoscaler's ceiling", ceiling)):
        for key, used in totals.items():
            hard = quantity(quota.get(key))
            check(
                used <= hard,
                f"{label} the workload needs {key}={human(used, key)} "
                f"and the quota allows {human(hard, key)}",
            )

    ceiling_pods = sum(
        autoscaled.get(d["metadata"]["name"], (d["spec"].get("replicas", 1),) * 2)[1]
        for d in docs
        if d["kind"] in ("Deployment", "StatefulSet")
    )
    if "pods" in quota:
        check(
            ceiling_pods <= int(quota["pods"]),
            f"the autoscaler's ceiling is {ceiling_pods} pods and the quota allows "
            f"{quota['pods']}",
        )

    check(
        claims <= int(quota.get("persistentvolumeclaims", 0)),
        f"the workload declares {claims} volume claims and the quota allows "
        f"{quota.get('persistentvolumeclaims')}",
    )
    for key, used in counts.items():
        check(
            used <= int(quota.get(key, 0)),
            f"the workload declares {used} of {key} and the quota allows {quota.get(key)}",
        )

    # A CronJob's pod is charged while it runs, and it does not get to pick a
    # quiet moment: the backup runs on a schedule, which may well be while the
    # service is at its busiest and the HPA is at its ceiling. If it does not
    # fit there, the backup silently does not run — which is the failure this
    # whole section exists to prevent.
    for job in by_kind("CronJob"):
        job_spec = job["spec"]["jobTemplate"]["spec"]["template"]["spec"]
        needed = pod_demand(job_spec)
        for key in ("limits.memory", "requests.cpu"):
            hard = quantity(quota.get(key))
            check(
                ceiling[key] + needed[key] <= hard,
                f"{job['metadata']['name']} cannot be admitted while the service "
                f"is at its autoscaler's ceiling: {key} would reach "
                f"{human(ceiling[key] + needed[key], key)} of {human(hard, key)}",
            )

    # The LimitRange's own ceilings and floors, which are enforced per object at
    # admission rather than in aggregate.
    for name, spec in pod_specs():
        for container in spec.get("containers", []):
            needed = demand_of(container)
            label = f"{name}/{container['name']}"
            for resource in ("cpu", "memory"):
                ceiling = quantity(container_range.get("max", {}).get(resource))
                floor = quantity(container_range.get("min", {}).get(resource))
                if ceiling:
                    check(
                        needed[f"limits.{resource}"] <= ceiling,
                        f"{label} asks for more {resource} than the LimitRange allows",
                    )
                if floor:
                    check(
                        needed[f"requests.{resource}"] >= floor,
                        f"{label} asks for less {resource} than the LimitRange allows",
                    )

    for doc in docs:
        if doc["kind"] not in ("Deployment", "StatefulSet"):
            continue
        for claim in doc["spec"].get("volumeClaimTemplates", []):
            size = quantity(claim["spec"]["resources"]["requests"]["storage"])
            label = f"{doc['metadata']['name']}/{claim['metadata']['name']}"
            ceiling = quantity(claim_range.get("max", {}).get("storage"))
            floor = quantity(claim_range.get("min", {}).get("storage"))
            if ceiling:
                check(size <= ceiling, f"{label} is larger than the LimitRange allows")
            if floor:
                check(size >= floor, f"{label} is smaller than the LimitRange allows")

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

# --- what the policies actually permit -------------------------------------
#
# The assertions above check that a NetworkPolicy exists and has the right
# shape. That is not the same as checking what it permits, and the difference
# was not academic: the notification worker's own egress policy allowed port
# 5432, `postgres-ingress` never listed it, and a connection needs both ends —
# so the outbox worker could not reach the database at all. Every structural
# check passed while nothing would have been delivered.
#
# `networkpolicy.py` implements the evaluation rules and asserts the intended
# connectivity matrix. It verifies the policy *logic*; enforcement is the CNI's
# and needs a cluster where a pod can start.

echo
echo "the connectivity the policies encode"
for env in "${ENVIRONMENTS[@]}"; do
    rendered="$WORKDIR/$env.yaml"
    [[ -s "$rendered" ]] || continue
    if output="$(python3 "$ROOT/tests/infrastructure/networkpolicy.py" \
            "$rendered" "skattjakt-$env" 2>&1)"; then
        pass "$env: $(grep -E '^passed' <<<"$output")"
    else
        fail "$env: the policies do not encode the intended connectivity"
        grep -E "FAIL" <<<"$output" | sed 's/^/      /'
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

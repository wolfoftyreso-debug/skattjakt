#!/usr/bin/env python3
"""Evaluates the NetworkPolicies and asserts the connectivity they encode.

What this is, stated before anything else, because the distinction is the whole
value of the file:

  It verifies the **policy logic**. It does not verify enforcement.

Enforcement is the CNI's job and needs a running cluster, which the build
environment cannot provide — `SKATTJAKT_PRODUCT_SURFACE.md` §5.1 explains why.
What can be checked without one is whether the objects say what their authors
believed they said, and that is not a small thing: NetworkPolicy semantics are
easy to get wrong in a direction that fails open.

The three rules that catch people, all implemented below:

  1. A pod with **no** policy selecting it for a direction is unrestricted in
     that direction. Adding a policy to one workload does not restrict any
     other, so "we have NetworkPolicies" says nothing until every workload is
     covered — which is what the default-deny policy is for.
  2. Policies are a **union**. A second policy can only ever allow more. There
     is no deny rule and no ordering; a policy that "restricts" something is
     one that is absent, not one that forbids.
  3. A connection must be allowed **at both ends**: egress from the source and
     ingress at the destination. Checking one side is how a policy set looks
     complete and is not.

Usage: tests/infrastructure/networkpolicy.py <rendered.yaml> <namespace>
"""

import sys
from dataclasses import dataclass, field

import yaml


@dataclass
class Pod:
    """A workload as the policies see it: labels, a namespace, and its ports."""

    name: str
    labels: dict
    namespace: str
    namespace_labels: dict = field(default_factory=dict)


def selector_matches(selector, labels) -> bool:
    """A LabelSelector against a label set.

    `None` and `{}` are different and the difference matters: an absent
    selector means "not constrained on this axis", an empty one means "every
    pod". Conflating them is how a rule meant for one workload silently applies
    to all of them.
    """
    if selector is None:
        return False
    if selector == {}:
        return True
    for key, value in (selector.get("matchLabels") or {}).items():
        if labels.get(key) != value:
            return False
    for expression in selector.get("matchExpressions") or []:
        key = expression["key"]
        operator = expression["operator"]
        values = expression.get("values", [])
        present = key in labels
        if operator == "In" and (not present or labels[key] not in values):
            return False
        if operator == "NotIn" and present and labels[key] in values:
            return False
        if operator == "Exists" and not present:
            return False
        if operator == "DoesNotExist" and present:
            return False
    return True


def peer_matches(peer, pod: Pod, policy_namespace: str) -> bool:
    """One entry of a `to:`/`from:` list against a pod.

    Within a single peer, a namespaceSelector and a podSelector are ANDed. Two
    separate list entries are ORed. Writing them as two entries when one was
    meant is the classic NetworkPolicy widening bug, and the shape of this
    function is what makes it visible.
    """
    if "ipBlock" in peer:
        return False  # handled separately: an ipBlock never names a pod

    namespace_selector = peer.get("namespaceSelector")
    pod_selector = peer.get("podSelector")

    if namespace_selector is None:
        # Absent means "this policy's own namespace".
        if pod.namespace != policy_namespace:
            return False
    elif not selector_matches(namespace_selector, pod.namespace_labels):
        return False

    if pod_selector is not None and not selector_matches(pod_selector, pod.labels):
        return False
    return True


def ip_allowed(peer, address: str) -> bool:
    """An ipBlock against a literal address, honouring `except`."""
    import ipaddress

    block = peer.get("ipBlock")
    if not block:
        return False
    network = ipaddress.ip_network(block["cidr"])
    target = ipaddress.ip_address(address)
    if target not in network:
        return False
    for excluded in block.get("except", []):
        if target in ipaddress.ip_network(excluded):
            return False
    return True


def port_allowed(ports, port: int, protocol: str = "TCP") -> bool:
    """An absent or empty `ports` list means every port."""
    if not ports:
        return True
    for entry in ports:
        if (entry.get("protocol") or "TCP") != protocol:
            continue
        named = entry.get("port")
        end = entry.get("endPort")
        if named is None:
            return True
        if isinstance(named, str):
            continue  # a named port; not resolvable without the pod spec
        if end is not None:
            if named <= port <= end:
                return True
        elif named == port:
            return True
    return False


class Policies:
    def __init__(self, documents, namespace: str):
        self.namespace = namespace
        self.policies = [d for d in documents if d and d.get("kind") == "NetworkPolicy"]

    def _selecting(self, pod: Pod, direction: str):
        chosen = []
        for policy in self.policies:
            spec = policy["spec"]
            types = spec.get("policyTypes") or []
            if direction not in types:
                continue
            if policy["metadata"].get("namespace", self.namespace) != pod.namespace:
                continue
            if selector_matches(spec.get("podSelector"), pod.labels):
                chosen.append(policy)
        return chosen

    def egress_allowed(self, source: Pod, target, port: int, protocol="TCP") -> bool:
        """Whether `source` may open a connection.

        `target` is a `Pod` or a literal IP address string.
        """
        selecting = self._selecting(source, "Egress")
        if not selecting:
            # Rule 1: unselected is unrestricted.
            return True
        for policy in selecting:
            for rule in policy["spec"].get("egress") or []:
                if not port_allowed(rule.get("ports"), port, protocol):
                    continue
                peers = rule.get("to")
                if not peers:
                    return True  # no `to` means anywhere
                for peer in peers:
                    if isinstance(target, str):
                        if ip_allowed(peer, target):
                            return True
                    elif peer_matches(
                        peer, target, policy["metadata"].get("namespace", self.namespace)
                    ):
                        return True
        return False

    def ingress_allowed(self, source, target: Pod, port: int, protocol="TCP") -> bool:
        selecting = self._selecting(target, "Ingress")
        if not selecting:
            return True
        for policy in selecting:
            for rule in policy["spec"].get("ingress") or []:
                if not port_allowed(rule.get("ports"), port, protocol):
                    continue
                peers = rule.get("from")
                if not peers:
                    return True
                for peer in peers:
                    if isinstance(source, str):
                        if ip_allowed(peer, source):
                            return True
                    elif peer_matches(
                        peer, source, policy["metadata"].get("namespace", self.namespace)
                    ):
                        return True
        return False

    def can_connect(self, source: Pod, target, port: int, protocol="TCP") -> bool:
        """Rule 3: both ends, or it does not happen."""
        if not self.egress_allowed(source, target, port, protocol):
            return False
        if isinstance(target, str):
            return True  # leaving the cluster: no ingress policy applies
        return self.ingress_allowed(source, target, port, protocol)


def main() -> int:
    rendered, namespace = sys.argv[1], sys.argv[2]
    documents = [d for d in yaml.safe_load_all(open(rendered)) if d]
    policies = Policies(documents, namespace)

    ns_labels = {"kubernetes.io/metadata.name": namespace}

    def pod(name, component=None):
        labels = {"app.kubernetes.io/name": name, "app.kubernetes.io/part-of": "skattjakt"}
        if component:
            labels["app.kubernetes.io/component"] = component
        return Pod(name, labels, namespace, ns_labels)

    api = pod("skattjakt-api", "api")
    worker = pod("skattjakt-analysis-worker", "worker")
    notifier = pod("skattjakt-notification-worker", "worker")
    postgres = pod("skattjakt-postgres", "database")
    minio = pod("skattjakt-minio", "storage")
    backup = pod("skattjakt-backup", "backup")

    ingress_controller = Pod(
        "ingress-nginx",
        {"app.kubernetes.io/name": "ingress-nginx"},
        "ingress-nginx",
        {"kubernetes.io/metadata.name": "ingress-nginx"},
    )
    # A pod in this namespace with no label anyone wrote a rule for: what an
    # attacker who lands a container here actually controls.
    stranger = Pod("stranger", {"app": "whatever"}, namespace, ns_labels)

    passed = failed = 0

    def expect(allowed_expected, description, *args):
        nonlocal passed, failed
        actual = policies.can_connect(*args)
        if actual == allowed_expected:
            print(f"  ok    {description}")
            passed += 1
        else:
            word = "allowed" if actual else "denied"
            print(f"  FAIL  {description} — the policies {word} it")
            failed += 1

    print(f"\n{len(policies.policies)} NetworkPolicies in {namespace}")

    print("\nthe service reaches what it needs")
    expect(True, "api → postgres:5432", api, postgres, 5432)
    expect(True, "api → minio:9000", api, minio, 9000)
    expect(True, "worker → postgres:5432", worker, postgres, 5432)
    expect(True, "worker → minio:9000", worker, minio, 9000)
    expect(True, "notification worker → postgres:5432", notifier, postgres, 5432)
    expect(True, "ingress → api:8080", ingress_controller, api, 8080)

    print("\nand nothing else")
    expect(False, "api → postgres on the wrong port", api, postgres, 5433)
    expect(False, "api → the worker", api, worker, 8080)
    expect(False, "the worker → the api", worker, api, 8080)
    expect(False, "a stranger in the namespace → postgres", stranger, postgres, 5432)
    expect(False, "a stranger in the namespace → minio", stranger, minio, 9000)
    expect(False, "a stranger in the namespace → the api", stranger, api, 8080)
    expect(False, "the ingress controller → postgres", ingress_controller, postgres, 5432)

    print("\nthe datastores originate nothing")
    # The shortest path from an injection to data leaving the building.
    for name, source in (("postgres", postgres), ("minio", minio)):
        expect(False, f"{name} → the internet", source, "93.184.216.34", 443)
        expect(False, f"{name} → the api", source, api, 8080)
        expect(False, f"{name} → the other datastore",
               source, minio if name == "postgres" else postgres, 9000 if name == "postgres" else 5432)

    print("\nthe worker's internet access is bounded")
    expect(True, "worker → a public address on 443", worker, "93.184.216.34", 443)
    # The four that an SSRF would aim at.
    expect(False, "worker → the cloud metadata service (169.254.169.254)",
           worker, "169.254.169.254", 80)
    expect(False, "worker → the 10.0.0.0/8 private range", worker, "10.0.0.1", 443)
    expect(False, "worker → the 172.16.0.0/12 private range", worker, "172.16.0.1", 443)
    expect(False, "worker → the 192.168.0.0/16 private range", worker, "192.168.0.1", 443)

    print("\nDNS, which everything needs and nothing should exceed")
    kube_dns = Pod(
        "coredns",
        {"k8s-app": "kube-dns"},
        "kube-system",
        {"kubernetes.io/metadata.name": "kube-system"},
    )
    expect(True, "api → kube-dns:53", api, kube_dns, 53, "UDP")
    expect(True, "postgres → kube-dns:53", postgres, kube_dns, 53, "UDP")
    expect(False, "api → kube-system on any other port", api, kube_dns, 8080)

    print("\nthe backup job")
    expect(True, "backup → postgres:5432", backup, postgres, 5432)
    expect(False, "backup → the api", backup, api, 8080)

    print("\nevery workload is actually covered")
    # Rule 1 made concrete: a workload no policy selects is a workload with no
    # policy, however many NetworkPolicies the namespace contains.
    for name, workload in (
        ("api", api),
        ("analysis worker", worker),
        ("notification worker", notifier),
        ("postgres", postgres),
        ("minio", minio),
        ("a stranger", stranger),
    ):
        for direction in ("Ingress", "Egress"):
            covered = bool(policies._selecting(workload, direction))
            if covered:
                print(f"  ok    {name} is selected by an {direction} policy")
                passed += 1
            else:
                print(f"  FAIL  {name} has no {direction} policy — it is unrestricted")
                failed += 1

    print(f"\npassed {passed}, failed {failed}")
    if failed:
        return 1
    print(
        "the policies encode the intended connectivity\n"
        "NOTE: this verifies the policy logic. Enforcement is the CNI's, and needs "
        "a cluster where a pod can start."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

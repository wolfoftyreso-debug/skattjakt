#!/usr/bin/env python3
"""Generates a CycloneDX SBOM from Cargo.lock (section 37).

Written here rather than taken from a tool for one reason: this needs to run in
CI *and* be auditable in a diff, and `cargo-sbom`/`syft` would each add a
binary dependency to the release path in exchange for parsing a TOML file that
is already fully specified. The lockfile is the source of truth for what is in
the binary, and this reads exactly that.

What it does not do: resolve licences or CVEs. Licence text is not in the
lockfile, and a licence field guessed from a package name is worse than an
absent one. `cargo deny` and `cargo audit` cover those, and run alongside this
in CI.

Usage:
    tests/supply-chain/generate-sbom.py Cargo.lock > sbom.cdx.json
    tests/supply-chain/generate-sbom.py --check Cargo.lock
"""

import hashlib
import json
import re
import sys
from pathlib import Path

CYCLONEDX_VERSION = "1.5"


def parse_lockfile(text: str) -> list[dict]:
    """Reads Cargo.lock without a TOML library.

    Cargo.lock is a small, rigidly generated subset of TOML: an array of
    `[[package]]` tables whose values are all quoted strings or arrays of
    quoted strings. Parsing that with a regex is defensible in a way that
    parsing arbitrary TOML would not be, and it keeps this script dependency
    free so it runs in any CI image.
    """
    packages = []
    current: dict | None = None

    for line in text.splitlines():
        stripped = line.strip()
        if stripped == "[[package]]":
            if current:
                packages.append(current)
            current = {}
            continue
        if stripped.startswith("[") and stripped != "[[package]]":
            # A non-package table (`[metadata]`, `[[patch...]]`) ends the run.
            if current:
                packages.append(current)
                current = None
            continue
        if current is None or "=" not in stripped:
            continue

        key, _, raw = stripped.partition("=")
        key = key.strip()
        raw = raw.strip()
        if raw.startswith('"') and raw.endswith('"'):
            current[key] = raw[1:-1]
        elif raw.startswith("["):
            current[key] = re.findall(r'"([^"]*)"', raw)

    if current:
        packages.append(current)
    return [p for p in packages if "name" in p and "version" in p]


def purl(package: dict) -> str:
    """A package URL. Local crates get a distinct namespace so an auditor can
    tell first-party code from a dependency at a glance."""
    name, version = package["name"], package["version"]
    if not package.get("source"):
        return f"pkg:generic/skattjakt/{name}@{version}"
    return f"pkg:cargo/{name}@{version}"


def build_sbom(lockfile: Path) -> dict:
    text = lockfile.read_text()
    packages = parse_lockfile(text)

    components = []
    for package in sorted(packages, key=lambda p: (p["name"], p["version"])):
        component = {
            "type": "library",
            "bom-ref": purl(package),
            "name": package["name"],
            "version": package["version"],
            "purl": purl(package),
            "scope": "required",
        }
        # The registry checksum, where cargo recorded one. This is what makes
        # the SBOM a supply-chain artefact rather than a list of names: it
        # states which bytes were compiled in.
        if checksum := package.get("checksum"):
            component["hashes"] = [{"alg": "SHA-256", "content": checksum}]
        if not package.get("source"):
            component["properties"] = [
                {"name": "skattjakt:origin", "value": "first-party"}
            ]
        components.append(component)

    # A deterministic serial number derived from the lockfile's own content.
    # Two builds of the same lockfile produce byte-identical SBOMs, so a diff
    # between two SBOMs shows dependency changes and nothing else. A random
    # UUID or a timestamp here would make every SBOM differ from every other,
    # which defeats the point of storing them.
    digest = hashlib.sha256(text.encode()).hexdigest()
    serial = (
        f"urn:uuid:{digest[0:8]}-{digest[8:12]}-{digest[12:16]}"
        f"-{digest[16:20]}-{digest[20:32]}"
    )

    return {
        "bomFormat": "CycloneDX",
        "specVersion": CYCLONEDX_VERSION,
        "serialNumber": serial,
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "bom-ref": "pkg:generic/skattjakt",
                "name": "skattjakt",
                "description": "Tax recovery and opportunity engine for Swedish limited companies",
            },
            "properties": [
                {"name": "skattjakt:lockfile-sha256", "value": digest},
                {"name": "skattjakt:component-count", "value": str(len(components))},
            ],
        },
        "components": components,
    }


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    check = "--check" in sys.argv

    lockfile = Path(args[0]) if args else Path("Cargo.lock")
    if not lockfile.exists():
        print(f"{lockfile} not found", file=sys.stderr)
        return 1

    sbom = build_sbom(lockfile)

    if check:
        components = sbom["components"]
        if not components:
            print("the SBOM is empty; the lockfile did not parse", file=sys.stderr)
            return 1
        without_hash = [
            c["name"]
            for c in components
            if "hashes" not in c
            and not any(
                p.get("value") == "first-party" for p in c.get("properties", [])
            )
        ]
        if without_hash:
            # A third-party crate with no checksum is one cargo could not
            # verify, which is a supply-chain hole rather than an untidy file.
            print(
                "third-party components without a checksum: "
                + ", ".join(sorted(without_hash)),
                file=sys.stderr,
            )
            return 1
        first_party = sum(
            1
            for c in components
            if any(p.get("value") == "first-party" for p in c.get("properties", []))
        )
        print(
            f"sbom ok: {len(components)} components "
            f"({first_party} first-party, {len(components) - first_party} vendored)"
        )
        return 0

    json.dump(sbom, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())

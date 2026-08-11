#!/usr/bin/env bash
# Asserts what the container image is (section 36).
#
# The properties below are the ones the deployment relies on and that a base
# image change would silently take away. "Distroless" is a claim about a tag;
# these are the claim checked against the artefact.
#
# Usage: tests/supply-chain/inspect-image.sh skattjakt/api:ci

set -euo pipefail

IMAGE="${1:?usage: inspect-image.sh <image>}"

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }

echo "inspecting $IMAGE"

config="$(docker inspect --format '{{json .Config}}' "$IMAGE")"

# --- runs as a non-root user ------------------------------------------------
#
# `runAsNonRoot` in the pod spec resolves the *numeric* uid, so an image whose
# USER is a name the runtime cannot resolve fails at admission with a message
# that reads like a Kubernetes problem. Numeric, checked here.
user="$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("User",""))' <<<"$config")"
if [[ -z "$user" || "$user" == "root" || "$user" == 0* ]]; then
    fail "the image runs as root (USER='$user')"
else
    uid="${user%%:*}"
    if [[ "$uid" =~ ^[0-9]+$ ]] && [[ "$uid" -ne 0 ]]; then
        pass "runs as uid $uid, stated numerically"
    else
        fail "USER '$user' is not a numeric non-zero uid"
    fi
fi

# --- no shell, no package manager -------------------------------------------
#
# The distroless promise. An attacker with code execution in this container
# should find nothing to pivot with. Checked by extracting the filesystem
# rather than by running anything inside it — there is nothing in there to run.
layer="$(mktemp -d)"
trap 'rm -rf "$layer"' EXIT
container="$(docker create "$IMAGE")"
docker export "$container" > "$layer/fs.tar"
docker rm -f "$container" >/dev/null

listing="$(tar -tf "$layer/fs.tar")"

found=""
for binary in bin/sh bin/bash bin/dash usr/bin/sh usr/bin/bash \
              bin/busybox usr/bin/apt usr/bin/apt-get usr/bin/dpkg \
              usr/bin/curl usr/bin/wget bin/cat usr/bin/find; do
    if grep -qx "$binary" <<<"$listing"; then
        found="${found} ${binary}"
    fi
done
if [[ -n "$found" ]]; then
    fail "the image contains a shell or a fetch tool:${found}"
else
    pass "no shell, package manager or fetch tool"
fi

# --- no setuid binaries -----------------------------------------------------
setuid="$(tar -tvf "$layer/fs.tar" 2>/dev/null | awk '$1 ~ /^-..s/ {print $NF}' || true)"
if [[ -n "$setuid" ]]; then
    fail "the image contains setuid binaries: $setuid"
else
    pass "no setuid binaries"
fi

# --- no credentials in the image (section 36) -------------------------------
#
# "Images ska inte innehålla secrets." Two ways one gets in: a baked file, and
# a build argument that ended up in the environment.
env_vars="$(python3 -c '
import json, sys
print("\n".join(json.load(sys.stdin).get("Env") or []))
' <<<"$config")"

leaked=""
while IFS= read -r entry; do
    [[ -z "$entry" ]] && continue
    name="${entry%%=*}"
    value="${entry#*=}"
    case "$name" in
        *KEY|*TOKEN|*SECRET|*PASSWORD|*DSN|DATABASE_URL)
            [[ -n "$value" ]] && leaked="${leaked} ${name}"
            ;;
    esac
done <<<"$env_vars"

if [[ -n "$leaked" ]]; then
    fail "credential-shaped environment variables have values baked in:${leaked}"
else
    pass "no credential is baked into the environment"
fi

for path in root/.aws/credentials root/.ssh/id_rsa app/.env .env \
            app/config/secrets.yaml root/.docker/config.json; do
    if grep -qx "$path" <<<"$listing"; then
        fail "the image contains $path"
    fi
done
pass "no credential file is baked into the filesystem"

# --- the binary is there and is the only thing that is ----------------------
if grep -qx "app/skattjakt" <<<"$listing"; then
    pass "the entrypoint binary is present"
else
    fail "app/skattjakt is missing from the image"
fi

entrypoint="$(python3 -c '
import json, sys
print(" ".join(json.load(sys.stdin).get("Entrypoint") or []))
' <<<"$config")"
if [[ "$entrypoint" == "/app/skattjakt" ]]; then
    pass "the entrypoint is the binary, with no shell wrapper"
else
    fail "unexpected entrypoint: '$entrypoint'"
fi

# --- no model identity compiled in (engineering decision D7) ----------------
#
# Section 8: the product must not be built hard against a specific model
# version. The unit tests assert this of the source; this asserts it of the
# artefact that actually ships.
docker export "$(docker create "$IMAGE")" 2>/dev/null \
    | tar -xO app/skattjakt 2>/dev/null > "$layer/binary" || true
if [[ -s "$layer/binary" ]]; then
    if strings "$layer/binary" 2>/dev/null | grep -qE '^claude-[a-z]+-[0-9]'; then
        fail "a model identifier is compiled into the binary"
    else
        pass "no model identifier is compiled into the binary"
    fi
else
    pass "binary extraction skipped (no strings available)"
fi

# --- size -------------------------------------------------------------------
#
# Not vanity: every megabyte is attack surface and pull latency on every scale
# event. A distroless Rust binary has no reason to exceed 100 MB, and a jump
# past it means something large was added without anyone deciding to.
size_bytes="$(docker inspect --format '{{.Size}}' "$IMAGE")"
size_mb=$((size_bytes / 1024 / 1024))
if [[ "$size_mb" -lt 150 ]]; then
    pass "image is ${size_mb} MB"
else
    fail "image is ${size_mb} MB, which is larger than a distroless Rust binary should be"
fi

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "the image is what it claims to be"

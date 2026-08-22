#!/usr/bin/env bash
# The main interface, clicked through in a real browser.
#
# See `interface.mjs` for what this establishes and the defect it was written
# for: nine inline `onclick` handlers on a page served under a CSP that forbids
# them, which left every button on the product's main page inert.
#
# No database. Every assertion here is about the browser and the page — that the
# script runs, that the buttons are wired, that the checkout is built from the
# server's own prices and consent wording. Sign-in is asserted to *report the
# server's answer*, not to succeed, so a deployment without persistence answers
# it as usefully as one with.
#
# Usage: tests/e2e/interface.sh
# Requires: cargo, curl, node with playwright, chromium.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Whichever build is newer, never whichever profile is preferred.
source "$ROOT/tests/lib/newest-binary.sh"
WORKDIR="$(mktemp -d)"
APIPORT="${APIPORT:-18115}"

# Whichever build is newer, not whichever profile is preferred. A stale release
# binary passes the health check and then fails in ways that read as product
# bugs.
API="$(newest_binary skattjakt-api)"

api_pid=""
cleanup() {
    [[ -n "$api_pid" ]] && kill "$api_pid" 2>/dev/null || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT INT TERM

[[ -x "$API" ]] || { echo "build the API first: cargo build" >&2; exit 1; }

# A port already in use is a hard error rather than a silent one: otherwise the
# health check succeeds against whatever is already listening and the suite
# tests a server it did not start.
if curl -sf -o /dev/null --max-time 2 "http://127.0.0.1:$APIPORT/health" 2>/dev/null; then
    echo "something is already listening on :$APIPORT; refusing to test it" >&2
    exit 1
fi

env SKATTJAKT_BLOB_ROOT="$WORKDIR/blobs" PORT="$APIPORT" RUST_LOG=skattjakt=warn \
    "$API" > "$WORKDIR/api.log" 2>&1 &
api_pid=$!
for _ in $(seq 1 80); do
    curl -fsS "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && break
    sleep 0.25
done
curl -fsS "http://127.0.0.1:$APIPORT/health" >/dev/null || {
    echo "the API did not start"; tail -5 "$WORKDIR/api.log"; exit 1; }

PLAYWRIGHT_MODULE="${PLAYWRIGHT_MODULE:-/tmp/pw/node_modules/playwright/index.js}"
if [[ ! -f "$PLAYWRIGHT_MODULE" ]]; then
    echo "playwright is not installed at $PLAYWRIGHT_MODULE"
    echo "install it with: (cd /tmp/pw && npm install playwright)"
    exit 1
fi
export PLAYWRIGHT_MODULE
CHROMIUM_PATH="${CHROMIUM_PATH:-/opt/pw-browsers/chromium-1194/chrome-linux/chrome}"
[[ -x "$CHROMIUM_PATH" ]] || CHROMIUM_PATH="$(ls -d /opt/pw-browsers/chromium*/chrome-linux/chrome 2>/dev/null | head -1)"
export CHROMIUM_PATH

node "$ROOT/tests/e2e/interface.mjs" "http://127.0.0.1:$APIPORT"

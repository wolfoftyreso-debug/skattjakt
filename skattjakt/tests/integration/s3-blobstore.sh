#!/usr/bin/env bash
# The S3 blob store against a real MinIO.
#
# The unit tests prove the SigV4 derivation against AWS's published worked
# example. They cannot prove that a whole request is well-formed, because the
# only thing that can is a server that verifies the signature and refuses when
# it does not match. So this runs one.
#
# What this catches that a unit test cannot: a wrong canonical path, a header
# signed but not sent, a percent-encoding mismatch, a payload hash that does not
# bind the body. Every one of those produces a valid-looking signature that
# MinIO rejects with 403.
#
# Usage: tests/integration/s3-blobstore.sh
# Requires: docker, cargo.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONTAINER=skattjakt-minio-test
PORT=19100
ACCESS_KEY=skattjakttest
SECRET_KEY=skattjakttestsecret
BUCKET=skattjakt

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }

cleanup() {
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

echo "starting minio"
docker run -d --name "$CONTAINER" \
    -p "127.0.0.1:$PORT:9000" \
    -e "MINIO_ROOT_USER=$ACCESS_KEY" \
    -e "MINIO_ROOT_PASSWORD=$SECRET_KEY" \
    mirror.gcr.io/minio/minio:latest server /data >/dev/null

# MinIO answers `/minio/health/live` with 200 while it is still formatting its
# pool, and rejects S3 operations with 503 until that finishes. Waiting on the
# health endpoint therefore returns too early and the next request fails —
# intermittently, depending on how busy the disk is. So wait for the operation
# the test actually needs instead: a bucket listing is the cheapest request that
# exercises the same path as everything after it.
for _ in $(seq 1 120); do
    curl -fsS -o /dev/null "http://127.0.0.1:$PORT/" \
        --user "$ACCESS_KEY:$SECRET_KEY" --aws-sigv4 "aws:amz:us-east-1:s3" 2>/dev/null && break
    sleep 0.5
done
curl -fsS -o /dev/null "http://127.0.0.1:$PORT/" \
    --user "$ACCESS_KEY:$SECRET_KEY" --aws-sigv4 "aws:amz:us-east-1:s3" || {
    echo "minio did not become ready"; docker logs "$CONTAINER" | tail -20; exit 1; }
echo "minio ready on :$PORT"

# The bucket, created with MinIO's own client so the test does not depend on the
# code it is testing to set itself up.
docker run --rm --network host --entrypoint sh mirror.gcr.io/minio/minio:latest -c "
    mc alias set t http://127.0.0.1:$PORT $ACCESS_KEY $SECRET_KEY >/dev/null 2>&1
    mc mb --ignore-existing t/$BUCKET >/dev/null 2>&1
" || {
    # Older images ship `mc` at a different path; fall back to the S3 API.
    curl -fsS -X PUT "http://127.0.0.1:$PORT/$BUCKET" \
        --user "$ACCESS_KEY:$SECRET_KEY" --aws-sigv4 "aws:amz:us-east-1:s3" >/dev/null 2>&1 || true
}
echo "bucket ready"

export SKATTJAKT_S3_ENDPOINT="http://127.0.0.1:$PORT"
export SKATTJAKT_S3_BUCKET="$BUCKET"
export SKATTJAKT_S3_ACCESS_KEY="$ACCESS_KEY"
export SKATTJAKT_S3_SECRET_KEY="$SECRET_KEY"
export SKATTJAKT_S3_REGION=us-east-1
export SKATTJAKT_S3_PATH_STYLE=1

echo
echo "the blob store against a server that verifies signatures"
cd "$ROOT"
if cargo test -p skattjakt-store --features live-s3 --test s3_live -- --nocapture --test-threads=1 2>&1 \
    | tee /tmp/s3-live.log | grep -qE '^test result: ok'; then
    pass "every live S3 operation succeeded"
    grep -E '^test .* \.\.\. ok$' /tmp/s3-live.log | sed 's/^/    /'
else
    fail "a live S3 operation failed"
    tail -40 /tmp/s3-live.log
fi

# --- the presigned URL, exercised the way a phone would ---------------------
#
# curl, with no credential at all. If this works, the signature is genuinely
# self-contained and the API never has to touch the bytes.

echo
echo "presigned URLs, used by a client with no credential"

PRESIGN_OUT="$(cargo run --quiet -p skattjakt-store --features live-s3 --example presign 2>/dev/null || true)"
PUT_URL="$(sed -n '1p' <<<"$PRESIGN_OUT")"
GET_URL="$(sed -n '2p' <<<"$PRESIGN_OUT")"

if [[ -n "$PUT_URL" ]]; then
    echo "hej från skattjakt" > /tmp/s3-presign-body.txt
    CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @/tmp/s3-presign-body.txt "$PUT_URL")"
    if [[ "$CODE" == "200" ]]; then
        pass "a presigned PUT is accepted with no credential"
    else
        fail "a presigned PUT was refused (HTTP $CODE)"
    fi

    BODY="$(curl -s "$GET_URL")"
    if [[ "$BODY" == "hej från skattjakt" ]]; then
        pass "a presigned GET returns exactly what was written"
    else
        fail "a presigned GET returned something else: $BODY"
    fi

    # The signature covers the key. Editing the URL to point at another
    # company's object must be refused by the server, not by our code.
    TAMPERED="${GET_URL/companies\/alfa/companies\/beta}"
    CODE="$(curl -s -o /dev/null -w '%{http_code}' "$TAMPERED")"
    if [[ "$CODE" == "403" ]]; then
        pass "editing the key in a presigned URL is refused (403)"
    else
        fail "a tampered presigned URL was not refused (HTTP $CODE)"
    fi

    # And the method. A presigned GET must not be usable to write.
    CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data 'overwritten' "$GET_URL")"
    if [[ "$CODE" == "403" ]]; then
        pass "a presigned GET cannot be used to write (403)"
    else
        fail "a presigned GET was accepted as a write (HTTP $CODE)"
    fi
else
    fail "the presign example produced no URL"
fi

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "the S3 blob store works against a real object store"

#!/usr/bin/env bash
# The whole product, end to end, against a real S3-compatible object store.
#
# The filesystem run proves the product works. This proves it works with the
# storage backend production actually uses — which is a different claim, and the
# one that matters for a deployment with more than one API replica.
#
# Usage: tests/integration/e2e-on-s3.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONTAINER=skattjakt-minio-e2e
PORT=19101

cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup

echo "starting minio"
docker run -d --name "$CONTAINER" \
    -p "127.0.0.1:$PORT:9000" \
    -e "MINIO_ROOT_USER=skattjakte2e" \
    -e "MINIO_ROOT_PASSWORD=skattjakte2esecret" \
    mirror.gcr.io/minio/minio:latest server /data >/dev/null

# MinIO answers `/minio/health/live` with 200 while it is still formatting its
# pool, and rejects S3 operations with 503 until that finishes. Waiting on the
# health endpoint therefore returns too early and the next request fails —
# intermittently, depending on how busy the disk is. So wait for the operation
# the test actually needs instead: a bucket listing is the cheapest request that
# exercises the same path as everything after it.
for _ in $(seq 1 120); do
    curl -fsS -o /dev/null "http://127.0.0.1:$PORT/" \
        --user "skattjakte2e:skattjakte2esecret" --aws-sigv4 "aws:amz:us-east-1:s3" 2>/dev/null && break
    sleep 0.5
done
curl -fsS -o /dev/null "http://127.0.0.1:$PORT/" \
    --user "skattjakte2e:skattjakte2esecret" --aws-sigv4 "aws:amz:us-east-1:s3" || {
    echo "minio did not become ready"; docker logs "$CONTAINER" | tail -20; exit 1; }

curl -fsS -X PUT "http://127.0.0.1:$PORT/skattjakt" \
    --user "skattjakte2e:skattjakte2esecret" \
    --aws-sigv4 "aws:amz:us-east-1:s3" >/dev/null
echo "minio ready with bucket skattjakt"

export SKATTJAKT_S3_ENDPOINT="http://127.0.0.1:$PORT"
export SKATTJAKT_S3_BUCKET=skattjakt
export SKATTJAKT_S3_ACCESS_KEY=skattjakte2e
export SKATTJAKT_S3_SECRET_KEY=skattjakte2esecret
export SKATTJAKT_S3_REGION=us-east-1
export SKATTJAKT_S3_PATH_STYLE=1

exec "$ROOT/tests/e2e/end-to-end.sh"

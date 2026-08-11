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

for _ in $(seq 1 60); do
    curl -fsS "http://127.0.0.1:$PORT/minio/health/live" >/dev/null 2>&1 && break
    sleep 0.5
done
curl -fsS "http://127.0.0.1:$PORT/minio/health/live" >/dev/null || {
    echo "minio did not start"; docker logs "$CONTAINER" | tail -20; exit 1; }

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

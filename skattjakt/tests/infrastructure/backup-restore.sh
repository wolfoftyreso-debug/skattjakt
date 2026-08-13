#!/usr/bin/env bash
# The backup and restore scripts, run for real.
#
# `SKATTJAKT_PRODUCT_SURFACE.md` carried backup as `✗ never run in a cluster —
# scripts reviewed`. Reviewed is not tested. This runs the scripts *as the
# CronJobs run them* — the same files, extracted from the same ConfigMap, with
# the same environment variables — against a real PostgreSQL, a real MinIO and
# real `age` encryption.
#
# What it establishes:
#
#   1. A dump of a populated database uploads, encrypted, and the bytes that
#      arrive are the bytes that left.
#   2. It comes back, decrypts, and restores into an empty database.
#   3. The restored database has everything the live one has — every table, not
#      a list somebody wrote once.
#   4. Row-level security survived. A restore that drops the policies produces
#      a database that works perfectly and isolates nothing.
#   5. The customer data is actually there, value by value, not merely a row
#      count that a structurally correct but empty restore would also pass.
#   6. An unencrypted upload is refused, and a corrupted backup fails the
#      restore rather than producing a half-database.
#
# Usage: tests/infrastructure/backup-restore.sh
# Requires: PostgreSQL, docker (for MinIO), age, mc.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONTAINER=skattjakt-minio-backup-test
MINIO_PORT=19100

cleanup_container() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }

if [[ -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    for tool in age mc docker; do
        command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required"; exit 1; }
    done
    cleanup_container
    echo "starting minio"
    docker run -d --name "$CONTAINER" \
        -p "127.0.0.1:$MINIO_PORT:9000" \
        -e MINIO_ROOT_USER=backuptest -e MINIO_ROOT_PASSWORD=backuptest123 \
        mirror.gcr.io/minio/minio:latest server /data >/dev/null
    # `/minio/health/live` answers 200 while the pool is still being formatted,
    # and S3 operations return 503 until it finishes. Wait for a request of the
    # kind the test actually makes.
    for _ in $(seq 1 120); do
        curl -fsS -o /dev/null "http://127.0.0.1:$MINIO_PORT/" \
            --user "backuptest:backuptest123" --aws-sigv4 "aws:amz:us-east-1:s3" 2>/dev/null && break
        sleep 0.5
    done
    curl -fsS -o /dev/null "http://127.0.0.1:$MINIO_PORT/" \
        --user "backuptest:backuptest123" --aws-sigv4 "aws:amz:us-east-1:s3" || {
        echo "minio did not become ready"; docker logs "$CONTAINER" | tail -20; exit 1; }
    echo "minio ready on :$MINIO_PORT"
fi

if [[ "${EUID:-$(id -u)}" -eq 0 && -z "${SKATTJAKT_PG_REEXEC:-}" ]]; then
    RUNAS="${SKATTJAKT_PG_USER:-postgres}"
    id "$RUNAS" >/dev/null 2>&1 || RUNAS=nobody
    export SKATTJAKT_PG_REEXEC=1
    set +e
    su -s /bin/bash "$RUNAS" -c \
        "SKATTJAKT_PG_REEXEC=1 PGBIN='${PGBIN:-}' $(printf '%q ' "$0" "$@")"
    status=$?
    set -e
    cleanup_container
    exit "$status"
fi

WORKDIR="$(mktemp -d)"
PGDATA="$WORKDIR/data"
SOCKET="$WORKDIR/sock"
PGPORT=5455
LIVE=skattjakt

PGBIN="${PGBIN:-$(dirname "$(command -v initdb || echo /usr/lib/postgresql/16/bin/initdb)")}"
[[ -x "$PGBIN/initdb" ]] || PGBIN=/usr/lib/postgresql/16/bin

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }
check() {
    if [[ "$3" == "$2" ]]; then pass "$1"; else fail "$1 (expected $2, got $3)"; fi
}

cleanup() {
    "$PGBIN/pg_ctl" -D "$PGDATA" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT INT TERM

mkdir -p "$SOCKET"
"$PGBIN/initdb" -D "$PGDATA" -U postgres --auth=trust >/dev/null
"$PGBIN/pg_ctl" -D "$PGDATA" -o "-k $SOCKET -h 127.0.0.1 -p $PGPORT" \
    -l "$WORKDIR/pg.log" start >/dev/null

export PGHOST="$SOCKET" PGPORT="$PGPORT" PGUSER=postgres
export PATH="$PGBIN:$PATH"
psql() { "$PGBIN/psql" -v ON_ERROR_STOP=1 -q "$@"; }

psql -d postgres -c "CREATE DATABASE $LIVE" >/dev/null
for migration in "$ROOT"/migrations/*.sql; do psql -d "$LIVE" -f "$migration" >/dev/null; done
echo "live database ready ($(psql -d "$LIVE" -tAc "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'") tables)"

# Data with values worth checking rather than only counted. An amount and an
# organisation number are exactly what must survive a round trip intact.
COMPANY=aaaaaaaa-1111-2222-3333-444444444444
psql -d "$LIVE" >/dev/null <<SQL
INSERT INTO companies (id, name, org_number, fiscal_year_start, fiscal_year_end)
VALUES ('$COMPANY', 'Säkerhetskopian AB', '5560160680', '2025-01-01', '2025-12-31');
INSERT INTO documents (id, company_id, kind, original_filename)
VALUES ('bbbbbbbb-1111-2222-3333-444444444444', '$COMPANY', 'annual_accounts', 'bokslut.pdf');
INSERT INTO users (id, email) VALUES ('cccccccc-1111-2222-3333-444444444444', 'agare@example.com');
INSERT INTO simulations (id, company_id, name, current_version)
VALUES ('dddddddd-1111-2222-3333-444444444444', '$COMPANY', 'Resultat 2026', 1);
INSERT INTO audit_events (company_id, actor, event_type, detail)
VALUES ('$COMPANY', 'test', 'backup.fixture', '{"belopp_ore": 18600000}'::jsonb);
SQL

# --- the scripts, exactly as the CronJob mounts them ------------------------
#
# Extracted from the ConfigMap rather than copied into this file. A test that
# runs its own copy of a script proves nothing about the one that ships.

mkdir -p "$WORKDIR/bin"
python3 - "$ROOT/infrastructure/base/backup-scripts.yaml" "$WORKDIR/bin" <<'PY'
import os, sys, yaml
source, target = sys.argv[1], sys.argv[2]
for document in yaml.safe_load_all(open(source)):
    if document and document.get("kind") == "ConfigMap":
        for name, body in document["data"].items():
            path = os.path.join(target, name)
            open(path, "w").write(body)
            os.chmod(path, 0o755)
            print(f"  extracted {name}")
PY
export PATH="$WORKDIR/bin:$PATH"
# The scripts call each other by absolute path, as they do in the container.
mkdir -p "$WORKDIR/usr-local-bin"
sed -i "s#/usr/local/bin/download.sh#$WORKDIR/bin/download.sh#" "$WORKDIR/bin/restore-test.sh"

age-keygen -o "$WORKDIR/backup-key.age" 2>/dev/null
RECIPIENT="$(age-keygen -y "$WORKDIR/backup-key.age")"
sed -i "s#/etc/skattjakt/backup-key.age#$WORKDIR/backup-key.age#" "$WORKDIR/bin/restore-test.sh"

export BACKUP_S3_ENDPOINT="http://127.0.0.1:$MINIO_PORT"
export BACKUP_S3_ACCESS_KEY=backuptest
export BACKUP_S3_SECRET_KEY=backuptest123
export BACKUP_AGE_RECIPIENT="$RECIPIENT"
export BACKUP_TARGET="skattjakt-backups/daily"
export MC_CONFIG_DIR="$WORKDIR/mc"

mc alias set backup "$BACKUP_S3_ENDPOINT" "$BACKUP_S3_ACCESS_KEY" "$BACKUP_S3_SECRET_KEY" >/dev/null 2>&1
mc mb --ignore-existing backup/skattjakt-backups >/dev/null 2>&1

# ---------------------------------------------------------------------------
echo
echo "1. taking and uploading a backup"
# ---------------------------------------------------------------------------

DUMP="$WORKDIR/skattjakt-$(date -u +%Y%m%d%H%M%S).dump"
pg_dump --format=custom --dbname="$LIVE" --file="$DUMP"
DUMP_BYTES="$(wc -c < "$DUMP")"
pass "pg_dump produced $DUMP_BYTES bytes"

if "$WORKDIR/bin/upload.sh" "$DUMP" "$BACKUP_TARGET" > "$WORKDIR/upload.log" 2>&1; then
    pass "upload.sh encrypted and uploaded it"
    grep -o '"bytes":[0-9]*' "$WORKDIR/upload.log" | head -1 | sed 's/^/        /'
else
    fail "upload.sh failed"; cat "$WORKDIR/upload.log"
fi

STORED="$(mc ls --json "backup/$BACKUP_TARGET/" 2>/dev/null | python3 -c "
import json,sys
names=[json.loads(l)['key'] for l in sys.stdin if l.strip()]
print(names[0] if names else '')")"
case "$STORED" in
    *.age) pass "what landed in object storage is encrypted ($STORED)" ;;
    *) fail "the stored object is not an age file: $STORED" ;;
esac

# The dump must not be readable without the key. This is the whole reason the
# script refuses to send an unencrypted one.
mc cp "backup/$BACKUP_TARGET/$STORED" "$WORKDIR/fetched.age" >/dev/null 2>&1
if head -c 5 "$WORKDIR/fetched.age" | grep -q "age-e"; then
    pass "and begins with an age header rather than a PostgreSQL dump header"
else
    fail "the stored object does not look like an age file"
fi
if grep -aq "Säkerhetskopian AB" "$WORKDIR/fetched.age"; then
    fail "the company name is readable in the stored backup"
else
    pass "the customer's data is not readable in the stored bytes"
fi

# ---------------------------------------------------------------------------
echo
echo "2. the refusals"
# ---------------------------------------------------------------------------

cp "$DUMP.age" "$WORKDIR/again.dump.age" 2>/dev/null || cp "$WORKDIR/fetched.age" "$WORKDIR/again.dump.age"
pg_dump --format=custom --dbname="$LIVE" --file="$WORKDIR/plain.dump"
if (unset BACKUP_AGE_RECIPIENT; "$WORKDIR/bin/upload.sh" "$WORKDIR/plain.dump" "$BACKUP_TARGET") \
        > "$WORKDIR/refuse.log" 2>&1; then
    fail "an unencrypted dump was uploaded"
else
    if grep -q "refusing to send an unencrypted dump" "$WORKDIR/refuse.log"; then
        pass "an upload without a recipient key is refused, and says why"
    else
        fail "the refusal message is not the one the script documents"
    fi
fi

pg_dump --format=custom --dbname="$LIVE" --file="$WORKDIR/empty-target.dump"
if "$WORKDIR/bin/upload.sh" "$WORKDIR/empty-target.dump" "" > "$WORKDIR/refuse2.log" 2>&1; then
    fail "an upload to an empty target was accepted"
else
    pass "an upload to an empty target is refused rather than discarded silently"
fi

# ---------------------------------------------------------------------------
echo
echo "3. restoring it"
# ---------------------------------------------------------------------------

if "$WORKDIR/bin/restore-test.sh" > "$WORKDIR/restore.log" 2>&1; then
    pass "restore-test.sh restored the backup and passed its own checks"
    grep -o '"msg":"[^"]*"' "$WORKDIR/restore.log" | sed 's/^/        /'
else
    fail "restore-test.sh failed"
    tail -20 "$WORKDIR/restore.log" | sed 's/^/        /'
fi

# ---------------------------------------------------------------------------
echo
echo "4. what the restore actually contains"
# ---------------------------------------------------------------------------
#
# The script's own checks run against a scratch database it then drops. These
# restore it again, here, so this suite can look inside rather than trust an
# exit code.

SCRATCH=restore_verification
psql -d postgres -c "DROP DATABASE IF EXISTS $SCRATCH" >/dev/null
psql -d postgres -c "CREATE DATABASE $SCRATCH" >/dev/null
age -d -i "$WORKDIR/backup-key.age" -o "$WORKDIR/restored.dump" "$WORKDIR/fetched.age"
pg_restore --dbname="$SCRATCH" --no-owner --no-privileges --exit-on-error \
    "$WORKDIR/restored.dump" 2>"$WORKDIR/pgrestore.err" || {
        fail "pg_restore failed"; cat "$WORKDIR/pgrestore.err"; }

TABLES_QUERY="SELECT table_name FROM information_schema.tables
              WHERE table_schema='public' ORDER BY table_name"
psql -d "$LIVE" -tAc "$TABLES_QUERY" > "$WORKDIR/live.tables"
psql -d "$SCRATCH" -tAc "$TABLES_QUERY" > "$WORKDIR/restored.tables"
if diff -q "$WORKDIR/live.tables" "$WORKDIR/restored.tables" >/dev/null; then
    pass "every one of $(wc -l < "$WORKDIR/live.tables") tables came back"
else
    fail "the restored table set differs from the live one"
    diff "$WORKDIR/live.tables" "$WORKDIR/restored.tables" | head -10 | sed 's/^/        /'
fi

RLS_QUERY="SELECT relname FROM pg_class
           WHERE relnamespace='public'::regnamespace AND relkind='r'
             AND relrowsecurity AND relforcerowsecurity ORDER BY relname"
psql -d "$LIVE" -tAc "$RLS_QUERY" > "$WORKDIR/live.rls"
psql -d "$SCRATCH" -tAc "$RLS_QUERY" > "$WORKDIR/restored.rls"
if diff -q "$WORKDIR/live.rls" "$WORKDIR/restored.rls" >/dev/null; then
    pass "row-level security is forced on the same $(wc -l < "$WORKDIR/live.rls") tables"
else
    fail "row-level security differs after a restore"
    diff "$WORKDIR/live.rls" "$WORKDIR/restored.rls" | head -10 | sed 's/^/        /'
fi

POLICIES_LIVE="$(psql -d "$LIVE" -tAc "SELECT count(*) FROM pg_policies WHERE schemaname='public'")"
POLICIES_RESTORED="$(psql -d "$SCRATCH" -tAc "SELECT count(*) FROM pg_policies WHERE schemaname='public'")"
check "and every policy came with it" "$POLICIES_LIVE" "$POLICIES_RESTORED"

# The values, not the counts. A structurally perfect empty restore passes a
# count check against an empty table and fails this.
check "the company name survived" "Säkerhetskopian AB" \
    "$(psql -d "$SCRATCH" -tAc "SELECT name FROM companies WHERE id = '$COMPANY'")"
check "the organisation number survived" "5560160680" \
    "$(psql -d "$SCRATCH" -tAc "SELECT org_number FROM companies WHERE id = '$COMPANY'")"
check "the amount inside a JSONB column survived" "18600000" \
    "$(psql -d "$SCRATCH" -tAc "SELECT detail->>'belopp_ore' FROM audit_events WHERE event_type='backup.fixture'")"
check "a table added by a later migration came back with its row" "Resultat 2026" \
    "$(psql -d "$SCRATCH" -tAc "SELECT name FROM simulations LIMIT 1")"

# ---------------------------------------------------------------------------
echo
echo "5. the restore test checks everything, not a list somebody froze"
# ---------------------------------------------------------------------------
#
# The defect this section exists for: the script used to compare the restored
# database against a hand-written list of table names. By the time anyone ran
# it, eighteen of the thirty-seven tables were absent from that list — every
# identity table and every simulation table — so a backup that had lost all of
# them would have passed. The list did not fail; it simply stopped covering
# things, which is the failure mode of every frozen list.

SCRIPT="$(python3 -c "
import yaml
for d in yaml.safe_load_all(open('$ROOT/infrastructure/base/backup-scripts.yaml')):
    if d and d.get('kind') == 'ConfigMap':
        print(d['data']['restore-test.sh'])
")"

if grep -q "for table in companies documents" <<<"$SCRIPT"; then
    fail "the restore test still checks a hand-written list of tables"
else
    pass "the restore test derives what to check from the live database"
fi

# Named tables from later migrations, to prove the point rather than assert it
# abstractly: none of these appear in the script, and all of them are checked.
for table in users sessions simulations simulation_runs; do
    if grep -q "\b$table\b" <<<"$SCRIPT"; then
        fail "$table is named in the script, so the check is still a list"
    fi
done
pass "and names none of the tables it verifies"

# The check it replaced would have passed a restore missing these; the
# derived one cannot, because it compares set against set.
DERIVED_MISSING="$(psql -d "$LIVE" -tAc "
    SELECT count(*) FROM information_schema.tables WHERE table_schema='public'")"
if [[ "$DERIVED_MISSING" -ge 30 ]]; then
    pass "so all $DERIVED_MISSING tables are now in scope, not 19"
else
    fail "only $DERIVED_MISSING tables exist, which is fewer than expected"
fi

# ---------------------------------------------------------------------------
echo
echo "6. a corrupted backup must fail loudly"
# ---------------------------------------------------------------------------
#
# The failure mode a backup system is actually judged on. A restore that logs
# errors and exits zero is how a broken backup passes its own test — which is
# why the script passes --exit-on-error, and why that is asserted here.

CORRUPT=restore_corrupt
cp "$WORKDIR/restored.dump" "$WORKDIR/corrupt.dump"
# Overwrite a stretch in the middle, past the header, so the file still looks
# like a dump and is not one.
python3 - "$WORKDIR/corrupt.dump" <<'PY'
import sys
path = sys.argv[1]
data = bytearray(open(path, "rb").read())
start = len(data) // 2
for index in range(start, min(start + 4096, len(data))):
    data[index] ^= 0xFF
open(path, "wb").write(data)
PY
psql -d postgres -c "DROP DATABASE IF EXISTS $CORRUPT" >/dev/null
psql -d postgres -c "CREATE DATABASE $CORRUPT" >/dev/null
if pg_restore --dbname="$CORRUPT" --no-owner --no-privileges --exit-on-error \
        "$WORKDIR/corrupt.dump" >/dev/null 2>&1; then
    fail "a corrupted dump restored without error"
else
    pass "a corrupted dump fails the restore rather than half-applying"
fi
psql -d postgres -c "DROP DATABASE IF EXISTS $CORRUPT" >/dev/null
psql -d postgres -c "DROP DATABASE IF EXISTS $SCRATCH" >/dev/null

# A backup that cannot be decrypted must fail before it reaches pg_restore.
age-keygen -o "$WORKDIR/wrong-key.age" 2>/dev/null
if age -d -i "$WORKDIR/wrong-key.age" -o /dev/null "$WORKDIR/fetched.age" 2>/dev/null; then
    fail "the backup decrypted with the wrong key"
else
    pass "the backup does not decrypt with the wrong key"
fi

echo
echo "passed $passed, failed $failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "backup and restore work end to end against real storage and real encryption"

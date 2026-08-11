# Skattjakt — Deployment

How Skattjakt is built, promoted and run. §9 lists honestly what has been
verified from this environment and what has not.

---

## 1. Environments

| | dev | staging | prod |
|---|---|---|---|
| Namespace | `skattjakt-dev` | `skattjakt-staging` | `skattjakt-prod` |
| API replicas | 1 | 1 | 3 |
| Worker replicas | 1 | 1 | 2 |
| Images | by tag | by digest | by digest |
| Argo `prune`/`selfHeal` | on | on | **off** |
| Tracks | `main` | `main` | tag `production` |
| Budget ceiling | 5 SEK | 25 SEK | 25 SEK |

The overlays differ in capacity, image references and hostname. They
deliberately do **not** differ in isolation: the same NetworkPolicies, the same
`pod-security.kubernetes.io/enforce: restricted`, the same probes. An
environment whose isolation differs from production cannot be used to test
whether production's isolation works.

---

## 2. Repository layout

```
skattjakt/
├── apps/api/                 the HTTP surface, its OpenAPI contract, its UI
├── workers/analysis-worker/  the process that runs analyses
├── crates/                   the libraries
├── rules/se-ruleset.json     the versioned rule set
├── migrations/               forward-only SQL
├── infrastructure/
│   ├── base/                 one kustomize base
│   ├── overlays/{dev,staging,prod}/
│   ├── gitops/               Argo CD Application manifests
│   └── monitoring/           PrometheusRule
├── tests/
│   ├── golden/               ten synthetic companies
│   ├── security/             tenant isolation, the attack suite
│   ├── failure/              job-system failure injection
│   ├── infrastructure/       manifest validation
│   ├── supply-chain/         SBOM, image inspection
│   └── e2e/                  the 20-step product test
├── docs/
└── Dockerfile                one file, two images
```

---

## 3. Images

One Dockerfile, two images, selected by `BINARY`. One file rather than two
because the build is identical up to the last `COPY`; two files would be two
things to keep in step, and the one that drifts is the one that stops being
distroless.

```bash
docker build --build-arg BINARY=skattjakt-api             -t skattjakt/api .
docker build --build-arg BINARY=skattjakt-analysis-worker -t skattjakt/analysis-worker .
```

Multi-stage: a Rust builder that compiles dependencies in their own layer, and a
`gcr.io/distroless/cc-debian12:nonroot` runtime containing one binary.

The API image is **13 MB**. Verified absent by
`tests/supply-chain/inspect-image.sh`: any shell, `busybox`, a package manager,
`curl`/`wget`, any setuid binary, any credential-shaped environment variable
with a value, any credential file, and any compiled-in model identifier.

`USER 65532:65532`, numerically — `runAsNonRoot` resolves the numeric uid, and
an image whose `USER` is an unresolvable name fails at admission with a message
that reads like a Kubernetes problem.

**Behind an inspecting proxy**, pass the CA as a build secret. It is consumed by
the discarded builder stage and never reaches the shipped image:

```bash
docker build --secret id=ca,src=/path/to/ca-bundle.crt …
```

---

## 4. Configuration

All from the environment. Nothing secret in git — not even a placeholder, since
a committed placeholder Secret is the thing that gets applied by accident and
then never rotated. `tests/infrastructure/validate-manifests.sh` fails the build
if any `Secret` is rendered from the repository.

### Required

| Variable | Notes |
|---|---|
| `DATABASE_URL` | As `skattjakt_app`, never as the owner |
| `SKATTJAKT_MODEL_ID` | **No compiled-in default.** Name the model this deployment runs on |
| `ANTHROPIC_API_KEY` | Absent → rules-only mode, reported on `/ready` |
| `SKATTJAKT_MODEL_PRICES` | JSON, micro-öre per Mtok. A model with no price cannot be called |
| `SKATTJAKT_ADMIN_TOKEN` | May create companies; grants no company's data |
| `SKATTJAKT_BLOB_ROOT` | Document storage path |

### Optional

| Variable | Default | Notes |
|---|---|---|
| `SKATTJAKT_ANALYSIS_BUDGET_SEK` | 25 | Per-analysis ceiling |
| `SKATTJAKT_MODEL_FALLBACK` | `1` | On. A fallback is always recorded and alerted on; the choice is refuse-or-accept-visibly |
| `SKATTJAKT_MODEL_TIMEOUT_SECS` | 600 | |
| `SKATTJAKT_DB_MAX_CONNECTIONS` | 10 | |
| `RUST_LOG` | `skattjakt=info,sqlx=warn` | `sqlx` at `warn` so it does not log statements |

**A configured model with no price is fatal at startup.** An unpriced call is an
unbounded call — the budget check would pass for it and the cost ceiling would
not exist. Failing here makes it a failed rollout a readiness probe catches,
rather than a worker that starts and dead-letters everything it claims.

### Secrets

Created out of band, by External Secrets or sealed-secrets:

```
skattjakt-secrets           ANTHROPIC_API_KEY, SKATTJAKT_ADMIN_TOKEN, DATABASE_URL
skattjakt-postgres-secrets  POSTGRES_USER, POSTGRES_PASSWORD, EXPORTER_DSN
skattjakt-minio-secrets     MINIO_ROOT_USER, MINIO_ROOT_PASSWORD
skattjakt-backup-secrets    BACKUP_TARGET, BACKUP_S3_*, BACKUP_AGE_RECIPIENT
```

---

## 5. GitOps

Argo CD reconciles each namespace against a path in this repository. The
cluster's state is a function of the repository, so "what is running" is
answerable by reading a commit rather than by asking a person.

Dev and staging have `prune` and `selfHeal` **on**. Without `selfHeal`, GitOps
is a suggestion: the one time someone patches by hand during an incident, that
patch survives silently until it causes the next one.

**Production has both off.** An automated prune is one bad merge away from
deleting the PersistentVolumeClaim holding every customer's documents. Argo
still reports drift within a minute; a human reconciles it. The trade is
explicit and is asserted by the manifest validator.

Production tracks the tag `production`, not a branch, so a merge to `main` is
not a production deploy.

---

## 6. The promotion pipeline

```
  commit
    │
    ├─► test                (fmt, clippy -D warnings, 370 tests, golden dataset)
    ├─► tenant-isolation    (10 checks, real Postgres)
    ├─► security            (39 checks, live API)
    ├─► end-to-end          (20 steps, API + worker as separate processes)
    ├─► contract            (OpenAPI 3.1 parses)
    ├─► manifests           (3 environments × 33 resources, schema + properties)
    ├─► supply-chain        (SBOM, cargo audit, cargo deny)
    └─► container           (both images, inspection, Trivy CRITICAL+HIGH)
    │
    ▼
  merge to main ──► Argo syncs dev, then staging
    │
    ▼
  tag `production` ──► Argo syncs prod (manual sync, no auto-prune)
```

Nothing in that list is advisory. A job that reports and does not fail is a job
that is ignored by the third week.

---

## 7. Migrations

Applied by the **owning** role, not by `skattjakt_app` — the application role
deliberately cannot create tables, so a compromised application cannot alter its
own schema.

Forward-only. There are no `down` scripts: a rollback that drops a column drops
the data in it, and the recovery path for a bad migration is a restore, which is
tested weekly.

Run before the new image rolls, and every migration must be compatible with the
previous version of the code for the duration of a rolling update. In practice:
add columns with defaults, never rename in one step.

---

## 8. Backup and restore

**Daily**, 01:15 UTC. `pg_dump --format=custom`, listed with `pg_restore --list`
to prove it is readable, size-checked, encrypted with `age`, uploaded
off-cluster, and read back to confirm the stored size matches.

The script refuses to upload if no `age` recipient is configured. An unencrypted
dump of every customer's accounts sitting in off-cluster storage is a worse
outcome than a failed backup job, which at least pages someone.

**Weekly**, Sunday 03:00 UTC — the job that makes it a backup. It restores the
latest dump into a throwaway database and asserts three things:

1. it restores at all (`pg_restore --exit-on-error`, because a restore that logs
   forty errors and exits zero is how a broken backup passes its own test);
2. every table the application needs is present, and **forced row-level security
   survived the round trip** — a restore that drops the policies produces a
   database that works perfectly and isolates nothing;
3. the row counts are plausible against production. A restore producing a
   structurally correct but empty database passes (1) and (2) and fails (3),
   which is exactly the case a naive check misses.

**RPO: 24 hours. RTO: 2 hours.** Both stated in `SKATTJAKT_RUNBOOK.md` with the
restore procedure.

---

## 9. What has and has not been verified here

Section 82 forbids claiming something works when it has not been shown to. This
section is that accounting.

### Verified in this environment

| | |
|---|---|
| 370 unit and integration tests | ✅ pass |
| Golden dataset, 10 companies | ✅ precision 1.000, recall 1.000 |
| Tenant isolation, real Postgres | ✅ 10 checks |
| Security suite, live API | ✅ 39 checks |
| Failure injection, real Postgres | ✅ 24 checks |
| End-to-end, API + worker | ✅ 20 steps |
| `cargo fmt`, `clippy -D warnings` | ✅ clean |
| Container image builds | ✅ 13 MB distroless |
| Image inspection | ✅ 9 checks |
| SBOM generation | ✅ 305 components, all checksummed |
| Manifests render (3 environments) | ✅ 33 resources each |
| Manifests validate against K8s 1.31 schemas | ✅ 99/99 |
| Manifest security properties | ✅ all environments |

### Not verified — no cluster is reachable from this environment

| | Status |
|---|---|
| Applying the manifests to a live cluster | Not done |
| NetworkPolicy **enforcement** | Written and asserted structurally; never enforced by a running CNI |
| HPA behaviour under load | Not exercised |
| The Prometheus adapter serving `skattjakt_jobs_queued` as an external metric | Assumed; the worker HPA degrades to `minReplicas` without it |
| Argo CD reconciliation | Manifests written; never applied |
| The backup CronJob end to end | Scripts reviewed; `mc` and `age` availability in the image is assumed |
| The restore test | Same |
| Ingress, TLS, cert-manager | Not exercised |
| Trivy image scan | Configured in CI; not run here |
| cosign signing and SLSA provenance | Configured in the release workflow; never run against a real registry |

The distinction that matters: everything in the first table was run and its
output is in this session. Everything in the second is code that has been
reviewed and validated as far as it can be without a cluster, and is not claimed
to work.

### Known gaps in the code itself

- **Blob storage is a filesystem implementation** of the `BlobStore` trait. The
  MinIO manifests exist; the S3 client does not. Single-node deployments work;
  multiple API replicas would need a shared volume until the S3 client lands.
- **Traces are propagated but not exported.** W3C trace context is parsed,
  minted and carried across the queue, and span ids reach the log stream. There
  is no OTLP exporter and no collector configured.
- **No signed upload URLs.** Uploads go through the API.
- **No OCR.** Scanned PDFs extract nothing; the analysis reports that it could
  not read the document rather than analysing an empty fact set.
- **Postgres is a single replica.** Recovery is restore-from-backup.
- **The rule set is unreviewed.** See `SKATTJAKT_RULE_ENGINE.md` §8.

---

## 10. Local development

```bash
cd skattjakt

# Everything that does not need a database
cargo test --workspace
cargo test -p skattjakt-pipeline --test golden -- --nocapture

# Everything that does
export PGBIN=$(ls -d /usr/lib/postgresql/*/bin | tail -1)
./tests/security/tenant-isolation.sh
./tests/security/security-suite.sh
./tests/failure/job-failures.sh
./tests/e2e/end-to-end.sh

# The manifests
./tests/infrastructure/validate-manifests.sh

# Run it
docker compose -f infrastructure/docker-compose.yml up
```

The API serves its own contract at `/v1/openapi.yaml` and the beta interface at
`/`. Both are compiled in, so a deployed build always serves the exact contract
it was built against.

Without `DATABASE_URL` the service runs statelessly: analyses are computed and
returned, never stored. `/ready` reports which mode is active.

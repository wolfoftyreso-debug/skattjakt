# Skattjakt — Runbook

It is 03:00 and something is wrong. Every alert in
`infrastructure/monitoring/alerts.yaml` has a section here, named by its
`runbook` annotation.

Read §1 first; it is the thirty seconds that decides which section you need.

---

## 1. Orientation

```bash
export NS=skattjakt-prod

kubectl -n $NS get pods
kubectl -n $NS get jobs --sort-by=.metadata.creationTimestamp | tail -5
kubectl -n $NS logs -l app.kubernetes.io/name=skattjakt-api --tail=50
```

The queue is the fastest read on whether the product is working, regardless of
what the alert said:

```sql
SELECT kind, state, count(*),
       max(now() - run_after) FILTER (WHERE state = 'queued') AS oldest_wait
FROM jobs GROUP BY kind, state ORDER BY kind, state;
```

**Following one customer's request.** Ask them for the correlation id from the
`x-correlation-id` response header, then:

```bash
kubectl -n $NS logs -l app.kubernetes.io/part-of=skattjakt --tail=10000 \
  | grep '"correlation_id":"<id>"'
```

Logs contain no company id, no org number and no amounts, by design
(`SKATTJAKT_SECURITY.md` §4). The correlation id is how a request is followed
without any log store holding customer identity. To go from a customer to a
correlation id, query `job_transitions` by their `company_id`.

**Debugging inside a pod.** The images are distroless: no shell, nothing to
`exec` into. That is deliberate. Use an ephemeral container:

```bash
kubectl -n $NS debug -it deploy/skattjakt-api \
  --image=mirror.gcr.io/library/busybox:1.36 --target=api
```

---

## 2. The API is down

**Alert:** `SkattjaktApiDown`

```bash
kubectl -n $NS get pods -l app.kubernetes.io/name=skattjakt-api
kubectl -n $NS describe pod <pod>
kubectl -n $NS logs <pod> --previous
```

| Symptom | Cause | Action |
|---|---|---|
| `CrashLoopBackOff`, exits immediately | Startup validation failed | Read the last log line: it names the misconfiguration |
| `Error: DATABASE_URL is set but unusable` | Database unreachable or credential wrong | §3 |
| `no price is configured for model …` | `SKATTJAKT_MODEL_PRICES` missing an entry | Add the model's price to the ConfigMap and roll |
| `OOMKilled` | Memory limit too low, or a leak | Check `container_memory_working_set_bytes`; raise the limit if genuinely needed |
| `Pending` | ResourceQuota exhausted, or no node has room | `kubectl -n $NS describe quota` |
| Ready but no traffic | Ingress or NetworkPolicy | §12 |

A startup failure is usually correct behaviour. The API and the worker fail
loudly on misconfiguration rather than starting degraded — a readiness probe
catching a bad rollout is much better than a worker that starts and
dead-letters everything it claims.

---

## 3. The API is returning errors

**Alert:** `SkattjaktApiErrorRate`

Find the shape first:

```promql
sum by (route) (rate(skattjakt_http_requests_total{status="5xx"}[5m]))
```

- **One route** → an application bug. Get a correlation id from the logs and
  follow it.
- **Every route** → a dependency. Check the database and MinIO pods.
- **Only `/v1/analyses/stored`** → the queue or the rate limiter. §4.

```bash
kubectl -n $NS get pods -l app.kubernetes.io/name=skattjakt-postgres
kubectl -n $NS exec sts/skattjakt-postgres -c postgres -- pg_isready
```

If the database is up but slow, check connection exhaustion:

```sql
SELECT count(*), state FROM pg_stat_activity
WHERE datname = 'skattjakt' GROUP BY state;
```

`SKATTJAKT_DB_MAX_CONNECTIONS` × (API replicas + worker replicas) must stay
below Postgres's `max_connections`. Three API replicas and two workers at 10
each is 50; the default `max_connections` is 100.

---

## 4. Analyses are queued and not running

**Alert:** `SkattjaktQueueNotDraining` — the oldest queued analysis is over 30
minutes old.

Measured on the *oldest item*, not on depth, because a long queue is fine and a
stalled one is not.

```bash
kubectl -n $NS get pods -l app.kubernetes.io/name=skattjakt-analysis-worker
```

**No worker pods.** The HPA is capped, or the deployment is scaled to zero, or
the pods cannot schedule. `kubectl -n $NS describe hpa skattjakt-analysis-worker`.

**Workers running but idle.** Check whether they are claiming:

```sql
SELECT id, state, attempt, leased_by, leased_until, last_error
FROM jobs WHERE kind = 'analysis' AND state IN ('queued','running','retrying')
ORDER BY run_after LIMIT 20;
```

- **All `retrying` with a future `run_after`** → they are in backoff. Read
  `last_error`; something is failing repeatedly. §6 or §7.
- **All `running` with an expired `leased_until`** → workers died and the reaper
  is not running. Restart a worker; its maintenance loop reaps every 15 seconds.
- **All `queued` and workers idle** → the workers cannot reach the database.
  Check the NetworkPolicy and the credential.

**Immediate mitigation:** scale workers up. The lease makes this safe; two
workers cannot claim one job (proved in `tests/failure/job-failures.sh`).

```bash
kubectl -n $NS scale deploy/skattjakt-analysis-worker --replicas=6
```

---

## 5. A job is in the dead letter queue

**Alert:** `SkattjaktDeadLetters`

A dead letter means an analysis exhausted its attempts. The customer has already
been told it failed — the worker writes a customer-facing message on the
transition to `dead_lettered`, so nobody is watching a spinner.

```sql
SELECT job_id, subject_id, attempts, last_error, correlation_id, created_at
FROM dead_letters WHERE acknowledged_at IS NULL ORDER BY created_at;

SELECT from_state, to_state, event, attempt, detail, at
FROM job_transitions WHERE job_id = '<job_id>' ORDER BY at;
```

`last_error` is a kind, never a message — document content never reaches an
operator's queue view.

| `last_error` | Meaning | Action |
|---|---|---|
| `lease_expired` | The worker died repeatedly | Check for OOM kills or node pressure |
| `provider_transient` | The model provider kept failing | §7 |
| `database_unavailable` | Postgres was down during the attempts | Re-enqueue once it is back |
| `blob_unavailable` | MinIO was unreachable | Check MinIO, then re-enqueue |
| `budget_exhausted` | Hit its cost ceiling | §8 |

To retry, create a **new** analysis. A dead-lettered job is terminal, and that
is deliberate — the state machine's terminal states are terminal, which is what
makes the audit trail trustworthy.

Acknowledge when handled:

```sql
UPDATE dead_letters
SET acknowledged_at = now(), acknowledged_by = '<you>', resolution = '<what you did>'
WHERE job_id = '<job_id>';
```

---

## 6. Analyses succeed but find nothing

**Alert:** `SkattjaktFindsNothing` — over 90% of successful analyses found
nothing in 24 hours.

The service is up, requests succeed, and it is producing nothing useful. No
technical alert fires for this, which is exactly why it exists.

Check extraction first, because it is the usual cause:

```promql
sum(rate(skattjakt_extracted_facts_total[6h]))
  / sum(rate(skattjakt_documents_uploaded_total[6h]))
```

Below about 5 facts per document means extraction has regressed
(`SkattjaktExtractionYieldDropped`).

| Cause | How to tell | Action |
|---|---|---|
| A new statement format | Recent uploads from one customer | Get a sample; add labels to `extract::swedish::LABELS` |
| Scanned PDFs | `page_count` present, facts zero | No OCR. Tell the customer to supply a text PDF |
| A rule regression | Extraction fine, rules not matching | Run the golden dataset: `cargo test -p skattjakt-pipeline --test golden` |
| Model degradation | Discovery returning no candidates | Check refusal and schema-failure rates |

If the golden dataset still passes at precision 1.000 and recall 1.000, the
pipeline is behaving as designed and the input has changed.

---

## 7. Model calls are failing

**Alerts:** `SkattjaktModelRefusals`, `SkattjaktSchemaFailures`

```promql
sum by (task, outcome) (rate(skattjakt_model_calls_total[15m]))
histogram_quantile(0.95, sum by (le) (rate(skattjakt_model_latency_ms_bucket[15m])))
```

| Outcome | Meaning | Action |
|---|---|---|
| `refused` | The model declined | Check which task. A spike on one task usually means a prompt change |
| `schema_violation` | Response did not satisfy the schema | Usually a model change. Check whether a fallback is involved (§9) |
| `truncated` | Hit `max_tokens` | Raise it for that task |
| `error` | Transport or HTTP | Provider outage; retries and backoff will handle a short one |

A provider outage is survivable by design: jobs back off with per-job jitter, so
a hundred analyses that failed in the same second do not retry in the same
second. If the outage is long, the jobs dead-letter and §5 applies.

**Running without the model.** If the provider is down for hours, a rules-only
deployment still produces evidence-backed findings. Remove `ANTHROPIC_API_KEY`
from the secret and roll; `/ready` will report the degraded mode. Restore it
afterwards.

---

## 8. Analyses are hitting the cost ceiling

**Alerts:** `SkattjaktBudgetsExceeded`, `SkattjaktModelSpendRate`

```sql
SELECT analysis_id, limit_micro_ore, spent_micro_ore, calls, exceeded_at
FROM analysis_budgets WHERE exceeded_at IS NOT NULL
ORDER BY exceeded_at DESC LIMIT 20;
```

| Pattern | Meaning |
|---|---|
| One analysis, many calls | A document that makes the pipeline loop. Get the document version id and reproduce |
| Many analyses, few calls each | Prices are wrong, or the ceiling is too low for real documents |
| Spend rate high, no ceiling hits | Volume, not a bug. Check whether it is one tenant |

The ceiling is per analysis and survives retries — three attempts cost one
budget. It is doing its job when it fires; raising it is a decision, not a fix:

```yaml
SKATTJAKT_ANALYSIS_BUDGET_SEK: "40"
```

If one tenant is responsible, the analysis rate limit (20/hour) is the lever.

---

## 9. A model call was served by a fallback

**Alert:** `SkattjaktModelFallbacks`

An analysis was produced by a model nobody chose. That is a reproducibility
problem, which is why any occurrence alerts.

```sql
SELECT analysis_id, requested_model, served_by_model, task, finished_at
FROM model_runs WHERE was_fallback ORDER BY finished_at DESC LIMIT 20;
```

With `SKATTJAKT_MODEL_FALLBACK=0` (the default), the gateway **refuses** the
response and the call fails. Seeing the alert at all with fallback disabled
means the provider substituted and the gateway caught it — working as intended.
Check whether the configured `SKATTJAKT_MODEL_ID` is still available.

With fallback enabled, the analysis completed on a different model. Decide
whether to re-run the affected analyses; the served model is on the record for
each.

---

## 10. Uploads contain instruction-like text

**Alert:** `SkattjaktPromptInjectionSpike`

```promql
sum by (task) (increase(skattjakt_prompt_injection_suspected_total[6h]))
```

This is a smoke detector, not a breach. The defence is that a model response
cannot promote a finding past the evidence gate; the counter is how the *next*
technique gets noticed.

Determine whether it is one customer or many. The counter carries no tenant
label — by design — so correlate through `job_transitions` by time window.

- **One customer, many documents** → likely deliberate. Their findings are still
  gated by the evidence rules; consider contacting them.
- **Many customers** → likely a false positive. Look at what changed: a new
  statement template with a phrase that trips the patterns.
- **A novel technique** → add the pattern to `INJECTION_PATTERNS`, and remember
  it is a monitoring aid, not the lock.

**No action is needed to protect the output.** A model fully compromised by an
injected instruction can, at most, propose a finding that the rule engine then
rejects.

---

## 11. Backups have stopped, or the restore test failed

**Alerts:** `SkattjaktBackupMissing`, `SkattjaktBackupFailed`,
`SkattjaktRestoreTestFailed`, `SkattjaktRestoreTestStale`

**Treat a failed restore test exactly as seriously as a failed backup.** A
backup nobody has restored is not a verified backup, and the restore test is the
only thing standing between "we take backups" and finding out during a disaster.

```bash
kubectl -n $NS get jobs | grep -E 'backup|restore-test'
kubectl -n $NS logs job/<job-name>
```

| Failure | Cause | Action |
|---|---|---|
| `dump is implausibly small` | The dump was truncated | Check disk on the Postgres pod |
| `BACKUP_AGE_RECIPIENT is unset` | Encryption key missing | Set it. The job **refuses** to upload unencrypted, correctly |
| `uploaded size does not match` | Upload truncated | Check the backup target and credentials |
| `restored database is missing tables` | The dump is incomplete | **Do not dismiss this.** Take a manual dump, verify it, then investigate |
| `…tenant tables without forced row-level security` | The policies did not survive | Serious: a restore would produce a database that works and isolates nothing. Check the dump flags |
| `restored X is empty while production has N rows` | The dump is structurally valid and empty | The worst case. Same response as missing tables |

### Restoring for real

**RPO: 24 hours. RTO: 2 hours.**

```bash
# 1. Stop writers. Analyses in flight will be retried; nothing is lost.
kubectl -n $NS scale deploy/skattjakt-api deploy/skattjakt-analysis-worker --replicas=0

# 2. Fetch and decrypt the latest dump.
kubectl -n $NS run restore --rm -it --restart=Never \
  --image=postgres:16-alpine --command -- sh
#   /usr/local/bin/download.sh "$BACKUP_TARGET" /tmp/dump.age
#   age -d -i /etc/skattjakt/backup-key.age -o /tmp/dump /tmp/dump.age

# 3. Restore into a NEW database. Never over the live one — if the restore
#    fails halfway you have neither a working database nor the old one.
#   psql -d postgres -c "CREATE DATABASE skattjakt_restored"
#   pg_restore --dbname=skattjakt_restored --no-owner --no-privileges \
#              --exit-on-error /tmp/dump

# 4. Verify before switching. The same three checks the weekly test runs.
#   psql -d skattjakt_restored -c "SELECT count(*) FROM companies"
#   psql -d skattjakt_restored -tAc "SELECT count(*) FROM pg_class
#     WHERE relname='documents' AND relrowsecurity AND relforcerowsecurity"

# 5. Switch, by renaming rather than by dropping.
#   ALTER DATABASE skattjakt RENAME TO skattjakt_before_restore;
#   ALTER DATABASE skattjakt_restored RENAME TO skattjakt;

# 6. Bring the service back.
kubectl -n $NS scale deploy/skattjakt-api --replicas=3
kubectl -n $NS scale deploy/skattjakt-analysis-worker --replicas=2
```

Keep `skattjakt_before_restore` until the restore is confirmed good. Document
blobs are in MinIO and are not covered by the database dump; a full disaster
recovery restores both, and any analysis whose blobs are missing fails with
`document_hash_mismatch` rather than analysing the wrong bytes.

---

## 12. A tenant boundary may have been crossed

**Treat as a breach until proven otherwise.**

1. **Preserve evidence first.** The retention job deletes on a schedule.

   ```bash
   kubectl -n $NS logs -l app.kubernetes.io/part-of=skattjakt --tail=100000 \
     > /tmp/incident-$(date -u +%Y%m%dT%H%M%SZ).log
   ```

2. **Establish what was actually accessed.** The audit trail is append-only and
   the application cannot have altered it:

   ```sql
   SELECT event, subject_id, detail, created_at
   FROM audit_events WHERE company_id = '<company>'
     AND created_at > now() - interval '7 days'
   ORDER BY created_at;
   ```

3. **Revoke the tokens involved.**

   ```sql
   DELETE FROM api_tokens WHERE company_id = '<company>';
   ```

4. **Re-run the isolation proofs** against a copy of the schema:

   ```bash
   ./tests/security/tenant-isolation.sh
   ./tests/security/security-suite.sh
   ```

5. **Check the role.** The most likely cause of a genuine breach is the
   application connecting as the owner rather than as `skattjakt_app`, which
   would bypass every policy:

   ```sql
   SELECT current_user, rolsuper, rolbypassrls
   FROM pg_roles WHERE rolname = current_user;
   ```

   `skattjakt_app`, `f`, `f`. Anything else is the incident.

---

## 13. A token may be compromised

Detection here is weak and is named as such in the threat model (T11): a stolen
token is indistinguishable from the customer using it.

```sql
-- Issue a replacement first, so the customer is never locked out.
-- Then revoke the old one.
DELETE FROM api_tokens WHERE id = '<token_id>';

-- What it touched.
SELECT event, subject_id, created_at FROM audit_events
WHERE company_id = '<company>' ORDER BY created_at DESC LIMIT 200;
```

Rate limiting bounds the damage rate: 20 analyses/hour and 100 uploads/hour per
tenant.

---

## 14. Disaster recovery

**Total cluster loss.**

1. Rebuild the cluster. Install Argo CD.
2. Apply `infrastructure/gitops/applications.yaml`. Everything else follows from
   the repository — that is the point of GitOps.
3. Restore secrets from the secret store.
4. Restore the database (§11) and the MinIO bucket.
5. Verify before announcing: `/ready` reports persistence and model
   configuration, then run the security suite against the restored service.

**RTO: 4 hours** for total cluster loss, against 2 hours for a database restore
alone. The difference is cluster rebuild and secret restoration.

**Loss of the model provider.** Not a disaster. Run rules-only (§7).

**Loss of MinIO but not Postgres.** Analyses that need a blob fail with
`document_hash_mismatch` — the hash check refuses to analyse bytes that do not
match what was recorded. Completed analyses are unaffected; their results are in
Postgres. Customers re-upload.

---

## 15. Routine operations

**Scaling.**

```bash
kubectl -n $NS scale deploy/skattjakt-analysis-worker --replicas=6
```

Safe at any time. The lease guarantees one worker per job, and a worker draining
during a scale-down has a ten-minute grace period.

**Rolling a deploy.** `maxUnavailable: 0`, so a deploy is not a capacity event.
Workers drain for up to ten minutes; an analysis that does not finish is not
lost, its lease expires and another worker claims it.

**Changing a rule.** Not an operational task. `SKATTJAKT_RULE_ENGINE.md` §9 —
it requires a second reviewer, and the database enforces that.

**Retention.** Runs as a job kind. To check what it would remove:

```sql
SELECT count(*) FROM document_versions
WHERE created_at < now() - interval '730 days';
```

**Deleting a customer's data on request.**

```sql
INSERT INTO deletion_requests (id, company_id, scope, requested_by)
VALUES (gen_random_uuid(), '<company>', 'company', '<who asked>');
```

Recorded before anything is removed, so an interrupted deletion is resumable. A
deletion that half-completed and left no record of itself is the one failure
mode that cannot be recovered from. Blobs go before rows — the storage key is
reachable only through the row.

The audit trail survives the deletion, by design: it holds identifiers and
outcomes, not the customer's economy, and it is the only record of what was
deleted and when.

---

## 15A. A source contradicts the rule set

`SkattjaktSourceContradictsRules`. The analysis worker fetched a paragraph the
rule set cites and it no longer contains what the rule assumes.

**This is not an outage.** Nothing is down, no customer request is failing, and
the engine has already done the safe thing: every finding resting on that rule
is capped at "investigate" rather than presented. What it needs is somebody who
can read a statute.

1. **Find out which source, and what changed.**

   ```bash
   kubectl -n $NS exec deploy/skattjakt-postgres -- \
     psql -U skattjakt -c "SELECT source_id, note, retrieved_at, sha256
                           FROM source_retrievals WHERE state = 'mismatch'"
   ```

   `note` names the specific string that went missing — usually a figure, e.g.
   "the source does not contain: 25 procent".

2. **Read the paragraph.** The registry entry in `rules/se-ruleset.json` has the
   `url` a person can open and the `asserted_claim` the rule set believed.

3. **Decide which of the three it is.**

   | What you find | What to do |
   |---|---|
   | The law changed | Update the rule and its `must_contain`, bump the rule version, re-run the golden dataset. The old version stays in the evidence graph — earlier analyses cited it honestly |
   | The rule was always wrong | Same, plus check `graph.affected_findings` for analyses that rested on it and decide whether customers need telling |
   | The publisher reformatted the page | The claim still holds; adjust `must_contain` to a phrasing that survives the new layout, and say so in the commit |

4. **Re-check without waiting six hours for the sweep.**

   ```bash
   kubectl -n $NS exec deploy/skattjakt-analysis-worker -- \
     skattjakt-analysis-worker verify-sources --write
   ```

**What not to do:** do not edit `source_retrievals` to clear the state. The row
is written by a retrieval and the database refuses a `verified` without a hash
and a timestamp. Marking it by hand would remove the alert and leave the wrong
figure in the arithmetic.

### The related alerts

- **`SkattjaktSourcesUnreachable`** — six hours of failed fetches. Check egress
  first (`curl` the `machine_url` from a worker pod); a moved document shows as
  a 404 in `note`, a blocked path as a connection failure. An earlier verified
  retrieval is deliberately *kept* through this, so nothing is downgraded — but
  the verification is ageing.
- **`SkattjaktSourceSweepStopped`** — the metric series is absent, meaning no
  sweep has finished in twelve hours. Usually the worker is down (§4 covers
  that) or the advisory lock is held by a pod that is wedged rather than dead.
  Check `pg_locks` for the sweep lock, and restart the holder.

---

## 16. Alert index

| Alert | Section |
|---|---|
| `SkattjaktApiDown` | §2 |
| `SkattjaktApiErrorRate`, `SkattjaktApiLatency` | §3 |
| `SkattjaktQueueNotDraining`, `SkattjaktQueueDepth`, `SkattjaktNoWorkers` | §4 |
| `SkattjaktDeadLetters` | §5 |
| `SkattjaktFindsNothing`, `SkattjaktExtractionYieldDropped` | §6 |
| `SkattjaktModelRefusals`, `SkattjaktSchemaFailures` | §7 |
| `SkattjaktBudgetsExceeded`, `SkattjaktModelSpendRate` | §8 |
| `SkattjaktModelFallbacks` | §9 |
| `SkattjaktPromptInjectionSpike` | §10 |
| `SkattjaktBackupMissing`, `SkattjaktBackupFailed`, `SkattjaktRestoreTestFailed`, `SkattjaktRestoreTestStale` | §11 |
| `SkattjaktRateLimiting` | §13 |
| `SkattjaktSourceContradictsRules`, `SkattjaktSourcesUnreachable`, `SkattjaktSourceSweepStopped` | §15A |
| `SkattjaktDiskFilling` | Expand the PVC; check the retention job is running |

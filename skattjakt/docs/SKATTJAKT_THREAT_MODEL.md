# Skattjakt — Threat model

Each threat below states its likelihood, its impact, what mitigates it, how the
system would detect it, and what to do when it happens.

Likelihood and impact are judgements about *this* product, not generic ratings.
Skattjakt holds complete year-end accounts for businesses that compete with each
other, and produces figures a customer may act on with Skatteverket. Those two
facts set the scale.

**Impact:** `critical` — the product would not survive it. `high` — a customer
suffers a real loss. `medium` — degraded service or an internal cost.
**Likelihood:** over the beta and first commercial year, assuming the controls
described here are in place.

---

## The assets, in order

| Asset | Why it matters |
|---|---|
| One tenant's financial data | Competitors are customers of the same product |
| The correctness of a stated tax position | A customer may act on it with Skatteverket |
| Document blobs | Complete year-end accounts, including personal data |
| API tokens | A token is total access to one tenant |
| The audit trail | The only record of what was done and when |
| Model spend | Unbounded cost is an availability threat |

## The adversaries

| Who | Capability | What they want |
|---|---|---|
| **A customer** | A valid token; full control of uploaded document content | Another tenant's data, or a fabricated deduction in their own report |
| **An unauthenticated attacker** | Network access to the ingress | Any data; a foothold |
| **A compromised dependency** | Code execution inside a pod | Data exfiltration; lateral movement |
| **A compromised model provider** | Full control of model responses | To have Skattjakt state something false |
| **An operator making a mistake** | Cluster access | Nothing — but the blast radius is the same |

The first is the one most often underweighted. A customer is authenticated, is
supposed to be here, and controls the bytes of a document that the system parses
and sends to a model.

---

## T1 — Cross-tenant data access

**Likelihood:** medium — the most likely serious bug in any multi-tenant system.
**Impact:** critical.

**Mitigation.** PostgreSQL row-level security with `FORCE`, so the policy binds
even the table owner. The application connects as a non-superuser, non-owner
role. The tenant is set per transaction via `set_config`. `Tenant<'_>` is the
only path to a tenant table from Rust. The tenant is derived from the token,
never from the request.

The property that follows: **a query that forgets its tenant returns nothing.**
The failure mode of the most likely mistake is an empty result, not a leak.

**Detection.** `tests/security/tenant-isolation.sh` (10 checks, real cluster)
and `tests/security/security-suite.sh` §1 (6 checks, live API) run on every
commit. At runtime, an authorisation failure is logged with its correlation id;
a spike in 403s from one token is visible in `skattjakt_http_requests_total`.

**Response.** Treat as a breach until proven otherwise. `SKATTJAKT_RUNBOOK.md` §
"a tenant boundary may have been crossed": revoke the tokens involved, query the
audit trail for what that company id touched, and preserve the log window before
the retention job runs.

**Residual risk.** The three tables outside RLS (`api_tokens`, `jobs`,
`job_transitions`/`dead_letters`). Each is constrained to identifiers, state and
timing, with no amounts, no document text and no payload column. A read of the
`jobs` table reveals that a company had an analysis and when — not what it
found.

---

## T2 — A fabricated finding presented as established

**Likelihood:** high — this is what language models do when unconstrained.
**Impact:** critical. A customer acting on a wrong deduction is exposed to
Skatteverket, and the product's entire claim is that its output is trustworthy.

**Mitigation.** Four independent gates, and a finding must pass all of them:

1. **The evidence gate.** `EvidenceChain::validate_actionable` requires at
   least one document value *and* at least one versioned rule. A model
   judgement alone can never satisfy it.
2. **The confidence caps.** No rule match, a contradiction score at or above
   0.5, or no document evidence each force the score below actionable —
   fail-closed, regardless of the other factors.
3. **The falsification pass may only demote.** The skeptic can remove a finding
   and can never promote one.
4. **The review gate.** While the rule set is unreviewed, no finding can be
   presented as `identified`. The best any finding reaches is "needs
   verification".

Money is an integer öre range throughout; there is no type in the domain model
that can express a single-figure estimate.

**Detection.** The golden dataset of ten synthetic companies runs on every
commit and fails the build on any false positive, and on any finding presented
as established. Current: precision 1.000, recall 1.000, 57 true positives, 0
false positives.

**Response.** If a wrong finding reaches a customer: use the evidence graph to
compute the blast radius (`affected_analyses` for the rule version), notify
every affected customer, and take the rule out of the set through the governance
workflow in `SKATTJAKT_RULE_ENGINE.md`.

**Residual risk.** The rule set itself has not been professionally reviewed. A
rule that is wrong produces findings that pass every gate, because the gates
check that a rule matched, not that the rule is correct. This is why the review
gate exists and why it stays on.

---

## T3 — Prompt injection through an uploaded document

**Likelihood:** high. Trivial to attempt, and the person who benefits is the
person uploading.
**Impact:** high, bounded by T2's gates.

**Mitigation.** Four layers, described in full in `SKATTJAKT_SECURITY.md` §5:
structural separation of data from instructions, delimiter integrity verified at
the gateway boundary, detection as a signal rather than a defence, and — the
layer that actually holds — output constrained to a schema with no path from a
model response to any capability.

The reasoning worth repeating: a model fully compromised by an injected
instruction can, at most, propose a finding. That finding still has to match a
versioned rule and a document value. The injection defence is not the last line;
T2's gates are.

**Detection.** `skattjakt_prompt_injection_suspected_total`, by category, with
an alert on more than ten in an hour.

**Response.** `SKATTJAKT_RUNBOOK.md` § "uploads contain instruction-like text".
Determine whether it is one customer or many; if a novel technique got past the
patterns, add it and note that the patterns are a smoke detector, not the lock.

**Residual risk.** The pattern list will always be behind. This is accepted
because it is a monitoring aid rather than a control.

---

## T4 — A compromised model provider

**Likelihood:** low.
**Impact:** high.

**Mitigation.** Identical to T3, and for the same reason: the response is JSON
against a schema, and it cannot promote a finding past the evidence gate.
Additionally, the provider's response names the model that served it, and a
mismatch against the requested model is recorded as a fallback — refused
outright when fallback is disabled, which is the default.

**Detection.** `skattjakt_model_fallbacks_total` alerts on any occurrence.
`skattjakt_model_schema_failures_total` catches malformed responses.

**Residual risk.** A provider that returns plausible, schema-valid, subtly wrong
reasoning would not be detected by any of this. Only the rule engine and the
evidence gate stand between that and a customer — which is the argument for
those gates being deterministic code rather than a prompt.

---

## T5 — Customer data in logs, metrics or traces

**Likelihood:** medium. The failure mode is a `tracing::info!` added at 02:00
during an incident with the value already in scope.
**Impact:** high. Log stores have different retention, different access lists
and different backup policies than the database.

**Mitigation.** Enforced at the emitter, not by review. `LabelSet::insert`
refuses anything above `PUBLIC` and rejects unbounded label names outright.
`LogRecord` fields carry a classification and are replaced with `[redacted]` on
render. `LogRecord::message` is `&'static str`, so a formatted message cannot
carry an amount. There is no escape hatch.

Correlation ids are what make this workable: one request can be followed end to
end without any log store holding a company id.

**Detection.** `tests/security/security-suite.sh` §8 greps the live service's
log output and `/metrics` body for tokens, org numbers and amounts, on every
commit.

**Residual risk.** A dependency logging on its own account. `RUST_LOG` is set to
`warn` for `sqlx` and `tower_http` in every environment for exactly this reason.

---

## T6 — Analyses lost during a deploy or a node failure

**Likelihood:** high — deploys and evictions are routine.
**Impact:** medium. No data loss, but a customer waits for an answer that never
comes and no alert fires, because from the API's point of view nothing failed.

**Mitigation.** The job row is the record. A worker holds a lease and extends
it while working; if the pod dies, the lease expires and another worker claims
the job on its next attempt. The attempt count is not refunded, so a job that
keeps killing its pod dead-letters rather than looping forever. The worker
catches SIGTERM and has a ten-minute termination grace period.

**Detection.** `SkattjaktQueueNotDraining` fires when the oldest queued analysis
is over 30 minutes old — deliberately measured on the *oldest item* rather than
on depth, because a long queue is fine and a stalled one is not.
`SkattjaktDeadLetters` fires on any dead letter.

**Proved by** `tests/failure/job-failures.sh`: 24 checks including a dead pod's
job returning to the queue, a repeatedly-crashing job dead-lettering, a live
lease not being stolen, and eight concurrent workers claiming one job exactly
once.

**Response.** `SKATTJAKT_RUNBOOK.md` § "a job is in the dead letter queue".

---

## T7 — Runaway model spend

**Likelihood:** medium. A document that makes the model loop, or a retry storm
after a provider outage.
**Impact:** medium — money, and an availability threat once a quota is hit.

**Mitigation.** A per-analysis ceiling (default 25 SEK) and a call ceiling
(24), checked **before** each call with the worst-case cost. Failed calls are
still charged, because they billed their input tokens. The budget survives a
retry, so three attempts cost one budget. An unpriced model cannot be called at
all — the worker refuses to start rather than issuing unbounded calls. Retry
backoff carries deterministic per-job jitter, so a hundred analyses that failed
in the same second do not retry in the same second.

**Detection.** `SkattjaktModelSpendRate` alerts above 100 SEK/hour;
`SkattjaktBudgetsExceeded` alerts when several analyses hit the ceiling.

**Residual risk.** No global monthly cap. The per-analysis ceiling times the
rate limit bounds it (20 analyses/hour/tenant × 25 SEK), but a large number of
tenants is not centrally capped.

---

## T8 — SQL injection

**Likelihood:** low. Every query is parameterised.
**Impact:** critical if it succeeded.

**Mitigation.** sqlx parameterisation throughout. The one place a column name is
interpolated takes it from a closed Rust enum and says so at the call site. The
application role cannot create or drop tables.

**Detection.** `tests/security/security-suite.sh` §4 fires payloads at paths,
query parameters and the idempotency-key header, then asserts all 22 tables
still exist.

---

## T9 — SSRF

**Likelihood:** low. There is no fetch-by-URL surface.
**Impact:** high if one were added — the cluster's internal services, and the
metadata endpoint.

**Mitigation.** No endpoint accepts a URL. At the network layer, the API has no
internet egress at all; the worker's egress excludes RFC 1918, `169.254.0.0/16`
and loopback, so even a code-execution bug in the worker cannot reach the
cluster's own services or a metadata endpoint.

**Detection.** `tests/security/security-suite.sh` §5 checks that no such surface
has appeared. `tests/infrastructure/validate-manifests.sh` asserts the exclusion
list is present in every environment.

---

## T10 — Path traversal on upload

**Likelihood:** low.
**Impact:** high — an arbitrary file write inside a pod.

**Mitigation.** Blob keys are derived from identifiers, never from a filename.
`FilesystemBlobStore::resolve` rejects `..`, absolute paths, backslashes and
null bytes. The root filesystem is read-only and the blob mount is the only
writable path.

**Detection.** `tests/security/security-suite.sh` §3 uploads four traversal
filenames and asserts nothing was written outside the blob root.

---

## T11 — Stolen API token

**Likelihood:** medium — customers mishandle credentials.
**Impact:** critical for that one tenant.

**Mitigation.** 256 bits of OS entropy, SHA-256 at rest, constant-time
comparison, shown once. A token grants exactly one tenant and nothing else. The
admin token can create companies and read no company's data.

**Detection.** Weak, and stated as such: a stolen token is indistinguishable
from the customer using it. Rate limiting bounds the damage rate; the audit
trail records what was accessed.

**Response.** `SKATTJAKT_RUNBOOK.md` § "a token may be compromised": revoke,
issue a replacement, and produce the audit trail for the affected period.

**Residual risk.** No per-token IP pinning, no anomaly detection, no expiry.
Rotation is manual. This is the weakest control in the model and is named as
such.

---

## T12 — Compromised dependency

**Likelihood:** low per release, non-trivial cumulatively.
**Impact:** critical — code execution inside a pod.

**Mitigation.** `--locked` builds. `cargo audit` and `cargo deny` in CI. An
SBOM with per-crate checksums. Distroless image with no shell to pivot into, no
setuid binary, a read-only root filesystem, all capabilities dropped, and no
service account token to steal. Egress restrictions mean a compromised API pod
cannot reach the internet at all, and a compromised worker cannot reach the
cluster's own services.

**Detection.** Trivy on the image in CI. `cargo audit` on every commit.

**Residual risk.** A dependency compromised between an audit run and a deploy.

---

## T13 — Data loss

**Likelihood:** low.
**Impact:** critical.

**Mitigation.** Daily `pg_dump` in custom format, verified listable before
upload, encrypted with `age`, stored off-cluster, size-checked on read-back. And
— the part that makes it a backup — a **weekly restore test** that restores into
a throwaway database and checks that the tables came back, that the row counts
are plausible against production, and that forced row-level security survived
the round trip. A restore that produces a structurally correct but empty
database fails that third check, which is the case a naive test misses.

**Detection.** `SkattjaktBackupMissing` at 48 hours without a backup;
`SkattjaktBackupFailed` and `SkattjaktRestoreTestFailed` on any failure;
`SkattjaktRestoreTestStale` at two weeks.

**Residual risk.** Postgres runs as a single replica. RPO is 24 hours and RTO is
in `SKATTJAKT_RUNBOOK.md`. Streaming replication is the next step and does not
exist yet.

---

## T14 — An operator mistake

**Likelihood:** high over time.
**Impact:** ranges to critical — a wrong `kubectl delete` against a
StatefulSet's PVC.

**Mitigation.** GitOps: the cluster is a function of the repository, so what is
running is answerable by reading a commit. Production tracks a tag rather than a
branch, so a merge to `main` is not a deploy. Production has `prune` and
`selfHeal` switched off, because an automated prune is one bad merge away from
deleting the volume holding every customer's documents. Namespaces separate the
environments; ResourceQuota bounds each one.

**Detection.** Argo CD reports drift within a minute even where it does not
correct it.

**Residual risk.** The trade is explicit: production tolerates drift in exchange
for not automatically deleting things. Drift is visible and is reconciled by a
human.

---

## T15 — Denial of service

**Likelihood:** medium.
**Impact:** medium.

**Mitigation.** Rate limiting at two layers — per client address at the ingress,
per tenant in the database. Body size bounded at 32 MB in both places. The
analysis quota (20/hour/tenant) bounds the expensive path. ResourceQuota and
LimitRange bound what any workload can consume. HPAs scale the API on CPU and
the worker on queue depth.

**Residual risk.** No distributed-DoS protection beyond the ingress. A large
document that is slow to parse is bounded by size but not by parse time.

---

## Summary

| ID | Threat | Likelihood | Impact | Strongest control |
|---|---|---|---|---|
| T1 | Cross-tenant access | medium | critical | Forced RLS + non-owner role |
| T2 | Fabricated finding | high | critical | Evidence gate + review gate |
| T3 | Prompt injection | high | high | Schema-only output, no capabilities |
| T4 | Compromised provider | low | high | Same as T3 |
| T5 | Data in logs/metrics | medium | high | Classification at the emitter |
| T6 | Lost analyses | high | medium | Durable job with lease |
| T7 | Runaway spend | medium | medium | Pre-flight budget check |
| T8 | SQL injection | low | critical | Parameterisation |
| T9 | SSRF | low | high | No URL surface + egress policy |
| T10 | Path traversal | low | high | Derived keys |
| T11 | Stolen token | medium | critical | Hashed at rest; **detection is weak** |
| T12 | Compromised dependency | low | critical | Distroless + egress policy |
| T13 | Data loss | low | critical | Tested restore |
| T14 | Operator mistake | high | high | GitOps, no auto-prune in prod |
| T15 | Denial of service | medium | medium | Two-layer rate limiting |

The two entries worth re-reading are **T11**, whose detection is genuinely weak,
and **T2**, whose residual risk — a rule that is simply wrong — is the reason
the review gate exists and must stay on until a professional has read the rule
set.

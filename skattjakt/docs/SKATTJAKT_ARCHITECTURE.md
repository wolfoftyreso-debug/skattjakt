# Skattjakt — Architecture

Skattjakt reads a Swedish limited company's year-end accounts and returns a
structured, evidence-backed list of things worth investigating: tax positions,
deductions, accruals, misclassifications and risks.

It never says "you are entitled to 186 000 kr". It says "we found a potential
position worth roughly 120 000–186 000 kr to investigate, here is what it rests
on, and here is what would confirm it". Every architectural decision below
follows from that sentence.

---

## 1. The two shapes

### The request path

```
                    ┌─────────────────────────────────────┐
   browser ───TLS──►│ ingress-nginx (the only way in)     │
                    └──────────────────┬──────────────────┘
                                       │
                    ┌──────────────────▼──────────────────┐
                    │ skattjakt-api  (2–6 replicas)       │
                    │   authenticate → authorise →        │
                    │   rate limit → validate → record →  │
                    │   enqueue → answer                  │
                    └────────┬─────────────────┬──────────┘
                             │                 │
                   ┌─────────▼──────┐   ┌──────▼─────────┐
                   │ PostgreSQL     │   │ MinIO          │
                   │ (RLS per row)  │   │ (document blobs)│
                   └────────────────┘   └────────────────┘
```

The API answers in milliseconds. It does no analysis. It writes a job row and
returns `202 Accepted` with a URL to poll.

### The analysis path

```
        ┌────────────────────────────────────────────────────┐
        │ skattjakt-analysis-worker  (2–8 replicas)          │
        │                                                    │
        │  claim (FOR UPDATE SKIP LOCKED) ──► lease          │
        │       │                                            │
        │       ├── heartbeat every lease/3                  │
        │       │                                            │
        │       ▼                                            │
        │  read pinned document versions ──► verify sha-256  │
        │       │                                            │
        │       ▼                                            │
        │  extract ──► canonical facts                       │
        │       │                                            │
        │       ├──────────────► model gateway ──► provider  │
        │       │                (priced, budgeted, wrapped) │
        │       ▼                                            │
        │  rule engine (versioned, cited, three-valued)      │
        │       │                                            │
        │       ▼                                            │
        │  falsification pass ──► evidence gate ──► ranking  │
        │       │                                            │
        │       ▼                                            │
        │  write result + model runs (one transaction)       │
        └────────────────────────────────────────────────────┘
```

The two paths share a database and nothing else. That separation is the single
most consequential structural decision in the system, and section 4 explains
why it was not optional.

---

## 2. The analysis pipeline

The detail is in `SKATTJAKT_ANALYSIS_PIPELINE.md`. In outline:

```
document bytes
     │
     ▼
 extraction ──────────────► pages of text  ─┐
     │                                      │
     ▼                                      │
 Swedish statement parser                   │  (bounded excerpt,
     │                                      │   wrapped as data)
     ▼                                      ▼
 canonical financial facts ──────────► model: discovery pass
     │                                      │
     ▼                                      ▼
 rule engine (versioned, cited)         candidates
     │                                      │
     ▼                                      │
 deterministic calculation                  │
     │                                      │
     └──────────────┬───────────────────────┘
                    ▼
        model: falsification pass  (may only demote)
                    │
                    ▼
        evidence validation ── gate: document value + rule
                    │
                    ▼
     confidence (6 measured factors, fail-closed)
                    │
                    ▼
              priority, ranking
                    │
                    ▼
              AnalysisResult
```

Two properties are worth naming here because they constrain everything else.

**Money is never a point estimate.** `Money` is an integer count of öre and
`MoneyRange` is a pair of them. There is no type in the domain model that can
express a single figure for a tax position, so no amount of downstream
carelessness can produce one.

**The model cannot promote a finding.** The discovery pass proposes; the rule
engine and the evidence gate dispose. The falsification pass can only demote.
A finding with no document value and no matching rule cannot be presented as
actionable regardless of how confident any model was.

---

## 3. The crates

A modular monolith, split into libraries by what they know rather than by what
they do (section 61). Two binaries, not twenty-five services.

| Crate | Holds | Deliberately does not |
|---|---|---|
| `core` | Money, facts, evidence, confidence, priority, classification, the analysis state machine, the evidence graph | Any I/O at all |
| `telemetry` | Metrics registry, log records, correlation ids, W3C trace context | Know what a company is |
| `rules` | The versioned rule set, three-valued evaluation, citations | Call a model |
| `model` | The provider abstraction and the Anthropic client | Decide what to send or what it costs |
| `gateway` | Pricing, budgets, fallback policy, injection defence | Know about analyses |
| `extract` | PDF and text extraction, the Swedish statement parser | Interpret what it read |
| `pipeline` | Orchestration of the two passes, report assembly | Persist anything |
| `store` | PostgreSQL, blobs, tenancy, retention, rate limits | Contain business rules |
| `jobs` | Queue policy and SQL: leases, retries, backoff, dead letters | Know what a job does |

`core` has no I/O on purpose. The rules about *meaning* — what may be called
actionable, what counts as evidence, how confidence is arrived at — are testable
in isolation and cannot be bypassed by a caller in a hurry.

---

## 4. Why the worker is a separate process

The analysis used to run in a `tokio::spawn` inside the HTTP handler. It was
moved, and the reason is not tidiness.

A background task inside the API process dies with the pod. A rolling deploy, a
node drain, an OOM kill — each silently loses every analysis in flight, for
customers who are watching a progress bar, and leaves no record that anything
was lost. There is no alert that can fire for it, because from the API's point
of view nothing failed.

With the work in a job row, the row *is* the record. A worker claims it, holds a
lease, and extends that lease while it works. If the pod dies, the lease expires
and the next worker picks the job up on its next attempt. An evicted pod becomes
a delay instead of a loss.

Three further consequences follow, and each of them independently justifies the
split:

- **Different scaling signals.** An analysis is minutes of model latency at
  almost no CPU. The API is milliseconds of CPU per request. One autoscaler
  cannot serve both: scaling the API on queue depth is nonsense, and scaling
  workers on CPU would sit at one replica while a hundred analyses queued.
- **Different failure blast radius.** A memory spike during extraction of a
  large PDF should not take down request serving.
- **Different rollout tempo.** The API rolls in seconds. The worker has a
  ten-minute termination grace period so an analysis in flight can finish.

---

## 5. Why PostgreSQL is the queue

Not Redis, not Kafka, not NATS. Three reasons, and none is that Postgres is
fashionable for this:

1. **Atomicity across the boundary that matters.** Moving an analysis to
   `succeeded` and writing its result is one transaction. With a separate
   broker they are two, and the window between them is where duplicated
   analyses and orphaned results live.
2. **`SELECT ... FOR UPDATE SKIP LOCKED` is a correct competing-consumer
   queue**, and has been for a decade. `tests/failure/job-failures.sh` proves
   it: eight concurrent workers claim one job exactly once.
3. **One system to back up, restore, monitor and secure.** Section 83 asks for
   exactly this judgement, and the volume this product will see for years —
   thousands of jobs a day — is not a broker's problem.

The cost is real and stated: the queue lives on the same database as the data,
so a database outage stops both. That is accepted because a database outage
stops the product anyway.

---

## 6. Tenant isolation

Enforced by PostgreSQL row-level security, not by application code.

Every tenant table has `ENABLE ROW LEVEL SECURITY` **and** `FORCE ROW LEVEL
SECURITY`, with a policy of `company_id = current_company_id()`. The application
connects as `skattjakt_app`, a non-superuser, non-owner role that cannot bypass
a policy even if it wanted to. The tenant is applied per transaction with
`set_config('skattjakt.company_id', $1, true)` — the parameterised equivalent of
`SET LOCAL`, so the value is never interpolated into SQL.

The consequence is the property worth having: **a query that forgets its tenant
returns nothing, rather than returning everything.** A missing `WHERE` clause is
a bug that produces an empty result, not a data leak.

`Tenant<'_>` is the only way to reach a tenant table from Rust, and it is a
transaction with the tenant already applied. `tests/security/tenant-isolation.sh`
proves the property against a real cluster rather than asserting it.

Three tables sit outside RLS, each deliberately and each documented at its
definition:

- `api_tokens`, because authentication happens before a tenant is known.
- `jobs`, because a queue is scanned across tenants by definition. Applying RLS
  here would require the worker to hold a `BYPASSRLS` role, which is strictly
  worse — that role would also bypass isolation on the tables holding the
  customer's economy. The table is kept safe by what it may contain:
  identifiers, state, timing, a correlation id. No amounts, no document text,
  no names, and no payload column at all.
- `job_transitions` and `dead_letters`, for the same reason as `jobs`.

---

## 7. Data classification

Four levels, defined in `core::classification` and enforced at the emitter
rather than by review:

| Level | Examples | May reach |
|---|---|---|
| `PUBLIC` | Rule ids, versions, stage names, status classes | Anywhere, including metric labels |
| `INTERNAL` | Durations, counts, error kinds, trace ids | Logs, traces |
| `CONFIDENTIAL` | Company id, document id, org number, filename | Authorised error bodies, the database |
| `HIGHLY_CONFIDENTIAL` | Amounts, extracted facts, document text, prompts | The model request boundary, and nowhere else |

The enforcement is structural. `LabelSet::insert` takes a `Classification` and
refuses anything above `PUBLIC`; it also rejects a list of inherently unbounded
label names outright. `LogRecord` fields carry their classification and are
replaced with `[redacted]` on the way out if they exceed the log ceiling. There
is no escape hatch, which is the point: the failure mode being defended against
is a `tracing::info!` written at 02:00 during an incident with the offending
value already in scope.

`SKATTJAKT_SECURITY.md` §4 has the full matrix.

---

## 8. The model boundary

Every model call in the product goes through `ModelGateway`. Nothing else holds
a `ModelProvider`. That single choke point makes all of the following true at
once rather than in most places:

- every call is priced and the budget is checked **before** the spend, because
  checking afterwards means the money is already gone;
- a call served by a different model than the one requested is recorded as a
  fallback with both names, rather than silently substituted;
- a refusal, a truncation and a schema violation are distinguishable in the
  metrics, because they mean different operational things;
- document content is wrapped as data and the fence is verified at the boundary;
- the model has no capability. It answers in a schema. There is no code path
  from a model response to code execution, SQL, a database write, a Kubernetes
  resource, a rule change or a permission change — section 52's list is enforced
  by the absence of the path, not by a check.

The model identity is configuration with **no compiled-in default**
(`SKATTJAKT_MODEL_ID`). A test asserts the source contains no `claude-` literal,
and `tests/supply-chain/inspect-image.sh` asserts the same of the shipped
binary.

---

## 9. Kubernetes

Three namespaces — `skattjakt-dev`, `-staging`, `-prod` — built from one
kustomize base with per-environment overlays. The overlays differ in capacity,
image digests and hostname. They deliberately do **not** differ in isolation:
the same NetworkPolicies, the same Pod Security level, the same probes. An
environment whose isolation differs from production cannot be used to test
whether production's isolation works.

The network posture is default-deny in **both** directions. Ingress-deny alone
is the common half-measure: it stops an attacker reaching a pod and does nothing
about a compromised pod reaching out, which is the half that matters for
exfiltration and for SSRF. The openings are explicit and few:

```
ingress-nginx ──► api :8080
monitoring    ──► api :8080, worker :9090, postgres :9187
api           ──► postgres :5432, minio :9000
worker        ──► postgres :5432, minio :9000, 0.0.0.0/0 :443
                  (except RFC1918, 169.254/16, 127/8)
postgres      ──► nothing
minio         ──► nothing
```

The API has **no** egress to the internet. It does not call the model — the
worker does — so an SSRF in an upload handler has nowhere to go. The worker's
internet rule excludes the link-local block that carries `169.254.169.254` and
every private range, so an SSRF there cannot reach the cluster's own services.
`tests/infrastructure/validate-manifests.sh` asserts each of these.

Deployment is GitOps (Argo CD). Production tracks a tag rather than a branch, so
a merge to `main` is not a production deploy, and has `prune` and `selfHeal`
switched off — an automated prune is one bad merge away from deleting the
PersistentVolumeClaim holding every customer's documents.

---

## 10. What is deliberately not here

Section 83 asks for this list.

- **No service mesh.** Nine NetworkPolicies and TLS at the ingress cover what a
  mesh would be bought for. A mesh adds a sidecar per pod, a control plane, a
  certificate rotation story and a new class of incident, for mutual TLS between
  four workloads in one namespace.
- **No Kafka, NATS or Redis.** The queue is Postgres; see §5. Redis would be a
  second stateful system to back up and secure, for a rate limiter and a cache
  neither of which is on the critical path.
- **No Neo4j.** The evidence graph is a derived structure over data already in
  Postgres, built in memory when a blast-radius question is asked. A graph
  database for a graph with hundreds of nodes per analysis is machinery for
  show.
- **No microservice split.** One API, one worker, one library workspace. The
  split that exists — API from worker — was forced by a real property (§4).
  Further splits would each add a network hop, a failure mode and a deployment
  unit in exchange for nothing yet.
- **No managed cloud services.** Self-hosted Postgres and MinIO, per the build
  order.

## 11. Known structural limits

Stated rather than discovered:

- **Postgres runs as a single replica.** Recovery is restore-from-backup, with
  the RPO and RTO in `SKATTJAKT_RUNBOOK.md`. Streaming replication is the next
  step and is not pretended to exist.
- **Traces are propagated but not exported.** W3C trace context is parsed,
  minted and carried across the queue, and span ids reach the log stream. There
  is no OTLP exporter and no collector configured. `SKATTJAKT_DEPLOYMENT.md`
  records this as an open gap.
- **The rule set has not been professionally reviewed.** Every rule carries
  `review: awaiting_professional_review`, and the pipeline refuses to present
  any finding as established while that holds. See `SKATTJAKT_RULE_ENGINE.md`.
- **Blob storage is a filesystem implementation of an object-store trait.** The
  MinIO manifests exist; the S3 client does not. See the gap list in
  `SKATTJAKT_DEPLOYMENT.md`.

---

## 12. Where to read next

| Question | Document |
|---|---|
| What is the threat model, and what protects against each threat? | `SKATTJAKT_THREAT_MODEL.md` |
| How is the system secured, concretely? | `SKATTJAKT_SECURITY.md` |
| What are the tables, and why are they shaped that way? | `SKATTJAKT_DATA_MODEL.md` |
| What happens between an upload and a report? | `SKATTJAKT_ANALYSIS_PIPELINE.md` |
| How does a rule work, and how is one changed? | `SKATTJAKT_RULE_ENGINE.md` |
| How is this deployed and promoted? | `SKATTJAKT_DEPLOYMENT.md` |
| It is 03:00 and something is wrong. | `SKATTJAKT_RUNBOOK.md` |
| Why was X decided that way? | `SKATTJAKT_ENGINEERING_DECISIONS.md` |
| What does the product promise the customer? | `SKATTJAKT_PRODUCT_SPEC.md` |

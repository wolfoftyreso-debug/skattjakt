# Skattjakt — Product Surface Matrix

The matrix required by §29 of the build constitution, and the platform findings
required by §4.

Read the platform findings first. They change what several rows of the matrix
can honestly say.

---

## 1. Platform findings (§4)

The constitution says to inspect the existing self-hosted AWS/Kubernetes
platform and reuse it rather than building duplicates. That inspection was
attempted. This is what it found.

| Looked for | Result |
|---|---|
| `kubectl`, a kubeconfig, in-cluster service account | **Absent.** No `/var/run/secrets/kubernetes.io`, no config |
| A reachable cluster | **None.** A local `kind` cluster was started to substitute; `kubeadm init` fails because `runc` cannot start a nested container in this sandbox (`can't get final child's PID from pipe: EOF`) |
| Service discovery for platform services | **Nothing resolves.** Keycloak, Dex, Vault, Argo CD, Prometheus, Grafana, Loki, Tempo, MinIO, Kafka, Redis — none |
| EC2/EKS instance metadata | **403** from the sandbox proxy |
| AWS credentials | Present in the environment, but their config is an `s3`-only stanza with no region and no account context. They belong to this sandbox, not to a Skattjakt platform |

**The finding, stated plainly: no AMOS platform is reachable from this
environment.** The AWS credentials that exist are not identifiable as the
platform's, and using them to enumerate infrastructure would be acting on a
guess about whose account they open. They were not used beyond reading their
own configuration.

### What follows from that

Everything the constitution says to *reuse* had to be **written to be
replaceable** instead. That is a materially different thing from building a
duplicate platform, and the difference is where the seams are:

| Platform capability | What was done instead | How it is replaced |
|---|---|---|
| Identity provider (§12, §13) | A password verifier, plus `CredentialMethod::Federated` as a first-class case that stores no secret | Add a verifier. Sessions, devices, roles, the API contract and every client are untouched |
| ID verification (§13) | `VerificationLevel` is a modelled axis with no provider behind it | Wire BankID to set the level; decide which operations demand `Strong` |
| Secrets (§14) | No secret in git, not even a placeholder. `Secret` objects are referenced and never rendered | Point External Secrets or sealed-secrets at the same names |
| Observability (§17) | Prometheus text on `/metrics`; W3C trace context minted, propagated **and exported over OTLP** | Scrape the endpoint; point `OTEL_EXPORTER_OTLP_ENDPOINT` at the platform collector |
| Object storage (§21) | `BlobStore` trait with **both** a filesystem and an S3 implementation, selected by configuration | Point `SKATTJAKT_S3_*` at the platform's endpoint |
| Push delivery (§22) | Outbox, device tokens, rendering, retry schedule and dispatch — all done. `PushSender` answers `NotConfigured` rather than pretending | Implement one transport; nothing else changes |
| Model serving (§15) | `ModelProvider` trait behind `ModelGateway` | Point at the platform's inference endpoint |

Nothing in that table is a stub that pretends to work (§31). Each is either
implemented, or is an explicit seam with nothing behind it and a document
saying so.

---

## 2. The matrix (§29)

**Architecture ready** means the data model, contracts and server-side
behaviour a component needs are decided and in place, so building it later is
building it — not rewriting the core (§32, §36).

| Component | Required now | Architecture ready | Implemented | Tested | Production ready |
|---|---|---|---|---|---|
| **Web** | ✓ | ✓ | ✓ beta interface, cookie sessions | ✓ e2e + 53 browser checks + 16 accessibility checks | ◐ see §4 below |
| **Apple / iOS** | — | ✓ | — deliberately not | — | — |
| **Android** | — | ✓ | — deliberately not | — | — |
| **API** | ✓ | ✓ | ✓ 38 paths | ✓ contract + live suites | ✓ |
| **Backend** | ✓ | ✓ | ✓ API + worker | ✓ 451 unit, 20-step e2e on both backends | ✓ |
| **Database** | ✓ | ✓ | ✓ 37 tables, RLS | ✓ isolation 10/10 | ✓ |
| **Memory / state** | ✓ | ✓ | ✓ four layers, §11 doc | ✓ | ✓ |
| **Authentication** | ✓ | ✓ | ✓ sessions, rotation, devices | ✓ 44 live checks | ◐ local verifier, not the platform IdP |
| **Identity** | ✓ | ✓ | ✓ users, membership | ✓ | ✓ |
| **Authorization** | ✓ | ✓ | ✓ 3 roles × 12 permissions | ✓ | ✓ |
| **ID verification** | — | ✓ | — no provider reachable | — | — |
| **File storage** | ✓ | ✓ | ✓ S3 + filesystem, presigned URLs | ✓ 7 live ops + full e2e on MinIO | ✓ |
| **Notifications** | — | ✓ | ✓ outbox, email, in-app | ✓ 15 checks incl. a real SMTP server | ◐ push has no provider |
| **Background jobs** | ✓ | ✓ | ✓ leases, retries, DLQ | ✓ failure 24/24 | ✓ |
| **Simulation / probability** | ✓ | ✓ | ✓ 11 distributions, expression model, 12 endpoints | ✓ 109 unit + 69 live checks | ✓ |
| **Observability** | ✓ | ✓ | ✓ metrics, logs, correlation, OTLP export | ✓ 12 checks against a real collector | ✓ |
| **Security** | ✓ | ✓ | ✓ + CSP and security headers | ✓ security 56/56, 41 connectivity checks | ✓ |
| **CI/CD** | ✓ | ✓ | ✓ 8 gates | ✓ | ✓ |
| **Kubernetes** | ✓ | ✓ | ✓ 37 objects × 3 envs | ✓ 111/111 accepted by a real API server; 3 defects found and fixed | ◐ **applied, no pod started** — see §5.1 |
| **Backup / recovery** | ✓ | ✓ | ✓ daily + weekly restore test | ✓ 20 checks: dump, encrypt, upload, restore, verify | ◐ run against real storage, not in a cluster |
| **Documentation** | ✓ | ✓ | ✓ 14 documents | ✓ CI checks the couplings | ✓ |

`◐` = partially. `✗` = not, and the reason is stated.

---

## 3. Why iOS and Android are empty rows

Because §31 says not to build a fake app to fill a cell, and building a real one
was not asked for.

What §32 *does* require — everything that can be built now so the client can be
built later without a backend rewrite — is done:

| A mobile client needs | State |
|---|---|
| A credential it can hold in the Keychain / Keystore | ✓ refresh tokens, 30-day lifetime for mobile vs 12 hours for web |
| To survive a lost response mid-refresh | ✓ grace window; a retry does not read as theft |
| Theft detection | ✓ rotation with reuse detection; the family is torn down |
| A device identity that outlives a sign-out | ✓ `devices`, keyed on a client-generated install id |
| Somewhere to put a push token | ✓ per device, with provider, and a dead-token marker |
| To be told a result is ready without polling | ✓ outbox, delivered by a worker; email works, push needs a transport |
| To upload a large file over a poor network | ✓ upload tickets; the API never handles the bytes |
| Multi-tenant switching without re-authenticating | ✓ `POST /v1/auth/switch-company`, verified against membership |
| Per-client session policy | ✓ `x-skattjakt-client` selects it |
| A contract to generate a client from | ✓ OpenAPI 3.1, 23 paths, 31 schemas |
| Error semantics it can act on | ✓ documented status codes; 404 for another tenant, 403 for a role |

`SKATTJAKT_CLIENT_ARCHITECTURE.md` is the document a mobile team would start
from.

---

## 4. Where the web client is `◐`

The beta interface is real, driven end to end in a browser, and served by the
build. It is not a finished production web application:

- No offline or degraded state beyond an error message.
- No client-side telemetry.

**Accessibility has now been audited** and is no longer on that list. axe-core
runs against both pages in every state a person sees — signed out, with a model
loaded, with a result on screen, and inside each of the five tab panels — in
light and dark, at 200% zoom, plus keyboard reachability and focus visibility
that automated rules cannot judge. 16 checks, all passing.

It found two real defects, both fixed: a table column heading that was empty,
so a screen reader announced the column and then nothing; and no focus
indicator on buttons, because the stylesheet styled focus for inputs and
selects and left buttons to a browser default that Chromium paints only for
`:focus-visible`.

What that audit **does not** claim is WCAG conformance. Automated tooling finds
a minority of the failures in the standard: it cannot judge whether a label is
meaningful, whether a reading order makes sense, or whether an error message is
comprehensible. It is a floor, and a genuine one — the machine-checkable rules
pass on every screen — but a conformance claim needs a person.

---

## 5. Honest totals

### 5.1 What happened when the manifests met a real cluster

They had never been applied. A k3s v1.31.2 cluster was brought up in this
environment and all three overlays were put through it. What that changed:

**Accepted.** 111 objects — 37 per environment — through server-side apply on a
real API server: schema, defaulting, admission and cross-references. The dev
overlay was then applied for real, and every object was created.

**Rejected, correctly, and this is the part a schema validator cannot do.**
A `ResourceQuota` is evaluated against the *sum* of what a namespace declares,
and no single document is invalid. Three defects, all in that shape:

1. **Dev had no object storage.** The overlay shrank the replica counts and the
   quota and left every pod and volume at its production size: 70Gi of storage
   declared against a 40Gi quota. MinIO's PVC was refused, so its StatefulSet
   never created a pod. Nothing was in a failing state — no crash, no failing
   rollout, one event on a controller nobody watches.
2. **Dev ran two of everything.** The overlay said `replicas: 1`; the base ships
   an HPA with `minReplicas: 2`, and the HPA owns that field within seconds of
   an apply. Applied as 1, read back as 2.
3. **Dev and staging were configured to autoscale into their own quota.** At
   their HPA ceilings they need 12.9Gi and 25.4Gi of memory limits against
   quotas of 8Gi and 16Gi. Under load the autoscaler would have asked for pods
   the quota refused — the service stops scaling exactly when it needs to, and
   the nightly backup is what gets refused first.

All three are fixed, and `validate-manifests.sh` now models the quota the way
the API server does: at rest, at the autoscaler's ceiling, and with a CronJob
running on top of the ceiling. Removing the fixes makes it fail.

**Verified live.** Pod Security `restricted` is genuinely enforced by the
namespace label, not merely asserted in a test: a pod requesting
`hostNetwork`, `hostPID`, `privileged`, `runAsUser: 0` and a `hostPath` mount
of `/` was rejected by the API server with seven separate violations.

**Not verified: no container ever started.** The sandbox this was built in
masks `CAP_SYS_RESOURCE` (`CapEff: 000001fffeffffff`). The kubelet sets
`oom_score_adj: -998` on every pod sandbox, lowering it below the parent's
value requires that capability, and the kernel refuses — even as root, and on
both container runtimes tried. Every pod reaches `Scheduled` and stops at
`FailedCreatePodSandBox`. This is a property of the build environment, not of
the manifests: it fails identically for coredns. See §6 for what it leaves
unproven.

### 5.2 Backup and restore, run for real

The matrix carried backup as `✗ never run in a cluster — scripts reviewed`.
Reviewed is not tested, so the scripts were run *as the CronJobs run them* —
the same files, extracted from the same ConfigMap, with the same environment —
against a real PostgreSQL, a real MinIO and real `age` encryption. 20 checks:
the dump uploads encrypted, the stored bytes do not contain the customer's
name, an unencrypted upload is refused, the backup comes back, decrypts and
restores, every table and every row-level security policy survives, individual
values survive, a corrupted dump fails the restore rather than half-applying,
and the wrong key does not decrypt.

**It found the defect that mattered.** The restore test compared the restored
database against a **hand-written list of table names**. By the time anyone ran
it, 18 of the 37 tables were absent from that list — every identity table
(`users`, `sessions`, `user_credentials`) and every simulation table. A backup
that had lost all of them would have passed its own verification.

The list did not fail. It stopped covering things, which is the failure mode of
every frozen list, and it is silent: a missing entry looks exactly like a
passing one. It now asks the live database what tables it has and compares set
against set, which cannot go stale because a migration adds a table to both
sides at once.

### 5.3 What the NetworkPolicies actually permit

Enforcement needs a cluster where a pod can start. The *logic* does not, and
the two are different claims: a policy that is enforced perfectly and says the
wrong thing is not protection.

An evaluator implements the NetworkPolicy rules — a pod no policy selects is
unrestricted; policies are a union and can only ever allow more; a connection
must be permitted at **both** ends — and asserts the intended connectivity
matrix for all three environments. 41 checks each.

**It found a defect that every structural check had passed.** The notification
worker's own egress policy allowed port 5432 to Postgres. `postgres-ingress`
never listed the notification worker. A connection needs both ends, so in
production the outbox worker **could not reach the database at all**: it would
have claimed nothing, delivered nothing, and said nothing louder than a
connection refusal in a log.

That is the same omission, one layer down, as the one that gave this worker no
Deployment at all — the workload was added, the route to the database was not.
Checking one side of a policy is how a policy set looks complete and is not.

### 5.4 Totals

Verified in this environment, in this session:

```
483 unit and integration tests          golden dataset  precision 1.000 recall 1.000
 61 session checks (live API)            10 tenant isolation checks (real Postgres)
 39 security checks (live API)           24 failure-injection checks (real Postgres)
 20 end-to-end product steps              5 S3 checks against a real MinIO
 20 end-to-end steps again, on S3        15 notification checks (real SMTP)
 12 trace checks (real OTel collector)  111 Kubernetes objects applied to a live
 15 migration checks: fresh install,       API server; 1 Pod Security rejection
    and upgrade-with-data from every       verified with 7 violations
    earlier version
  9 container image assertions          305 SBOM components, all checksummed
 18 documentation coupling checks
```

Not verified, and not claimed:

- **Anything that requires a running container.** NetworkPolicy *enforcement*,
  HPA behaviour under real load, probe outcomes, Argo CD reconciliation,
  ingress and TLS termination. The manifests are applied and admitted; no pod
  has run. §5.1 says why, and it is the environment rather than the manifests.

  NetworkPolicy *semantics* are no longer on this list. What the policies
  permit is now evaluated directly — see §5.3 — which is a different claim from
  enforcement and worth separating: the policies were wrong, and no amount of
  enforcement would have made a wrong policy right.
- The backup and restore CronJobs running **on a schedule, in a cluster**. The
  scripts themselves are no longer untested: §5.2.
- Trivy, cosign signing, SLSA provenance against a registry.
- Any mobile client, because none was built.

---

## 6. What to do next, in the constitution's order

**Phase 1–2 are complete.** Foundation and core product are built and tested.

**Phase 3 — platform completeness.** In the order that removes the most risk:

1. ~~The S3 client behind `BlobStore`.~~ **Done.** Hand-written SigV4,
   verified against a real MinIO: seven live operations, presigned PUT and GET
   used by `curl` with no credential, and the whole 20-step product test on S3
   instead of the filesystem.
2. ~~The notification delivery worker.~~ **Done.** A third process, because a
   notification behind a four-minute analysis defeats the point of sending it.
   Email over hand-written SMTP, verified against a real Mailpit; the delivered
   message is read back and checked for the figures it must not carry.
3. ~~The OTLP exporter.~~ **Done.** Spans leave both processes and a trace
   started by an HTTP request continues into the worker across the queue —
   asserted against a real collector, not inferred.
4. ~~Move the web interface onto sessions.~~ **Done.** Email and password,
   `HttpOnly` `SameSite=Strict` cookies, and a CSRF defence that requires a
   custom header the browser will not send cross-origin. The company token
   remains for integrations and for bootstrapping the first user.
5. ~~Wire the upload-ticket routes into the API.~~ **Done.** Four endpoints:
   issue a ticket, upload through the API when storage cannot presign, redeem
   the ticket against what actually arrived, and list notifications. A ticket
   for a small file cannot be redeemed for a large one, and the storage key is
   derived from identifiers rather than from the filename.
6. ~~Apply the manifests to a real cluster.~~ **Done, as far as this
   environment allows.** All three overlays are accepted by a live API server
   and the dev overlay is applied; three quota defects were found and fixed
   (§5.1). No container has started, and the reason is the sandbox rather than
   the manifests. **Now the top item** for anyone with an ordinary cluster:
   run the dev overlay somewhere a pod can start, which is the only way to
   verify NetworkPolicy enforcement.
7. **The push sender.** The outbox drains over email; push answers
   `NotConfigured`, which is honest and is not a delivery channel. It is the
   last backend gap before a mobile client can ship.

**Phase 4 — mobile.** Only after (7), because a phone without push is a worse
product than the web client.

**Phase 5 — hardening.** Everything in §5's "not verified" list, all of which
needs a cluster where containers can run.

**And before any of it:** a Swedish tax professional reads the 14 rules. Until
that happens the product cannot present a finding as established, and that is
the ceiling on what it is worth to anyone.

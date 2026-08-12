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
| Observability (§17) | Prometheus text on `/metrics`; W3C trace context minted and propagated | Scrape the endpoint; add an OTLP exporter |
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
| **Web** | ✓ | ✓ | ✓ beta interface | ✓ e2e through a browser | ◐ see §4 below |
| **Apple / iOS** | — | ✓ | — deliberately not | — | — |
| **Android** | — | ✓ | — deliberately not | — | — |
| **API** | ✓ | ✓ | ✓ 23 paths | ✓ contract + live suites | ✓ |
| **Backend** | ✓ | ✓ | ✓ API + worker | ✓ 451 unit, 20-step e2e on both backends | ✓ |
| **Database** | ✓ | ✓ | ✓ 28 tables, RLS | ✓ isolation 10/10 | ✓ |
| **Memory / state** | ✓ | ✓ | ✓ four layers, §11 doc | ✓ | ✓ |
| **Authentication** | ✓ | ✓ | ✓ sessions, rotation, devices | ✓ 44 live checks | ◐ local verifier, not the platform IdP |
| **Identity** | ✓ | ✓ | ✓ users, membership | ✓ | ✓ |
| **Authorization** | ✓ | ✓ | ✓ 3 roles × 12 permissions | ✓ | ✓ |
| **ID verification** | — | ✓ | — no provider reachable | — | — |
| **File storage** | ✓ | ✓ | ✓ S3 + filesystem, presigned URLs | ✓ 7 live ops + full e2e on MinIO | ✓ |
| **Notifications** | — | ✓ | ✓ outbox, email, in-app | ✓ 15 checks incl. a real SMTP server | ◐ push has no provider |
| **Background jobs** | ✓ | ✓ | ✓ leases, retries, DLQ | ✓ failure 24/24 | ✓ |
| **Observability** | ✓ | ✓ | ✓ metrics, logs, correlation | ✓ | ◐ no trace export |
| **Security** | ✓ | ✓ | ✓ | ✓ security 39/39 | ✓ |
| **CI/CD** | ✓ | ✓ | ✓ 8 gates | ✓ | ✓ |
| **Kubernetes** | ✓ | ✓ | ✓ 33 objects × 3 envs | ✓ 99/99 schema-valid | ✗ **never applied** |
| **Backup / recovery** | ✓ | ✓ | ✓ daily + weekly restore test | ✓ scripts reviewed | ✗ never run in a cluster |
| **Documentation** | ✓ | ✓ | ✓ 11 documents | ✓ CI checks the couplings | ✓ |

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

The beta interface is real, driven end to end, and served by the build. It is
not a finished production web application:

- It authenticates with the company token, not a session. The session surface
  exists and is tested; the interface has not been moved onto it.
- No offline or degraded state beyond an error message.
- Accessibility has not been audited against WCAG.
- No client-side telemetry.

Stated rather than implied, because "implemented" and "production ready" are
different columns for a reason.

---

## 5. Honest totals

Verified in this environment, in this session:

```
465 unit and integration tests          golden dataset  precision 1.000 recall 1.000
 44 session checks (live API)            10 tenant isolation checks (real Postgres)
 39 security checks (live API)           24 failure-injection checks (real Postgres)
 20 end-to-end product steps              5 S3 checks against a real MinIO
 20 end-to-end steps again, on S3        15 notification checks (real SMTP)
 99 Kubernetes objects schema-valid
  9 container image assertions          305 SBOM components, all checksummed
 17 documentation coupling checks
```

Not verified, and not claimed:

- Applying the manifests to a live cluster, and therefore NetworkPolicy
  *enforcement*, HPA behaviour, Argo CD reconciliation, ingress and TLS.
- The upload-ticket HTTP endpoints. The store layer and presigning are tested;
  nothing serves them yet.
- The backup and restore CronJobs running for real.
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
3. The OTLP exporter. Context is propagated and goes nowhere. **Now the top
   item.**
4. Move the web interface onto sessions, and retire the company token for
   human access — keeping it for integrations.
5. Wire the upload-ticket routes into the API. The store layer and the
   presigning are done; no HTTP endpoint issues a ticket yet.

**Phase 4 — mobile.** Only after (1) and (2), because a phone without upload
tickets and without push is a worse product than the web client.

**Phase 5 — hardening.** Everything in §5's "not verified" list, which needs a
cluster.

**And before any of it:** a Swedish tax professional reads the 14 rules. Until
that happens the product cannot present a finding as established, and that is
the ceiling on what it is worth to anyone.

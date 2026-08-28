# Skattjakt

**Tax Recovery & Opportunity Engine for Swedish limited companies.**

Upload a preliminary or final set of accounts; get back a structured list of
things worth investigating — potential tax positions, deductions, accruals,
misclassifications and control points — each traceable to a line in a document
and a cited rule.

> Det finns ofta mer att hitta i ett bokslut än man tror.

---

## ⚠️ Read this before using it on real accounts

**The Swedish tax rules in this repository were drafted by a language model and
have not been reviewed by a qualified adviser.** They carry statutory citations
and they are plausible; that is not the same as verified.

The system knows this about itself. Every rule records its review state, and the
engine refuses to present a finding from an unreviewed rule as established — the
strongest status such a finding can reach is *Verifiera* (needs verification).
`GET /v1/rules` reports how many rules are unreviewed, and the golden dataset
fails the build if anything is ever presented as established while that count is
above zero.

Nothing here should be relied on for a filing or a decision without an
accountant checking it. That is also the product's own position: Skattjakt's
job is to help you ask better questions of the person qualified to answer them.

---

## Status

| Area | State |
|---|---|
| Domain model, rule engine, extraction, pipeline | Implemented, 732 tests |
| Golden dataset, 11 cases | Precision 1.000, recall 1.000, zero false positives |
| Tenant isolation (Postgres RLS) | 10 checks verified against a real cluster |
| Security suite (§50) | 56 checks verified against a live API |
| Identity: sessions, rotation, devices, roles | 61 checks verified against a live API |
| Failure injection (§77) | 24 checks verified against a real cluster |
| End-to-end product test | 20 steps, API and worker as separate processes |
| Durable job system, analysis state machine | Implemented and verified |
| Model gateway: cost, budgets, fallback, injection defence | Implemented and verified |
| Observability: `/metrics`, correlation ids, trace context | Implemented and verified |
| Object storage | S3 and filesystem behind one trait; verified against a real MinIO |
| Container images | Built and inspected — 13 MB distroless, 9 checks |
| SBOM | Generated — 305 components, all checksummed |
| Kubernetes manifests | 33 resources × 3 environments, schema-valid, properties asserted |
| Kubernetes cluster | **Never applied** — no cluster is reachable from this environment |
| OTLP trace export | Implemented — spans exported over OTLP/HTTP, verified against a real collector |
| OCR for scanned PDFs | **Not implemented** — unreadable pages are reported, not read |

A full account of what has and has not been verified is in
[`docs/SKATTJAKT_DEPLOYMENT.md`](docs/SKATTJAKT_DEPLOYMENT.md) §9.

---

## Quick start

```sh
cargo test --workspace                                  # 613 tests
cargo test -p skattjakt-pipeline --test golden -- --nocapture   # the golden dataset
cargo test -p skattjakt-simulate --release --test performance -- --ignored --nocapture

export PGBIN=$(ls -d /usr/lib/postgresql/*/bin | tail -1)
./tests/security/tenant-isolation.sh                    # RLS against a real cluster
./tests/security/security-suite.sh                      # the attacks of section 50
./tests/security/session-suite.sh                       # sessions, rotation, roles
./tests/failure/job-failures.sh                         # what a dead pod does
./tests/e2e/end-to-end.sh                               # the whole product

./tests/integration/s3-blobstore.sh                     # S3 against a real MinIO
./tests/integration/e2e-on-s3.sh                        # the whole product on S3
./tests/integration/simulations.sh                      # the Monte Carlo chain
./tests/infrastructure/backup-restore.sh                # dump, encrypt, restore, verify
./tests/infrastructure/validate-manifests.sh            # 3 environments + connectivity
./tests/infrastructure/validate-docs.sh                 # the docs still describe it

# In a browser. Needs playwright and axe-core; see the header of each script.
./tests/e2e/simulation-ui.sh                            # the interface, wired to the API
./tests/e2e/accessibility.sh                            # axe-core on every screen

SKATTJAKT_API_TOKEN=dev cargo run -p skattjakt-api
```

Then open <http://localhost:8080/> for the analysis flow, or
<http://localhost:8080/simulations> for the Monte Carlo layer.

Then:

```sh
curl -s localhost:8080/ready
curl -s localhost:8080/v1/openapi.yaml

curl -s -X POST localhost:8080/v1/analyses \
  -H 'authorization: Bearer dev' -H 'content-type: application/json' -d '{
  "company": {
    "name": "Demo AB",
    "org_number": "556016-0680",
    "fiscal_year": {"start": "2025-01-01", "end": "2025-12-31"},
    "employee_count": 8, "owner_count": 2, "in_group": false,
    "has_vehicles": false, "does_development_work": false,
    "owners_active_in_company": true
  },
  "documents": [{
    "filename": "bokslut.txt", "mime_type": "text/plain",
    "text": "Nettoomsättning    12 500 000\nPersonalkostnader   -5 800 000\nSkattemässigt resultat  3 000 000\nMateriella anläggningstillgångar 1 800 000\n"
  }]
}'
```

Without `ANTHROPIC_API_KEY` and `SKATTJAKT_MODEL_ID` the service runs
**rules-only** — the rule engine produces evidence-backed findings on its own,
and `/ready` reports the degraded state rather than pretending.

### With persistence

Set `DATABASE_URL` (migrations run as the owning role; the service connects as
`skattjakt_app`, which is subject to row-level security) and
`SKATTJAKT_ADMIN_TOKEN`. Then:

```sh
# The admin token can only create companies — it reaches no company's data.
curl -X POST localhost:8080/v1/companies -H "authorization: Bearer $ADMIN" \
  -H 'content-type: application/json' \
  -d '{"company":{"name":"Demo AB","org_number":"556016-0680",
       "fiscal_year":{"start":"2025-01-01","end":"2025-12-31"}}}'
# → returns a company token, once. Only its SHA-256 is stored.

curl -X POST localhost:8080/v1/documents      -H "authorization: Bearer $TOKEN" ...
curl -X POST localhost:8080/v1/analyses/stored -H "authorization: Bearer $TOKEN" \
  -d '{"document_version_ids":["..."]}'       # → 202, poll GET /v1/analyses/{id}
curl "localhost:8080/v1/analyses/$ID/report?format=markdown" -H "authorization: Bearer $TOKEN"
```

Configuration: copy `.env.example`. There is deliberately no default model id;
see engineering decision D7.

---

## How it works

```
documents → extraction → financial facts → model discovery → rule engine
   → deterministic calculation → falsification → evidence validation
   → confidence → priority → result
```

The model interprets, spots patterns and forms hypotheses. The rule engine
decides whether a rule applies, which exceptions bite, and what the arithmetic
is. **Nothing reaches the user as actionable without a value read from a
document and a versioned, cited rule behind it.**

Design properties, each enforced by a test rather than a convention:

- **Money is exact and always an interval.** Integer öre throughout; tax rates in
  basis points. No type in the system can express a single-figure estimate.
- **Rules are data, not prompts.** Prompts are tested to contain no digits at
  all, so a rate or threshold cannot leak out of the rule engine.
- **Unknown ≠ no.** Conditions evaluate in three-valued logic, so an unanswered
  onboarding question makes a rule undecidable rather than silently inapplicable.
- **Confidence is computed, never taken from the model.** Six measured factors,
  and three hard caps that no weighting can override.
- **Falsification can only demote.** The second pass exists to disprove; it
  cannot endorse, and its absence is not treated as approval.
- **Finding nothing is a designed result**, reported with which areas were
  checked and what further material would help.
- **No chain-of-thought is stored.** The response type has nowhere to put it, and
  a test asserts the serialised shape.

---

## Layout

```
apps/api/                the HTTP surface, its OpenAPI contract, its beta interface
workers/analysis-worker/ the process that claims jobs and runs analyses
crates/core              money, facts, evidence, confidence, classification,
                         the analysis state machine, the evidence graph
crates/telemetry         metrics, correlation ids, W3C trace context, redacted logs
crates/rules             versioned rule evaluation, three-valued conditions
crates/model             provider abstraction, prompts, output validation
crates/gateway           pricing, budgets, fallback policy, injection defence
crates/extract           text extraction, Swedish statement parser
crates/pipeline          orchestration of the two passes, the report
crates/store             tenant-scoped Postgres, blobs, retention, rate limits
crates/jobs              durable queue: leases, retries, backoff, dead letters
rules/se-ruleset.json    the versioned rule set
migrations/              forward-only schema with row-level security
infrastructure/          kustomize base, three overlays, GitOps, alerts
tests/golden/            ten synthetic companies with expected findings
tests/security/          tenant isolation and the attack suite
tests/failure/           job-system failure injection
tests/infrastructure/    manifest and documentation validation
tests/supply-chain/      SBOM generation, image inspection
tests/e2e/               the 20-step product test
docs/                    the eight documents below
Dockerfile               one file, two images
```

## Documentation

| Document | Answers |
|---|---|
| [Architecture](docs/SKATTJAKT_ARCHITECTURE.md) | What the shape is, and why the worker is a separate process |
| [Security](docs/SKATTJAKT_SECURITY.md) | What protects the system, where it is enforced, how it is tested |
| [Threat model](docs/SKATTJAKT_THREAT_MODEL.md) | 15 threats with likelihood, impact, mitigation, detection, response |
| [Data model](docs/SKATTJAKT_DATA_MODEL.md) | 22 tables, and why each is shaped that way |
| [Analysis pipeline](docs/SKATTJAKT_ANALYSIS_PIPELINE.md) | What happens between an upload and a report |
| [Rule engine](docs/SKATTJAKT_RULE_ENGINE.md) | How a rule works, and how one is changed |
| [Deployment](docs/SKATTJAKT_DEPLOYMENT.md) | How it is built and promoted — and §9, what is not verified |
| [Runbook](docs/SKATTJAKT_RUNBOOK.md) | It is 03:00 and something is wrong |
| [Product surface matrix](docs/SKATTJAKT_PRODUCT_SURFACE.md) | Which surfaces exist, which are prepared, and what platform was reachable |
| [Client architecture](docs/SKATTJAKT_CLIENT_ARCHITECTURE.md) | What web, Apple and Android need — and what the backend already guarantees them |
| [Memory architecture](docs/SKATTJAKT_MEMORY_ARCHITECTURE.md) | The four state layers, and why there is no cache and no vector store |
| [Monte Carlo layer](docs/SKATTJAKT_SIMULATION.md) | Distributions, reproducible runs, sensitivity, convergence — and why a simulated figure is never evidence |
| [Engineering decisions](docs/SKATTJAKT_ENGINEERING_DECISIONS.md) | Why X was decided that way |
| [Product specification](docs/SKATTJAKT_PRODUCT_SPEC.md) | What the product promises a customer |

## Disclaimer

Skattjakt är ett analys- och upptäcktsverktyg. Resultaten är preliminära och ska
inte betraktas som juridisk rådgivning, revisionsuttalande, skattebesked eller
garanti om skatteåterbäring eller besparing. Identifierade möjligheter bör
verifieras mot aktuella regler och företagets fullständiga underlag innan någon
åtgärd vidtas.

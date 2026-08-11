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
| Domain model, rule engine, extraction, pipeline | Implemented, 233 tests |
| Golden dataset, 10 cases | Precision 1.000, recall 1.000, zero false positives |
| Tenant isolation (Postgres RLS) | Implemented, 10 checks verified against a real cluster |
| HTTP API + OpenAPI contract | Implemented, 13 endpoints, running |
| Persistence, document storage, async analyses, report | Implemented, verified end to end against a real cluster |
| Beta interface | Implemented, driven through a real browser |
| OCR for scanned PDFs | **Not implemented** — unreadable pages are reported, not read |
| Docker image | **Authored, unbuilt** — the registry is unreachable from the build environment |
| Kubernetes | **Authored, never applied** — no cluster was available |

Known gaps are listed in full at the end of
[`docs/SKATTJAKT_ENGINEERING_DECISIONS.md`](docs/SKATTJAKT_ENGINEERING_DECISIONS.md).

---

## Quick start

```sh
cargo test --workspace                                  # 233 tests
cargo test -p skattjakt-pipeline --test golden -- --nocapture   # the golden dataset
./scripts/test-tenant-isolation.sh                      # RLS against a real cluster
./scripts/test-end-to-end.sh                            # the whole product, section 40

SKATTJAKT_API_TOKEN=dev cargo run -p skattjakt-api
```

Then open <http://localhost:8080/>, paste `dev` as the token, and run the flow.

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
api/openapi.yaml         the contract — source of truth, served by the build
crates/core              money, facts, evidence, opportunities, confidence
crates/rules             versioned rule data, three-valued conditions, calculations
crates/extract           text extraction, Swedish statement parser
crates/model             provider abstraction, prompts, output validation
crates/pipeline          orchestration and the report
crates/store             tenant-scoped Postgres access, immutable blob storage
crates/api               HTTP surface and the beta interface (crates/api/ui)
migrations/              schema with row-level security
testdata/golden/         ten synthetic companies with expected findings
scripts/                 tenant isolation and end-to-end product tests
deploy/                  Dockerfile, compose, Kubernetes manifests
docs/                    architecture, product spec, engineering decisions
```

## Documentation

- [Architecture](docs/SKATTJAKT_ARCHITECTURE.md)
- [Product specification](docs/SKATTJAKT_PRODUCT_SPEC.md)
- [Engineering decisions](docs/SKATTJAKT_ENGINEERING_DECISIONS.md) — including
  the known gaps

## Disclaimer

Skattjakt är ett analys- och upptäcktsverktyg. Resultaten är preliminära och ska
inte betraktas som juridisk rådgivning, revisionsuttalande, skattebesked eller
garanti om skatteåterbäring eller besparing. Identifierade möjligheter bör
verifieras mot aktuella regler och företagets fullständiga underlag innan någon
åtgärd vidtas.

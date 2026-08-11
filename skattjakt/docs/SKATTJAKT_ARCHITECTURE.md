# Skattjakt — Architecture

## The shape of the system

```
document bytes
     │
     ▼
 extraction ──────────────► pages of text  ─┐
     │                                      │
     ▼                                      │
 Swedish statement parser                   │  (bounded excerpt)
     │                                      │
     ▼                                      ▼
 canonical financial facts ──────────► model: discovery pass
     │                                      │
     │                                      ▼
     │                                 candidates
     ▼                                      │
 rule engine (versioned, cited)             │
     │                                      │
     ▼                                      │
 deterministic calculation                  │
     │                                      │
     └──────────────┬───────────────────────┘
                    ▼
        model: falsification pass
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

The division of labour is the architecture. The model interprets, spots patterns
and forms hypotheses. The rule engine decides whether a rule applies, which
exceptions bite, which period it covers, and what the arithmetic is. Nothing
reaches the user as actionable without a value read from a document *and* a
versioned rule behind it.

## Crates

| Crate | Owns | Depends on |
|---|---|---|
| `skattjakt-core` | Money, company identity, facts, evidence chains, opportunities, confidence, priority. No I/O. | — |
| `skattjakt-rules` | Versioned rule data, three-valued conditions, deterministic money expressions, per-year constants. | core |
| `skattjakt-extract` | Text extraction, Swedish statement parsing. | core |
| `skattjakt-model` | Provider abstraction, Anthropic adapter, versioned prompts, structured-output validation. | core |
| `skattjakt-pipeline` | Orchestration of the flow above, and the report. | all of the above |
| `skattjakt-store` | Tenant-scoped Postgres access and immutable blob storage. | core, model |
| `skattjakt-api` | HTTP surface implementing `api/openapi.yaml`, plus the beta interface. | pipeline, store |

Dependencies point one way. `core` knows nothing about databases, HTTP or models,
which is why the rules about *meaning* — what may be called actionable, how
confidence is arrived at — are testable in isolation and cannot be bypassed by a
caller in a hurry.

## Where each rule of the build order lives

| Principle | Enforced by |
|---|---|
| Evidence first (§7) | `EvidenceChain::validate_actionable` — requires a document value and a rule. Called by the pipeline before a finding may be `identified`. |
| Rules ≠ model (§10) | Rules are JSON with citations, loaded and validated at startup. A test asserts no prompt contains a digit. |
| Deterministic when possible (§9) | `ImpactSpec` expression trees over integer öre, evaluated in `skattjakt-rules`. |
| Fail closed (§35) | `Confidence::compute` caps the score below actionable when rule match, document evidence, or consistency is absent. |
| Tenant safe (§20) | Postgres row-level security; `scripts/test-tenant-isolation.sh`. |
| Reproducible (§35) | Analysis pins document versions, rule set version, prompt version and model run. A golden test runs each case twice and compares. |
| No chain-of-thought stored (§21) | `ModelResponse` has no field for it; a test asserts the serialised shape. |

## Money

Integer öre throughout (`Money`), never floating point. Tax rates are basis
points, so 20.6 % of 100 000 kr is exactly 20 600 kr. Every user-facing amount is
a `MoneyRange`; there is no type in the system that expresses a single-figure
economic estimate, which is the strongest available guarantee that §13 is not
violated by accident.

## Three-valued conditions

`Truth` is `True | False | Unknown`, with Kleene semantics. An unanswered
onboarding question is `Unknown`, not `False`. A rule that ends `Unknown` becomes
`Indeterminate` — a finding that says what could not be decided and which
question would decide it. Rules guard on the presence of their trigger fact, so
"we have no data at all here" is *not applicable* rather than *undecided*; see
decision D3.

## Confidence

Six measured factors, weighted and configurable:

| Factor | Meaning | Default weight |
|---|---|---|
| `document_evidence` | Fraction of needed values actually read from a document | 0.25 |
| `rule_match` | How cleanly a versioned rule matched | 0.25 |
| `calculation_certainty` | Whether the arithmetic is reproducible | 0.15 |
| `missing_information` | How much needed information is absent (inverted) | 0.15 |
| `contradiction_score` | How strongly falsification objected (inverted) | 0.10 |
| `model_agreement` | Agreement across passes | 0.10 |

Then three hard caps, which no weighting can override: no rule matched, a live
contradiction, or no document evidence each force the score below the actionable
threshold. The model's own agreement carries the smallest weight and cannot
alone make anything actionable.

## Data model and tenancy

Thirteen tables (`migrations/0001_init.sql`). Every tenant table carries
`company_id` and has `FORCE ROW LEVEL SECURITY` with a policy keyed on
`current_setting('skattjakt.company_id')`. The application connects as
`skattjakt_app`, which is neither superuser nor owner, so the policies apply to
it. Two operational facts follow:

- **Every transaction must set the tenant.** `SET LOCAL skattjakt.company_id`.
  Forgetting it returns zero rows, never another tenant's.
- **Migrations run as the owner**, which bypasses the policies. That is intended
  and is why the migration role and the application role are different.

`audit_events` has `UPDATE` and `DELETE` revoked from the application role, so a
recorded step cannot be rewritten.

## Failure modes

| Condition | Behaviour |
|---|---|
| Scanned page, no text | Page listed in `unreadable_pages`, `unreadable_page` warning, extraction confidence scaled down |
| Two documents disagree | Both readings retained, best-supported one canonical, `conflicting_values` warning |
| Rule set has no version for the year | `PipelineError::TaxYearNotCovered` → HTTP 400 naming the year. Never an empty analysis |
| Rule names a constant that does not resolve | `RuleOutcome::RuleError` → warning; the rule produces no finding |
| Model refuses or fails | Run recorded with status; analysis continues rules-only |
| Model returns off-schema output | `SchemaViolation`; the pass is treated as failed |
| Response truncated | `ProviderError::Truncated`; never parsed as a partial result |
| Nothing found | Designed empty state with covered areas, limitations and disclaimer |

## Reproducing an analysis

An analysis is pinned to: document version ids, rule set version, prompt version,
provider and model id, and the stored structured output of each model run. The
rule expression is stored with each calculation, so the arithmetic can be re-run
verbatim. What is deliberately *not* stored is any reasoning trace.

## Deployment

Stateless API container; Postgres for state; object storage for documents (not
yet wired). `/health` is liveness and touches nothing; `/ready` is readiness and
reports what is missing. `SIGTERM` drains in flight analyses. The Kubernetes
manifests in `deploy/k8s` run as a non-root user with a read-only root
filesystem and all capabilities dropped.

**Neither the container image nor the manifests have been built or applied.**
The daemon runs in the build environment, but its egress to Docker Hub is
rate-limited, so no base image can be pulled; and no cluster was available.
They are authored, not verified.

### Running modes

The service runs in two modes, and says which on `/ready`:

- **Stateless** — no `DATABASE_URL`. `POST /v1/analyses` takes documents inline,
  runs the pipeline, returns the result, and stores nothing.
- **Persistent** — with `DATABASE_URL`. Companies, documents, analyses,
  opportunities, evidence, calculations, model runs and audit events are stored.
  Analyses run in the background and the client polls the stage.

An analysis that runs for minutes is normal at high effort, which is why the
persistent path returns `202` rather than holding the request open.

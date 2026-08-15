# Skattjakt — Analysis pipeline

What happens between a customer pressing "analysera" and a report appearing.

The pipeline's job is not to find as much as possible. It is to find things that
are **true**, state what each rests on, and refuse to present anything it cannot
support. Most of what follows is machinery for refusing.

---

## 1. The state machine

An analysis is a long-lived, retryable, cancellable, externally visible process.
Left as a `status` string, that becomes a place where impossible states
accumulate: a run that is `failed` and later `succeeded`, a `cancelled` run
still charging model tokens, a `succeeded` run with no result row.

So the transitions are enumerated, and `AnalysisState::try_transition` is the
only way to move.

```
                 ┌──────────┐
    created ────►│  queued  │◄──────────────┐
                 └────┬─────┘               │
                      │ claimed             │ backoff_elapsed
                      ▼                     │
                 ┌──────────┐          ┌────┴─────┐
                 │ running  ├─────────►│ retrying │
                 └────┬─────┘ transient└────┬─────┘
                      │       lease_expired │
       completed      │                     │ attempts_exhausted
          ┌───────────┼──────────┐          │
          ▼           ▼          ▼          ▼
    ┌───────────┐ ┌────────┐ ┌─────────┐ ┌───────────────┐
    │ succeeded │ │ failed │ │cancelled│ │ dead_lettered │
    └───────────┘ └────────┘ └─────────┘ └───────────────┘
                    terminal — nothing leaves
```

Three properties are enforced and tested:

- **Nothing leaves a terminal state.** A retry starts a *new* analysis rather
  than resurrecting an old one. This is what makes the audit trail trustworthy.
- **`is_active` and `is_terminal` partition the state space.** A test asserts no
  state is both or neither.
- **Every failure event leads to a legal transition.** A property test walks
  every attempt count against every retryability and checks the resulting event
  is accepted from the current state.

Two failure paths, deliberately distinguished:

| | `failed` | `dead_lettered` |
|---|---|---|
| Cause | Retrying is pointless | It might have worked and did not |
| Examples | Unreadable PDF, fiscal year outside the rule set | Provider timeout, lost lease |
| Who sees it | The customer, with an actionable message | An operator, in the dead-letter queue |

Collapsing the two would either bury customer-actionable errors in a queue
nobody reads, or fill the operator's queue with unreadable uploads.

---

## 2. Stage 1 — Extraction

**Crate:** `skattjakt-extract`.

Bytes to pages of text. PDF via `pdf-extract`, plain text directly. The
document's SHA-256 is verified against what was recorded at upload **before**
anything is parsed.

Then the Swedish statement parser, which is more careful than it looks.

**The label table.** A line yields an amount only if it carries a recognised
Swedish statement label — `Nettoomsättning`, `Personalkostnader`,
`Periodiseringsfond`, and so on. Numbers on unlabelled lines are ignored. A
page number is not revenue.

**The three-digit group rule.** Swedish statements print `12 500 000` with
spaces, and print two years side by side:

```
Nettoomsättning        12 500 000    11 200 000
```

A naive space-stripping parser reads that as `1250000011200000`. The parser
accepts a space as a thousands separator only when followed by *exactly* three
digits not themselves followed by a fourth, and treats a run of multiple spaces
as a column break.

**Scale detection.** `tkr` in a heading multiplies by 1000. A statement in
thousands read as kronor is wrong by three orders of magnitude and looks
entirely plausible.

**Sign normalisation.** Swedish income statements print costs negative.
`FactKind::is_cost()` identifies them and `build_fact_set` stores the magnitude,
keeping the printed sign in `source_text`. Without this, `personnel_costs > 0`
never fires and `depreciation < 30% of fixed_assets` passes for any negative
depreciation — both real bugs the golden dataset caught.

Every fact carries its document version, page, source line and an extraction
confidence. **Every reading is kept**, not just the best: two conflicting
readings of revenue are a contradiction the analysis must report.

---

## 3. Stage 2 — Discovery

**Crate:** `skattjakt-pipeline`, via `skattjakt-gateway`.

The model gets: the canonical facts, the company profile, and a bounded excerpt
of document text wrapped as data (`SKATTJAKT_SECURITY.md` §5).

It returns candidate areas worth investigating: a title, a category, an
observation, the question to ask, the facts it thinks support it, what is
missing, and which rules it thinks apply.

**What a candidate is not:** a finding. It is a hypothesis. Nothing here reaches
the customer without passing every gate below.

**No rule may be fabricated by the model.** A candidate may reference a rule id;
if that id is not in the versioned rule set, the reference is dropped. There is
no path by which a model invents a rule.

**No prompt contains a number.** A test panics on any ASCII digit in any prompt,
because a figure in a prompt is a figure the model may reproduce as though it
came from the document.

---

## 4. Stage 3 — Rules

**Crate:** `skattjakt-rules`. Full detail in `SKATTJAKT_RULE_ENGINE.md`.

Every rule is evaluated against the facts using **three-valued logic**:
`True`, `False`, `Unknown`.

The third value is the point. A rule about periodiseringsfond, evaluated against
a company whose accounts never mention one, is not false — it is unanswered.
Collapsing `Unknown` to `False` hides positions; collapsing it to `True`
fabricates them. Kleene logic keeps the distinction, and an `Unknown` result
surfaces as "we could not determine this; here is what would settle it".

Outcomes: `Matched`, `NotApplicable`, `Indeterminate`, `ExceptionApplies`,
`OutOfScope`, `RuleError`.

A rule whose referenced facts are **all** absent is skipped entirely. Without
this, a rule about untaxed reserves returns `Unknown` for a company whose
accounts never mention them, becomes `Indeterminate`, and produces a finding
about something the document never discussed. Fixing this took the golden
dataset's precision from 0.864 to 1.000.

---

## 5. Stage 4 — Calculation

**Deterministic code. Never the model's arithmetic.**

Integer öre throughout, tax rates in basis points, rounding stated explicitly
per operation. Every calculation records its method, its inputs and its result,
so a figure can be re-derived rather than trusted.

Results are ranges. `MoneyRange::around(value, uncertainty_bp)` widens a figure
by a stated uncertainty; the width is a claim about how much is unknown, and
narrowing it is a decision someone has to make in code.

---

## 6. Stage 5 — Falsification

The second model pass, and the one that earns its cost.

The skeptic gets each candidate with its evidence and is asked to **refute** it:
what would have to be true for this to be wrong, what in the document
contradicts it, what is assumed rather than shown.

**The skeptic can only demote.** There is no code path by which the
falsification pass raises a confidence, promotes a status or adds a finding. A
pass that could promote would be a second discovery pass with extra steps, and
its objections would compete with its enthusiasm.

Objection strength feeds `contradiction_score`, which is one of the confidence
factors and also one of the hard caps.

---

## 7. Stage 6 — The evidence gate

The narrowest point in the pipeline.

```rust
pub fn validate_actionable(&self) -> CoreResult<()> {
    if !self.has_document_anchor() { return Err(...); }
    if !self.has_rule() { return Err(...); }
    Ok(())
}
```

A finding must rest on **at least one value read from a specific page of a
specific document version** and **at least one versioned, cited rule**. A model
judgement and an assumption are recorded but cannot satisfy the gate.

The evidence graph restates this structurally: `Supports` edges come from
document values, rules and calculations; `Informs` edges come from model
judgements and assumptions. `finding_is_document_anchored` walks `Supports`
edges only.

---

## 8. Stage 7 — Confidence

Six measured factors, weighted:

| Factor | Weight | Measures |
|---|---|---|
| `document_evidence` | 0.25 | How directly the figures come from a page |
| `rule_match` | 0.25 | Whether a rule matched or merely was not excluded |
| `calculation_certainty` | 0.15 | How wide the range is |
| `missing_information` | 0.15 | How much is unanswered |
| `contradiction_score` | 0.10 | What the skeptic found |
| `model_agreement` | 0.10 | Whether the passes agreed |

Then **three hard caps, applied after the weighted score and fail-closed**:

1. No rule match → below actionable.
2. Contradiction ≥ 0.5 → below actionable.
3. No document evidence → below actionable.

A cap is not a penalty. A finding hitting any of them cannot be presented as
actionable regardless of how the other five factors scored. The bands are
`Strong` ≥ 90, `Good` ≥ 75, `Investigate` ≥ 50, below that `NotActionable`.

---

## 9. Stage 8 — The review gate

`require_reviewed_rules_for_identified` caps every finding below `identified`
unless the rule is *grounded*, which it is in one of two ways:

- a professional reviewed it (`review: reviewed { reviewer, date }`), or
- every source it cites has been fetched and found to say what the rule assumes
  (`source_state: verified`), which
  `accept_verified_sources_in_place_of_review` allows by default.

Today neither holds for any rule: all 14 carry
`review: awaiting_professional_review` and all 24 sources are `unretrieved`. The
best any finding can be called is "needs verification".

One state overrides both: a cited source in `mismatch` — fetched, and it
contradicts the rule — drops the finding to `investigate` regardless of the
flags. Evidence pointing the wrong way is not a weaker form of no evidence.

This is machine-enforced, not a policy: the golden dataset fails the build if
anything is ever presented as `identified` while nothing is grounded, and
`GET /v1/rules` discloses the unreviewed count and each rule's `source_state` to
any caller. `SKATTJAKT_RULE_ENGINE.md` §8.1 argues why a retrieval is allowed to
carry the weight a signature otherwise would.

---

## 10. Stage 9 — Priority and the report

Priority combines impact, confidence, effort, risk and urgency. Impact enters as
the *countable* impact — a finding below the actionable threshold contributes
zero to the headline total, so an uncertain 900 000 kr cannot inflate a summary.

The report has nine sections, and two of them exist because their absence is
what makes a tool untrustworthy:

- **Covered areas**, including areas where nothing was found. "We looked here
  and found nothing" is a result.
- **Limitations**, stating plainly what the system could not determine.

Plus the disclaimer, held in one constant in `core` so every surface shows the
same words.

**The empty result is a designed state.** `found_nothing` drives a screen that
says so, rather than an empty list. A tool that always finds something is a tool
that is making things up.

---

## 11. Cost control, throughout

Every model call the pipeline makes goes through `ModelGateway` — the pipeline
holds the gateway and no provider of its own, and a test asserts that of the
source. That is not a stylistic point: it was briefly untrue, and while it was,
the gateway's own tests all passed while every production call bypassed the cost
ceiling, the fallback check and the document-data fence.

The budget is opened before the first model call and charged after each one:

- checked **before** the call using the worst-case cost, because checking
  afterwards means the money is already spent;
- a failed call is still charged, because it billed its input tokens and a
  retry loop that did not count its failures would have no ceiling;
- the budget **survives a retry** — three attempts cost one budget, not three.

An unpriced model cannot be called at all. The worker refuses to start rather
than issue unbounded calls.

---

## 12. Reproducibility

An analysis records: the document versions it read (pinned at creation), the
rule set version, the prompt versions, the model that actually served each call,
the token counts and the cost.

The end-to-end test runs the same input twice and asserts identical findings.

What is **not** reproducible, stated plainly: a model's output is not
deterministic across time. What is reproducible is everything the pipeline does
with it — the rules, the calculations, the gates and the ranking are code, and
the model's contribution is recorded so a divergence can be seen rather than
guessed at.

---

## 13. Where each property is proved

| Property | Test |
|---|---|
| No false positives on ten synthetic companies | `crates/pipeline/tests/golden.rs` |
| No finding without document evidence and a rule | `golden.rs`, `core::evidence` |
| Nothing presented as established while rules are unreviewed | `golden.rs` |
| Costs are normalised so cost rules fire | `pipeline::facts` |
| Two-year columns parse as two figures | `extract::swedish` |
| The state machine admits no impossible transition | `core::state_machine` (10 tests) |
| A dead pod's analysis is retried, not lost | `tests/failure/job-failures.sh` |
| The pipeline cannot reach a model except through the gateway | `pipeline_tests.rs` |
| Document text reaches the model wrapped as data | `pipeline_tests.rs` |
| An escaped forged fence is still reported to the customer | `pipeline_tests.rs` |
| An exhausted budget degrades to rules-only rather than failing | `pipeline_tests.rs` |
| The whole product works end to end | `tests/e2e/end-to-end.sh` (20 steps) |

Current golden dataset: **57 true positives, 0 false positives, 0 false
negatives. Precision 1.000, recall 1.000.**

---

## 10. One engine, three presentation layers

The product serves three readers — a private individual, a company, an
accounting assistant reviewing a client's closing. They are **not** three
products. `report::Audience` selects a rendering of one analysis; the rules, the
evidence, the confidence and the status ladder are identical in all three, and
`every_audience_reads_the_same_analysis` fails the build if they ever diverge.

Three products would mean three rule sets to keep in step, and the one that fell
behind would be the one nobody noticed.

### What each layer adds

Everyone gets the **action plan**: every finding's recommended action, ordered
by priority band and then by the top of its money range, deduplicated on the
action text, and marked `Kontroll` or `Möjlighet`. A report is not a
deliverable; a decision is, and seven findings each carrying "recommended
action" leaves the reader to do the ranking the product exists to do.

`accountant` adds `control_review`, in four bands:

| Band | What is in it |
|---|---|
| Måste kontrolleras | Findings the engine could not settle — status `warning`, `investigate` or `verify` |
| Möjlig förbättring | Only `identified`: strong evidence and a reviewed rule |
| Ser korrekt ut | Rules evaluated against values we actually read, which did not apply |
| Värt att ta upp med kunden | Findings with a computed amount, largest first |

### Why `verify` is a must-check and not an improvement

`verify` means the rule or the calculation needs checking. It sat under "möjlig
förbättring" until a real analysis was run through all three layers and
compared: every one of the six findings was `verify` — because no legal source
has been retrieved and no rule professionally reviewed, which caps every finding
in this build — so "måste kontrolleras" was empty while six improvements each
rested on an unverified rule.

That is the failure the source-state ladder exists to prevent, reintroduced one
layer above it. A band promising "what must be checked before filing" that is
empty precisely when everything needs checking is worse than no band at all.

### Why "ser korrekt ut" is not simply "did not fire"

A rule that did not fire is not automatically a clean bill of health. A rule
whose trigger fact was never found in the documents decided nothing, and
reporting it as checked would be the most damaging kind of wrong: reassurance in
place of a look. Only a rule whose referenced facts were all readable is
cleared; the rest surface as missing information.

The one case that looks like a hole and is not: a rule keyed on a profile
answer references no document fact at all, so the readable-facts test passes
vacuously. That is correct, because Kleene logic (§3) means an *unanswered*
profile question yields `Indeterminate`, never `NotApplicable` — a profile-driven
rule reaching `NotApplicable` means somebody answered and the answer excluded it.
Both directions are asserted in `a_profile_driven_rule_may_be_cleared_without_reading_a_document`.

### What the layers deliberately do not change

- **No point estimates.** A talking point carries a `MoneyRange`, like
  everything else that expresses money. An assistant who quotes a single figure
  to a client owns that figure; the product will not hand them one.
- **No lifted ceiling.** `accountant` does not unlock `identified`. The gate in
  §9 applies to every audience, so a review for a professional is capped by the
  same unverified rule set as a report for a consumer.

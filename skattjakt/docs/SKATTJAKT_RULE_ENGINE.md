# Skattjakt — Rule engine

The rule set is the part of Skattjakt that says what Swedish tax law allows. It
is data, not code; it is versioned; every rule cites its source; and no model
can add to it.

**The current rule set has not been reviewed by a tax professional.** Every rule
carries `review: awaiting_professional_review`. **No source in the registry has
been retrieved either** — the statute databases are unreachable from the build
environment, so all 24 sit at `unretrieved`. With neither of the two gates in §8
satisfied, the pipeline refuses to present any finding as established. §8
explains how that is enforced rather than promised, and §8.1 explains why a
retrieved source is allowed to do a reviewer's job.

---

## 1. Why rules are data

A rule expressed as Rust is a rule that requires a deploy to correct, cannot be
diffed by someone who does not read Rust, and cannot state its own provenance.

`rules/se-ruleset.json` is loaded at startup, validated, and embedded in the
binary with `include_str!` — so a deployed build always carries the exact rule
set it was built and tested against, and a rule set cannot be swapped
underneath a running process.

The trade: a rule change is a rebuild. That is the correct cost. A rule change
alters what customers are told about their tax position, and it should go
through review, the golden dataset and a promotion pipeline — not a
configuration reload.

---

## 2. The shape of a rule

```json
{
  "rule_id": "se.tax.periodiseringsfond.outnyttjat_utrymme",
  "version": "2025.1",
  "jurisdiction": "SE",
  "tax_year_from": 2023,
  "title": "Periodiseringsfond",
  "description": "Ett aktiebolag med skattemässigt överskott får sätta av …",
  "category": "tax",
  "conditions": { "type": "all", "of": [ … ] },
  "exceptions": [],
  "impact": { "kind": "range", "low": { … }, "high": { … } },
  "effect": "reduction",
  "required_evidence": ["taxable_result"],
  "missing_information_hints": [ … ],
  "recommended_action": "…",
  "sources": ["il-30-5", "il-30-6"],
  "review": {
    "state": "awaiting_professional_review",
    "note": "Avsättningsunderlaget är överskottet före avsättning; regeln
             approximerar detta med det skattemässiga resultatet och behöver
             verifieras mot deklarationsunderlaget."
  },
  "effort": "low", "risk": "low", "urgency": "before_year_end",
  "relevance": 0.9
}
```

Three fields are load-bearing beyond the obvious.

**`sources` is mandatory and is a list of registry ids.** `RuleEngine::validate`
rejects a rule set where any rule cites nothing, or cites an id that is not in
the registry, at startup, so the binary will not run. A finding whose provenance
is "the system says so" is not something a customer can take to their
accountant.

The indirection is what makes §2.1 possible: a citation that is a free-text
string can only be read, while a citation that is a key into a registry of
fetchable documents can be checked.

**`review.note` states the known weakness of the rule**, in the rule. The
example above admits that it approximates the reservation base. When a
professional reviews this set, the note is the question they are being asked.

**`tax_year_from`** bounds applicability. A rule set with no version covering the
analysed year stops the analysis rather than guessing — `PipelineError::TaxYearNotCovered`.
Constants exist for 2023, 2024 and 2025 only; 2026 is deliberately absent, so an
analysis of a 2026 fiscal year fails loudly rather than applying 2025's figures.

---

## 2.1 The source registry

Every rule and every figure points into one registry at the top of
`rules/se-ruleset.json`. There are 24 entries. One of them:

```json
"il-30-5": {
  "authority": "Sveriges riksdag",
  "collection": "SFS",
  "document": "1999:1229",
  "title": "Inkomstskattelag (1999:1229)",
  "locator": "30 kap. 5 §",
  "url": "https://www.riksdagen.se/sv/dokument-och-lagar/…",
  "machine_url": "https://data.riksdagen.se/dokument/sfs-1999-1229.html",
  "asserted_claim": "En juridisk person får dra av högst 25 procent av
                     överskottet av näringsverksamheten före avdraget till
                     periodiseringsfond.",
  "must_contain": ["25 procent", "periodiseringsfond"],
  "retrieval": { "state": "unretrieved", "at": null, "sha256": null, "note": null }
}
```

`asserted_claim` is what the rule set believes the paragraph says. `must_contain`
is the operative words and figures the rule depends on — the strings whose
absence means either the law moved or the rule was wrong when it was written.

The check itself lives in `crates/rules/src/verify.rs` and is pure: it is handed
text and returns a verdict. It confirms four things — the document is the one
cited (the SFS number appears in it), the cited locator appears, every
`must_contain` string appears, and it records a SHA-256 of the text it read.
Markup is stripped first, and `script` and `style` contents are dropped rather
than flattened: a page whose only "25 procent" sits inside a `<script>` does not
say 25 procent.

The states form a ladder, weakest first:

| state | meaning |
|---|---|
| `unretrieved` | nobody has fetched it |
| `unreachable` | a fetch was attempted and failed — a network fact, not a legal one |
| `mismatch` | it was fetched and it does **not** say what the rule assumes |
| `verified` | it was fetched and it does |

A rule's state is the **weakest** of its sources — a rule resting on one checked
paragraph and one unchecked one is unchecked — with one exception:
`engine::combine` makes a `mismatch` dominate. Taking the plain minimum reports
`unreachable` for a rule citing one contradicted paragraph and one that could
not be fetched, which hides the contradiction behind a network failure and lets
the gate pass a rule we hold evidence against.

### Where the state lives, and why not in the file

The registry above ships inside the binary. Its `retrieval` block is only the
**default**: the live record is a row in `source_retrievals`, written by the
worker and read by every analysis. `migrations/0008_source_retrievals.sql`
explains the reasoning, which is short — a verification that can only be
recorded at build time can only be as current as the last build, and the law
does not change on our release schedule.

So the claim (which paragraph, what it is assumed to say, which strings must
appear) is versioned with the code, and the check (when, what hash, what
verdict) is not.

### How it runs

- **Continuously.** The analysis worker sweeps every six hours, considering it
  every five minutes so a fresh deployment converges quickly. Requests are
  spaced 750 ms apart, and a Postgres advisory lock stops two workers sweeping
  at once — the guard holds its connection, because a lock released through a
  pool can land on a different connection and silently fail.
- **On demand.** `skattjakt-analysis-worker verify-sources [--ruleset PATH]
  [--write]` runs the same check and prints a report. Exit codes are for a
  pipeline: 0 all verified, 1 a source contradicted the rule set, 2 nothing
  could be retrieved.

One check, called from two places. Two implementations of "does this paragraph
still say 25 per cent" drift, and the one that drifts is the one nobody runs.

### What is enforced rather than intended

- `RuleEngine::validate` rejects any source claiming `verified` without both a
  hash and a timestamp, and the `verified_carries_its_evidence` constraint
  rejects the same row in the database. The state cannot be granted by editing
  a file.
- A failed fetch **never** clears an earlier successful retrieval. A proxy
  outage today is not evidence about the law, and discarding last week's
  verified hash because a gateway said no would make the record less true.
- `tests/integration/source-verification.sh` runs the whole path against a real
  Postgres and a real HTTP server: fixtures that agree, a rate that has moved, a
  404, a refused connection, the hash changing when the page does, the failure
  streak accumulating, and — the one that matters most — a verified source
  surviving a later unreachable check. It finishes by starting the API and
  asserting it reports what the sweep found rather than what the binary was
  built with. 27 checks.
- 16 unit tests in `verify.rs` cover the judgement itself, including the ones a
  fixture server cannot easily produce: a paragraph number matching a longer
  one, a chapter and paragraph too far apart to be the same citation, entities,
  and an unterminated `<script>`.

**Current state: 0 verified, 0 mismatched, 24 unretrieved.** Every statute host
(`riksdagen.se`, `data.riksdagen.se`, `rkrattsbaser.gov.se`) is blocked by the
build environment's egress proxy. The registry says so rather than being filled
in from memory, because a citation nobody fetched and a citation somebody
fetched are different things and the difference is the entire point of the file.

---

## 3. Three-valued logic

Conditions evaluate to `True`, `False` or `Unknown`, with Kleene semantics.

The third value is the entire reason the engine is written this way. A rule
about periodiseringsfond, evaluated against a company whose accounts never
mention one, is not false — it is *unanswered*. Collapsing `Unknown` to `False`
hides real positions; collapsing it to `True` fabricates them. Keeping it lets
the analysis say "we could not determine this, and here is what would settle
it", which is the honest answer and is often the most useful thing in the
report.

```
  AND    │ T  F  U        OR     │ T  F  U        NOT │
  ───────┼────────        ───────┼────────        ────┼───
    T    │ T  F  U          T    │ T  T  T          T │ F
    F    │ F  F  F          F    │ T  F  U          F │ T
    U    │ U  F  U          U    │ T  U  U          U │ U
```

`F AND U = F` and `T OR U = T`: one decided operand can settle the expression
even when another is unknown, which is what keeps `Unknown` from swallowing
every rule that touches an optional fact.

### Condition types

| Type | Meaning |
|---|---|
| `fact_present` | The fact was read from a document at all |
| `compare` | An arithmetic comparison between expressions |
| `profile_flag` | An answer from the company questionnaire |
| `all` / `any` / `not` | Kleene combinators |

`fact_present` guards matter. A rule whose trigger fact is absent returns
`Unknown`, becomes `Indeterminate`, and produces a finding about something the
document never mentioned. Adding these guards to nine rules took the golden
dataset from precision 0.864 to 1.000.

The engine additionally skips any rule whose referenced facts are **all** absent
— belt and braces, and it exempts profile-only rules, which reference no facts
by design.

### Expressions

`fact`, `fact_or_zero`, `amount`, `rate`, `add`, `sub`, `mul_rate`, `max0`.

Deliberately not a general expression language: no loops, no user functions, no
string operations. A rule set is data loaded at startup, and a Turing-complete
expression language in that position is a code-execution surface.

`fact_or_zero` is distinct from `fact` and the distinction is meaningful: `fact`
propagates `Unknown` when the fact is missing, `fact_or_zero` asserts that
absence means zero. Choosing between them is a modelling decision the rule
author has to make explicitly.

---

## 4. Outcomes

| Outcome | Meaning |
|---|---|
| `Matched` | Conditions true, no exception applies |
| `NotApplicable` | Conditions decidedly false |
| `Indeterminate` | Something is unknown; surfaces as missing information |
| `ExceptionApplies` | Conditions true but an exception excludes it |
| `OutOfScope` | The rule does not cover this tax year |
| `RuleError` | The rule is malformed — a bug, logged, never shown |

`ExceptionApplies` is kept distinct from `NotApplicable` because they are
different answers to the customer: "this does not apply to you" versus "this
would apply, but this specific exception excludes it" — the second is worth
telling them, because the exception may not hold next year.

---

## 5. Rates and constants

Rates are **basis points**, integers. Amounts are **öre**, integers. Nothing in
the tax arithmetic touches floating point.

```json
{ "tax_year": 2023,
  "parameters": {
    "prisbasbelopp":  { "kind": "amount_ore", "value": 5250000, "source": "sfb-2-6" },
    "corporate_tax":  { "kind": "rate_bp",    "value": 2060,    "source": "il-65-10" }
  } }
```

Per tax year, and **per figure**, each naming the registry entry it comes from.
The earlier shape carried one prose paragraph covering every constant in a year,
which meant a single wrong figure could not be traced to a single document —
and a reader checking one number had to re-read the sentence covering all of
them. `sfb-2-6` is the statute that defines prisbasbeloppet; `il-65-10` is the
one that sets the corporate rate; both are individually fetchable and
individually falsifiable.

`validate()` rejects:

- a rule referencing a rate that cannot be resolved for a year the rule claims
  to cover, so a rule that would silently compute against a missing constant
  fails at startup instead;
- a parameter citing a source id that is not in the registry.

---

## 6. Impact

```json
"impact": {
  "kind": "range",
  "low":  { "op": "amount", "sek": 0 },
  "high": { "op": "mul_rate", "of": { … }, "rate": "corporate_tax" }
},
"effect": "reduction"
```

`kind: "range"` is the only form that produces money, and both bounds are
written by the rule author. There is no path from a rule to a single figure,
because `MoneyRange` is the only money type the impact code can produce.

There used to be a `kind: "point"` that took one expression and widened it by
an `uncertainty_bp` band — ±15% on one rule, ±10% on another. Those numbers came
from nowhere. Nobody had measured them, and no input to the calculation was
known to that tolerance, so the interval looked like a statement about
uncertainty and was a decoration on a point estimate. Writing both bounds forces
the author to say what the low end *means*: for an unused allowance it is "the
company makes no allocation", for a carried-forward loss it is "a spärr removes
the deduction entirely". Both are states of the world. ±10% never was.

`effect` says whether the amount is tax saved or tax postponed, and defaults to
`reduction`. A rule marked `deferral` — periodiseringsfond is the shipped case —
keeps its amount and its own line in the report, and contributes nothing to the
headline total. Adding an allocation to a periodiseringsfond to a missed
deduction answered a question nobody asked: the first is money the company
keeps, the second is the same money paid in a later year with a schablonintäkt
charged every year the fund stands.

`kind: "none"` is for rules that flag something worth checking without claiming
an amount. Several rules use it, and that is correct: "you have reserves that
must be returned within six years, check which year each was set aside" is
valuable and un-quantifiable from the accounts alone.

---

## 7. Validation at startup

`RuleEngine::validate()` refuses to load a rule set with:

- duplicate rule ids;
- a rule that cites no source, or cites a source id not in the registry;
- a parameter that cites a source id not in the registry;
- a source claiming `verified` without a hash and a retrieval timestamp;
- a rate reference that cannot be resolved for a covered year;
- an inverted year window (`tax_year_to < tax_year_from`);
- a malformed condition or expression tree.

Failing at startup rather than at evaluation. A rule set that is wrong should be
a failed rollout that a readiness probe catches, not a wrong answer to a
customer six weeks later.

---

## 8. The review gate

Every rule carries:

```json
"review": { "state": "awaiting_professional_review", "note": "…" }
```

`PipelineConfig::require_reviewed_rules_for_identified` defaults to `true`.
While it holds, no finding produced by an ungrounded rule can be presented as
`identified`; the best any finding reaches is "needs verification". A rule is
grounded when **either** a professional has reviewed it **or** every source it
cites is `verified` — §8.1 on why the second counts.

This is enforced in three places, deliberately redundantly:

1. **In the pipeline**, at `status_for()`.
2. **In the golden dataset test**, which fails the build if any finding is ever
   presented as `identified` while the flag holds. A future change that removes
   the gate breaks the build rather than shipping.
3. **In the API**, where `GET /v1/rules` discloses the unreviewed count to any
   caller, and per rule the `sources[]` with each one's state and a
   `source_state` that is the weakest of them. The limitation is visible to a
   customer, not buried.

The gate is machine-enforced because a policy of "we will be careful until the
review happens" survives about one deadline.

---

## 8.1 Why a retrieved source may stand in for a reviewer

`PipelineConfig::accept_verified_sources_in_place_of_review` defaults to `true`.

A signature is a weak guarantee. It is unfalsifiable, it does not survive the
law changing, and nobody can check it afterwards without repeating the whole
review. A retrieval is a stronger guarantee for exactly the opposite reasons: it
names a document anybody can open, it records a hash so a change is detectable,
and re-running one command re-establishes or destroys it.

What a retrieval establishes, precisely: the paragraph the rule cites was
fetched from the authority that publishes it, and the operative words and
figures the rule depends on were present in it.

What it does not establish: that the rule *applies* its source correctly. That a
paragraph says 25 percent does not establish that this rule computes the right
base to apply it to. That question still needs a person — but the arithmetic
between the paragraph and the figure is deterministic Rust with its own tests,
which is itself checkable in a way a signature is not.

Two things bound the flag:

- Setting it to `false` restores signature-only grounding, and
  `verified_sources_unlock_nothing_when_the_operator_says_they_may_not` fails
  the build if it ever stops doing that.
- A cited source in the `mismatch` state overrides the flag entirely and drops
  the finding to `investigate`. A source that was fetched and contradicts the
  rule is not a weaker form of no evidence — it is evidence pointing the other
  way. Without this, turning source verification on would make a known-broken
  rule *more* trusted than an unchecked one.

Today the flag changes no outcome, because nothing is verified. The five tests
in `pipeline_tests.rs` construct each state and assert the ladder, so the branch
that eventually matters is not shipping untested.

---

## 9. Changing a rule

Section 53's workflow, enforced by the database rather than by process.

**1. Propose.** Edit `rules/se-ruleset.json`. Bump `version`. A rule whose
*meaning* changes gets a new version string, not an edited one — two versions of
a rule are two nodes in the evidence graph, which is what makes step 3 possible.

**2. Establish the blast radius.** Query the evidence graph:

```rust
let rule = NodeId::Rule { rule_id, rule_version };
graph.affected_analyses(&rule);   // which runs rested on it
graph.affected_findings(&rule);   // which findings would change
```

Without this, correcting a rule means re-running everything and hoping.

**3. Record the proposal**, with its blast radius:

```sql
INSERT INTO rule_set_approvals
  (rule_set_version, proposed_by, change_summary, affected_analyses)
VALUES ('se-2025.2', 'anna', 'raise the reserve ceiling', 41);
```

**4. Review by someone else.** The database enforces this:

```sql
CONSTRAINT reviewer_is_not_the_proposer CHECK (reviewed_by <> proposed_by)
CONSTRAINT a_decision_names_its_reviewer CHECK (
    approved IS NULL OR (reviewed_by IS NOT NULL AND reviewed_at IS NOT NULL))
```

A workflow enforced only by process is a workflow that gets skipped at 17:55 on
a Friday. `tests/failure/job-failures.sh` proves that a proposer cannot approve
their own change and that a change cannot be approved without naming a reviewer.

**5. The golden dataset must still pass**, at precision 1.000. A rule change
that introduces a false positive fails CI.

**6. Promote** through dev → staging → production, as a normal deploy.

`UPDATE` and `DELETE` on `rule_set_approvals` are revoked once a decision is
recorded. A changed mind is a new version, not an edit to the record of the old
decision.

---

## 10. The current rule set

`se-2025.1`, 14 rules, covering tax years 2023–2025.

| Category | Rules |
|---|---|
| `tax` | 6 |
| `costs` | 2 |
| `risk` | 2 |
| `investments`, `personnel`, `vat`, `research_and_development` | 1 each |

All 14 are `awaiting_professional_review`.

---

## 11. What the model may and may not do

**May:** propose that a rule is worth evaluating, and reference a rule id it
believes applies.

**May not:** create a rule, edit a rule, change a rate, change a citation,
change a review state, or cause a rule to match. A referenced id that is not in
the versioned set is dropped.

There is no code path from a model response to the rule set. The rule set is
embedded in the binary at compile time; nothing at runtime can write to it. This
is section 52's constraint enforced by absence rather than by a check.

---

## 12. Known limitations

- **No professional review.** The single largest limitation, and the reason for
  §8. Until a Swedish tax professional has read all 14 rules and their notes,
  Skattjakt presents nothing as established.
- **No 2026 constants.** Deliberate. An analysis of a 2026 fiscal year fails
  loudly rather than applying 2025's figures.
- **14 rules is a beta rule set**, not comprehensive coverage of Swedish
  corporate taxation. The report's "covered areas" section is honest about what
  was and was not examined.
- **No group taxation rules.** Koncernbidrag appears as a fact but no rule
  reasons about group structures; the questionnaire asks, and the answer feeds
  missing-information rather than a finding.
- **No industry-specific rules.** `sni_code` is collected and unused.
- **Rules approximate where the accounts are ambiguous**, and each says so in
  its `review.note` rather than in a document nobody reads next to the code.

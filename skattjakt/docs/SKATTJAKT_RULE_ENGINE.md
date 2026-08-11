# Skattjakt — Rule engine

The rule set is the part of Skattjakt that says what Swedish tax law allows. It
is data, not code; it is versioned; every rule cites its source; and no model
can add to it.

**The current rule set has not been reviewed by a tax professional.** Every rule
carries `review: awaiting_professional_review`, and the pipeline refuses to
present any finding as established while that is true. §8 explains how that is
enforced rather than promised.

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
  "impact": { "kind": "point", "expr": { … }, "uncertainty_bp": 1500 },
  "required_evidence": ["taxable_result"],
  "missing_information_hints": [ … ],
  "recommended_action": "…",
  "source": {
    "citation": "Inkomstskattelagen (1999:1229) 30 kap. 5–6 §§",
    "source_version": "2025",
    "url": "https://www.riksdagen.se/…"
  },
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

**`source.citation` is mandatory.** `RuleEngine::validate` rejects a rule set
where any rule lacks one, at startup, so the binary will not run. A finding
whose provenance is "the system says so" is not something a customer can take to
their accountant.

**`review.note` states the known weakness of the rule**, in the rule. The
example above admits that it approximates the reservation base. When a
professional reviews this set, the note is the question they are being asked.

**`tax_year_from`** bounds applicability. A rule set with no version covering the
analysed year stops the analysis rather than guessing — `PipelineError::TaxYearNotCovered`.
Constants exist for 2023, 2024 and 2025 only; 2026 is deliberately absent, so an
analysis of a 2026 fiscal year fails loudly rather than applying 2025's figures.

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
  "source": "Prisbasbelopp och inkomstbasbelopp enligt SCB/regeringens
             fastställande. Bolagsskattesats enligt IL 65 kap. 10 §. Samtliga
             värden ska verifieras mot Skatteverket före produktionsbruk.",
  "amounts": { "prisbasbelopp": 5250000, … } }
```

Per tax year, with the source stated, and with the verification requirement
written into the data rather than left to a reader's assumption.

`validate()` rejects a rule referencing a rate that cannot be resolved for a
year the rule claims to cover — so a rule that would silently compute against a
missing constant fails at startup instead.

---

## 6. Impact

```json
"impact": {
  "kind": "point",
  "expr": { "op": "mul_rate", "of": { … }, "rate": "corporate_tax" },
  "uncertainty_bp": 1500
}
```

`kind: "point"` names the *expression* form, not the output. The output is
always a range: `MoneyRange::around(value, uncertainty_bp)` widens the computed
figure by the stated uncertainty. There is no path from a rule to a single
figure, because `MoneyRange` is the only money type the impact code can produce.

`uncertainty_bp` is a claim about how much the rule does not know — 1500 = ±15%.
Narrowing it is a decision a rule author has to make and a reviewer can
challenge.

`kind: "none"` is for rules that flag something worth checking without claiming
an amount. Several rules use it, and that is correct: "you have reserves that
must be returned within six years, check which year each was set aside" is
valuable and un-quantifiable from the accounts alone.

---

## 7. Validation at startup

`RuleEngine::validate()` refuses to load a rule set with:

- duplicate rule ids;
- a missing or empty `source.citation`;
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
While it holds, no finding produced by an unreviewed rule can be presented as
`identified`; the best any finding reaches is "needs verification".

This is enforced in three places, deliberately redundantly:

1. **In the pipeline**, at `status_for()`.
2. **In the golden dataset test**, which fails the build if any finding is ever
   presented as `identified` while the flag holds. A future change that removes
   the gate breaks the build rather than shipping.
3. **In the API**, where `GET /v1/rules` discloses the unreviewed count to any
   caller. The limitation is visible to a customer, not buried.

The gate is machine-enforced because a policy of "we will be careful until the
review happens" survives about one deadline.

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

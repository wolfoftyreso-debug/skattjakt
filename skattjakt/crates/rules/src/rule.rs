//! Rule definitions.
//!
//! A rule is data, not code, and not a prompt (section 10). It carries its own
//! validity window, its source, and — importantly — whether a qualified human
//! has reviewed it. An unreviewed rule can still run; it just cannot produce a
//! finding presented as established.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use skattjakt_core::{
    FactKind, InvestigationEffort, Money, MoneyRange, OpportunityCategory, RiskLevel, Urgency,
};

use crate::condition::{Condition, ProfileFlag};
use crate::expr::Expr;

/// Where a rule comes from. Section 10: no rule may be fabricated, and every
/// rule carries a source reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleSource {
    /// Primary source, e.g. "Inkomstskattelagen (1999:1229) 30 kap. 5–6 §§".
    pub citation: String,
    /// Which edition or retrieval the citation refers to.
    pub source_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// How far a citation has been checked against the thing it cites.
///
/// The distinction this enum exists to make: naming a source and having read
/// it are different, and only the second is evidence. Every rule in this set
/// cited a statute from the day it was written; not one of those citations had
/// been fetched, so "cited" meant only that somebody had typed a chapter and a
/// paragraph.
///
/// `Verified` is granted **only** by a retrieval — the worker's sweep, or
/// `skattjakt-analysis-worker verify-sources` run by hand — which fetches
/// the document and records a hash of what it read. The rule set is rejected at
/// load if anything claims `Verified` without one, so the state cannot be
/// awarded by editing a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    /// Cited, never fetched. The citation is a claim about where to look.
    Unretrieved,
    /// The fetch failed. Says nothing about the law, only about the network.
    Unreachable,
    /// Fetched, and it did **not** say what the rule assumed. The loudest of
    /// the four: either the law moved or the rule was wrong when written.
    Mismatch,
    /// Fetched, and the document, the paragraph and the operative figures were
    /// all found in it.
    Verified,
}

impl SourceState {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceState::Unretrieved => "unretrieved",
            SourceState::Unreachable => "unreachable",
            SourceState::Mismatch => "mismatch",
            SourceState::Verified => "verified",
        }
    }
}

/// What a retrieval recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retrieval {
    #[serde(default = "unretrieved")]
    pub state: SourceState,
    /// When the document was read.
    #[serde(default)]
    pub at: Option<String>,
    /// SHA-256 of the text that was read, so a later retrieval can tell that
    /// the source changed rather than only that it still verifies.
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

fn unretrieved() -> SourceState {
    SourceState::Unretrieved
}

/// One citable authority: a paragraph of statute, or a published position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub authority: String,
    pub collection: String,
    /// The identifier within the collection — an SFS number, for instance.
    pub document: String,
    pub title: String,
    /// Which part of it: `30 kap. 5 §`.
    pub locator: String,
    #[serde(default)]
    pub url: Option<String>,
    /// A machine-readable rendering of the same document, where one exists.
    #[serde(default)]
    pub machine_url: Option<String>,
    /// What the rule set assumes this source says. An assertion to be tested,
    /// never a quotation: nothing here was copied from the source, because
    /// nothing here has read it.
    pub asserted_claim: String,
    /// The operative words and figures a retrieval must find. This is what
    /// turns "the paragraph exists" into "the paragraph still says 25 per
    /// cent" — the second is what the arithmetic depends on.
    #[serde(default)]
    pub must_contain: Vec<String>,
    #[serde(default = "default_retrieval")]
    pub retrieval: Retrieval,
}

fn default_retrieval() -> Retrieval {
    Retrieval {
        state: SourceState::Unretrieved,
        at: None,
        sha256: None,
        note: None,
    }
}

impl Source {
    /// A source for a fixture: cited, never retrieved, which is the state
    /// every real source in the shipped set is also in.
    pub fn fixture(title: &str, locator: &str, claim: &str) -> Self {
        Self {
            authority: "fixture".into(),
            collection: "fixture".into(),
            document: "0000:0".into(),
            title: title.into(),
            locator: locator.into(),
            url: None,
            machine_url: None,
            asserted_claim: claim.into(),
            must_contain: Vec::new(),
            retrieval: default_retrieval(),
        }
    }

    pub fn state(&self) -> SourceState {
        self.retrieval.state
    }

    /// The citation as a reader would write it.
    ///
    /// A paragraph reference is set directly against the document —
    /// `Inkomstskattelag (1999:1229) 30 kap. 5 §` — because that is how statute
    /// is cited. Everything else is a named part of a document rather than a
    /// reference into it, and running the two together produced citations that
    /// read as a sentence fragment. Those take a dash.
    pub fn citation(&self) -> String {
        if self.locator.contains('§') || self.locator.contains("kap") {
            format!("{} {}", self.title, self.locator)
        } else {
            format!("{} — {}", self.title, self.locator)
        }
    }
}

/// Whether a qualified person has checked this rule against the current law.
///
/// This exists because the rule set in this repository was drafted by a
/// language model. Marking that state in the data — rather than in a comment
/// nobody reads — lets the engine enforce a ceiling on what unreviewed rules
/// may claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewState {
    /// Checked by a named adviser on a date.
    Reviewed { reviewer: String, date: String },
    /// Drafted but not professionally verified. The default.
    AwaitingProfessionalReview {
        #[serde(default)]
        note: String,
    },
}

impl ReviewState {
    pub fn is_reviewed(&self) -> bool {
        matches!(self, ReviewState::Reviewed { .. })
    }
}

/// A named carve-out that disqualifies an otherwise matching rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exception {
    pub id: String,
    /// When true, the rule does not apply.
    pub when: Condition,
    /// Shown to the user when the exception fires or is undecidable.
    pub explanation: String,
}

/// How the economic effect is computed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImpactSpec {
    /// No monetary effect. Risk and control findings use this.
    None,
    /// Explicit lower and upper expressions.
    ///
    /// The only shape that produces money, and deliberately the only one.
    /// There used to be a `Point` variant that took one figure and widened it
    /// by an `uncertainty_bp` band — ±15 % on one rule, ±10 % on another. Those
    /// numbers came from nowhere: nobody measured them, and no input to the
    /// calculation was known to that tolerance. The result looked like a
    /// measurement of uncertainty and was a decoration on a point estimate.
    ///
    /// Writing both bounds forces the author to say what the low end *means*.
    /// For an unused allowance it is "the company makes no allocation"; for a
    /// carried-forward loss it is "a spärr removes the deduction entirely".
    /// Both are real states of the world, which ±10 % never was.
    Range { low: Expr, high: Expr },
}

impl ImpactSpec {
    pub fn required_facts(&self) -> Vec<FactKind> {
        match self {
            ImpactSpec::None => Vec::new(),
            ImpactSpec::Range { low, high } => {
                let mut facts = low.required_facts();
                facts.extend(high.required_facts());
                facts.dedup_by_key(|f| f.key());
                facts
            }
        }
    }

    pub fn method_name(&self) -> &'static str {
        match self {
            ImpactSpec::None => "none",
            ImpactSpec::Range { .. } => "explicit_range",
        }
    }
}

/// Who a rule is about.
///
/// Swedish tax law for an aktiebolag and for a private individual are different
/// bodies of rules, and a finding from one is not merely less relevant to the
/// other — it is wrong. Periodiseringsfond does not exist for an employee; a
/// ROT deduction does not exist for a company.
///
/// This is on the rule rather than inferred from the documents because the
/// engine must be able to answer "can I analyse this kind of taxpayer at all?"
/// **before** it runs, so that nothing is sold that cannot be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Taxpayer {
    /// An aktiebolag. Every rule shipped today.
    Company,
    /// A private individual filing their own return.
    PrivateIndividual,
}

impl Taxpayer {
    /// The audience key a report for this taxpayer is served under.
    ///
    /// `Company` covers two audiences — the owner reading about their own
    /// company, and an accountant reviewing it — which is why this returns a
    /// set rather than one key. They are the same rules seen from two chairs.
    pub fn audience_keys(self) -> &'static [&'static str] {
        match self {
            Taxpayer::Company => &["company", "accountant"],
            Taxpayer::PrivateIndividual => &["private"],
        }
    }
}

/// One versioned rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// Stable identifier, e.g. `se.tax.periodiseringsfond.headroom`.
    pub rule_id: String,
    pub version: String,
    /// ISO country code. Only `SE` is served today.
    pub jurisdiction: String,

    /// Who this rule is about.
    ///
    /// Deliberately **not** defaulted. A missing value on a new rule would
    /// silently make it a company rule, and the one place that matters is the
    /// first private-individual rule somebody writes — which would then be
    /// invisible to private analyses and never fire, with nothing to say why.
    pub taxpayer: Taxpayer,

    /// First tax year the rule applies to.
    pub tax_year_from: i32,
    /// Last tax year, inclusive. `None` means "still current".
    #[serde(default)]
    pub tax_year_to: Option<i32>,

    pub title: String,
    pub description: String,
    pub category: OpportunityCategory,

    /// All must hold for the rule to match.
    pub conditions: Condition,
    #[serde(default)]
    pub exceptions: Vec<Exception>,

    pub impact: ImpactSpec,
    /// Whether the impact is tax saved or tax postponed.
    ///
    /// Defaults to `Reduction`, which is what most rules are. A rule that
    /// defers — periodiseringsfond, överavskrivning — has to say so, and the
    /// consequence is that its amount is reported under its own heading rather
    /// than added to the headline total.
    #[serde(default)]
    pub effect: skattjakt_core::opportunity::EffectKind,

    /// Facts a reviewer should expect to see cited. Used to explain what the
    /// finding rests on and what is absent.
    #[serde(default)]
    pub required_evidence: Vec<FactKind>,
    /// Things that are not facts but would settle the question, phrased for a
    /// human, e.g. "föregående års deklaration".
    #[serde(default)]
    pub missing_information_hints: Vec<String>,

    /// The next step, written as a question to put to the accountant.
    pub recommended_action: String,

    /// Keys into the rule set's source registry.
    ///
    /// A list rather than one citation, because a rule usually rests on more
    /// than one paragraph and the weakest of them is what bounds the whole
    /// rule. Previously this was a single sentence naming several paragraphs,
    /// which could not be checked one at a time and so was never checked.
    #[serde(default)]
    pub sources: Vec<String>,
    pub review: ReviewState,

    pub effort: InvestigationEffort,
    pub risk: RiskLevel,
    pub urgency: Urgency,
    /// How specific this rule is to the company as opposed to generic advice.
    /// 0.0–1.0; feeds the priority formula.
    pub relevance: f64,
}

impl Rule {
    pub fn applies_to_tax_year(&self, tax_year: i32) -> bool {
        tax_year >= self.tax_year_from && self.tax_year_to.is_none_or(|to| tax_year <= to)
    }

    /// All facts the rule touches, whether in conditions or in the calculation.
    pub fn referenced_facts(&self) -> Vec<FactKind> {
        let mut facts = self.conditions.referenced_facts();
        for exception in &self.exceptions {
            facts.extend(exception.when.referenced_facts());
        }
        facts.extend(self.impact.required_facts());
        facts.extend(self.required_evidence.iter().cloned());

        // `Vec::dedup_by_key` removes only *adjacent* duplicates, which this
        // list is not: a fact named in the conditions and again in
        // `required_evidence` has the impact's facts between them and survives.
        // The customer then sees the same balance-sheet line quoted twice under
        // "what this rests on", which reads as two pieces of evidence.
        //
        // First occurrence wins, so the order stays conditions → exceptions →
        // impact → evidence, which is the order the reasoning happened in.
        let mut seen = BTreeSet::new();
        facts.retain(|fact| seen.insert(fact.key()));
        facts
    }
}

/// The result of evaluating one rule against one company.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleEvaluation {
    pub rule_id: String,
    pub rule_version: String,
    pub title: String,
    pub category: OpportunityCategory,
    pub outcome: RuleOutcome,
    /// Present only when the rule matched and its impact could be computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact: Option<MoneyRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calculation: Option<CalculationRecord>,
    /// Facts the rule needed but did not find.
    #[serde(default)]
    pub missing_facts: Vec<FactKind>,
    /// Onboarding questions that would resolve an undecidable condition.
    #[serde(default)]
    pub unanswered_questions: Vec<ProfileFlag>,
    /// Exceptions that fired, or that could not be ruled out.
    #[serde(default)]
    pub exception_notes: Vec<String>,
    /// True when the rule has not been professionally reviewed, which caps how
    /// strongly the finding may be stated.
    pub awaiting_review: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RuleOutcome {
    /// Conditions held and no exception applied.
    Matched,
    /// Conditions did not hold. Not a finding.
    NotApplicable { reason: String },
    /// Conditions could not be decided on the available information. This is a
    /// finding — "something to investigate" — not a silent drop.
    Indeterminate { reason: String },
    /// An exception disqualifies the rule.
    ExceptionApplies {
        exception_id: String,
        explanation: String,
    },
    /// The rule does not cover this tax year.
    OutOfScope { reason: String },
    /// The rule is structurally broken — a constant it names does not exist for
    /// the year. Surfaced loudly rather than swallowed (section 31).
    RuleError { reason: String },
}

impl RuleOutcome {
    /// Whether the evaluation should produce something the user sees.
    pub fn produces_finding(&self) -> bool {
        matches!(
            self,
            RuleOutcome::Matched | RuleOutcome::Indeterminate { .. }
        )
    }
}

/// A reproducible record of one calculation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalculationRecord {
    pub method: String,
    pub inputs: Vec<CalculationInputRecord>,
    pub result_low: Money,
    pub result_high: Money,
    /// The expression as stored, so the arithmetic can be re-run verbatim.
    pub expression: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalculationInputRecord {
    pub name: String,
    pub value: Money,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(from: i32, to: Option<i32>) -> Rule {
        Rule {
            rule_id: "se.test".into(),
            version: "1".into(),
            jurisdiction: "SE".into(),
            taxpayer: Taxpayer::Company,
            tax_year_from: from,
            tax_year_to: to,
            title: "t".into(),
            description: "d".into(),
            category: OpportunityCategory::Tax,
            conditions: Condition::Always,
            exceptions: vec![],
            effect: skattjakt_core::opportunity::EffectKind::Reduction,
            impact: ImpactSpec::None,
            required_evidence: vec![],
            missing_information_hints: vec![],
            recommended_action: "a".into(),
            sources: vec!["il-30-5".into()],
            review: ReviewState::AwaitingProfessionalReview {
                note: String::new(),
            },
            effort: InvestigationEffort::Low,
            risk: RiskLevel::Low,
            urgency: Urgency::Routine,
            relevance: 1.0,
        }
    }

    #[test]
    fn open_ended_rules_apply_to_every_later_year() {
        let r = rule(2021, None);
        assert!(r.applies_to_tax_year(2021));
        assert!(r.applies_to_tax_year(2030));
        assert!(!r.applies_to_tax_year(2020));
    }

    #[test]
    fn closed_rules_stop_applying_after_their_final_year() {
        let r = rule(2019, Some(2020));
        assert!(r.applies_to_tax_year(2020));
        assert!(!r.applies_to_tax_year(2021));
    }

    #[test]
    fn unreviewed_is_the_default_review_state() {
        assert!(!rule(2021, None).review.is_reviewed());
        let reviewed = ReviewState::Reviewed {
            reviewer: "X".into(),
            date: "2026-01-01".into(),
        };
        assert!(reviewed.is_reviewed());
    }

    #[test]
    fn matched_and_indeterminate_both_produce_findings() {
        assert!(RuleOutcome::Matched.produces_finding());
        assert!(RuleOutcome::Indeterminate { reason: "r".into() }.produces_finding());
        assert!(!RuleOutcome::NotApplicable { reason: "r".into() }.produces_finding());
        assert!(!RuleOutcome::RuleError { reason: "r".into() }.produces_finding());
    }

    #[test]
    fn referenced_facts_span_conditions_impact_and_evidence() {
        let mut r = rule(2021, None);
        r.conditions = Condition::FactPresent {
            fact: FactKind::TaxableResult,
        };
        r.impact = ImpactSpec::Range {
            low: Expr::Amount { sek: 0 },
            high: Expr::Fact {
                fact: FactKind::Cash,
            },
        };
        r.required_evidence = vec![FactKind::Equity];
        let facts = r.referenced_facts();
        assert!(facts.contains(&FactKind::TaxableResult));
        assert!(facts.contains(&FactKind::Cash));
        assert!(facts.contains(&FactKind::Equity));
    }

    #[test]
    fn a_fact_named_twice_is_listed_once() {
        // The shape that got through: `taxable_result` in the conditions and
        // again in `required_evidence`, with the impact's fact between them.
        // `dedup_by_key` only removes adjacent duplicates, so both survived and
        // the finding quoted the same line from the accounts twice.
        let mut r = rule(2021, None);
        r.conditions = Condition::FactPresent {
            fact: FactKind::TaxableResult,
        };
        r.impact = ImpactSpec::Range {
            low: Expr::Amount { sek: 0 },
            high: Expr::Fact {
                fact: FactKind::Cash,
            },
        };
        r.required_evidence = vec![FactKind::TaxableResult];

        let facts = r.referenced_facts();
        assert_eq!(
            facts,
            vec![FactKind::TaxableResult, FactKind::Cash],
            "a fact named in two places must be listed once, in first-seen order"
        );
    }

    #[test]
    fn every_shipped_rule_lists_each_fact_once() {
        // The property, asserted against the rules that actually ship rather
        // than against a fixture — this is where the defect lived.
        let engine = crate::RuleEngine::load_embedded().unwrap();
        for rule in engine.rules() {
            let facts = rule.referenced_facts();
            let mut unique: Vec<_> = facts.iter().map(|f| f.key()).collect();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(
                facts.len(),
                unique.len(),
                "{} lists a fact more than once",
                rule.rule_id
            );
        }
    }

    #[test]
    fn rules_round_trip_through_json() {
        let r = rule(2021, None);
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Rule>(&json).unwrap(), r);
    }
}

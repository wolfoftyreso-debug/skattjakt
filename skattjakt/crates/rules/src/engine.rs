//! The rule engine.
//!
//! Decides whether a rule is *actually* relevant, which conditions apply, which
//! exceptions exist, which period it covers, and how the calculation is done
//! (section 9). The model proposes; this decides.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use skattjakt_core::{FactKind, Money, MoneyRange};
use thiserror::Error;

use crate::condition::{EvalContext, Truth};
use crate::expr::{EvalError, Expr, TaxYearConstants};
use crate::rule::{
    CalculationInputRecord, CalculationRecord, ImpactSpec, Rule, RuleEvaluation, RuleOutcome,
    Source, SourceState,
};

/// A versioned set of rules plus the per-year constants they reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleSet {
    /// Version of the whole set, recorded on every analysis for reproducibility.
    pub version: String,
    pub jurisdiction: String,
    #[serde(default)]
    pub constants: Vec<TaxYearConstants>,
    /// Every authority the rules and the figures cite, keyed by id.
    #[serde(default)]
    pub sources: BTreeMap<String, Source>,
    pub rules: Vec<Rule>,
}

impl RuleSet {
    pub fn source_by_id(&self, id: &str) -> Option<&Source> {
        self.sources.get(id)
    }

    /// How far a rule's sources have been checked, taken together.
    ///
    /// See [`combine`] for the rule and why it is not simply the minimum.
    pub fn source_state_of(&self, rule: &Rule) -> SourceState {
        combine(rule.sources.iter().map(|id| {
            self.sources
                .get(id)
                .map(Source::state)
                .unwrap_or(SourceState::Unretrieved)
        }))
    }
}

/// Folds several sources' states into the one a rule reports.
///
/// Two rules, and the second is the one that is easy to get wrong.
///
/// **The weakest wins.** A rule resting on two paragraphs is only as checked as
/// the less checked of them; reporting the stronger would describe the rule as
/// better established than it is.
///
/// **Except that a contradiction dominates everything.** `Mismatch` sits above
/// `Unreachable` on the ladder, because the ladder measures how far a check
/// *got*, and a mismatch got all the way. But a mismatch is not merely a
/// well-progressed check — it is positive evidence that the rule's basis is
/// wrong, and the minimum would bury it: a rule citing one contradicted
/// paragraph and one unreachable one would report `unreachable`, the gate would
/// see no contradiction, and the finding would be capped at "verify" instead of
/// dropped to "investigate". The bad news would be hidden by worse news about
/// the network.
///
/// So a contradiction is sticky. Anything else takes the weakest.
pub fn combine(states: impl IntoIterator<Item = SourceState>) -> SourceState {
    let mut weakest: Option<SourceState> = None;
    for state in states {
        if state == SourceState::Mismatch {
            return SourceState::Mismatch;
        }
        weakest = Some(match weakest {
            Some(current) => current.min(state),
            None => state,
        });
    }
    weakest.unwrap_or(SourceState::Unretrieved)
}

#[derive(Debug, Error)]
pub enum RuleSetError {
    #[error("rule set is not valid JSON: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("rule set failed validation:\n{}", .0.join("\n"))]
    Invalid(Vec<String>),
}

/// Evaluates a rule set against one company's facts and profile.
#[derive(Debug, Clone)]
pub struct RuleEngine {
    set: RuleSet,
    constants_by_year: BTreeMap<i32, TaxYearConstants>,
}

/// The rule set shipped with the binary. Held in the repository, versioned with
/// the code, and loaded without touching the filesystem so a deployment cannot
/// end up running rules nobody committed.
const EMBEDDED_RULES: &str = include_str!("../../../rules/se-ruleset.json");

impl RuleEngine {
    pub fn load_embedded() -> Result<Self, RuleSetError> {
        Self::from_json(EMBEDDED_RULES)
    }

    pub fn from_json(json: &str) -> Result<Self, RuleSetError> {
        let set: RuleSet = serde_json::from_str(json)?;
        Self::new(set)
    }

    pub fn new(set: RuleSet) -> Result<Self, RuleSetError> {
        let constants_by_year = set
            .constants
            .iter()
            .map(|c| (c.tax_year, c.clone()))
            .collect();
        let engine = Self {
            set,
            constants_by_year,
        };
        let problems = engine.validate();
        if problems.is_empty() {
            Ok(engine)
        } else {
            Err(RuleSetError::Invalid(problems))
        }
    }

    pub fn version(&self) -> &str {
        &self.set.version
    }

    pub fn rules(&self) -> &[Rule] {
        &self.set.rules
    }

    pub fn rule(&self, rule_id: &str) -> Option<&Rule> {
        self.set.rules.iter().find(|r| r.rule_id == rule_id)
    }

    pub fn constants_for(&self, tax_year: i32) -> Option<&TaxYearConstants> {
        self.constants_by_year.get(&tax_year)
    }

    /// Structural problems that make the set unsafe to run: duplicate ids,
    /// names that do not resolve, impossible year windows. Checked at load so a
    /// broken rule never reaches an analysis.
    /// The rule set this engine was built from, including its source registry.
    pub fn set(&self) -> &RuleSet {
        &self.set
    }

    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();

        for rule in &self.set.rules {
            *seen.entry(rule.rule_id.as_str()).or_insert(0) += 1;

            if let Some(to) = rule.tax_year_to {
                if to < rule.tax_year_from {
                    problems.push(format!(
                        "{}: tax_year_to ({to}) precedes tax_year_from ({})",
                        rule.rule_id, rule.tax_year_from
                    ));
                }
            }

            if rule.jurisdiction != self.set.jurisdiction {
                problems.push(format!(
                    "{}: jurisdiction {} does not match the set's {}",
                    rule.rule_id, rule.jurisdiction, self.set.jurisdiction
                ));
            }

            if !(0.0..=1.0).contains(&rule.relevance) {
                problems.push(format!(
                    "{}: relevance must be within 0.0..=1.0",
                    rule.rule_id
                ));
            }

            if rule.sources.is_empty() {
                problems.push(format!(
                    "{}: a rule must cite at least one source",
                    rule.rule_id
                ));
            }
            for id in &rule.sources {
                if !self.set.sources.contains_key(id) {
                    problems.push(format!(
                        "{}: cites the source {id:?}, which is not in the registry",
                        rule.rule_id
                    ));
                }
            }

            // Every year the rule claims to cover must have constants that
            // resolve the names it uses.
            for year in self.years_covered(rule) {
                let Some(constants) = self.constants_by_year.get(&year) else {
                    continue;
                };
                if let Err(e) = rule.conditions.validate(constants) {
                    problems.push(format!("{} ({year}): {e}", rule.rule_id));
                }
                for exception in &rule.exceptions {
                    if let Err(e) = exception.when.validate(constants) {
                        problems.push(format!(
                            "{} ({year}) exception {}: {e}",
                            rule.rule_id, exception.id
                        ));
                    }
                }
                if let Err(e) = validate_impact(&rule.impact, constants) {
                    problems.push(format!("{} ({year}) impact: {e}", rule.rule_id));
                }
            }
        }

        for (id, count) in seen {
            if count > 1 {
                problems.push(format!("duplicate rule_id `{id}` appears {count} times"));
            }
        }

        // --- the source registry -------------------------------------------
        //
        // The invariant that makes `verified` mean something: it is granted by
        // a retrieval that recorded a hash of what it read, and by nothing
        // else. Without this, the state is a word in a file — and a word in a
        // file is exactly the kind of assurance this registry exists to
        // replace.
        for (id, source) in &self.set.sources {
            if source.state() == SourceState::Verified {
                if source.retrieval.sha256.as_deref().unwrap_or("").is_empty() {
                    problems.push(format!(
                        "source `{id}` claims to be verified with no hash of what was read; \
                         only a retrieval may grant that state"
                    ));
                }
                if source.retrieval.at.as_deref().unwrap_or("").is_empty() {
                    problems.push(format!(
                        "source `{id}` claims to be verified with no retrieval timestamp"
                    ));
                }
            }
            if source.asserted_claim.trim().is_empty() {
                problems.push(format!(
                    "source `{id}` states no claim, so a retrieval has nothing to check"
                ));
            }
            if source.locator.trim().is_empty() {
                problems.push(format!("source `{id}` has no locator"));
            }
        }

        // Every figure the rules multiply by must name where it comes from.
        for constants in &self.set.constants {
            for (name, parameter) in &constants.parameters {
                if !self.set.sources.contains_key(&parameter.source) {
                    problems.push(format!(
                        "the {} parameter `{name}` cites the source `{}`, which is not in \
                         the registry",
                        constants.tax_year, parameter.source
                    ));
                }
            }
        }

        problems
    }

    fn years_covered(&self, rule: &Rule) -> Vec<i32> {
        self.constants_by_year
            .keys()
            .copied()
            .filter(|y| rule.applies_to_tax_year(*y))
            .collect()
    }

    /// Evaluates every rule in scope for the year. Rules outside the year are
    /// skipped rather than reported, to keep the output about the company.
    pub fn evaluate_all(&self, ctx: &EvalContext<'_>) -> Vec<RuleEvaluation> {
        self.set
            .rules
            .iter()
            .filter(|r| r.applies_to_tax_year(ctx.tax_year))
            .map(|r| self.evaluate(r, ctx))
            .collect()
    }

    /// Whether the engine has rules and constants for a tax year at all.
    /// Section 31 requires this to be an explicit state, not an empty result.
    pub fn covers_tax_year(&self, tax_year: i32) -> bool {
        self.constants_by_year.contains_key(&tax_year)
            && self
                .set
                .rules
                .iter()
                .any(|r| r.applies_to_tax_year(tax_year))
    }

    pub fn evaluate(&self, rule: &Rule, ctx: &EvalContext<'_>) -> RuleEvaluation {
        let awaiting_review = !rule.review.is_reviewed();
        let base = |outcome: RuleOutcome| RuleEvaluation {
            rule_id: rule.rule_id.clone(),
            rule_version: rule.version.clone(),
            title: rule.title.clone(),
            category: rule.category,
            outcome,
            impact: None,
            calculation: None,
            missing_facts: Vec::new(),
            unanswered_questions: Vec::new(),
            exception_notes: Vec::new(),
            awaiting_review,
        };

        if !rule.applies_to_tax_year(ctx.tax_year) {
            return base(RuleOutcome::OutOfScope {
                reason: format!("regeln gäller inte beskattningsår {}", ctx.tax_year),
            });
        }

        // A rule whose constants are missing for the year must not quietly
        // evaluate to "not applicable" — that would hide a stale rule set.
        if let Err(problem) = self.check_resolvable(rule, ctx.constants) {
            return base(RuleOutcome::RuleError { reason: problem });
        }

        let verdict = rule.conditions.eval(ctx);
        if verdict == Truth::False {
            return base(RuleOutcome::NotApplicable {
                reason: "villkoren för regeln är inte uppfyllda".to_string(),
            });
        }

        // Exceptions are checked even when the main conditions are undecided:
        // a definite exception disqualifies the rule regardless.
        let mut exception_notes = Vec::new();
        for exception in &rule.exceptions {
            match exception.when.eval(ctx) {
                Truth::True => {
                    let mut eval = base(RuleOutcome::ExceptionApplies {
                        exception_id: exception.id.clone(),
                        explanation: exception.explanation.clone(),
                    });
                    eval.exception_notes.push(exception.explanation.clone());
                    return eval;
                }
                Truth::Unknown => exception_notes.push(exception.explanation.clone()),
                Truth::False => {}
            }
        }

        let missing_facts = self.missing_facts_for(rule, ctx);
        let unanswered_questions = rule.conditions.unanswered_profile_questions(ctx);

        let (impact, calculation, impact_error) = self.compute_impact(rule, ctx);

        let outcome = if verdict == Truth::Unknown {
            RuleOutcome::Indeterminate {
                reason: "det saknas underlag för att avgöra om regeln är tillämplig".to_string(),
            }
        } else if let Some(err) = impact_error {
            if err.is_missing_information() {
                RuleOutcome::Indeterminate {
                    reason:
                        "regeln är tillämplig men underlaget räcker inte för att beräkna effekten"
                            .to_string(),
                }
            } else {
                RuleOutcome::RuleError {
                    reason: err.to_string(),
                }
            }
        } else if !exception_notes.is_empty() {
            RuleOutcome::Indeterminate {
                reason: "ett undantag kan inte uteslutas på tillgängligt underlag".to_string(),
            }
        } else {
            RuleOutcome::Matched
        };

        RuleEvaluation {
            rule_id: rule.rule_id.clone(),
            rule_version: rule.version.clone(),
            title: rule.title.clone(),
            category: rule.category,
            outcome,
            impact,
            calculation,
            missing_facts,
            unanswered_questions,
            exception_notes,
            awaiting_review,
        }
    }

    fn check_resolvable(&self, rule: &Rule, constants: &TaxYearConstants) -> Result<(), String> {
        rule.conditions
            .validate(constants)
            .map_err(|e| e.to_string())?;
        validate_impact(&rule.impact, constants).map_err(|e| e.to_string())
    }

    fn missing_facts_for(&self, rule: &Rule, ctx: &EvalContext<'_>) -> Vec<FactKind> {
        let mut needed = rule.impact.required_facts();
        needed.extend(rule.required_evidence.iter().cloned());
        needed.dedup_by_key(|f| f.key());
        ctx.facts.missing_among(&needed)
    }

    fn compute_impact(
        &self,
        rule: &Rule,
        ctx: &EvalContext<'_>,
    ) -> (
        Option<MoneyRange>,
        Option<CalculationRecord>,
        Option<EvalError>,
    ) {
        match &rule.impact {
            ImpactSpec::None => (None, None, None),

            ImpactSpec::Point {
                expr,
                uncertainty_bp,
            } => match expr.eval(ctx.facts, ctx.constants) {
                Ok(value) => match MoneyRange::around(value, *uncertainty_bp) {
                    Ok(range) => {
                        let record = self.record(expr, "point_with_uncertainty", ctx, range);
                        (Some(range), Some(record), None)
                    }
                    Err(_) => (None, None, Some(EvalError::Overflow("uncertainty".into()))),
                },
                Err(e) => (None, None, Some(e)),
            },

            ImpactSpec::Range { low, high } => {
                let lo = low.eval(ctx.facts, ctx.constants);
                let hi = high.eval(ctx.facts, ctx.constants);
                match (lo, hi) {
                    (Ok(a), Ok(b)) => {
                        let range = MoneyRange::new(a, b);
                        let record = self.record(low, "explicit_range", ctx, range);
                        (Some(range), Some(record), None)
                    }
                    (Err(e), _) | (_, Err(e)) => (None, None, Some(e)),
                }
            }
        }
    }

    /// Captures the inputs that went into a calculation so it can be re-run and
    /// so the evidence card can show its working.
    fn record(
        &self,
        expr: &Expr,
        method: &str,
        ctx: &EvalContext<'_>,
        range: MoneyRange,
    ) -> CalculationRecord {
        let inputs = expr
            .referenced_facts()
            .into_iter()
            .map(|kind| CalculationInputRecord {
                name: kind.key(),
                value: ctx.facts.value(&kind).unwrap_or(Money::ZERO),
            })
            .collect();

        CalculationRecord {
            method: method.to_string(),
            inputs,
            result_low: range.low,
            result_high: range.high,
            expression: serde_json::to_value(expr).unwrap_or(serde_json::Value::Null),
        }
    }
}

fn validate_impact(impact: &ImpactSpec, constants: &TaxYearConstants) -> Result<(), EvalError> {
    match impact {
        ImpactSpec::None => Ok(()),
        ImpactSpec::Point { expr, .. } => crate::condition::validate_expr(expr, constants),
        ImpactSpec::Range { low, high } => {
            crate::condition::validate_expr(low, constants)?;
            crate::condition::validate_expr(high, constants)
        }
    }
}

/// Convenience: a context builder used by the pipeline and by tests.
pub fn context<'a>(
    facts: &'a skattjakt_core::FactSet,
    profile: &'a skattjakt_core::CompanyProfile,
    constants: &'a TaxYearConstants,
    tax_year: i32,
    accounts_state: skattjakt_core::document::AccountsState,
) -> EvalContext<'a> {
    EvalContext {
        facts,
        profile,
        constants,
        tax_year,
        accounts_state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::condition::{CmpOp, Condition};
    use crate::rule::{Exception, Retrieval, ReviewState};
    use skattjakt_core::document::AccountsState;
    use skattjakt_core::{
        CompanyId, CompanyProfile, DocumentVersionId, FactSet, FinancialFact, FinancialFactId,
        FiscalYear, InvestigationEffort, OpportunityCategory, OrgNumber, RiskLevel, UnitInterval,
        Urgency,
    };

    fn constants(year: i32) -> TaxYearConstants {
        TaxYearConstants {
            tax_year: year,
            ..Default::default()
        }
        .with_rate("corporate_tax", 2060, "il-65-10")
        .with_amount("prisbasbelopp", 5_880_000, "sfb-2-6")
    }

    /// The registry the fixtures cite. Every id used above appears here, for
    /// the same reason the real set is validated: a citation to nothing is
    /// what this whole change exists to make impossible.
    fn sources() -> BTreeMap<String, Source> {
        [
            (
                "il-65-10",
                "Inkomstskattelag (1999:1229)",
                "65 kap. 10 §",
                "Statlig inkomstskatt för juridiska personer är 20,6 procent.",
            ),
            (
                "il-30-5",
                "Inkomstskattelag (1999:1229)",
                "30 kap. 5 §",
                "Avdrag för avsättning till periodiseringsfond med högst 25 procent.",
            ),
            (
                "sfb-2-6",
                "Socialförsäkringsbalk (2010:110)",
                "2 kap. 6 §",
                "Prisbasbeloppet fastställs av regeringen.",
            ),
        ]
        .into_iter()
        .map(|(id, title, locator, claim)| (id.to_string(), Source::fixture(title, locator, claim)))
        .collect()
    }

    fn profile() -> CompanyProfile {
        CompanyProfile {
            id: CompanyId::new(),
            name: "Testbolaget AB".into(),
            org_number: OrgNumber::parse("556016-0680").unwrap(),
            fiscal_year: FiscalYear::calendar(2025).unwrap(),
            industry: None,
            sni_code: None,
            employee_count: None,
            owner_count: None,
            in_group: Some(false),
            operations_outside_sweden: Some(false),
            does_development_work: Some(false),
            owns_premises: Some(false),
            has_vehicles: Some(false),
            owners_active_in_company: Some(true),
        }
    }

    fn facts(pairs: &[(FactKind, i64)]) -> FactSet {
        FactSet::from_facts(pairs.iter().map(|(kind, sek)| FinancialFact {
            id: FinancialFactId::new(),
            company_id: CompanyId::new(),
            document_version_id: DocumentVersionId::new(),
            period: FiscalYear::calendar(2025).unwrap(),
            kind: kind.clone(),
            value: Money::from_sek(*sek).unwrap(),
            currency: "SEK".into(),
            account: None,
            source_page: Some(1),
            source_text: Some("x".into()),
            extraction_confidence: UnitInterval::ONE,
        }))
    }

    fn test_rule() -> Rule {
        Rule {
            rule_id: "se.test.headroom".into(),
            version: "2025.1".into(),
            jurisdiction: "SE".into(),
            tax_year_from: 2021,
            tax_year_to: None,
            title: "Testregel".into(),
            description: "d".into(),
            category: OpportunityCategory::Tax,
            conditions: Condition::Compare {
                left: Expr::Fact {
                    fact: FactKind::TaxableResult,
                },
                op: CmpOp::Gt,
                right: Expr::Amount { sek: 0 },
            },
            exceptions: vec![],
            impact: ImpactSpec::Point {
                expr: Expr::MulRate {
                    of: Box::new(Expr::MulBp {
                        of: Box::new(Expr::Fact {
                            fact: FactKind::TaxableResult,
                        }),
                        bp: 2500,
                    }),
                    rate: "corporate_tax".into(),
                },
                uncertainty_bp: 1000,
            },
            required_evidence: vec![FactKind::TaxableResult],
            missing_information_hints: vec![],
            recommended_action: "Fråga redovisningskonsulten.".into(),
            sources: vec!["il-30-5".into()],
            review: ReviewState::AwaitingProfessionalReview {
                note: String::new(),
            },
            effort: InvestigationEffort::Low,
            risk: RiskLevel::Low,
            urgency: Urgency::BeforeFiling,
            relevance: 0.9,
        }
    }

    fn engine_with(rules: Vec<Rule>) -> RuleEngine {
        RuleEngine::new(RuleSet {
            version: "test.1".into(),
            jurisdiction: "SE".into(),
            constants: vec![constants(2025)],
            sources: sources(),
            rules,
        })
        .expect("test rule set should be valid")
    }

    fn ctx<'a>(f: &'a FactSet, p: &'a CompanyProfile, c: &'a TaxYearConstants) -> EvalContext<'a> {
        context(f, p, c, 2025, AccountsState::Preliminary)
    }

    #[test]
    fn a_matching_rule_computes_its_impact() {
        let engine = engine_with(vec![test_rule()]);
        let (f, p, c) = (
            facts(&[(FactKind::TaxableResult, 1_000_000)]),
            profile(),
            constants(2025),
        );
        let eval = engine.evaluate(&engine.rules()[0], &ctx(&f, &p, &c));

        assert_eq!(eval.outcome, RuleOutcome::Matched);
        // 25 % of 1 000 000 = 250 000, taxed at 20.6 % = 51 500, ±10 %.
        let impact = eval.impact.expect("impact should be computed");
        assert_eq!(impact.low, Money::from_sek(46_350).unwrap());
        assert_eq!(impact.high, Money::from_sek(56_650).unwrap());
        assert!(eval.calculation.is_some());
        assert!(eval.awaiting_review);
    }

    #[test]
    fn a_failing_condition_is_not_applicable_rather_than_a_finding() {
        let engine = engine_with(vec![test_rule()]);
        let (f, p, c) = (
            facts(&[(FactKind::TaxableResult, -5_000)]),
            profile(),
            constants(2025),
        );
        let eval = engine.evaluate(&engine.rules()[0], &ctx(&f, &p, &c));
        assert!(matches!(eval.outcome, RuleOutcome::NotApplicable { .. }));
        assert!(!eval.outcome.produces_finding());
    }

    #[test]
    fn a_missing_fact_yields_indeterminate_and_names_what_is_missing() {
        let engine = engine_with(vec![test_rule()]);
        let (f, p, c) = (facts(&[]), profile(), constants(2025));
        let eval = engine.evaluate(&engine.rules()[0], &ctx(&f, &p, &c));
        assert!(matches!(eval.outcome, RuleOutcome::Indeterminate { .. }));
        assert!(
            eval.outcome.produces_finding(),
            "a gap is still worth surfacing"
        );
        assert!(eval.missing_facts.contains(&FactKind::TaxableResult));
    }

    #[test]
    fn a_definite_exception_disqualifies_the_rule() {
        let mut rule = test_rule();
        rule.exceptions.push(Exception {
            id: "koncern".into(),
            when: Condition::Profile {
                flag: crate::condition::ProfileFlag::InGroup,
                is: true,
            },
            explanation: "Koncernbidrag kan påverka bedömningen.".into(),
        });
        let engine = engine_with(vec![rule]);
        let p = CompanyProfile {
            in_group: Some(true),
            ..profile()
        };
        let (f, c) = (
            facts(&[(FactKind::TaxableResult, 1_000_000)]),
            constants(2025),
        );
        let eval = engine.evaluate(&engine.rules()[0], &ctx(&f, &p, &c));
        assert!(matches!(eval.outcome, RuleOutcome::ExceptionApplies { .. }));
        assert!(
            eval.impact.is_none(),
            "a disqualified rule must not report money"
        );
    }

    #[test]
    fn an_undecidable_exception_downgrades_a_match_to_indeterminate() {
        let mut rule = test_rule();
        rule.exceptions.push(Exception {
            id: "koncern".into(),
            when: Condition::Profile {
                flag: crate::condition::ProfileFlag::InGroup,
                is: true,
            },
            explanation: "Koncernförhållande är inte utrett.".into(),
        });
        let engine = engine_with(vec![rule]);
        let p = CompanyProfile {
            in_group: None,
            ..profile()
        };
        let (f, c) = (
            facts(&[(FactKind::TaxableResult, 1_000_000)]),
            constants(2025),
        );
        let eval = engine.evaluate(&engine.rules()[0], &ctx(&f, &p, &c));
        assert!(matches!(eval.outcome, RuleOutcome::Indeterminate { .. }));
        assert!(!eval.exception_notes.is_empty());
    }

    #[test]
    fn a_rule_outside_its_year_window_is_out_of_scope() {
        let mut rule = test_rule();
        rule.tax_year_to = Some(2022);
        let engine = engine_with(vec![rule]);
        let (f, p, c) = (
            facts(&[(FactKind::TaxableResult, 1)]),
            profile(),
            constants(2025),
        );
        let eval = engine.evaluate(&engine.rules()[0], &ctx(&f, &p, &c));
        assert!(matches!(eval.outcome, RuleOutcome::OutOfScope { .. }));
    }

    #[test]
    fn evaluate_all_skips_rules_outside_the_year() {
        let mut old = test_rule();
        old.rule_id = "se.test.old".into();
        old.tax_year_to = Some(2022);
        let engine = engine_with(vec![test_rule(), old]);
        let (f, p, c) = (
            facts(&[(FactKind::TaxableResult, 1_000)]),
            profile(),
            constants(2025),
        );
        let evals = engine.evaluate_all(&ctx(&f, &p, &c));
        assert_eq!(evals.len(), 1);
        assert_eq!(evals[0].rule_id, "se.test.headroom");
    }

    #[test]
    fn a_missing_constant_for_the_year_is_a_rule_error_not_a_silent_pass() {
        let engine = engine_with(vec![test_rule()]);
        // A year the constants table does not cover.
        let empty = TaxYearConstants {
            tax_year: 2030,
            ..Default::default()
        };
        let (f, p) = (facts(&[(FactKind::TaxableResult, 1_000_000)]), profile());
        let context = context(&f, &p, &empty, 2030, AccountsState::Preliminary);
        let eval = engine.evaluate(&engine.rules()[0], &context);
        assert!(matches!(eval.outcome, RuleOutcome::RuleError { .. }));
        assert!(!eval.outcome.produces_finding());
    }

    #[test]
    fn covers_tax_year_reports_gaps_in_the_rule_set() {
        let engine = engine_with(vec![test_rule()]);
        assert!(engine.covers_tax_year(2025));
        assert!(!engine.covers_tax_year(2030), "no constants for 2030");
    }

    #[test]
    fn duplicate_rule_ids_are_rejected_at_load() {
        let err = RuleEngine::new(RuleSet {
            version: "v".into(),
            jurisdiction: "SE".into(),
            constants: vec![constants(2025)],
            sources: sources(),
            rules: vec![test_rule(), test_rule()],
        })
        .unwrap_err();
        assert!(err.to_string().contains("duplicate rule_id"));
    }

    #[test]
    fn a_rule_without_a_citation_is_rejected_at_load() {
        let mut rule = test_rule();
        rule.sources.clear();
        let err = RuleEngine::new(RuleSet {
            version: "v".into(),
            jurisdiction: "SE".into(),
            constants: vec![constants(2025)],
            sources: sources(),
            rules: vec![rule],
        })
        .unwrap_err();
        assert!(err.to_string().contains("cite at least one source"));
    }

    #[test]
    fn a_rule_citing_a_source_that_does_not_exist_is_rejected_at_load() {
        // The failure the old free-text citation could not have: a string
        // naming a paragraph was always "valid", whatever it named.
        let mut rule = test_rule();
        rule.sources = vec!["il-99-99".into()];
        let err = RuleEngine::new(RuleSet {
            version: "v".into(),
            jurisdiction: "SE".into(),
            constants: vec![constants(2025)],
            sources: sources(),
            rules: vec![rule],
        })
        .unwrap_err();
        assert!(err.to_string().contains("not in the registry"), "{err}");
    }

    /// The invariant the whole registry rests on.
    #[test]
    fn a_source_cannot_claim_to_be_verified_without_a_retrieval() {
        let mut registry = sources();
        let forged = registry.get_mut("il-30-5").unwrap();
        forged.retrieval.state = SourceState::Verified;

        let err = RuleEngine::new(RuleSet {
            version: "v".into(),
            jurisdiction: "SE".into(),
            constants: vec![constants(2025)],
            sources: registry,
            rules: vec![test_rule()],
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("no hash of what was read"),
            "editing a file must not be able to grant the verified state: {err}"
        );
    }

    #[test]
    fn a_parameter_citing_an_unknown_source_is_rejected_at_load() {
        // Every figure the arithmetic multiplies by has to name where it came
        // from, and the name has to resolve.
        let constants = constants(2025).with_rate("invented", 1234, "does-not-exist");
        let err = RuleEngine::new(RuleSet {
            version: "v".into(),
            jurisdiction: "SE".into(),
            constants: vec![constants],
            sources: sources(),
            rules: vec![test_rule()],
        })
        .unwrap_err();
        assert!(err.to_string().contains("not in the registry"), "{err}");
    }

    #[test]
    fn a_contradiction_is_not_buried_by_a_network_failure() {
        // The shape that got through review and was caught by running it: a
        // rule citing one paragraph that contradicts it and one that could not
        // be fetched. Taking the minimum reports `unreachable`, the gate sees
        // no contradiction, and the finding is capped at "verify" instead of
        // being dropped to "investigate" — the bad news hidden by worse news
        // about the network.
        assert_eq!(
            combine([SourceState::Mismatch, SourceState::Unreachable]),
            SourceState::Mismatch
        );
        assert_eq!(
            combine([SourceState::Unretrieved, SourceState::Mismatch]),
            SourceState::Mismatch
        );
        assert_eq!(
            combine([SourceState::Verified, SourceState::Mismatch]),
            SourceState::Mismatch
        );
    }

    #[test]
    fn without_a_contradiction_the_weakest_source_decides() {
        assert_eq!(
            combine([SourceState::Verified, SourceState::Unretrieved]),
            SourceState::Unretrieved
        );
        assert_eq!(
            combine([SourceState::Verified, SourceState::Unreachable]),
            SourceState::Unreachable
        );
        assert_eq!(
            combine([SourceState::Verified, SourceState::Verified]),
            SourceState::Verified
        );
    }

    #[test]
    fn a_rule_citing_nothing_is_unchecked_rather_than_perfect() {
        // `min()` of an empty iterator is None, and the tempting default for a
        // "how good is this" fold is the best value. Validation rejects a rule
        // with no sources, so this is unreachable in a loaded set — asserted
        // anyway, because the day validation changes this is the failure that
        // would silently promote every rule.
        assert_eq!(combine([]), SourceState::Unretrieved);
    }

    #[test]
    fn a_rules_source_state_is_the_weakest_of_its_sources() {
        let mut registry = sources();
        registry.get_mut("il-30-5").unwrap().retrieval = Retrieval {
            state: SourceState::Verified,
            at: Some("2026-08-12T00:00:00+00:00".into()),
            sha256: Some("a".repeat(64)),
            note: None,
        };
        let mut rule = test_rule();
        rule.sources = vec!["il-30-5".into(), "il-65-10".into()];

        let set = RuleSet {
            version: "v".into(),
            jurisdiction: "SE".into(),
            constants: vec![constants(2025)],
            sources: registry,
            rules: vec![rule.clone()],
        };
        // One verified, one never retrieved: the rule is as checked as the
        // less checked of them, not as the better one.
        assert_eq!(set.source_state_of(&rule), SourceState::Unretrieved);
    }

    #[test]
    fn a_rule_naming_an_unknown_rate_is_rejected_at_load() {
        let mut rule = test_rule();
        rule.impact = ImpactSpec::Point {
            expr: Expr::MulRate {
                of: Box::new(Expr::Fact {
                    fact: FactKind::TaxableResult,
                }),
                rate: "does_not_exist".into(),
            },
            uncertainty_bp: 0,
        };
        let err = RuleEngine::new(RuleSet {
            version: "v".into(),
            jurisdiction: "SE".into(),
            constants: vec![constants(2025)],
            sources: sources(),
            rules: vec![rule],
        })
        .unwrap_err();
        assert!(err.to_string().contains("unknown rate"));
    }

    #[test]
    fn an_inverted_year_window_is_rejected_at_load() {
        let mut rule = test_rule();
        rule.tax_year_from = 2025;
        rule.tax_year_to = Some(2021);
        let err = RuleEngine::new(RuleSet {
            version: "v".into(),
            jurisdiction: "SE".into(),
            constants: vec![constants(2025)],
            sources: sources(),
            rules: vec![rule],
        })
        .unwrap_err();
        assert!(err.to_string().contains("precedes"));
    }

    #[test]
    fn the_calculation_record_captures_inputs_for_reproduction() {
        let engine = engine_with(vec![test_rule()]);
        let (f, p, c) = (
            facts(&[(FactKind::TaxableResult, 1_000_000)]),
            profile(),
            constants(2025),
        );
        let eval = engine.evaluate(&engine.rules()[0], &ctx(&f, &p, &c));
        let calc = eval.calculation.unwrap();
        assert_eq!(calc.inputs.len(), 1);
        assert_eq!(calc.inputs[0].name, "taxable_result");
        assert_eq!(calc.inputs[0].value, Money::from_sek(1_000_000).unwrap());
        assert!(
            calc.expression.is_object(),
            "the expression must be stored verbatim"
        );
    }
}

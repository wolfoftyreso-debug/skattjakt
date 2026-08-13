//! # skattjakt-rules
//!
//! The rule engine. Rules live here as versioned data with source citations,
//! never inside a model prompt (section 10), and every calculation they perform
//! is deterministic arithmetic over extracted facts (section 9).
//!
//! The engine answers four questions the model is not allowed to answer:
//! whether a rule is genuinely relevant, which conditions and exceptions apply,
//! which period it covers, and how the figure is computed.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod condition;
pub mod engine;
pub mod expr;
pub mod rule;
pub mod verify;

pub use condition::{CmpOp, Condition, EvalContext, ProfileFlag, ProfileNumber, Truth};
pub use engine::{context, RuleEngine, RuleSet, RuleSetError};
pub use expr::{EvalError, Expr, Parameter, ParameterKind, TaxYearConstants};
pub use rule::{
    CalculationInputRecord, CalculationRecord, Exception, ImpactSpec, Retrieval, ReviewState, Rule,
    RuleEvaluation, RuleOutcome, RuleSource, Source, SourceState,
};
pub use verify::CheckOutcome;

#[cfg(test)]
mod embedded_tests {
    use super::*;

    #[test]
    fn the_shipped_rule_set_loads_and_validates() {
        let engine = RuleEngine::load_embedded().expect("embedded rule set must be valid");
        assert!(!engine.rules().is_empty());
        assert_eq!(engine.version(), "se-2025.1");
    }

    #[test]
    fn every_shipped_rule_cites_a_source_that_exists() {
        let engine = RuleEngine::load_embedded().unwrap();
        let set = engine.set();
        for rule in engine.rules() {
            assert!(!rule.sources.is_empty(), "{} cites nothing", rule.rule_id);
            for id in &rule.sources {
                let source = set
                    .source_by_id(id)
                    .unwrap_or_else(|| panic!("{} cites the unknown source {id}", rule.rule_id));
                assert!(!source.locator.trim().is_empty(), "{id} has no locator");
                assert!(
                    !source.asserted_claim.trim().is_empty(),
                    "{id} states no claim, so a retrieval has nothing to check"
                );
            }
        }
    }

    #[test]
    fn every_shipped_figure_cites_a_source_that_exists() {
        // The change that matters more than the rule citations: twelve numbers
        // per year used to share one sentence describing where all of them
        // came from. These are the values the arithmetic multiplies by.
        let engine = RuleEngine::load_embedded().unwrap();
        let set = engine.set();
        for constants in &set.constants {
            assert!(
                !constants.parameters.is_empty(),
                "{} has no parameters",
                constants.tax_year
            );
            for (name, parameter) in &constants.parameters {
                assert!(
                    set.source_by_id(&parameter.source).is_some(),
                    "the {} parameter {name} cites the unknown source {}",
                    constants.tax_year,
                    parameter.source
                );
            }
        }
    }

    /// The invariant that makes the whole registry worth having.
    #[test]
    fn nothing_in_the_shipped_set_claims_to_be_verified() {
        // Not a permanent property — it is what a retrieval is supposed to
        // change. It holds today because no source has ever been fetched, and
        // asserting it here means the day one is, this test fails and somebody
        // has to look at what changed rather than at a green build.
        let engine = RuleEngine::load_embedded().unwrap();
        for (id, source) in &engine.set().sources {
            match source.state() {
                SourceState::Verified => panic!(
                    "{id} claims to be verified. If a retrieval actually \
                     retrieved it, update this test and the documents that say the rule \
                     set is unsourced — deliberately, together."
                ),
                SourceState::Mismatch => panic!(
                    "{id} was retrieved and contradicted the rule set: {:?}",
                    source.retrieval.note
                ),
                SourceState::Unretrieved | SourceState::Unreachable => {}
            }
        }
    }

    #[test]
    fn every_shipped_rule_states_its_review_status() {
        // The rule set in this repository was drafted, not professionally
        // reviewed. The engine must be able to see that.
        let engine = RuleEngine::load_embedded().unwrap();
        for rule in engine.rules() {
            if let ReviewState::AwaitingProfessionalReview { note } = &rule.review {
                assert!(
                    !note.trim().is_empty(),
                    "{} must explain what is unverified",
                    rule.rule_id
                );
            }
        }
    }

    #[test]
    fn every_shipped_rule_offers_a_next_step() {
        let engine = RuleEngine::load_embedded().unwrap();
        for rule in engine.rules() {
            assert!(
                rule.recommended_action.len() > 20,
                "{} needs a usable recommended action",
                rule.rule_id
            );
        }
    }

    #[test]
    fn rules_that_compute_money_declare_the_facts_they_need() {
        let engine = RuleEngine::load_embedded().unwrap();
        for rule in engine.rules() {
            if !matches!(rule.impact, ImpactSpec::None) {
                assert!(
                    !rule.impact.required_facts().is_empty(),
                    "{} computes an amount from no facts",
                    rule.rule_id
                );
            }
        }
    }

    #[test]
    fn the_rule_set_covers_the_years_it_declares_constants_for() {
        let engine = RuleEngine::load_embedded().unwrap();
        for year in [2023, 2024, 2025] {
            assert!(engine.covers_tax_year(year), "no coverage for {year}");
        }
    }

    #[test]
    fn an_unsupported_tax_year_is_reported_rather_than_silently_empty() {
        // 2026 constants are deliberately absent: the figures were not
        // verifiable when this set was written, and guessing them would be
        // worse than declining to cover the year.
        let engine = RuleEngine::load_embedded().unwrap();
        assert!(!engine.covers_tax_year(2026));
    }

    #[test]
    fn all_categories_in_the_product_spec_have_at_least_one_rule() {
        use skattjakt_core::OpportunityCategory::*;
        let engine = RuleEngine::load_embedded().unwrap();
        for category in [
            Tax,
            Costs,
            Vat,
            Personnel,
            Investments,
            ResearchAndDevelopment,
            Risk,
        ] {
            assert!(
                engine.rules().iter().any(|r| r.category == category),
                "no rule covers {category:?}"
            );
        }
    }
}

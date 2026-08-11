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

pub use condition::{CmpOp, Condition, EvalContext, ProfileFlag, ProfileNumber, Truth};
pub use engine::{context, RuleEngine, RuleSet, RuleSetError};
pub use expr::{EvalError, Expr, TaxYearConstants};
pub use rule::{
    CalculationInputRecord, CalculationRecord, Exception, ImpactSpec, ReviewState, Rule,
    RuleEvaluation, RuleOutcome, RuleSource,
};

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
    fn every_shipped_rule_cites_a_source() {
        let engine = RuleEngine::load_embedded().unwrap();
        for rule in engine.rules() {
            assert!(
                !rule.source.citation.trim().is_empty(),
                "{} has no citation",
                rule.rule_id
            );
            assert!(
                !rule.source.source_version.trim().is_empty(),
                "{} has no source version",
                rule.rule_id
            );
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

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
    RuleEvaluation, RuleOutcome, RuleSource, Source, SourceState, Taxpayer,
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

#[cfg(test)]
mod shipped_set_invariants {
    /// Whether the condition tree requires this fact to be present at all.
    ///
    /// Walks the tree instead of matching the serialised text: field order in a
    /// serialisation is not something a test should depend on, and the first
    /// version of this one passed for the wrong reason.
    fn requires_fact(node: &serde_json::Value, fact: &str) -> bool {
        match node {
            serde_json::Value::Object(map) => {
                if map.get("type").and_then(|v| v.as_str()) == Some("fact_present")
                    && map.get("fact").and_then(|v| v.as_str()) == Some(fact)
                {
                    return true;
                }
                map.values().any(|v| requires_fact(v, fact))
            }
            serde_json::Value::Array(items) => items.iter().any(|v| requires_fact(v, fact)),
            _ => false,
        }
    }

    /// Every `fact_or_zero` anywhere in an expression tree.
    fn collect_fact_or_zero(node: &serde_json::Value, out: &mut Vec<String>) {
        match node {
            serde_json::Value::Object(map) => {
                if map.get("op").and_then(|v| v.as_str()) == Some("fact_or_zero") {
                    if let Some(f) = map.get("fact").and_then(|v| v.as_str()) {
                        out.push(f.to_string());
                    }
                }
                for v in map.values() {
                    collect_fact_or_zero(v, out);
                }
            }
            serde_json::Value::Array(items) => {
                for v in items {
                    collect_fact_or_zero(v, out);
                }
            }
            _ => {}
        }
    }

    use crate::rule::ImpactSpec;
    use crate::RuleEngine;

    /// Every bound a rule produces is written, not derived from a band.
    ///
    /// The `Point` variant is gone from the type, so this cannot regress by
    /// accident — but the set is data, and data is where a "temporary" ±10 %
    /// would come back. Asserting on the loaded set says it in the one place a
    /// rule author will see it.
    #[test]
    fn no_rule_states_an_amount_it_did_not_compute_both_ends_of() {
        let engine = RuleEngine::load_embedded().unwrap();
        for rule in engine.set().rules.iter() {
            match &rule.impact {
                ImpactSpec::None => {}
                ImpactSpec::Range { .. } => {}
            }
        }
    }

    /// A rule may not subtract a value it could not read.
    ///
    /// `fact_or_zero` is right for a line that only exists when it happened —
    /// an allocation to a periodiseringsfond, a group contribution. It is wrong
    /// for a line every complete statement carries, because absence there means
    /// the document was not fully read, and reading it as zero makes every
    /// subtraction from it larger. The depreciation rule did exactly that: the
    /// same company reported 0–70 040 kr with its depreciation line and
    /// 0–148 320 kr without it.
    ///
    /// `FactKind::absence_means_zero` carries the distinction; this holds the
    /// shipped rule set to it, because the rule set is data and data is where
    /// the next `fact_or_zero` will be written.
    #[test]
    fn no_rule_treats_an_unreadable_mandatory_line_as_zero() {
        let engine = RuleEngine::load_embedded().unwrap();
        for rule in engine.set().rules.iter() {
            let impact = serde_json::to_value(&rule.impact).unwrap();
            let conditions = serde_json::to_value(&rule.conditions).unwrap();
            let mut lenient = Vec::new();
            collect_fact_or_zero(&impact, &mut lenient);
            collect_fact_or_zero(&conditions, &mut lenient);

            for fact in lenient {
                let kind: skattjakt_core::FactKind =
                    serde_json::from_value(serde_json::Value::String(fact.clone())).unwrap();
                if kind.absence_means_zero() {
                    continue;
                }
                assert!(
                    requires_fact(&conditions, &fact),
                    "{} reads {fact} with fact_or_zero without requiring it; a document \
                     that did not yield {fact} would make the finding bigger, not absent",
                    rule.rule_id
                );
            }
        }
    }

    /// A rule that postpones tax says so.
    ///
    /// Periodiseringsfond is the case: 20,6 % of the unused headroom is real
    /// money, and it is money paid in a later year with a schablonintäkt
    /// charged in the meantime. Adding it to the same figure as a missed
    /// deduction answered a question nobody asked.
    #[test]
    fn the_rules_that_defer_tax_are_marked_as_deferrals() {
        use skattjakt_core::opportunity::EffectKind;
        let engine = RuleEngine::load_embedded().unwrap();
        for rule in engine.set().rules.iter() {
            let defers = rule.rule_id.contains("periodiseringsfond");
            assert_eq!(
                rule.effect == EffectKind::Deferral,
                defers,
                "{} is marked {:?}",
                rule.rule_id,
                rule.effect
            );
        }
    }

    /// The 30 % huvudregel is applied to equipment, never to the heading.
    ///
    /// `FixedAssets` is *Materiella anläggningstillgångar*: buildings, land and
    /// equipment together. The rule that reads it and multiplies by 30 % is the
    /// one that invented 630 360 kr for a company that owned its premises.
    #[test]
    fn the_depreciation_rule_reads_equipment_and_not_the_heading() {
        use skattjakt_core::FactKind;
        let engine = RuleEngine::load_embedded().unwrap();
        let rule = engine
            .set()
            .rules
            .iter()
            .find(|r| r.rule_id == "se.investments.inventarier.avskrivningsutrymme")
            .expect("the shipped set contains the depreciation rule");
        let facts = rule.referenced_facts();
        assert!(
            facts.contains(&FactKind::Equipment),
            "it must read the equipment line"
        );
        assert!(
            !facts.contains(&FactKind::FixedAssets),
            "and must not read the heading, which carries buildings and land"
        );
    }

    /// The pension frame is a share of cash pay, not of the personnel heading.
    #[test]
    fn the_pension_rule_reads_wages_and_not_total_personnel_cost() {
        use skattjakt_core::FactKind;
        let engine = RuleEngine::load_embedded().unwrap();
        let rule = engine
            .set()
            .rules
            .iter()
            .find(|r| r.rule_id == "se.personnel.pension.avdragsutrymme")
            .expect("the shipped set contains the pension rule");
        let facts = rule.referenced_facts();
        assert!(facts.contains(&FactKind::Wages));
        assert!(
            !facts.contains(&FactKind::PersonnelCosts),
            "personnel costs carry employer's contributions and would raise the \
             threshold by about a third"
        );
    }

    /// A spärr after an ownership change reaches a company that is not in a group.
    #[test]
    fn the_loss_rule_guards_on_ownership_change_and_not_only_on_group_membership() {
        let engine = RuleEngine::load_embedded().unwrap();
        let rule = engine
            .set()
            .rules
            .iter()
            .find(|r| r.rule_id == "se.tax.underskott.kvittning")
            .expect("the shipped set contains the loss rule");
        let json = serde_json::to_string(&rule.exceptions).unwrap();
        assert!(
            json.contains("ownership_changed"),
            "beloppsspärren följer av ägarförändring, inte av koncerntillhörighet"
        );
        assert!(
            json.contains("in_group"),
            "group membership is still one route"
        );
    }
}

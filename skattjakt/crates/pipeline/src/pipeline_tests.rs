use std::sync::Arc;

use serde_json::json;
use skattjakt_core::document::AccountsState;
use skattjakt_core::{
    AnalysisId, CompanyId, CompanyProfile, FiscalYear, OpportunityCategory, OpportunityStatus,
    OrgNumber,
};
use skattjakt_extract::{ExtractedDocument, Page, Scale};
use skattjakt_model::{ReasoningTask, ScriptedProvider};
use skattjakt_rules::RuleEngine;

use super::*;

const INCOME_STATEMENT: &str = "\
RESULTATRÄKNING
Nettoomsättning                12 500 000
Personalkostnader              -5 800 000
Av- och nedskrivningar           -450 000
Rörelseresultat                 3 200 000
Räntekostnader                    -85 000
Resultat efter finansiella poster 3 115 000
Skattemässigt resultat          3 000 000
Materiella anläggningstillgångar 1 800 000
Summa tillgångar                7 720 000
Summa eget kapital och skulder  7 720 000
";

fn profile() -> CompanyProfile {
    CompanyProfile {
        id: CompanyId::new(),
        name: "Testbolaget AB".into(),
        org_number: OrgNumber::parse("556016-0680").unwrap(),
        fiscal_year: FiscalYear::calendar(2025).unwrap(),
        industry: Some("Konsult".into()),
        sni_code: None,
        employee_count: Some(8),
        owner_count: Some(2),
        in_group: Some(false),
        operations_outside_sweden: Some(false),
        does_development_work: Some(false),
        owns_premises: Some(false),
        has_vehicles: Some(false),
        owners_active_in_company: Some(true),
    }
}

fn document(text: &str) -> DocumentInput {
    DocumentInput {
        document_id: skattjakt_core::DocumentId::new(),
        document_version_id: skattjakt_core::DocumentVersionId::new(),
        extracted: ExtractedDocument {
            pages: vec![Page {
                number: 1,
                text: text.to_string(),
            }],
            unreadable_pages: vec![],
            scale: Scale::Kronor,
        },
    }
}

fn input(documents: Vec<DocumentInput>) -> AnalysisInput {
    AnalysisInput {
        analysis_id: AnalysisId::new(),
        company: profile(),
        documents,
        accounts_state: AccountsState::Preliminary,
    }
}

fn pipeline(provider: ScriptedProvider) -> AnalysisPipeline {
    AnalysisPipeline::new(
        Arc::new(RuleEngine::load_embedded().unwrap()),
        Arc::new(provider),
        PipelineConfig::default(),
    )
}

/// A provider that returns nothing from either pass.
fn silent_provider() -> ScriptedProvider {
    ScriptedProvider::new()
        .with(
            ReasoningTask::OpportunityDiscovery,
            json!({"candidates": []}),
        )
        .with(ReasoningTask::ContradictionCheck, json!({"verdicts": []}))
}

#[tokio::test]
async fn an_analysis_with_no_documents_is_rejected() {
    let result = pipeline(silent_provider())
        .run(&input(vec![]), &SilentObserver)
        .await;
    assert!(matches!(result, Err(PipelineError::NoDocuments)));
}

#[tokio::test]
async fn an_uncovered_tax_year_fails_loudly_rather_than_returning_nothing() {
    // Section 31: a rule set with no version for the year must say so.
    let mut analysis = input(vec![document(INCOME_STATEMENT)]);
    analysis.company.fiscal_year = FiscalYear::calendar(2030).unwrap();

    let result = pipeline(silent_provider())
        .run(&analysis, &SilentObserver)
        .await;
    assert!(matches!(
        result,
        Err(PipelineError::TaxYearNotCovered(2030))
    ));
}

#[tokio::test]
async fn the_rule_engine_produces_findings_without_any_model_candidates() {
    let (result, _) = pipeline(silent_provider())
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();

    assert!(
        !result.opportunities.is_empty(),
        "a taxable surplus should trigger at least one rule"
    );
    assert!(result
        .opportunities
        .iter()
        .any(|o| o.title.contains("Periodiseringsfond")));
}

#[tokio::test]
async fn an_unreviewed_rule_can_never_reach_identified() {
    // The review gate. Every rule shipped in this repository is unreviewed, so
    // nothing may be presented as established.
    let (result, _) = pipeline(silent_provider())
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();

    for opportunity in &result.opportunities {
        assert_ne!(
            opportunity.status,
            OpportunityStatus::Identified,
            "{} was presented as established despite an unreviewed rule",
            opportunity.title
        );
    }
}

#[tokio::test]
async fn every_presented_finding_carries_a_document_value_and_a_rule() {
    let (result, _) = pipeline(silent_provider())
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();

    for opportunity in result
        .opportunities
        .iter()
        .filter(|o| !o.rule_ids.is_empty())
    {
        assert!(
            opportunity.evidence.has_document_anchor(),
            "{} has no document value behind it",
            opportunity.title
        );
        assert!(
            opportunity.evidence.has_rule(),
            "{} cites no rule",
            opportunity.title
        );
        assert!(!opportunity.evidence.is_model_only());
    }
}

#[tokio::test]
async fn a_computed_finding_reports_a_range_never_a_single_figure() {
    let (result, _) = pipeline(silent_provider())
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();

    let with_money: Vec<_> = result
        .opportunities
        .iter()
        .filter(|o| !o.impact.is_zero())
        .collect();
    assert!(
        !with_money.is_empty(),
        "expected at least one quantified finding"
    );
    for opportunity in with_money {
        assert!(
            opportunity.impact.low < opportunity.impact.high,
            "{} reported a point estimate",
            opportunity.title
        );
        assert!(opportunity.evidence.has_calculation());
    }
}

#[tokio::test]
async fn the_skeptic_can_remove_a_finding_entirely() {
    let provider = ScriptedProvider::new()
        .with(
            ReasoningTask::OpportunityDiscovery,
            json!({"candidates": []}),
        )
        .with(
            ReasoningTask::ContradictionCheck,
            json!({"verdicts": [{
                "title": "Periodiseringsfond",
                "survives": false,
                "reasoning": "Avsättning har redan gjorts enligt not 3.",
                "objection_strength": 0.9
            }]}),
        );

    let (result, _) = pipeline(provider)
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();

    assert!(
        !result
            .opportunities
            .iter()
            .any(|o| o.title == "Periodiseringsfond"),
        "a refuted finding must not be presented"
    );
    let rejected = result
        .rejected
        .iter()
        .find(|o| o.title == "Periodiseringsfond")
        .expect("the rejection should be retained for the audit trail");
    assert!(rejected.rejection_reason.is_some());
}

#[tokio::test]
async fn a_surviving_objection_lowers_confidence_without_removing_the_finding() {
    let strong = pipeline(silent_provider())
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap()
        .0;

    let provider = ScriptedProvider::new()
        .with(
            ReasoningTask::OpportunityDiscovery,
            json!({"candidates": []}),
        )
        .with(
            ReasoningTask::ContradictionCheck,
            json!({"verdicts": [{
                "title": "Periodiseringsfond",
                "survives": true,
                "reasoning": "Underlaget är preliminärt.",
                "objection_strength": 0.4
            }]}),
        );
    let doubted = pipeline(provider)
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap()
        .0;

    let score = |result: &skattjakt_core::analysis::AnalysisResult| {
        result
            .opportunities
            .iter()
            .find(|o| o.title == "Periodiseringsfond")
            .map(|o| o.confidence.score)
            .expect("finding should be present")
    };

    assert!(score(&doubted) < score(&strong));
}

#[tokio::test]
async fn a_model_candidate_with_no_rule_behind_it_is_never_actionable() {
    let provider = ScriptedProvider::new()
        .with(
            ReasoningTask::OpportunityDiscovery,
            json!({"candidates": [{
                "title": "Ovanligt hög konsultkostnad",
                "category": "costs",
                "observation": "Externa kostnader ser höga ut i förhållande till omsättningen.",
                "question": "Fråga din redovisningskonsult vad posten innehåller.",
                "supporting_facts": ["external_costs"],
                "missing_information": ["Kontospecifikation"],
                "suggested_rule_ids": []
            }]}),
        )
        .with(ReasoningTask::ContradictionCheck, json!({"verdicts": []}));

    let (result, _) = pipeline(provider)
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();

    let finding = result
        .opportunities
        .iter()
        .find(|o| o.title == "Ovanligt hög konsultkostnad")
        .expect("the candidate should still be surfaced as a question");

    assert_eq!(finding.status, OpportunityStatus::Investigate);
    assert!(
        finding.impact.is_zero(),
        "no rule, no calculation, therefore no figure"
    );
    assert!(
        !finding.confidence.is_actionable(),
        "a model-only finding must not be actionable"
    );
    assert!(finding.rule_ids.is_empty());
}

#[tokio::test]
async fn a_model_failure_degrades_to_a_rules_only_analysis() {
    // The provider has no scripted responses at all, so both passes fail.
    let (result, runs) = pipeline(ScriptedProvider::new())
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();

    assert!(
        !result.opportunities.is_empty(),
        "rule-based findings should survive a model outage"
    );
    assert!(runs
        .iter()
        .any(|r| r.status == skattjakt_model::ModelRunStatus::Failed));
    assert!(runs
        .iter()
        .all(|r| r.error.is_some() || r.output != serde_json::Value::Null));
}

#[tokio::test]
async fn model_corroboration_raises_confidence_but_cannot_carry_a_finding_alone() {
    let corroborating = ScriptedProvider::new()
        .with(
            ReasoningTask::OpportunityDiscovery,
            json!({"candidates": [{
                "title": "Periodiseringsfond",
                "category": "tax",
                "observation": "Skattemässigt överskott utan avsättning.",
                "question": "Bör avsättning göras?",
                "supporting_facts": ["taxable_result"],
                "missing_information": [],
                "suggested_rule_ids": ["se.tax.periodiseringsfond.outnyttjat_utrymme"]
            }]}),
        )
        .with(ReasoningTask::ContradictionCheck, json!({"verdicts": []}));

    let (with_model, _) = pipeline(corroborating)
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();
    let (without_model, _) = pipeline(silent_provider())
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();

    let score = |r: &skattjakt_core::analysis::AnalysisResult| {
        r.opportunities
            .iter()
            .find(|o| o.title == "Periodiseringsfond")
            .unwrap()
            .confidence
            .score
    };
    assert!(score(&with_model) >= score(&without_model));

    // Corroboration must not have created a duplicate finding.
    assert_eq!(
        with_model
            .opportunities
            .iter()
            .filter(|o| o.title == "Periodiseringsfond")
            .count(),
        1
    );
}

#[tokio::test]
async fn a_document_with_nothing_to_find_is_a_designed_result_not_an_empty_page() {
    // Section 32: report what was checked, even when nothing was found.
    let (result, _) = pipeline(silent_provider())
        .run(
            &input(vec![document(
                "RESULTATRÄKNING\nNettoomsättning    100 000\n",
            )]),
            &SilentObserver,
        )
        .await
        .unwrap();

    assert!(!result.covered_areas.is_empty());
    assert!(result.covered_areas.iter().any(|a| a.rules_evaluated > 0));
    assert!(!result.limitations.is_empty());
    assert!(!result.disclaimer.is_empty());
}

#[tokio::test]
async fn an_unbalanced_balance_sheet_becomes_a_warning_finding() {
    let text = "\
BALANSRÄKNING
Summa tillgångar                7 720 000
Summa eget kapital och skulder  7 000 000
";
    let (result, _) = pipeline(silent_provider())
        .run(&input(vec![document(text)]), &SilentObserver)
        .await
        .unwrap();

    let finding = result
        .opportunities
        .iter()
        .find(|o| o.category == OpportunityCategory::Risk)
        .expect("an unbalanced balance sheet is a risk finding");
    assert_eq!(finding.status, OpportunityStatus::Warning);
    assert!(finding.impact.is_zero(), "a risk finding carries no money");
}

#[tokio::test]
async fn conflicting_values_across_documents_produce_a_warning() {
    let documents = vec![
        document("Nettoomsättning    12 500 000"),
        document("Nettoomsättning     9 000 000"),
    ];
    let (result, _) = pipeline(silent_provider())
        .run(&input(documents), &SilentObserver)
        .await
        .unwrap();

    assert!(result
        .warnings
        .iter()
        .any(|w| w.code == "conflicting_values"));
}

#[tokio::test]
async fn an_unreadable_page_is_reported_rather_than_ignored() {
    let mut doc = document(INCOME_STATEMENT);
    doc.extracted.pages.push(Page {
        number: 2,
        text: String::new(),
    });
    doc.extracted.unreadable_pages.push(2);

    let (result, _) = pipeline(silent_provider())
        .run(&input(vec![doc]), &SilentObserver)
        .await
        .unwrap();

    assert!(result.warnings.iter().any(|w| w.code == "unreadable_page"));
}

#[tokio::test]
async fn an_unanswered_profile_question_becomes_a_request_for_information() {
    let mut analysis = input(vec![document(INCOME_STATEMENT)]);
    analysis.company.in_group = None;

    let (result, _) = pipeline(silent_provider())
        .run(&analysis, &SilentObserver)
        .await
        .unwrap();

    assert!(
        result
            .missing_information
            .iter()
            .any(|m| m.description.contains("koncern")),
        "an undecidable condition should surface as a question to answer"
    );
}

#[tokio::test]
async fn model_runs_are_recorded_without_any_reasoning_trace() {
    let (_, runs) = pipeline(silent_provider())
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();

    assert_eq!(
        runs.len(),
        2,
        "discovery and skeptic should both be recorded"
    );
    for run in &runs {
        assert!(!run.prompt_version.is_empty());
        assert!(!run.document_version_ids.is_empty());
        let serialised = serde_json::to_string(run).unwrap();
        assert!(!serialised.contains("thinking"));
        assert!(!serialised.contains("chain_of_thought"));
    }
}

#[tokio::test]
async fn the_headline_total_only_counts_findings_the_system_stands_behind() {
    let (result, _) = pipeline(silent_provider())
        .run(&input(vec![document(INCOME_STATEMENT)]), &SilentObserver)
        .await
        .unwrap();

    let expected = result
        .opportunities
        .iter()
        .filter(|o| o.status.is_presented())
        .map(|o| o.countable_impact())
        .fold(skattjakt_core::MoneyRange::ZERO, |acc, r| {
            acc.checked_add(r).unwrap()
        });

    assert_eq!(result.summary.estimated_total, expected);
}

#[tokio::test]
async fn stages_are_reported_in_order() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<AnalysisStage>>);
    impl StageObserver for Recorder {
        fn stage(&self, stage: AnalysisStage) {
            self.0.lock().unwrap().push(stage);
        }
    }

    let recorder = Recorder::default();
    pipeline(silent_provider())
        .run(&input(vec![document(INCOME_STATEMENT)]), &recorder)
        .await
        .unwrap();

    let seen = recorder.0.lock().unwrap().clone();
    assert_eq!(seen.first(), Some(&AnalysisStage::ReadingDocuments));
    assert_eq!(seen.last(), Some(&AnalysisStage::Done));
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(seen, sorted, "stages must not go backwards");
}

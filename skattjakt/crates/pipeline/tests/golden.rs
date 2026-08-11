//! The golden dataset harness (section 29).
//!
//! Ten synthetic Swedish companies with expected findings, run through the
//! whole pipeline with a scripted model so the measurement is of *the
//! pipeline*, not of a model's mood on the day.
//!
//! Section 30 sets the goal: precision over recall. A missed opportunity costs
//! the user nothing they had; a fabricated one costs them trust and an hour of
//! their accountant's time. The thresholds below are set accordingly, and the
//! critical-error checks are absolute — any one of them fails the suite.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use skattjakt_core::analysis::AnalysisResult;
use skattjakt_core::document::AccountsState;
use skattjakt_core::{
    AnalysisId, CompanyId, CompanyProfile, FiscalYear, OpportunityStatus, OrgNumber,
};
use skattjakt_extract::{ExtractedDocument, Page, Scale};
use skattjakt_model::{ReasoningTask, ScriptedProvider};
use skattjakt_pipeline::pipeline::{AnalysisPipeline, PipelineConfig, SilentObserver};
use skattjakt_pipeline::{AnalysisInput, DocumentInput};
use skattjakt_rules::RuleEngine;

#[derive(Debug, Deserialize)]
struct GoldenCase {
    id: String,
    name: String,
    description: String,
    profile: GoldenProfile,
    documents: Vec<String>,
    accounts_state: String,
    model_candidates: serde_json::Value,
    model_verdicts: serde_json::Value,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
struct GoldenProfile {
    name: String,
    org_number: String,
    fiscal_year: i32,
    industry: String,
    employee_count: u32,
    owner_count: u32,
    in_group: bool,
    operations_outside_sweden: bool,
    does_development_work: bool,
    owns_premises: bool,
    has_vehicles: bool,
    owners_active_in_company: bool,
}

#[derive(Debug, Deserialize)]
struct Expected {
    must_find_rule_ids: Vec<String>,
    must_not_find_rule_ids: Vec<String>,
    expect_no_findings: bool,
    #[serde(default)]
    expect_warning_codes: Vec<String>,
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden")
        .canonicalize()
        .expect("the golden dataset directory must exist")
}

fn load_cases() -> Vec<GoldenCase> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(golden_dir())
        .expect("golden dataset is readable")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();

    paths
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path).expect("case file is readable");
            serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} is not a valid case: {e}", path.display()))
        })
        .collect()
}

async fn run_case(case: &GoldenCase) -> AnalysisResult {
    let provider = ScriptedProvider::new()
        .with(
            ReasoningTask::OpportunityDiscovery,
            case.model_candidates.clone(),
        )
        .with(
            ReasoningTask::ContradictionCheck,
            case.model_verdicts.clone(),
        );

    let pipeline = AnalysisPipeline::new(
        Arc::new(RuleEngine::load_embedded().expect("rule set loads")),
        Arc::new(skattjakt_gateway::ModelGateway::for_testing(Arc::new(
            provider,
        ))),
        PipelineConfig::default(),
    );

    let documents: Vec<DocumentInput> = case
        .documents
        .iter()
        .map(|text| {
            let pages = vec![Page {
                number: 1,
                text: text.clone(),
            }];
            let unreadable = pages
                .iter()
                .filter(|p| p.is_empty())
                .map(|p| p.number)
                .collect();
            DocumentInput {
                document_id: skattjakt_core::DocumentId::new(),
                document_version_id: skattjakt_core::DocumentVersionId::new(),
                extracted: ExtractedDocument {
                    pages,
                    unreadable_pages: unreadable,
                    scale: Scale::Kronor,
                },
            }
        })
        .collect();

    let input = AnalysisInput {
        analysis_id: AnalysisId::new(),
        company: CompanyProfile {
            id: CompanyId::new(),
            name: case.profile.name.clone(),
            org_number: OrgNumber::parse(&case.profile.org_number).expect("valid org number"),
            fiscal_year: FiscalYear::calendar(case.profile.fiscal_year).expect("valid year"),
            industry: Some(case.profile.industry.clone()),
            sni_code: None,
            employee_count: Some(case.profile.employee_count),
            owner_count: Some(case.profile.owner_count),
            in_group: Some(case.profile.in_group),
            operations_outside_sweden: Some(case.profile.operations_outside_sweden),
            does_development_work: Some(case.profile.does_development_work),
            owns_premises: Some(case.profile.owns_premises),
            has_vehicles: Some(case.profile.has_vehicles),
            owners_active_in_company: Some(case.profile.owners_active_in_company),
        },
        documents,
        accounts_state: match case.accounts_state.as_str() {
            "final" => AccountsState::Final,
            "unknown" => AccountsState::Unknown,
            _ => AccountsState::Preliminary,
        },
    };

    pipeline
        .run(&input, &SilentObserver)
        .await
        .unwrap_or_else(|e| panic!("{} failed to analyse: {e}", case.id))
        .0
}

#[derive(Debug, Default)]
struct Metrics {
    true_positives: usize,
    false_positives: usize,
    false_negatives: usize,
    critical_errors: Vec<String>,
}

impl Metrics {
    fn precision(&self) -> f64 {
        let denominator = self.true_positives + self.false_positives;
        if denominator == 0 {
            1.0
        } else {
            self.true_positives as f64 / denominator as f64
        }
    }

    fn recall(&self) -> f64 {
        let denominator = self.true_positives + self.false_negatives;
        if denominator == 0 {
            1.0
        } else {
            self.true_positives as f64 / denominator as f64
        }
    }
}

/// Invariants that must hold for every finding in every case. A breach here is
/// a critical error, not a scoring miss.
fn check_critical(case: &GoldenCase, result: &AnalysisResult, metrics: &mut Metrics) {
    let mut fail = |message: String| {
        metrics
            .critical_errors
            .push(format!("{}: {message}", case.id))
    };

    for opportunity in &result.opportunities {
        if opportunity.status == OpportunityStatus::Identified {
            fail(format!(
                "\"{}\" was presented as established, but no rule in this set has been reviewed",
                opportunity.title
            ));
        }

        if opportunity.confidence.is_actionable()
            && opportunity.evidence.validate_actionable().is_err()
        {
            fail(format!(
                "\"{}\" is actionable without a document value and a rule",
                opportunity.title
            ));
        }

        if opportunity.evidence.is_model_only() && opportunity.confidence.is_actionable() {
            fail(format!(
                "\"{}\" rests on the model alone",
                opportunity.title
            ));
        }

        if !opportunity.impact.is_zero() && !opportunity.evidence.has_calculation() {
            fail(format!(
                "\"{}\" reports money with no calculation behind it",
                opportunity.title
            ));
        }

        if !opportunity.impact.is_zero() && opportunity.impact.low == opportunity.impact.high {
            fail(format!(
                "\"{}\" reports a point estimate, not a range",
                opportunity.title
            ));
        }

        if opportunity.recommended_action.trim().is_empty() {
            fail(format!("\"{}\" offers no next step", opportunity.title));
        }
    }

    // A rejected finding must never reach the presented list or the total.
    for rejected in &result.rejected {
        if result.opportunities.iter().any(|o| o.id == rejected.id) {
            fail(format!(
                "\"{}\" was both rejected and presented",
                rejected.title
            ));
        }
    }

    if result.disclaimer != skattjakt_core::DISCLAIMER_SV {
        fail("the disclaimer is missing or altered".to_string());
    }
}

#[tokio::test]
async fn the_golden_dataset_meets_its_quality_targets() {
    let cases = load_cases();
    assert_eq!(cases.len(), 10, "section 29 requires ten cases");

    let mut metrics = Metrics::default();
    let mut report = String::new();

    for case in &cases {
        let result = run_case(case).await;
        let found: Vec<String> = result
            .opportunities
            .iter()
            .flat_map(|o| o.rule_ids.clone())
            .collect();

        let mut case_missing = Vec::new();
        for expected in &case.expected.must_find_rule_ids {
            if found.contains(expected) {
                metrics.true_positives += 1;
            } else {
                metrics.false_negatives += 1;
                case_missing.push(expected.clone());
            }
        }

        let mut case_spurious = Vec::new();
        for forbidden in &case.expected.must_not_find_rule_ids {
            if found.contains(forbidden) {
                metrics.false_positives += 1;
                case_spurious.push(forbidden.clone());
            }
        }

        if case.expected.expect_no_findings && !result.opportunities.is_empty() {
            metrics.critical_errors.push(format!(
                "{}: expected nothing to be found, got {}",
                case.id,
                result
                    .opportunities
                    .iter()
                    .map(|o| o.title.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        for code in &case.expected.expect_warning_codes {
            if !result.warnings.iter().any(|w| &w.code == code) {
                metrics.critical_errors.push(format!(
                    "{}: expected a `{code}` warning and got none",
                    case.id
                ));
            }
        }

        check_critical(case, &result, &mut metrics);

        report.push_str(&format!(
            "{} {:<42} findings {:>2}  hög {:>2}  {}\n",
            case.id,
            case.name,
            result.summary.identified_opportunities,
            result.summary.high_priority_count,
            result.summary.estimated_total,
        ));
        if !case_missing.is_empty() {
            report.push_str(&format!("    saknade: {}\n", case_missing.join(", ")));
        }
        if !case_spurious.is_empty() {
            report.push_str(&format!("    felaktiga: {}\n", case_spurious.join(", ")));
        }
    }

    println!("\n{report}");
    println!(
        "true positives {}  false positives {}  false negatives {}\nprecision {:.3}  recall {:.3}",
        metrics.true_positives,
        metrics.false_positives,
        metrics.false_negatives,
        metrics.precision(),
        metrics.recall()
    );

    assert!(
        metrics.critical_errors.is_empty(),
        "critical errors:\n{}",
        metrics.critical_errors.join("\n")
    );
    assert_eq!(
        metrics.false_positives, 0,
        "a rule fired on a case where it must not; precision is the priority (section 30)"
    );
    assert!(
        metrics.precision() >= 0.95,
        "precision {:.3} is below the 0.95 target",
        metrics.precision()
    );
    assert!(
        metrics.recall() >= 0.80,
        "recall {:.3} is below the 0.80 target",
        metrics.recall()
    );
}

#[tokio::test]
async fn every_case_is_reproducible() {
    // Section 35: the same inputs must produce the same analysis. Anything
    // that varies between runs — an unstable sort, a map iteration order —
    // shows up here.
    for case in load_cases() {
        let first = run_case(&case).await;
        let second = run_case(&case).await;

        let titles = |r: &AnalysisResult| {
            r.opportunities
                .iter()
                .map(|o| o.title.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            titles(&first),
            titles(&second),
            "{} is not reproducible",
            case.id
        );
        assert_eq!(
            first.summary.estimated_total, second.summary.estimated_total,
            "{} produced a different total on a second run",
            case.id
        );
    }
}

#[tokio::test]
async fn a_case_with_nothing_to_find_still_explains_what_was_checked() {
    // Section 32: "found nothing" is a designed state.
    let cases = load_cases();
    let case = cases
        .iter()
        .find(|c| c.expected.expect_no_findings)
        .expect("the dataset must include a case with nothing to find");

    let result = run_case(case).await;
    assert!(result.summary.found_nothing);
    assert!(
        result.covered_areas.iter().any(|a| a.rules_evaluated > 0),
        "the user must be told which areas were checked"
    );
    assert!(!result.limitations.is_empty());
    assert_eq!(
        result.summary.estimated_total,
        skattjakt_core::MoneyRange::ZERO
    );
}

#[test]
fn every_case_documents_what_it_is_testing() {
    for case in load_cases() {
        assert!(!case.name.trim().is_empty(), "{} has no name", case.id);
        assert!(
            case.description.len() > 30,
            "{} does not explain what it exercises",
            case.id
        );
        assert!(
            !case.expected.must_not_find_rule_ids.is_empty() || case.expected.expect_no_findings,
            "{} asserts nothing about false positives",
            case.id
        );
    }
}

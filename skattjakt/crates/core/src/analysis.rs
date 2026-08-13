//! Analysis jobs and their results.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{AnalysisId, CompanyId, DocumentVersionId};
use crate::money::MoneyRange;
use crate::opportunity::{Opportunity, OpportunityStatus, PriorityBand};

/// The stages shown to the user while an analysis runs (section 3, step 3).
/// The enum is the single source of truth for progress: the UI renders it and
/// the orchestrator advances it, so the two cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisStage {
    Queued,
    ReadingDocuments,
    UnderstandingStructure,
    IdentifyingAreas,
    SearchingOpportunities,
    CheckingRules,
    Falsifying,
    VerifyingCalculations,
    Prioritising,
    Done,
}

impl AnalysisStage {
    pub fn label_sv(self) -> &'static str {
        match self {
            AnalysisStage::Queued => "I kö",
            AnalysisStage::ReadingDocuments => "Läser bokslutet",
            AnalysisStage::UnderstandingStructure => "Förstår företagets ekonomiska struktur",
            AnalysisStage::IdentifyingAreas => "Identifierar relevanta skattemässiga områden",
            AnalysisStage::SearchingOpportunities => "Söker efter möjliga möjligheter",
            AnalysisStage::CheckingRules => "Kontrollerar regler och villkor",
            AnalysisStage::Falsifying => "Försöker falsifiera upptäckterna",
            AnalysisStage::VerifyingCalculations => "Verifierar beräkningar",
            AnalysisStage::Prioritising => "Prioriterar resultat",
            AnalysisStage::Done => "Klar",
        }
    }

    /// Fraction complete, for the progress indicator.
    pub fn progress(self) -> f32 {
        let all = Self::ordered();
        let index = all.iter().position(|s| *s == self).unwrap_or(0);
        index as f32 / (all.len() - 1) as f32
    }

    pub fn ordered() -> [AnalysisStage; 10] {
        [
            AnalysisStage::Queued,
            AnalysisStage::ReadingDocuments,
            AnalysisStage::UnderstandingStructure,
            AnalysisStage::IdentifyingAreas,
            AnalysisStage::SearchingOpportunities,
            AnalysisStage::CheckingRules,
            AnalysisStage::Falsifying,
            AnalysisStage::VerifyingCalculations,
            AnalysisStage::Prioritising,
            AnalysisStage::Done,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisStatus {
    Pending,
    Running,
    Succeeded,
    /// The run stopped. `AnalysisJob::error` says why, in terms the user can act on.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisJob {
    pub id: AnalysisId,
    pub company_id: CompanyId,
    /// The exact document versions this analysis read. Pinned at creation so a
    /// later upload cannot change what a finished analysis was based on.
    pub document_version_ids: Vec<DocumentVersionId>,
    pub status: AnalysisStatus,
    pub stage: AnalysisStage,
    /// Version of the rule set used, for reproducibility (section 35).
    pub rule_set_version: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Something that should be looked at but is not an opportunity: a missing
/// document, an unreadable page, an inconsistency between statements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A gap in the input, phrased as what would improve the analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissingInformation {
    pub code: String,
    pub description: String,
    /// What it would unlock, so the ask is justified rather than bureaucratic.
    pub unlocks: String,
}

/// An area the analysis covered. Shown even when nothing was found, because
/// "we looked here and found nothing" is itself a result (section 32).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoveredArea {
    pub category: String,
    pub rules_evaluated: usize,
    pub findings: usize,
}

/// A rule that was evaluated against real values and did not fire.
///
/// The distinction this type exists to protect: "we checked this and it does
/// not apply to you" and "we could not see enough to check this" are different
/// answers, and only the first is worth showing as a green line. A rule whose
/// trigger fact was never found in the documents is *not* cleared — it is
/// unchecked, and it belongs in `missing_information` instead.
///
/// Nothing consumed this before. For an accounting assistant reviewing a
/// closing, the checks that passed are half the value: a review that lists only
/// problems cannot be distinguished from a review that did not run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClearedCheck {
    pub rule_id: String,
    pub title: String,
    pub category: String,
    /// Why it does not apply, in the rule's own words.
    pub reason: String,
}

/// Something the system cannot determine, stated plainly (section 28, part 9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limitation {
    pub statement: String,
}

/// The complete result of an analysis, matching section 23.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub analysis_id: AnalysisId,
    pub company_id: CompanyId,
    pub summary: AnalysisSummary,
    pub opportunities: Vec<Opportunity>,
    pub warnings: Vec<Warning>,
    pub missing_information: Vec<MissingInformation>,
    pub covered_areas: Vec<CoveredArea>,
    /// Rules that were evaluated against values from the documents and did not
    /// fire. Empty for an analysis where nothing could be read.
    #[serde(default)]
    pub cleared: Vec<ClearedCheck>,
    pub recommended_actions: Vec<String>,
    pub limitations: Vec<Limitation>,
    /// Findings the skeptic pass removed. Not shown by default, kept for audit.
    #[serde(default)]
    pub rejected: Vec<Opportunity>,
    pub disclaimer: String,
    pub generated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisSummary {
    pub identified_opportunities: usize,
    pub high_priority_count: usize,
    pub needs_investigation_count: usize,
    pub missing_information_count: usize,
    pub warnings_count: usize,
    pub estimated_total: MoneyRange,
    /// True when nothing cleared the bar. Drives the "found nothing" screen,
    /// which is a designed state rather than an empty list.
    pub found_nothing: bool,
}

impl AnalysisResult {
    /// Derives the summary from the findings, so the headline numbers cannot
    /// disagree with the cards below them.
    ///
    /// The parameter list is long because every section of the report is built
    /// by the pipeline and assembled here. Grouping them into a struct would
    /// only move the same fields behind one more name.
    #[allow(clippy::too_many_arguments)]
    pub fn summarise(
        analysis_id: AnalysisId,
        company_id: CompanyId,
        opportunities: Vec<Opportunity>,
        warnings: Vec<Warning>,
        missing_information: Vec<MissingInformation>,
        covered_areas: Vec<CoveredArea>,
        cleared: Vec<ClearedCheck>,
        limitations: Vec<Limitation>,
        rejected: Vec<Opportunity>,
    ) -> Self {
        let presented: Vec<&Opportunity> = opportunities
            .iter()
            .filter(|o| o.status.is_presented())
            .collect();

        let high_priority_count = presented
            .iter()
            .filter(|o| o.priority.band == PriorityBand::High)
            .count();
        let needs_investigation_count = presented
            .iter()
            .filter(|o| o.priority.band == PriorityBand::ShouldInvestigate)
            .count();
        let needs_more_evidence = presented
            .iter()
            .filter(|o| o.priority.band == PriorityBand::NeedsMoreEvidence)
            .count();

        let estimated_total = presented
            .iter()
            .map(|o| o.countable_impact())
            .try_fold(MoneyRange::ZERO, |acc, r| acc.checked_add(r))
            // An overflow here means the inputs are nonsense; reporting zero is
            // safer than reporting a wrapped number.
            .unwrap_or(MoneyRange::ZERO);

        let recommended_actions = Self::derive_actions(&presented);

        let summary = AnalysisSummary {
            identified_opportunities: presented.len(),
            high_priority_count,
            needs_investigation_count,
            missing_information_count: needs_more_evidence,
            warnings_count: warnings.len(),
            estimated_total,
            found_nothing: presented.is_empty(),
        };

        Self {
            analysis_id,
            company_id,
            summary,
            opportunities,
            warnings,
            missing_information,
            covered_areas,
            cleared,
            recommended_actions,
            limitations,
            rejected,
            disclaimer: crate::DISCLAIMER_SV.to_string(),
            generated_at: Utc::now(),
        }
    }

    /// The three questions to take to the accountant (section 34). Ordered by
    /// priority and capped, because a list of twenty "next steps" is not a next
    /// step.
    fn derive_actions(presented: &[&Opportunity]) -> Vec<String> {
        let mut ranked: Vec<&&Opportunity> = presented
            .iter()
            .filter(|o| o.status != OpportunityStatus::Rejected && o.confidence.is_actionable())
            .collect();
        ranked.sort_by(|a, b| {
            b.priority
                .score
                .partial_cmp(&a.priority.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ranked
            .iter()
            .take(3)
            .map(|o| o.recommended_action.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confidence::{Confidence, ConfidenceBand};
    use crate::evidence::EvidenceChain;
    use crate::ids::OpportunityId;
    use crate::money::Money;
    use crate::opportunity::{
        InvestigationEffort, OpportunityCategory, Priority, RiskLevel, Urgency,
    };

    fn opp(
        band: PriorityBand,
        status: OpportunityStatus,
        score: u8,
        low: i64,
        high: i64,
    ) -> Opportunity {
        let conf_band = if score >= 90 {
            ConfidenceBand::Strong
        } else if score >= 75 {
            ConfidenceBand::Good
        } else if score >= 50 {
            ConfidenceBand::Investigate
        } else {
            ConfidenceBand::NotActionable
        };
        Opportunity {
            id: OpportunityId::new(),
            analysis_id: AnalysisId::new(),
            category: OpportunityCategory::Tax,
            status,
            title: format!("finding {low}"),
            rationale: "r".into(),
            impact: MoneyRange::new(
                Money::from_sek(low).unwrap(),
                Money::from_sek(high).unwrap(),
            ),
            evidence: EvidenceChain::new(),
            missing_information: vec![],
            recommended_action: format!("action {low}"),
            confidence: Confidence {
                score,
                band: conf_band,
            },
            risk: RiskLevel::Low,
            effort: InvestigationEffort::Low,
            urgency: Urgency::Routine,
            priority: Priority {
                score: low as f64 / 1_000_000.0,
                band,
            },
            rule_ids: vec![],
            rejection_reason: None,
            created_at: Utc::now(),
        }
    }

    fn summarise(opportunities: Vec<Opportunity>) -> AnalysisResult {
        AnalysisResult::summarise(
            AnalysisId::new(),
            CompanyId::new(),
            opportunities,
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        )
    }

    #[test]
    fn counts_split_by_priority_band() {
        let result = summarise(vec![
            opp(
                PriorityBand::High,
                OpportunityStatus::Identified,
                95,
                10_000,
                20_000,
            ),
            opp(
                PriorityBand::High,
                OpportunityStatus::Identified,
                92,
                5_000,
                6_000,
            ),
            opp(
                PriorityBand::ShouldInvestigate,
                OpportunityStatus::Investigate,
                80,
                1_000,
                2_000,
            ),
            opp(
                PriorityBand::NeedsMoreEvidence,
                OpportunityStatus::Verify,
                60,
                500,
                700,
            ),
        ]);
        assert_eq!(result.summary.identified_opportunities, 4);
        assert_eq!(result.summary.high_priority_count, 2);
        assert_eq!(result.summary.needs_investigation_count, 1);
        assert_eq!(result.summary.missing_information_count, 1);
        assert!(!result.summary.found_nothing);
    }

    #[test]
    fn rejected_findings_are_excluded_from_the_headline() {
        let result = summarise(vec![
            opp(
                PriorityBand::High,
                OpportunityStatus::Identified,
                95,
                10_000,
                10_000,
            ),
            opp(
                PriorityBand::High,
                OpportunityStatus::Rejected,
                95,
                999_000,
                999_000,
            ),
        ]);
        assert_eq!(result.summary.identified_opportunities, 1);
        assert_eq!(
            result.summary.estimated_total.high,
            Money::from_sek(10_000).unwrap()
        );
    }

    #[test]
    fn unactionable_findings_add_no_money_to_the_total() {
        let result = summarise(vec![
            opp(
                PriorityBand::High,
                OpportunityStatus::Identified,
                95,
                10_000,
                20_000,
            ),
            opp(
                PriorityBand::NeedsMoreEvidence,
                OpportunityStatus::Investigate,
                30,
                500_000,
                900_000,
            ),
        ]);
        assert_eq!(
            result.summary.estimated_total.low,
            Money::from_sek(10_000).unwrap()
        );
        assert_eq!(
            result.summary.estimated_total.high,
            Money::from_sek(20_000).unwrap()
        );
    }

    #[test]
    fn totals_are_summed_bound_wise_across_findings() {
        let result = summarise(vec![
            opp(
                PriorityBand::High,
                OpportunityStatus::Identified,
                95,
                10_000,
                20_000,
            ),
            opp(
                PriorityBand::High,
                OpportunityStatus::Identified,
                95,
                5_000,
                8_000,
            ),
        ]);
        assert_eq!(
            result.summary.estimated_total.low,
            Money::from_sek(15_000).unwrap()
        );
        assert_eq!(
            result.summary.estimated_total.high,
            Money::from_sek(28_000).unwrap()
        );
    }

    #[test]
    fn an_empty_analysis_is_a_designed_state_not_an_error() {
        let result = summarise(vec![]);
        assert!(result.summary.found_nothing);
        assert_eq!(result.summary.estimated_total, MoneyRange::ZERO);
        assert!(result.recommended_actions.is_empty());
        assert!(!result.disclaimer.is_empty());
    }

    #[test]
    fn recommended_actions_take_the_top_three_by_priority() {
        let result = summarise(vec![
            opp(
                PriorityBand::High,
                OpportunityStatus::Identified,
                95,
                100_000,
                100_000,
            ),
            opp(
                PriorityBand::High,
                OpportunityStatus::Identified,
                95,
                400_000,
                400_000,
            ),
            opp(
                PriorityBand::ShouldInvestigate,
                OpportunityStatus::Investigate,
                80,
                200_000,
                200_000,
            ),
            opp(
                PriorityBand::ShouldInvestigate,
                OpportunityStatus::Investigate,
                80,
                300_000,
                300_000,
            ),
        ]);
        assert_eq!(result.recommended_actions.len(), 3);
        assert_eq!(result.recommended_actions[0], "action 400000");
        assert_eq!(result.recommended_actions[1], "action 300000");
    }

    #[test]
    fn stage_progress_runs_from_zero_to_one() {
        assert_eq!(AnalysisStage::Queued.progress(), 0.0);
        assert_eq!(AnalysisStage::Done.progress(), 1.0);
        assert!(
            AnalysisStage::CheckingRules.progress() > AnalysisStage::ReadingDocuments.progress()
        );
    }

    #[test]
    fn every_stage_has_swedish_copy() {
        for stage in AnalysisStage::ordered() {
            assert!(!stage.label_sv().is_empty());
        }
    }
}

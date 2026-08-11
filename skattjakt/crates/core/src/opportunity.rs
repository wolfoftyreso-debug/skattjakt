//! Opportunities: what Skattjakt actually hands back.
//!
//! An opportunity is a question worth asking, not a conclusion. The type
//! reflects that: it carries what is known, what is missing, what should be
//! done next, and the evidence for all three.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::confidence::{Confidence, ConfidenceBand};
use crate::evidence::EvidenceChain;
use crate::ids::{AnalysisId, OpportunityId};
use crate::money::{Money, MoneyRange};

/// The areas Skattjakt searches, per section 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpportunityCategory {
    Tax,
    Costs,
    Vat,
    Personnel,
    Investments,
    ResearchAndDevelopment,
    /// Findings that do not save money but should be looked at anyway.
    Risk,
}

impl OpportunityCategory {
    pub fn label_sv(self) -> &'static str {
        match self {
            OpportunityCategory::Tax => "Skatt",
            OpportunityCategory::Costs => "Kostnader",
            OpportunityCategory::Vat => "Moms",
            OpportunityCategory::Personnel => "Personal",
            OpportunityCategory::Investments => "Investeringar",
            OpportunityCategory::ResearchAndDevelopment => "FoU",
            OpportunityCategory::Risk => "Risk",
        }
    }
}

/// Status levels from section 5. The distinction between these is the product:
/// an observation is not a fact, and a fact is not advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpportunityStatus {
    /// Strong evidence.
    Identified,
    /// A potential opportunity, but information is missing.
    Investigate,
    /// The rule or the calculation needs further checking.
    Verify,
    /// Something that should be reviewed, whether or not it saves money.
    Warning,
    /// Did not survive the skeptic pass. Retained for the audit trail.
    Rejected,
}

impl OpportunityStatus {
    pub fn label_sv(self) -> &'static str {
        match self {
            OpportunityStatus::Identified => "Identifierad",
            OpportunityStatus::Investigate => "Undersök",
            OpportunityStatus::Verify => "Verifiera",
            OpportunityStatus::Warning => "Varning",
            OpportunityStatus::Rejected => "Avvisad",
        }
    }

    /// Whether it appears in the user-facing result at all.
    pub fn is_presented(self) -> bool {
        !matches!(self, OpportunityStatus::Rejected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub fn label_sv(self) -> &'static str {
        match self {
            RiskLevel::Low => "Låg",
            RiskLevel::Medium => "Medel",
            RiskLevel::High => "Hög",
        }
    }
}

/// How much work it takes to settle the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvestigationEffort {
    /// A question to the accountant; minutes.
    Low,
    /// Requires pulling a document or a calculation.
    Medium,
    /// Requires an assessment, external input, or reconstruction.
    High,
}

impl InvestigationEffort {
    fn divisor(self) -> f64 {
        match self {
            InvestigationEffort::Low => 1.0,
            InvestigationEffort::Medium => 1.6,
            InvestigationEffort::High => 2.5,
        }
    }

    pub fn label_sv(self) -> &'static str {
        match self {
            InvestigationEffort::Low => "Låg",
            InvestigationEffort::Medium => "Medel",
            InvestigationEffort::High => "Hög",
        }
    }
}

/// Whether the window to act is closing. A reserve that must be booked before
/// the return is filed is worth more attention than one that is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    /// No deadline pressure.
    Routine,
    /// Should be settled before the year is closed.
    BeforeYearEnd,
    /// Should be settled before the tax return is filed.
    BeforeFiling,
}

impl Urgency {
    fn multiplier(self) -> f64 {
        match self {
            Urgency::Routine => 1.0,
            Urgency::BeforeYearEnd => 1.15,
            Urgency::BeforeFiling => 1.3,
        }
    }
}

/// Tunable inputs to the priority formula of section 24.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriorityWeights {
    /// Amount at which the economic term saturates. Beyond this, a larger
    /// number stops buying priority, so one huge speculative item cannot bury
    /// several well-evidenced ones.
    pub economic_reference: Money,
    /// Weight of the finding's own relevance to this company.
    pub relevance_weight: f64,
    /// Priority at or above which an item is "high priority" in the summary.
    pub high_priority_threshold: f64,
}

impl Default for PriorityWeights {
    fn default() -> Self {
        Self {
            economic_reference: Money::from_ore(20_000_000), // 200 000 kr
            relevance_weight: 1.0,
            high_priority_threshold: 0.5,
        }
    }
}

/// The computed ranking of an opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Priority {
    /// 0.0–1.0.
    pub score: f64,
    pub band: PriorityBand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityBand {
    /// "Hög prioritet"
    High,
    /// "Bör undersökas"
    ShouldInvestigate,
    /// "Kräver mer underlag"
    NeedsMoreEvidence,
}

impl PriorityBand {
    pub fn label_sv(self) -> &'static str {
        match self {
            PriorityBand::High => "Hög prioritet",
            PriorityBand::ShouldInvestigate => "Bör undersökas",
            PriorityBand::NeedsMoreEvidence => "Kräver mer underlag",
        }
    }
}

/// One finding, with everything needed to render its evidence card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Opportunity {
    pub id: OpportunityId,
    pub analysis_id: AnalysisId,
    pub category: OpportunityCategory,
    pub status: OpportunityStatus,

    /// Short Swedish title, e.g. "Periodiseringsfond".
    pub title: String,
    /// Why this was flagged, in plain language.
    pub rationale: String,
    /// Estimated economic effect. Always an interval (section 13).
    pub impact: MoneyRange,

    pub evidence: EvidenceChain,
    /// What is absent that would settle the question.
    #[serde(default)]
    pub missing_information: Vec<String>,
    /// The concrete next step, usually a question to put to the accountant.
    pub recommended_action: String,

    pub confidence: Confidence,
    pub risk: RiskLevel,
    pub effort: InvestigationEffort,
    pub urgency: Urgency,
    pub priority: Priority,

    /// Rules that were evaluated for this finding.
    #[serde(default)]
    pub rule_ids: Vec<String>,
    /// Why it was rejected, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,

    pub created_at: DateTime<Utc>,
}

impl Opportunity {
    /// Section 24's formula: economic value, scaled by confidence and
    /// relevance, divided by the effort to investigate.
    ///
    /// `relevance` is how specific the finding is to this company as opposed to
    /// generic advice; it is supplied by the pipeline.
    pub fn compute_priority(
        impact: MoneyRange,
        confidence: Confidence,
        effort: InvestigationEffort,
        urgency: Urgency,
        relevance: f64,
        weights: &PriorityWeights,
    ) -> Priority {
        let reference = weights.economic_reference.ore().max(1) as f64;
        let magnitude = impact.midpoint().ore().unsigned_abs() as f64;
        let economic = (magnitude / reference).min(1.0);

        let confidence_term = confidence.score as f64 / 100.0;
        let relevance_term = (relevance.clamp(0.0, 1.0)) * weights.relevance_weight;

        let raw = economic * confidence_term * relevance_term * urgency.multiplier() / effort.divisor();
        let score = raw.clamp(0.0, 1.0);

        // A finding that cannot be acted on is never "high priority", however
        // large the number attached to it.
        let band = if !confidence.is_actionable() {
            PriorityBand::NeedsMoreEvidence
        } else if score >= weights.high_priority_threshold {
            PriorityBand::High
        } else if confidence.band == ConfidenceBand::Investigate {
            PriorityBand::NeedsMoreEvidence
        } else {
            PriorityBand::ShouldInvestigate
        };

        Priority { score, band }
    }

    /// A risk finding carries no money but still deserves attention, so it is
    /// ranked on confidence and risk level alone.
    pub fn compute_risk_priority(confidence: Confidence, risk: RiskLevel) -> Priority {
        let risk_term = match risk {
            RiskLevel::Low => 0.3,
            RiskLevel::Medium => 0.6,
            RiskLevel::High => 0.9,
        };
        let score = (confidence.score as f64 / 100.0) * risk_term;
        let band = if !confidence.is_actionable() {
            PriorityBand::NeedsMoreEvidence
        } else if risk == RiskLevel::High && confidence.is_actionable() {
            PriorityBand::High
        } else {
            PriorityBand::ShouldInvestigate
        };
        Priority { score, band }
    }

    /// The economic effect to include in a total. Rejected findings and
    /// non-actionable ones contribute nothing, so the headline range never
    /// includes money the system does not stand behind.
    pub fn countable_impact(&self) -> MoneyRange {
        if self.status == OpportunityStatus::Rejected || !self.confidence.is_actionable() {
            MoneyRange::ZERO
        } else {
            self.impact
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confidence::{ConfidenceBand, ConfidenceFactors, ConfidenceThresholds, ConfidenceWeights, UnitInterval};
    use crate::evidence::EvidenceChain;
    use crate::ids::AnalysisId;

    fn confidence(score: u8) -> Confidence {
        let band = if score >= 90 {
            ConfidenceBand::Strong
        } else if score >= 75 {
            ConfidenceBand::Good
        } else if score >= 50 {
            ConfidenceBand::Investigate
        } else {
            ConfidenceBand::NotActionable
        };
        Confidence { score, band }
    }

    fn range(low: i64, high: i64) -> MoneyRange {
        MoneyRange::new(Money::from_sek(low).unwrap(), Money::from_sek(high).unwrap())
    }

    #[test]
    fn larger_impact_ranks_higher_all_else_equal() {
        let w = PriorityWeights::default();
        let small = Opportunity::compute_priority(range(1_000, 2_000), confidence(90), InvestigationEffort::Low, Urgency::Routine, 1.0, &w);
        let large = Opportunity::compute_priority(range(100_000, 150_000), confidence(90), InvestigationEffort::Low, Urgency::Routine, 1.0, &w);
        assert!(large.score > small.score);
    }

    #[test]
    fn higher_effort_ranks_lower() {
        let w = PriorityWeights::default();
        let easy = Opportunity::compute_priority(range(100_000, 100_000), confidence(90), InvestigationEffort::Low, Urgency::Routine, 1.0, &w);
        let hard = Opportunity::compute_priority(range(100_000, 100_000), confidence(90), InvestigationEffort::High, Urgency::Routine, 1.0, &w);
        assert!(easy.score > hard.score);
    }

    #[test]
    fn urgency_lifts_an_otherwise_equal_finding() {
        let w = PriorityWeights::default();
        let routine = Opportunity::compute_priority(range(50_000, 50_000), confidence(80), InvestigationEffort::Low, Urgency::Routine, 1.0, &w);
        let urgent = Opportunity::compute_priority(range(50_000, 50_000), confidence(80), InvestigationEffort::Low, Urgency::BeforeFiling, 1.0, &w);
        assert!(urgent.score > routine.score);
    }

    #[test]
    fn a_huge_but_unactionable_finding_never_reaches_high_priority() {
        let w = PriorityWeights::default();
        let p = Opportunity::compute_priority(range(5_000_000, 9_000_000), confidence(20), InvestigationEffort::Low, Urgency::BeforeFiling, 1.0, &w);
        assert_eq!(p.band, PriorityBand::NeedsMoreEvidence);
    }

    #[test]
    fn priority_score_stays_within_bounds() {
        let w = PriorityWeights::default();
        let p = Opportunity::compute_priority(range(50_000_000, 90_000_000), confidence(100), InvestigationEffort::Low, Urgency::BeforeFiling, 1.0, &w);
        assert!((0.0..=1.0).contains(&p.score), "score was {}", p.score);
    }

    #[test]
    fn economic_term_saturates_at_the_reference_amount() {
        let w = PriorityWeights::default();
        let at_reference = Opportunity::compute_priority(range(200_000, 200_000), confidence(100), InvestigationEffort::Low, Urgency::Routine, 1.0, &w);
        let far_above = Opportunity::compute_priority(range(2_000_000, 2_000_000), confidence(100), InvestigationEffort::Low, Urgency::Routine, 1.0, &w);
        assert_eq!(at_reference.score, far_above.score);
    }

    #[test]
    fn high_risk_findings_are_prioritised_without_any_money() {
        let p = Opportunity::compute_risk_priority(confidence(85), RiskLevel::High);
        assert_eq!(p.band, PriorityBand::High);
    }

    #[test]
    fn unactionable_risk_findings_need_more_evidence() {
        let p = Opportunity::compute_risk_priority(confidence(30), RiskLevel::High);
        assert_eq!(p.band, PriorityBand::NeedsMoreEvidence);
    }

    #[test]
    fn rejected_and_unactionable_findings_contribute_no_money_to_totals() {
        let base = Opportunity {
            id: OpportunityId::new(),
            analysis_id: AnalysisId::new(),
            category: OpportunityCategory::Tax,
            status: OpportunityStatus::Identified,
            title: "T".into(),
            rationale: "R".into(),
            impact: range(10_000, 20_000),
            evidence: EvidenceChain::new(),
            missing_information: vec![],
            recommended_action: "A".into(),
            confidence: confidence(95),
            risk: RiskLevel::Low,
            effort: InvestigationEffort::Low,
            urgency: Urgency::Routine,
            priority: Priority { score: 0.5, band: PriorityBand::High },
            rule_ids: vec![],
            rejection_reason: None,
            created_at: Utc::now(),
        };
        assert_eq!(base.countable_impact(), range(10_000, 20_000));

        let rejected = Opportunity { status: OpportunityStatus::Rejected, ..base.clone() };
        assert_eq!(rejected.countable_impact(), MoneyRange::ZERO);

        let weak = Opportunity { confidence: confidence(10), ..base };
        assert_eq!(weak.countable_impact(), MoneyRange::ZERO);
    }

    #[test]
    fn confidence_is_computed_not_asserted() {
        // Guards the architectural rule: a caller cannot hand in a score.
        let factors = ConfidenceFactors {
            document_evidence: UnitInterval::saturating(0.9),
            rule_match: UnitInterval::saturating(1.0),
            calculation_certainty: UnitInterval::saturating(0.8),
            missing_information: UnitInterval::saturating(0.2),
            contradiction_score: UnitInterval::saturating(0.0),
            model_agreement: UnitInterval::saturating(1.0),
        };
        let c = Confidence::compute(&factors, &ConfidenceWeights::default(), &ConfidenceThresholds::default());
        assert!(c.score >= 75 && c.score <= 100, "score was {}", c.score);
    }

    #[test]
    fn rejected_status_is_not_presented() {
        assert!(!OpportunityStatus::Rejected.is_presented());
        assert!(OpportunityStatus::Identified.is_presented());
        assert!(OpportunityStatus::Warning.is_presented());
    }
}

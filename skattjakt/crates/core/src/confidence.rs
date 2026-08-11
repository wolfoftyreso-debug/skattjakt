//! The confidence engine.
//!
//! Confidence is *computed*, never taken from the model. A language model
//! asked "how sure are you?" produces a fluent number, not a measurement, and
//! section 12 of the product spec forbids using it as the score. What the model
//! contributes here is one input among six, and it is not the heaviest.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};

/// A value constrained to `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnitInterval(f64);

impl UnitInterval {
    pub const ZERO: UnitInterval = UnitInterval(0.0);
    pub const ONE: UnitInterval = UnitInterval(1.0);

    pub fn new(factor: &'static str, value: f64) -> CoreResult<Self> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(CoreError::ConfidenceOutOfRange {
                factor,
                value: value.to_string(),
            });
        }
        Ok(Self(value))
    }

    /// Clamps instead of failing. Used where an upstream component produces a
    /// nearly-valid number and rejecting the whole analysis would be worse than
    /// pinning it to the boundary.
    pub fn saturating(value: f64) -> Self {
        if value.is_nan() {
            Self(0.0)
        } else {
            Self(value.clamp(0.0, 1.0))
        }
    }

    pub const fn get(self) -> f64 {
        self.0
    }

    pub fn inverse(self) -> Self {
        Self(1.0 - self.0)
    }
}

impl Eq for UnitInterval {}

#[allow(clippy::derive_ord_xor_partial_ord)]
impl Ord for UnitInterval {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Safe: the constructor rejects NaN, so a total order exists.
        self.0.partial_cmp(&other.0).unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// The six measured inputs to a confidence score.
///
/// Polarity differs between fields and is documented per field rather than
/// normalised away, because these names are the ones the product spec uses and
/// they appear in stored model-run records.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceFactors {
    /// How well the claim is anchored in extracted document values. Higher is better.
    pub document_evidence: UnitInterval,
    /// How cleanly a versioned rule's conditions matched. Higher is better.
    pub rule_match: UnitInterval,
    /// How reproducible the arithmetic is. Higher is better.
    pub calculation_certainty: UnitInterval,
    /// How much needed information is absent. Higher is *worse*.
    pub missing_information: UnitInterval,
    /// How strongly the skeptic pass contradicted the finding. Higher is *worse*.
    pub contradiction_score: UnitInterval,
    /// Agreement across model passes on the same candidate. Higher is better.
    pub model_agreement: UnitInterval,
}

impl ConfidenceFactors {
    /// A neutral starting point: nothing known, nothing contradicted.
    pub fn unknown() -> Self {
        Self {
            document_evidence: UnitInterval::ZERO,
            rule_match: UnitInterval::ZERO,
            calculation_certainty: UnitInterval::ZERO,
            missing_information: UnitInterval::ONE,
            contradiction_score: UnitInterval::ZERO,
            model_agreement: UnitInterval::ZERO,
        }
    }
}

/// Weights applied to each factor. Configurable per section 12.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceWeights {
    pub document_evidence: f64,
    pub rule_match: f64,
    pub calculation_certainty: f64,
    pub information_completeness: f64,
    pub consistency: f64,
    pub model_agreement: f64,
}

impl Default for ConfidenceWeights {
    fn default() -> Self {
        // Document evidence and rule match dominate; the model's own agreement
        // with itself is deliberately the smallest term.
        Self {
            document_evidence: 0.25,
            rule_match: 0.25,
            calculation_certainty: 0.15,
            information_completeness: 0.15,
            consistency: 0.10,
            model_agreement: 0.10,
        }
    }
}

impl ConfidenceWeights {
    pub fn total(&self) -> f64 {
        self.document_evidence
            + self.rule_match
            + self.calculation_certainty
            + self.information_completeness
            + self.consistency
            + self.model_agreement
    }
}

/// Thresholds separating the confidence bands. Configurable per section 12.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceThresholds {
    pub strong: u8,
    pub good: u8,
    pub investigate: u8,
}

impl Default for ConfidenceThresholds {
    fn default() -> Self {
        Self { strong: 90, good: 75, investigate: 50 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceBand {
    /// 90–100. Strong signal.
    Strong,
    /// 75–89. Good candidate.
    Good,
    /// 50–74. Needs investigation.
    Investigate,
    /// Below 50. Never shown as actionable.
    NotActionable,
}

impl ConfidenceBand {
    /// Whether a finding in this band may be presented as something to act on.
    pub fn is_actionable(self) -> bool {
        !matches!(self, ConfidenceBand::NotActionable)
    }
}

/// A computed confidence score, on a 0–100 scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confidence {
    pub score: u8,
    pub band: ConfidenceBand,
}

impl Confidence {
    pub fn compute(factors: &ConfidenceFactors, weights: &ConfidenceWeights, thresholds: &ConfidenceThresholds) -> Self {
        let total_weight = weights.total();
        // A misconfigured weight set must not divide by zero and must not
        // silently produce a confident answer.
        if total_weight <= f64::EPSILON {
            return Self { score: 0, band: ConfidenceBand::NotActionable };
        }

        let weighted = factors.document_evidence.get() * weights.document_evidence
            + factors.rule_match.get() * weights.rule_match
            + factors.calculation_certainty.get() * weights.calculation_certainty
            + factors.missing_information.inverse().get() * weights.information_completeness
            + factors.contradiction_score.inverse().get() * weights.consistency
            + factors.model_agreement.get() * weights.model_agreement;

        let mut score = ((weighted / total_weight) * 100.0).round().clamp(0.0, 100.0) as u8;

        // Fail closed (section 35). These caps encode the cases where a high
        // weighted average would be misleading no matter how good the other
        // factors look.
        if factors.rule_match.get() <= f64::EPSILON {
            // Nothing in the versioned rule set actually matched.
            score = score.min(thresholds.investigate.saturating_sub(1));
        }
        if factors.contradiction_score.get() >= 0.5 {
            // The skeptic pass found a live objection.
            score = score.min(thresholds.investigate.saturating_sub(1));
        }
        if factors.document_evidence.get() <= f64::EPSILON {
            // No extracted value backs the claim; it cannot be actionable.
            score = score.min(thresholds.investigate.saturating_sub(1));
        }

        Self { score, band: Self::band_for(score, thresholds) }
    }

    fn band_for(score: u8, thresholds: &ConfidenceThresholds) -> ConfidenceBand {
        if score >= thresholds.strong {
            ConfidenceBand::Strong
        } else if score >= thresholds.good {
            ConfidenceBand::Good
        } else if score >= thresholds.investigate {
            ConfidenceBand::Investigate
        } else {
            ConfidenceBand::NotActionable
        }
    }

    pub fn is_actionable(self) -> bool {
        self.band.is_actionable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(v: f64) -> UnitInterval {
        UnitInterval::new("test", v).unwrap()
    }

    fn strong_factors() -> ConfidenceFactors {
        ConfidenceFactors {
            document_evidence: u(1.0),
            rule_match: u(1.0),
            calculation_certainty: u(1.0),
            missing_information: u(0.0),
            contradiction_score: u(0.0),
            model_agreement: u(1.0),
        }
    }

    fn compute(f: &ConfidenceFactors) -> Confidence {
        Confidence::compute(f, &ConfidenceWeights::default(), &ConfidenceThresholds::default())
    }

    #[test]
    fn unit_interval_rejects_out_of_range_and_nan() {
        assert!(UnitInterval::new("x", 1.1).is_err());
        assert!(UnitInterval::new("x", -0.0001).is_err());
        assert!(UnitInterval::new("x", f64::NAN).is_err());
        assert_eq!(UnitInterval::saturating(f64::NAN).get(), 0.0);
        assert_eq!(UnitInterval::saturating(5.0).get(), 1.0);
    }

    #[test]
    fn perfect_factors_score_one_hundred_and_are_strong() {
        let c = compute(&strong_factors());
        assert_eq!(c.score, 100);
        assert_eq!(c.band, ConfidenceBand::Strong);
        assert!(c.is_actionable());
    }

    #[test]
    fn unknown_factors_are_not_actionable() {
        let c = compute(&ConfidenceFactors::unknown());
        assert_eq!(c.band, ConfidenceBand::NotActionable);
        assert!(!c.is_actionable());
    }

    #[test]
    fn no_rule_match_caps_below_actionable_however_good_the_rest_is() {
        let mut f = strong_factors();
        f.rule_match = u(0.0);
        let c = compute(&f);
        assert!(c.score < 50, "score was {}", c.score);
        assert_eq!(c.band, ConfidenceBand::NotActionable);
    }

    #[test]
    fn a_live_contradiction_caps_below_actionable() {
        let mut f = strong_factors();
        f.contradiction_score = u(0.6);
        let c = compute(&f);
        assert!(c.score < 50, "score was {}", c.score);
    }

    #[test]
    fn missing_document_evidence_caps_below_actionable() {
        let mut f = strong_factors();
        f.document_evidence = u(0.0);
        assert!(compute(&f).score < 50);
    }

    #[test]
    fn model_agreement_alone_cannot_make_a_finding_actionable() {
        let mut f = ConfidenceFactors::unknown();
        f.model_agreement = u(1.0);
        let c = compute(&f);
        assert!(!c.is_actionable(), "model self-agreement must not carry a finding");
    }

    #[test]
    fn missing_information_lowers_the_score_monotonically() {
        let mut a = strong_factors();
        a.missing_information = u(0.0);
        let mut b = strong_factors();
        b.missing_information = u(0.5);
        let mut c = strong_factors();
        c.missing_information = u(1.0);
        assert!(compute(&a).score > compute(&b).score);
        assert!(compute(&b).score > compute(&c).score);
    }

    #[test]
    fn bands_follow_the_configured_thresholds() {
        let t = ConfidenceThresholds::default();
        assert_eq!(Confidence::band_for(100, &t), ConfidenceBand::Strong);
        assert_eq!(Confidence::band_for(90, &t), ConfidenceBand::Strong);
        assert_eq!(Confidence::band_for(89, &t), ConfidenceBand::Good);
        assert_eq!(Confidence::band_for(75, &t), ConfidenceBand::Good);
        assert_eq!(Confidence::band_for(74, &t), ConfidenceBand::Investigate);
        assert_eq!(Confidence::band_for(50, &t), ConfidenceBand::Investigate);
        assert_eq!(Confidence::band_for(49, &t), ConfidenceBand::NotActionable);
    }

    #[test]
    fn zero_weights_fail_closed_rather_than_dividing_by_zero() {
        let weights = ConfidenceWeights {
            document_evidence: 0.0,
            rule_match: 0.0,
            calculation_certainty: 0.0,
            information_completeness: 0.0,
            consistency: 0.0,
            model_agreement: 0.0,
        };
        let c = Confidence::compute(&strong_factors(), &weights, &ConfidenceThresholds::default());
        assert_eq!(c.score, 0);
        assert!(!c.is_actionable());
    }
}

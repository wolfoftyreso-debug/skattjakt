//! Statistics over a finished run.
//!
//! Everything here takes a *sorted* slice of outcomes. Sorting once and
//! computing every percentile from the same array is both faster and more
//! accurate than a streaming estimator, and at the sizes this engine works in
//! — one output, up to ten million doubles — it is affordable. A t-digest would
//! save memory and give approximate percentiles; for a number a person is going
//! to make a decision with, the exact one is worth the eighty megabytes.
//!
//! Summation is Welford's, not a running total. On ten million values of
//! wildly different magnitude a naive sum loses low-order bits steadily, and
//! the variance computed as `E[x²] − E[x]²` can come out *negative* — which
//! then produces a NaN standard deviation from perfectly good data.

use serde::{Deserialize, Serialize};

use crate::spec::{Output, TargetDirection};

/// The full statistical description of one output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Statistics {
    pub count: u64,
    pub mean: f64,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub std_dev: f64,
    pub variance: f64,
    pub p5: f64,
    pub p10: f64,
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
    /// The probability of reaching the output's target, when one is set.
    /// `null` means no target was set — never "zero".
    ///
    /// Serialised as an explicit `null` rather than omitted, and that is the
    /// whole point of the field being an `Option`. An absent key is invisible:
    /// a client reading it gets `undefined`, renders nothing, and the
    /// distinction between "no target" and "cannot happen" — which every
    /// comment in this crate exists to preserve — disappears at the last step.
    /// The stored read path returns `null` too, so the two agree.
    pub probability_of_target: Option<f64>,
    /// The share of outcomes below zero. Meaningful for a profit, meaningless
    /// for a headcount, which is why it is reported rather than interpreted.
    pub probability_of_loss: f64,
    pub probability_below_threshold: Option<f64>,
    pub probability_above_threshold: Option<f64>,
    /// A 95% confidence interval **for the mean** — the sampling error of this
    /// run, not the spread of the outcomes. Two different things that look
    /// alike on a screen, so the field name says which.
    ///
    /// `null` below thirty samples, where the central limit theorem is not a
    /// safe thing to lean on.
    pub mean_confidence_interval_95: Option<[f64; 2]>,
    /// The width of that interval, as a share of the mean. The honest measure
    /// of "have we run enough iterations".
    pub relative_standard_error: Option<f64>,
}

/// A percentile of a sorted slice, by linear interpolation between order
/// statistics.
///
/// The same definition R calls type 7 and NumPy uses by default. Stating which
/// one matters: the seven common definitions disagree by up to a whole rank,
/// and a P90 that moves when the tool changes is a P90 nobody can act on.
pub fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let fraction = fraction.clamp(0.0, 1.0);
    let position = fraction * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        return sorted[lower];
    }
    let weight = position - lower as f64;
    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
}

/// The share of samples at or below `value`, by binary search.
///
/// This is the function behind "there is an X% chance the result is at least
/// Y" in the interface. `O(log n)` per query, so hovering across a chart does
/// not rescan ten million values.
pub fn share_at_most(sorted: &[f64], value: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let index = sorted.partition_point(|x| *x <= value);
    index as f64 / sorted.len() as f64
}

pub fn share_below(sorted: &[f64], value: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let index = sorted.partition_point(|x| *x < value);
    index as f64 / sorted.len() as f64
}

pub fn share_at_least(sorted: &[f64], value: f64) -> f64 {
    1.0 - share_below(sorted, value)
}

/// Mean and variance by Welford's online algorithm.
pub fn mean_and_variance(values: &[f64]) -> (f64, f64) {
    let mut count = 0.0_f64;
    let mut mean = 0.0_f64;
    let mut m2 = 0.0_f64;
    for value in values {
        count += 1.0;
        let delta = value - mean;
        mean += delta / count;
        m2 += delta * (value - mean);
    }
    if count < 2.0 {
        return (mean, 0.0);
    }
    // The sample variance, dividing by n−1. A Monte Carlo run is a sample from
    // the model's distribution, not the population itself.
    (mean, m2 / (count - 1.0))
}

impl Statistics {
    /// Computes everything from a sorted slice and the output's own definition.
    pub fn compute(sorted: &[f64], output: &Output) -> Self {
        let (mean, variance) = mean_and_variance(sorted);
        let std_dev = variance.sqrt();
        let count = sorted.len() as u64;

        let probability_of_target = output.target.map(|target| match output.target_direction {
            TargetDirection::AtLeast => share_at_least(sorted, target),
            TargetDirection::AtMost => share_at_most(sorted, target),
        });

        let (below, above) = match output.critical_threshold {
            Some(threshold) => (
                Some(share_below(sorted, threshold)),
                Some(share_at_least(sorted, threshold)),
            ),
            None => (None, None),
        };

        let (interval, relative_standard_error) = if count >= 30 {
            let standard_error = std_dev / (count as f64).sqrt();
            // 1.96 is the normal quantile, which is what the central limit
            // theorem justifies at this sample size. A t-quantile would differ
            // in the fourth decimal at n ≥ 30 and in no decimal at n ≥ 1000.
            let half_width = 1.96 * standard_error;
            (
                Some([mean - half_width, mean + half_width]),
                if mean.abs() > 0.0 {
                    Some(half_width / mean.abs())
                } else {
                    None
                },
            )
        } else {
            (None, None)
        };

        Self {
            count,
            mean,
            median: percentile(sorted, 0.5),
            min: sorted.first().copied().unwrap_or(f64::NAN),
            max: sorted.last().copied().unwrap_or(f64::NAN),
            std_dev,
            variance,
            p5: percentile(sorted, 0.05),
            p10: percentile(sorted, 0.10),
            p25: percentile(sorted, 0.25),
            p50: percentile(sorted, 0.50),
            p75: percentile(sorted, 0.75),
            p90: percentile(sorted, 0.90),
            p95: percentile(sorted, 0.95),
            p99: percentile(sorted, 0.99),
            probability_of_target,
            probability_of_loss: share_below(sorted, 0.0),
            probability_below_threshold: below,
            probability_above_threshold: above,
            mean_confidence_interval_95: interval,
            relative_standard_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_with(target: Option<f64>, threshold: Option<f64>) -> Output {
        Output {
            id: "y".into(),
            name: "y".into(),
            expression: "x".into(),
            unit: None,
            description: None,
            target,
            target_direction: TargetDirection::AtLeast,
            critical_threshold: threshold,
        }
    }

    #[test]
    fn percentiles_of_a_known_sequence() {
        // 1..=100. Checked against the type-7 definition by hand.
        let sorted: Vec<f64> = (1..=100).map(f64::from).collect();
        assert!((percentile(&sorted, 0.0) - 1.0).abs() < 1e-12);
        assert!((percentile(&sorted, 1.0) - 100.0).abs() < 1e-12);
        assert!((percentile(&sorted, 0.5) - 50.5).abs() < 1e-12);
        assert!((percentile(&sorted, 0.25) - 25.75).abs() < 1e-12);
        assert!((percentile(&sorted, 0.9) - 90.1).abs() < 1e-12);
    }

    #[test]
    fn percentiles_of_a_single_value() {
        assert_eq!(percentile(&[7.0], 0.0), 7.0);
        assert_eq!(percentile(&[7.0], 0.5), 7.0);
        assert_eq!(percentile(&[7.0], 1.0), 7.0);
    }

    #[test]
    fn percentiles_of_nothing_are_not_a_number() {
        assert!(percentile(&[], 0.5).is_nan());
        assert!(share_at_most(&[], 1.0).is_nan());
    }

    #[test]
    fn percentiles_never_decrease() {
        let sorted: Vec<f64> = (0..1000).map(|i| f64::from(i) * 0.7).collect();
        let mut previous = f64::NEG_INFINITY;
        for step in 0..=100 {
            let value = percentile(&sorted, f64::from(step) / 100.0);
            assert!(value >= previous, "P{step} went backwards");
            previous = value;
        }
    }

    #[test]
    fn shares_read_off_the_right_side_of_a_value() {
        let sorted = vec![1.0, 2.0, 2.0, 3.0, 10.0];
        assert_eq!(share_at_most(&sorted, 2.0), 0.6);
        assert_eq!(share_below(&sorted, 2.0), 0.2);
        assert_eq!(share_at_least(&sorted, 2.0), 0.8);
        assert_eq!(share_at_least(&sorted, 0.0), 1.0);
        assert_eq!(share_at_most(&sorted, 100.0), 1.0);
    }

    #[test]
    fn variance_is_the_sample_variance() {
        // Hand-computed: mean 3, deviations -2,-1,0,1,2, sum of squares 10,
        // divided by n−1 = 4 → 2.5.
        let (mean, variance) = mean_and_variance(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert!((mean - 3.0).abs() < 1e-12);
        assert!((variance - 2.5).abs() < 1e-12);
    }

    #[test]
    fn variance_of_identical_values_is_zero_and_not_negative() {
        // The naive E[x²] − E[x]² formula returns a small negative number here
        // for large values, and the square root of that is NaN.
        let values = vec![1e9; 10_000];
        let (mean, variance) = mean_and_variance(&values);
        assert_eq!(mean, 1e9);
        assert_eq!(variance, 0.0);
        assert_eq!(variance.sqrt(), 0.0);
    }

    #[test]
    fn variance_survives_a_large_offset() {
        // The classic catastrophic-cancellation case: tiny spread on a huge
        // offset. The answer should still be 2.5.
        let values: Vec<f64> = [1.0, 2.0, 3.0, 4.0, 5.0].iter().map(|x| x + 1e12).collect();
        let (_, variance) = mean_and_variance(&values);
        assert!((variance - 2.5).abs() < 1e-4, "variance was {variance}");
    }

    #[test]
    fn a_full_statistics_block_is_internally_consistent() {
        let mut samples: Vec<f64> = (0..10_000).map(|i| f64::from(i) - 5000.0).collect();
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let stats = Statistics::compute(&samples, &output_with(Some(0.0), Some(-1000.0)));

        assert_eq!(stats.count, 10_000);
        assert_eq!(stats.min, -5000.0);
        assert_eq!(stats.max, 4999.0);
        assert_eq!(stats.median, stats.p50);
        assert!(stats.p10 < stats.p25);
        assert!(stats.p25 < stats.p50);
        assert!(stats.p50 < stats.p75);
        assert!(stats.p75 < stats.p90);
        assert!(stats.p90 < stats.p99);
        assert!((stats.probability_of_loss - 0.5).abs() < 0.001);
        assert!((stats.probability_of_target.unwrap() - 0.5).abs() < 0.001);
        assert!((stats.probability_below_threshold.unwrap() - 0.4).abs() < 0.001);
        let interval = stats.mean_confidence_interval_95.unwrap();
        assert!(interval[0] < stats.mean && stats.mean < interval[1]);
    }

    #[test]
    fn a_ceiling_target_counts_the_other_side() {
        let sorted: Vec<f64> = (1..=100).map(f64::from).collect();
        let mut output = output_with(Some(30.0), None);
        output.target_direction = TargetDirection::AtMost;
        let stats = Statistics::compute(&sorted, &output);
        // 30 of 100 values are at most 30.
        assert!((stats.probability_of_target.unwrap() - 0.30).abs() < 1e-9);
    }

    #[test]
    fn no_target_means_no_number_rather_than_zero() {
        let sorted = vec![1.0, 2.0, 3.0];
        let stats = Statistics::compute(&sorted, &output_with(None, None));
        assert!(stats.probability_of_target.is_none());
        assert!(stats.probability_below_threshold.is_none());
    }

    #[test]
    fn a_small_sample_gets_no_confidence_interval() {
        let sorted = vec![1.0, 2.0, 3.0];
        let stats = Statistics::compute(&sorted, &output_with(None, None));
        assert!(stats.mean_confidence_interval_95.is_none());
    }

    #[test]
    fn a_zero_variance_output_is_described_without_nan() {
        let sorted = vec![5.0; 1000];
        let stats = Statistics::compute(&sorted, &output_with(Some(5.0), None));
        assert_eq!(stats.std_dev, 0.0);
        assert_eq!(stats.p10, 5.0);
        assert_eq!(stats.p90, 5.0);
        assert_eq!(stats.mean_confidence_interval_95, Some([5.0, 5.0]));
        assert!(stats.probability_of_target.unwrap() > 0.999);
    }
}

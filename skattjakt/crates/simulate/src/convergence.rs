//! Has this run settled?
//!
//! Section 10. A Monte Carlo result is an estimate, and the only honest way to
//! say whether it is a good one is to show what it looked like with fewer
//! iterations. If the median moved by 4% between a hundred thousand and a
//! million, the third significant figure on the screen is noise.
//!
//! The tails are checked more loosely than the centre, and deliberately. A
//! median is an average of the middle and settles quickly; a P90 is estimated
//! from a tenth of the sample and always converges last. Holding them to the
//! same tolerance would either pass unstable tails or fail on stable medians.

use serde::{Deserialize, Serialize};

use crate::stats::{mean_and_variance, percentile};

/// The state of one statistic at one iteration count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub iterations: u32,
    pub mean: f64,
    pub median: f64,
    pub p10: f64,
    pub p90: f64,
}

/// The convergence report for one output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Convergence {
    pub output_id: String,
    pub checkpoints: Vec<Checkpoint>,
    /// Whether the last step moved every statistic less than its tolerance.
    pub stable: bool,
    /// The largest relative movement between the last two checkpoints, over all
    /// four statistics. The number behind `stable`.
    pub largest_relative_change: f64,
    /// Present when `stable` is false: what to do about it, in Swedish, for the
    /// interface.
    pub warning: Option<String>,
}

/// How much the centre may move between the last two checkpoints.
const CENTRE_TOLERANCE: f64 = 0.01;
/// How much a tail may move. Looser, because a tail is estimated from a tenth
/// of the sample and converges roughly three times more slowly.
const TAIL_TOLERANCE: f64 = 0.03;

/// The iteration counts to measure at.
///
/// Decades and their halves — 1k, 5k, 10k, 50k … — so the report reads as a
/// progression across orders of magnitude rather than as arbitrary fractions,
/// and always including the full count so the last checkpoint is the result
/// actually reported.
pub fn checkpoints_for(iterations: u32) -> Vec<u32> {
    let mut points = Vec::new();
    let mut step = 1_000u64;
    while step < u64::from(iterations) {
        points.push(step as u32);
        let half = step * 5;
        if half < u64::from(iterations) {
            points.push(half as u32);
        }
        step *= 10;
    }
    points.push(iterations);
    points.dedup();
    // Below a thousand iterations there is one checkpoint, so there is nothing
    // to compare and `stable` will be reported as unknown-by-way-of-false.
    points
}

/// How much a statistic moved, relative to the scale it lives on.
///
/// The denominator is the larger of the values themselves and the width of the
/// distribution, and the second half of that is not a detail. Measured against
/// the value alone, a statistic that sits near zero is never stable: a mean
/// wandering between 0.001 and 0.002 on a standard normal is a 100% change and
/// a completely converged one. Measured against the spread as well, the
/// question becomes the one worth asking — did the estimate move by enough to
/// matter on the axis it will be drawn on?
fn relative_change(previous: f64, current: f64, spread: f64) -> f64 {
    let scale = previous.abs().max(current.abs()).max(spread.abs());
    if scale == 0.0 {
        // Everything is zero: no movement, rather than a division by zero.
        return 0.0;
    }
    (current - previous).abs() / scale
}

/// Builds the report from a run's samples.
///
/// `samples` is in iteration order. Each checkpoint sorts a copy of the prefix,
/// which is what makes the percentiles at that checkpoint the ones a shorter
/// run would genuinely have produced — computing them from the fully sorted
/// array instead would silently use information from iterations that had not
/// happened yet.
pub fn analyse(output_id: &str, samples: &[f64]) -> Convergence {
    let total = samples.len() as u32;
    let mut checkpoints = Vec::new();
    let mut scratch: Vec<f64> = Vec::with_capacity(samples.len());

    for count in checkpoints_for(total) {
        let take = (count as usize).min(samples.len());
        if take == 0 {
            continue;
        }
        scratch.clear();
        scratch.extend_from_slice(&samples[..take]);
        scratch.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let (mean, _) = mean_and_variance(&scratch);
        checkpoints.push(Checkpoint {
            iterations: take as u32,
            mean,
            median: percentile(&scratch, 0.5),
            p10: percentile(&scratch, 0.10),
            p90: percentile(&scratch, 0.90),
        });
    }

    if checkpoints.len() < 2 {
        return Convergence {
            output_id: output_id.to_string(),
            checkpoints,
            stable: false,
            largest_relative_change: f64::NAN,
            warning: Some(
                "För få iterationer för att bedöma om resultatet har stabiliserats. \
                 Kör minst 1 000 iterationer."
                    .to_string(),
            ),
        };
    }

    let last = &checkpoints[checkpoints.len() - 1];
    let previous = &checkpoints[checkpoints.len() - 2];

    // The scale the numbers live on, taken from the fullest checkpoint: the
    // width of the middle 80% of the distribution.
    let spread = (last.p90 - last.p10).abs();

    let centre = relative_change(previous.mean, last.mean, spread).max(relative_change(
        previous.median,
        last.median,
        spread,
    ));
    let tails = relative_change(previous.p10, last.p10, spread).max(relative_change(
        previous.p90,
        last.p90,
        spread,
    ));

    let stable = centre <= CENTRE_TOLERANCE && tails <= TAIL_TOLERANCE;
    let warning = if stable {
        None
    } else {
        Some(format!(
            "Resultatet har inte stabiliserats: värdena ändrades med upp till \
             {:.1} % mellan {} och {} iterationer. Kör fler iterationer innan \
             siffrorna används som beslutsunderlag.",
            centre.max(tails) * 100.0,
            previous.iterations,
            last.iterations
        ))
    };

    Convergence {
        output_id: output_id.to_string(),
        checkpoints,
        stable,
        largest_relative_change: centre.max(tails),
        warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    #[test]
    fn checkpoints_climb_by_decades_and_end_at_the_total() {
        assert_eq!(checkpoints_for(1_000_000).first(), Some(&1_000));
        assert_eq!(checkpoints_for(1_000_000).last(), Some(&1_000_000));
        assert_eq!(
            checkpoints_for(100_000),
            vec![1_000, 5_000, 10_000, 50_000, 100_000]
        );
        assert_eq!(checkpoints_for(2_000), vec![1_000, 2_000]);
        // Below the first decade there is nothing to compare against.
        assert_eq!(checkpoints_for(500), vec![500]);
    }

    #[test]
    fn a_large_stable_run_is_reported_as_stable() {
        let mut rng = Rng::new(1);
        let samples: Vec<f64> = (0..200_000).map(|_| rng.standard_normal()).collect();
        let report = analyse("y", &samples);
        assert!(
            report.stable,
            "largest change was {}",
            report.largest_relative_change
        );
        assert!(report.warning.is_none());
        assert_eq!(report.checkpoints.last().unwrap().iterations, 200_000);
    }

    #[test]
    fn a_heavy_tailed_short_run_is_flagged_rather_than_reported_as_settled() {
        // A lognormal with a large sigma has a mean dominated by rare enormous
        // draws, which is exactly the case where a short run looks confident
        // and is not.
        let mut rng = Rng::new(2);
        let samples: Vec<f64> = (0..2_000)
            .map(|_| (rng.standard_normal() * 4.0).exp())
            .collect();
        let report = analyse("y", &samples);
        assert!(!report.stable);
        assert!(report.warning.unwrap().contains("stabiliserats"));
    }

    #[test]
    fn too_few_iterations_is_said_rather_than_guessed() {
        let samples = vec![1.0, 2.0, 3.0];
        let report = analyse("y", &samples);
        assert!(!report.stable);
        assert_eq!(report.checkpoints.len(), 1);
        assert!(report.warning.unwrap().contains("För få iterationer"));
    }

    #[test]
    fn a_constant_output_does_not_divide_by_zero() {
        let samples = vec![0.0; 20_000];
        let report = analyse("y", &samples);
        assert!(report.stable);
        assert_eq!(report.largest_relative_change, 0.0);
    }

    #[test]
    fn every_checkpoint_uses_only_the_iterations_it_names() {
        // The first thousand samples are all 1; the rest are all 1000. If a
        // checkpoint leaked later data, the 1k median would not be 1.
        let mut samples = vec![1.0; 1_000];
        samples.extend(std::iter::repeat_n(1_000.0, 9_000));
        let report = analyse("y", &samples);
        assert_eq!(report.checkpoints[0].iterations, 1_000);
        assert_eq!(report.checkpoints[0].median, 1.0);
        assert!(!report.stable);
    }
}

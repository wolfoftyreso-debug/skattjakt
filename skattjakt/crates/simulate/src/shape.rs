//! The visualisation payload.
//!
//! Section 16 asks for three things to be kept apart: the raw simulation data,
//! the statistical aggregates, and what a chart needs. This module is the
//! third. It turns ten million doubles into about four kilobytes — a histogram,
//! a density curve and a sampled cumulative distribution — which is what gets
//! stored and what gets sent to a browser.
//!
//! Sending the raw samples instead would be a 76 MB response for a chart 900
//! pixels wide. Nothing in the picture would change.

use serde::{Deserialize, Serialize};

use crate::stats::percentile;

/// How many histogram bars. Enough to show a shape, few enough to render as
/// discrete bars at phone width without becoming a smear.
pub const DEFAULT_BINS: usize = 48;
/// How many points to sample the cumulative distribution at. A CDF is smooth,
/// so this is about the resolution of a chart rather than of the data.
pub const CDF_POINTS: usize = 201;
/// The most samples the density estimate looks at. Its cost is
/// `points × samples`, and beyond this the curve stops changing.
const DENSITY_SAMPLE_LIMIT: usize = 20_000;

/// One histogram bar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bin {
    pub low: f64,
    pub high: f64,
    pub count: u64,
    /// Share of all outcomes in this bar, in `[0, 1]`.
    pub share: f64,
}

/// A point on the cumulative distribution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CdfPoint {
    pub value: f64,
    /// The probability of an outcome at most `value`.
    pub probability: f64,
}

/// Everything a chart needs, and nothing else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shape {
    pub output_id: String,
    pub bins: Vec<Bin>,
    /// A smoothed density at each bin centre, scaled to the same axis as
    /// `share`. Empty when the output is constant and there is nothing to
    /// smooth.
    pub density: Vec<f64>,
    pub cdf: Vec<CdfPoint>,
}

/// Builds the payload from a sorted slice.
pub fn build(output_id: &str, sorted: &[f64], bin_count: usize) -> Shape {
    let bin_count = bin_count.clamp(4, 512);
    if sorted.is_empty() {
        return Shape {
            output_id: output_id.to_string(),
            bins: Vec::new(),
            density: Vec::new(),
            cdf: Vec::new(),
        };
    }

    let low = sorted[0];
    let high = sorted[sorted.len() - 1];
    let total = sorted.len() as f64;

    // A constant output has no width to bin. One bar holding everything is the
    // truthful picture; splitting a zero-width range into 48 bars is a division
    // by zero dressed as a chart.
    //
    // The finiteness check comes first and is not redundant: a comparison
    // against a NaN is false in both directions, so `high > low` alone would
    // fall through to the binning arithmetic and produce a chart of NaNs.
    let has_width = (high - low).is_finite() && high > low;
    if !has_width {
        return Shape {
            output_id: output_id.to_string(),
            bins: vec![Bin {
                low,
                high,
                count: sorted.len() as u64,
                share: 1.0,
            }],
            density: Vec::new(),
            cdf: vec![
                CdfPoint {
                    value: low,
                    probability: 0.0,
                },
                CdfPoint {
                    value: low,
                    probability: 1.0,
                },
            ],
        };
    }

    let width = (high - low) / bin_count as f64;
    let mut counts = vec![0u64; bin_count];
    for value in sorted {
        // `min` rather than a branch: the maximum lands exactly on the upper
        // edge and would otherwise index one past the end.
        let index = (((value - low) / width) as usize).min(bin_count - 1);
        counts[index] += 1;
    }

    let bins: Vec<Bin> = counts
        .iter()
        .enumerate()
        .map(|(index, count)| Bin {
            low: low + width * index as f64,
            high: low + width * (index + 1) as f64,
            count: *count,
            share: *count as f64 / total,
        })
        .collect();

    let centres: Vec<f64> = bins.iter().map(|bin| (bin.low + bin.high) / 2.0).collect();
    let density = gaussian_density(sorted, &centres, width);

    let cdf = (0..CDF_POINTS)
        .map(|index| {
            let probability = index as f64 / (CDF_POINTS - 1) as f64;
            CdfPoint {
                value: percentile(sorted, probability),
                probability,
            }
        })
        .collect();

    Shape {
        output_id: output_id.to_string(),
        bins,
        density,
        cdf,
    }
}

/// A Gaussian kernel density estimate, evaluated at the bin centres and scaled
/// so it sits on the same axis as the bars.
///
/// Silverman's rule for the bandwidth, using the interquartile range as well as
/// the standard deviation: on a skewed or heavy-tailed output the standard
/// deviation alone picks a bandwidth wide enough to flatten the shape it was
/// meant to show.
fn gaussian_density(sorted: &[f64], centres: &[f64], bin_width: f64) -> Vec<f64> {
    let n = sorted.len();
    if n < 2 || centres.is_empty() {
        return Vec::new();
    }

    let std_dev = crate::stats::mean_and_variance(sorted).1.sqrt();
    let iqr = percentile(sorted, 0.75) - percentile(sorted, 0.25);
    let spread = if iqr > 0.0 {
        std_dev.min(iqr / 1.349).max(f64::MIN_POSITIVE)
    } else {
        std_dev
    };
    if !spread.is_finite() || spread <= 0.0 {
        return Vec::new();
    }
    let bandwidth = 0.9 * spread * (n as f64).powf(-0.2);
    if !bandwidth.is_finite() || bandwidth <= 0.0 {
        return Vec::new();
    }

    // Evenly spaced subsample when the run is large. Even spacing rather than
    // the first N: the samples are in iteration order, and a prefix of a
    // cancelled or partially-drained run would not represent the whole.
    let step = (n / DENSITY_SAMPLE_LIMIT).max(1);
    let used: Vec<f64> = sorted.iter().copied().step_by(step).collect();
    let used_count = used.len() as f64;

    let scale = bin_width / (bandwidth * (2.0 * std::f64::consts::PI).sqrt() * used_count);
    centres
        .iter()
        .map(|centre| {
            let sum: f64 = used
                .iter()
                .map(|value| {
                    let z = (centre - value) / bandwidth;
                    // Beyond four bandwidths the kernel contributes less than
                    // 0.03% and the exp is the expensive part of the loop.
                    if z.abs() > 4.0 {
                        0.0
                    } else {
                        (-0.5 * z * z).exp()
                    }
                })
                .sum();
            sum * scale
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    fn sorted_normal(n: usize, seed: u64) -> Vec<f64> {
        let mut rng = Rng::new(seed);
        let mut samples: Vec<f64> = (0..n).map(|_| rng.standard_normal()).collect();
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        samples
    }

    #[test]
    fn every_sample_lands_in_exactly_one_bin() {
        let samples = sorted_normal(50_000, 1);
        let shape = build("y", &samples, DEFAULT_BINS);
        let counted: u64 = shape.bins.iter().map(|bin| bin.count).sum();
        assert_eq!(counted, 50_000);
        let shares: f64 = shape.bins.iter().map(|bin| bin.share).sum();
        assert!((shares - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_bins_span_the_data_and_touch_at_the_edges() {
        let samples = sorted_normal(10_000, 2);
        let shape = build("y", &samples, DEFAULT_BINS);
        assert!((shape.bins[0].low - samples[0]).abs() < 1e-9);
        assert!((shape.bins[shape.bins.len() - 1].high - samples[samples.len() - 1]).abs() < 1e-9);
        for window in shape.bins.windows(2) {
            assert!((window[0].high - window[1].low).abs() < 1e-9);
        }
    }

    #[test]
    fn the_maximum_lands_in_the_last_bin_rather_than_past_the_end() {
        let samples = vec![0.0, 1.0, 2.0, 3.0, 4.0];
        let shape = build("y", &samples, 4);
        assert_eq!(shape.bins.len(), 4);
        assert_eq!(shape.bins[3].count, 2); // 3.0 and 4.0
    }

    #[test]
    fn the_cumulative_curve_never_goes_backwards() {
        let samples = sorted_normal(20_000, 3);
        let shape = build("y", &samples, DEFAULT_BINS);
        assert_eq!(shape.cdf.len(), CDF_POINTS);
        assert_eq!(shape.cdf[0].probability, 0.0);
        assert_eq!(shape.cdf[CDF_POINTS - 1].probability, 1.0);
        for window in shape.cdf.windows(2) {
            assert!(window[1].value >= window[0].value);
            assert!(window[1].probability > window[0].probability);
        }
    }

    #[test]
    fn a_constant_output_is_one_bar_rather_than_a_division_by_zero() {
        let samples = vec![7.0; 1000];
        let shape = build("y", &samples, DEFAULT_BINS);
        assert_eq!(shape.bins.len(), 1);
        assert_eq!(shape.bins[0].count, 1000);
        assert_eq!(shape.bins[0].share, 1.0);
        assert!(shape.density.is_empty());
        assert!(shape.cdf.iter().all(|point| point.value == 7.0));
    }

    #[test]
    fn an_empty_run_produces_an_empty_shape_rather_than_nan() {
        let shape = build("y", &[], DEFAULT_BINS);
        assert!(shape.bins.is_empty());
        assert!(shape.cdf.is_empty());
    }

    #[test]
    fn the_density_tracks_the_histogram() {
        // Both are estimates of the same thing on the same scale, so the peak
        // should land in roughly the same place.
        let samples = sorted_normal(50_000, 4);
        let shape = build("y", &samples, DEFAULT_BINS);
        assert_eq!(shape.density.len(), shape.bins.len());

        let peak_bar = shape
            .bins
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.share.partial_cmp(&b.1.share).unwrap())
            .unwrap()
            .0;
        let peak_curve = shape
            .density
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;
        assert!(
            peak_bar.abs_diff(peak_curve) <= 2,
            "histogram peaked at bin {peak_bar}, density at {peak_curve}"
        );

        let area: f64 = shape.density.iter().sum();
        assert!(
            (area - 1.0).abs() < 0.05,
            "the density integrated to {area}"
        );
    }

    #[test]
    fn the_payload_is_small_regardless_of_the_run() {
        let samples = sorted_normal(500_000, 5);
        let shape = build("y", &samples, DEFAULT_BINS);
        let bytes = serde_json::to_vec(&shape).unwrap().len();
        assert!(
            bytes < 32_000,
            "the visualisation payload was {bytes} bytes"
        );
    }
}

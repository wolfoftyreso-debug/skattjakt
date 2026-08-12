//! Which inputs actually drive the answer.
//!
//! Three measures, because one is not enough and each fails differently.
//!
//! **Pearson correlation** measures a straight-line relationship. It is the
//! familiar one and it is the one that misleads: an input with a strong but
//! curved effect — a threshold, a cap, anything with an `if` in it — can show a
//! Pearson correlation near zero while dominating the outcome.
//!
//! **Spearman rank correlation** measures whether the output rises when the
//! input rises, of whatever shape. It survives non-linearity and outliers, and
//! it is the one to read when the two disagree.
//!
//! **Contribution to variance** turns the rank correlations into shares that
//! sum to one, which is the form the question is usually asked in: "how much of
//! the uncertainty comes from this?" It is a decomposition of the *rank*
//! correlations and assumes the inputs are independent of one another — true
//! here, because each input is drawn from its own stream, and stated because it
//! would not be true of a model with correlated inputs.
//!
//! An input whose samples never vary has no correlation with anything, and the
//! answer is reported as unknown rather than as zero. Zero would read as "this
//! does not matter", and the truth is "this run cannot tell you".

use serde::{Deserialize, Serialize};

/// One input's influence on one output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputSensitivity {
    pub input_id: String,
    pub input_name: String,
    /// `null` when either side has no variation, so no correlation exists.
    ///
    /// Explicit rather than omitted: an absent key reads as `undefined` in a
    /// client and the statement "this run cannot tell you" is lost. Zero would
    /// be worse still — it reads as "measured, and it does not matter".
    pub correlation: Option<f64>,
    pub rank_correlation: Option<f64>,
    /// Share of the output's variance attributable to this input, in `[0, 1]`.
    pub variance_contribution: f64,
    /// 1 is the most influential.
    pub rank: u32,
    /// Whether the output's expression reads this input at all.
    pub referenced: bool,
}

/// The sensitivity of one output to every input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sensitivity {
    pub output_id: String,
    /// How many iterations the correlations were computed over. Not always the
    /// whole run — see `SENSITIVITY_SAMPLE` in the engine.
    pub sample_size: u32,
    pub inputs: Vec<InputSensitivity>,
    /// Set when no input has any measurable influence, which happens when the
    /// output is constant. Without it the caller sees a list of zeroes and no
    /// explanation.
    pub note: Option<String>,
}

/// Pearson's r. `None` when either series is constant.
pub fn correlation(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() != ys.len() || xs.len() < 2 {
        return None;
    }
    let n = xs.len() as f64;
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;

    let mut covariance = 0.0;
    let mut variance_x = 0.0;
    let mut variance_y = 0.0;
    for (x, y) in xs.iter().zip(ys) {
        let dx = x - mean_x;
        let dy = y - mean_y;
        covariance += dx * dy;
        variance_x += dx * dx;
        variance_y += dy * dy;
    }
    if variance_x <= 0.0 || variance_y <= 0.0 {
        return None;
    }
    let r = covariance / (variance_x * variance_y).sqrt();
    // Floating-point error can push a perfect correlation a hair past 1, and a
    // reported 1.0000000002 looks like a bug in a report.
    Some(r.clamp(-1.0, 1.0))
}

/// Fractional ranks, averaging ties.
///
/// Ties matter here rather than being a technicality: a Bernoulli input has two
/// distinct values and half a million ties, and ranking them arbitrarily would
/// invent an ordering that is not in the data.
fn ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| {
        values[*a]
            .partial_cmp(&values[*b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut result = vec![0.0; values.len()];
    let mut index = 0;
    while index < order.len() {
        let mut end = index + 1;
        while end < order.len() && values[order[end]] == values[order[index]] {
            end += 1;
        }
        // Average rank for the whole tied block.
        let average = ((index + end - 1) as f64) / 2.0 + 1.0;
        for slot in &order[index..end] {
            result[*slot] = average;
        }
        index = end;
    }
    result
}

/// Spearman's rho: Pearson on the ranks.
pub fn rank_correlation(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() != ys.len() || xs.len() < 2 {
        return None;
    }
    correlation(&ranks(xs), &ranks(ys))
}

/// Builds the sensitivity report for one output.
///
/// `input_samples` holds one vector per input, all the same length as
/// `output_samples`, and all in the iteration order they were drawn in — the
/// pairing between an input's value and the outcome it produced is the whole
/// measurement, so any reordering would silently destroy it.
pub fn analyse(
    output_id: &str,
    input_ids: &[(String, String)],
    input_samples: &[Vec<f64>],
    output_samples: &[f64],
    referenced: &[bool],
) -> Sensitivity {
    let sample_size = output_samples.len() as u32;
    let mut entries: Vec<InputSensitivity> = Vec::with_capacity(input_ids.len());

    for (index, (id, name)) in input_ids.iter().enumerate() {
        let is_referenced = referenced.get(index).copied().unwrap_or(true);
        // An input the expression never reads cannot influence the output, and
        // a finite sample will always show some spurious correlation for it.
        // Reporting that number would be reporting noise as a finding.
        let (pearson, spearman) = if is_referenced {
            (
                correlation(&input_samples[index], output_samples),
                rank_correlation(&input_samples[index], output_samples),
            )
        } else {
            (None, None)
        };
        entries.push(InputSensitivity {
            input_id: id.clone(),
            input_name: name.clone(),
            correlation: pearson,
            rank_correlation: spearman,
            variance_contribution: 0.0,
            rank: 0,
            referenced: is_referenced,
        });
    }

    // Shares of the squared rank correlations. Normalising makes them a
    // decomposition rather than a set of unrelated coefficients, which is what
    // "42% of the uncertainty" means.
    let total: f64 = entries
        .iter()
        .map(|entry| entry.rank_correlation.unwrap_or(0.0).powi(2))
        .sum();

    let note = if total <= 0.0 {
        Some(
            "Ingen indata har mätbar påverkan på detta utfall i den här körningen. \
             Det inträffar när utfallet är konstant eller när all variation ligger \
             utanför modellen."
                .to_string(),
        )
    } else {
        for entry in &mut entries {
            entry.variance_contribution = entry.rank_correlation.unwrap_or(0.0).powi(2) / total;
        }
        None
    };

    entries.sort_by(|a, b| {
        b.variance_contribution
            .partial_cmp(&a.variance_contribution)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.input_id.cmp(&b.input_id))
    });
    for (index, entry) in entries.iter_mut().enumerate() {
        entry.rank = index as u32 + 1;
    }

    Sensitivity {
        output_id: output_id.to_string(),
        sample_size,
        inputs: entries,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_perfect_line_correlates_perfectly() {
        let xs: Vec<f64> = (0..100).map(f64::from).collect();
        let ys: Vec<f64> = xs.iter().map(|x| 3.0 * x + 7.0).collect();
        assert!((correlation(&xs, &ys).unwrap() - 1.0).abs() < 1e-12);
        assert!((rank_correlation(&xs, &ys).unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_perfect_inverse_line_correlates_minus_one() {
        let xs: Vec<f64> = (0..100).map(f64::from).collect();
        let ys: Vec<f64> = xs.iter().map(|x| -2.0 * x).collect();
        assert!((correlation(&xs, &ys).unwrap() + 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_constant_series_has_no_correlation_rather_than_zero() {
        let xs = vec![5.0; 100];
        let ys: Vec<f64> = (0..100).map(f64::from).collect();
        assert_eq!(correlation(&xs, &ys), None);
        assert_eq!(rank_correlation(&xs, &ys), None);
    }

    #[test]
    fn rank_correlation_sees_a_monotone_curve_that_pearson_understates() {
        // y = x^5 on [0, 1]: monotone, so Spearman is exactly 1, while Pearson
        // is well below it. This is the case the module exists for.
        let xs: Vec<f64> = (0..500).map(|i| f64::from(i) / 500.0).collect();
        let ys: Vec<f64> = xs.iter().map(|x| x.powi(5)).collect();
        let pearson = correlation(&xs, &ys).unwrap();
        let spearman = rank_correlation(&xs, &ys).unwrap();
        assert!((spearman - 1.0).abs() < 1e-9, "spearman was {spearman}");
        assert!(
            pearson < 0.9,
            "pearson was {pearson}, expected well below 1"
        );
    }

    #[test]
    fn ties_are_ranked_by_their_average() {
        // Values 1, 2, 2, 3 → ranks 1, 2.5, 2.5, 4.
        assert_eq!(ranks(&[1.0, 2.0, 2.0, 3.0]), vec![1.0, 2.5, 2.5, 4.0]);
        // All tied → all the same rank, and no correlation with anything.
        assert_eq!(ranks(&[7.0; 3]), vec![2.0, 2.0, 2.0]);
    }

    #[test]
    fn contributions_sum_to_one_and_rank_in_order() {
        let n = 2000;
        let a: Vec<f64> = (0..n).map(|i| f64::from(i % 97)).collect();
        let b: Vec<f64> = (0..n).map(|i| f64::from(i % 13)).collect();
        // The output leans much harder on `a` than on `b`.
        let y: Vec<f64> = (0..n as usize).map(|i| 10.0 * a[i] + 0.1 * b[i]).collect();

        let report = analyse(
            "y",
            &[("a".into(), "A".into()), ("b".into(), "B".into())],
            &[a, b],
            &y,
            &[true, true],
        );

        let total: f64 = report
            .inputs
            .iter()
            .map(|entry| entry.variance_contribution)
            .sum();
        assert!(
            (total - 1.0).abs() < 1e-9,
            "contributions summed to {total}"
        );
        assert_eq!(report.inputs[0].input_id, "a");
        assert_eq!(report.inputs[0].rank, 1);
        assert!(report.inputs[0].variance_contribution > report.inputs[1].variance_contribution);
        assert!(report.note.is_none());
    }

    #[test]
    fn an_input_the_output_never_reads_is_reported_as_unreferenced() {
        let n = 500;
        let a: Vec<f64> = (0..n).map(f64::from).collect();
        let unused: Vec<f64> = (0..n).map(|i| f64::from((i * 7919) % 101)).collect();
        let y: Vec<f64> = a.clone();

        let report = analyse(
            "y",
            &[("a".into(), "A".into()), ("unused".into(), "Unused".into())],
            &[a, unused],
            &y,
            &[true, false],
        );

        let unused_entry = report
            .inputs
            .iter()
            .find(|entry| entry.input_id == "unused")
            .unwrap();
        assert!(!unused_entry.referenced);
        assert_eq!(unused_entry.correlation, None);
        assert_eq!(unused_entry.variance_contribution, 0.0);
    }

    #[test]
    fn a_constant_output_says_so_rather_than_listing_zeroes() {
        let n = 200;
        let a: Vec<f64> = (0..n).map(f64::from).collect();
        let y = vec![42.0; n as usize];
        let report = analyse("y", &[("a".into(), "A".into())], &[a], &y, &[true]);
        assert!(report.note.is_some());
        assert_eq!(report.inputs[0].variance_contribution, 0.0);
    }
}

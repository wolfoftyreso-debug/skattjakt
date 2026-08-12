//! # skattjakt-simulate
//!
//! A Monte Carlo engine: distributions, a calculation model over them, and the
//! analysis of what comes out — statistics, sensitivity, convergence and the
//! shape a chart needs.
//!
//! It is a general layer rather than a feature of one screen. Nothing in here
//! knows about tax, about companies, or about HTTP. It has no clock, no I/O, no
//! runtime and no database; a run is a pure function of a specification, a seed
//! and an iteration count. That is what makes the whole thing testable without
//! a process, and reproducible without a recording.
//!
//! ## What it is not
//!
//! It is not a forecaster. A result describes the *model* — the distributions
//! someone chose and the arithmetic they wrote — and nothing else. Every
//! outcome carries [`engine::DISCLAIMER`] for that reason, and the API and the
//! interface both display it. Section 15 of the build constitution is explicit
//! that a probability must never be rendered as a certainty, and the shape of
//! this crate's output is built to make that the easy path: probabilities are
//! `Option` where they are undefined, correlations are `None` rather than zero
//! where they cannot be computed, and an unconverged run carries its own
//! warning text rather than leaving the caller to notice.
//!
//! ## The relationship to money in this product
//!
//! Skattjakt's domain type for money is [`MoneyRange`][skattjakt_core] — an
//! interval, because no type in the product is allowed to express a
//! single-figure tax saving. This crate deals in `f64` and produces
//! distributions, which is a *stronger* statement of uncertainty than an
//! interval rather than a weaker one, and the two must not be quietly bridged:
//! a simulated P50 is not evidence, and nothing here may become the amount on a
//! finding. The API keeps them in separate resources for exactly that reason.
//!
//! ## Layout
//!
//! | Module | What it owns |
//! |---|---|
//! | [`rng`] | The deterministic generator, and the per-input streams |
//! | [`distribution`] | Eleven distributions: validation, sampling, moments |
//! | [`expr`] | The expression language an output is written in |
//! | [`spec`] | Inputs, outputs, constraints, compilation, the model hash |
//! | [`engine`] | The run loop, cancellation, quality checks |
//! | [`stats`] | Percentiles, moments, probabilities, confidence intervals |
//! | [`sensitivity`] | Correlation, rank correlation, contribution to variance |
//! | [`convergence`] | Whether the run has settled |
//! | [`shape`] | Histogram, density and CDF — the visualisation payload |

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod convergence;
pub mod distribution;
pub mod engine;
pub mod expr;
pub mod rng;
pub mod sensitivity;
pub mod shape;
pub mod spec;
pub mod stats;

pub use convergence::{Checkpoint, Convergence};
pub use distribution::{Distribution, DistributionError, Moments, Sampler, MAX_CATEGORIES};
pub use engine::{
    run, EngineError, Quality, RunControl, RunOutcome, DISCLAIMER, ENGINE_VERSION,
    MAX_SAMPLE_CELLS, SENSITIVITY_SAMPLE,
};
pub use expr::{ExprError, Expression};
pub use sensitivity::{InputSensitivity, Sensitivity};
pub use shape::{Bin, CdfPoint, Shape};
pub use spec::{
    CompiledSpec, Confidence, ConstraintMode, Constraints, Input, InputSummary, Output, RunConfig,
    SimulationSpec, SpecError, TargetDirection, MAX_INPUTS, MAX_ITERATIONS, MAX_OUTPUTS,
    MIN_ITERATIONS,
};
pub use stats::Statistics;

/// The catalogue the interface offers when someone adds an input.
///
/// Returned by the API so a client does not carry its own copy of the list —
/// a duplicated catalogue is one that drifts, and a client offering a
/// distribution the engine does not have is a 422 the user cannot explain.
pub fn catalogue() -> Vec<serde_json::Value> {
    use serde_json::json;

    let examples = [
        Distribution::Normal {
            mean: 0.0,
            std_dev: 1.0,
        },
        Distribution::Lognormal {
            log_mean: 0.0,
            log_std_dev: 0.5,
        },
        Distribution::Uniform {
            low: 0.0,
            high: 1.0,
        },
        Distribution::Triangular {
            low: 0.0,
            mode: 0.5,
            high: 1.0,
        },
        Distribution::Beta {
            alpha: 2.0,
            beta: 2.0,
            low: 0.0,
            high: 1.0,
        },
        Distribution::Exponential { rate: 1.0 },
        Distribution::Poisson { lambda: 1.0 },
        Distribution::Bernoulli { p: 0.5 },
        Distribution::Binomial { trials: 10, p: 0.5 },
        Distribution::Discrete {
            values: vec![0.0, 1.0],
            weights: vec![1.0, 1.0],
        },
        Distribution::Custom {
            points: vec![(0.0, 0.0), (1.0, 1.0)],
        },
    ];

    examples
        .iter()
        .map(|distribution| {
            let (label, guidance) = distribution.label();
            json!({
                "kind": distribution.kind(),
                "label": label,
                "guidance": guidance,
                "parameters": distribution
                    .parameters()
                    .into_iter()
                    .map(|(name, value)| json!({"name": name, "example": value}))
                    .collect::<Vec<_>>(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_covers_every_distribution_the_engine_can_run() {
        let catalogue = catalogue();
        assert_eq!(
            catalogue.len(),
            11,
            "the specification requires eleven distributions"
        );
        let kinds: Vec<&str> = catalogue
            .iter()
            .map(|entry| entry["kind"].as_str().unwrap())
            .collect();
        for expected in [
            "normal",
            "lognormal",
            "uniform",
            "triangular",
            "beta",
            "exponential",
            "poisson",
            "bernoulli",
            "binomial",
            "discrete",
            "custom",
        ] {
            assert!(kinds.contains(&expected), "{expected} is missing");
        }
    }

    #[test]
    fn every_catalogue_entry_carries_guidance_and_a_valid_example() {
        for entry in catalogue() {
            assert!(!entry["label"].as_str().unwrap().is_empty());
            assert!(!entry["guidance"].as_str().unwrap().is_empty());
            assert!(!entry["parameters"].as_array().unwrap().is_empty());
        }
    }
}

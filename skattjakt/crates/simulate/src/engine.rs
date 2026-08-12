//! The simulation engine.
//!
//! Draw, calculate, record — a few million times — then hand the samples to the
//! statistics, sensitivity, convergence and shape modules. The loop itself is
//! deliberately boring; everything interesting is in what surrounds it:
//!
//! **Per-input streams.** Every input draws from a generator seeded from the
//! run's seed and the input's own identifier. Adding, removing or reordering an
//! input therefore leaves every other input's numbers untouched, and a stored
//! seed keeps meaning what it meant. A single shared stream would make every
//! historical run irreproducible the first time somebody added a variable.
//!
//! **Batching.** Progress and cancellation are checked once per batch rather
//! than once per iteration. At a million iterations, an atomic load in the
//! inner loop costs more than the arithmetic it guards.
//!
//! **Bounded memory.** Output samples are kept in full, because exact
//! percentiles are worth it. Input samples are kept only for the first
//! `SENSITIVITY_SAMPLE` iterations, because correlation over a hundred thousand
//! independent draws is already precise to three decimals and the other 9.9
//! million would cost gigabytes to learn nothing.
//!
//! **Nothing invalid is returned.** A NaN or an infinity anywhere in an output
//! fails the run and names the iteration that produced it. Section 11: the
//! engine must never quietly produce a statistically or numerically invalid
//! result, and a mean over a vector containing one infinity is exactly that.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::convergence::{self, Convergence};
use crate::rng::Rng;
use crate::sensitivity::{self, Sensitivity};
use crate::shape::{self, Shape};
use crate::spec::{CompiledSpec, ConstraintMode, RunConfig, SpecError};
use crate::stats::Statistics;

/// The engine's own version, part of the reproducibility record.
///
/// **Bump this whenever a change could alter the numbers a given seed
/// produces** — a different sampler, a changed rejection bound, a reordered
/// draw. A stored run records the version that produced it, so a result from an
/// older engine is marked as unreproducible on this one rather than silently
/// recomputed into different figures.
pub const ENGINE_VERSION: &str = "1.0.0";

/// Iterations between progress reports and cancellation checks.
const BATCH: u32 = 4_096;

/// How many iterations of input values are retained for sensitivity analysis.
///
/// The standard error of a correlation estimated from n samples is about
/// `1/√n`, so a hundred thousand gives roughly ±0.003 — three decimal places on
/// a number reported as a percentage. Ten million would give ±0.0003, at a
/// hundred times the memory.
pub const SENSITIVITY_SAMPLE: u32 = 100_000;

/// The most sample cells — iterations × outputs — one run may hold.
///
/// The bound that stops a request from choosing how much memory a worker uses.
/// At eight bytes a cell this is about 190 MB of samples, alongside which the
/// process holds a database connection and little else.
pub const MAX_SAMPLE_CELLS: u64 = 24_000_000;

/// How many times a constrained input may be redrawn before the run gives up.
const MAX_CONSTRAINT_ATTEMPTS: u32 = 1_000;

/// Why a run did not produce a result.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EngineError {
    #[error("the specification is not runnable: {0}")]
    Spec(#[from] SpecError),
    #[error(
        "output {output:?} produced {kind} at iteration {iteration}; a statistic computed \
         over it would be meaningless, so the run failed instead"
    )]
    NonFinite {
        output: String,
        iteration: u32,
        kind: &'static str,
    },
    #[error(
        "input {input:?} could not satisfy its constraints in {attempts} attempts at \
         iteration {iteration}; the permitted range holds almost none of the distribution"
    )]
    ConstraintUnsatisfiable {
        input: String,
        iteration: u32,
        attempts: u32,
    },
    #[error(
        "{iterations} iterations of {outputs} outputs needs {cells} sample values, and the \
         engine holds at most {limit}; reduce the iteration count or the number of outputs"
    )]
    TooLarge {
        iterations: u32,
        outputs: usize,
        cells: u64,
        limit: u64,
    },
    #[error("the run was cancelled after {completed} of {total} iterations")]
    Cancelled { completed: u32, total: u32 },
}

/// How a caller watches and stops a run.
///
/// Deliberately not a channel and not `async`: this crate has no runtime and no
/// I/O, which is what lets the whole engine be tested without one. The worker
/// wraps these in whatever it needs.
#[derive(Debug, Default)]
pub struct RunControl {
    cancelled: AtomicBool,
    completed: AtomicU32,
}

impl RunControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Asks the run to stop at the end of the current batch.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Iterations finished so far. Readable from another thread while the run
    /// is in progress; this is what a progress bar reads.
    pub fn completed(&self) -> u32 {
        self.completed.load(Ordering::Relaxed)
    }

    fn record(&self, completed: u32) {
        self.completed.store(completed, Ordering::Relaxed);
    }
}

/// What the engine noticed while running.
///
/// Reported alongside the result rather than logged and forgotten: a run where
/// a third of the draws were rejected by a constraint is a run whose input
/// distribution is not what its author thinks it is.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Quality {
    /// Draws discarded because they fell outside an input's constraints.
    pub constraint_resamples: u64,
    /// Draws moved to a bound by a clamping constraint.
    pub clamped_samples: u64,
    /// Notes for the reader, in Swedish, ready to display.
    pub warnings: Vec<String>,
}

/// A finished run.
#[derive(Debug, Clone, Serialize)]
pub struct RunOutcome {
    pub engine_version: &'static str,
    pub spec_hash: String,
    pub seed: u64,
    pub iterations: u32,
    pub duration_ms: u64,
    pub iterations_per_second: f64,
    pub statistics: Vec<(String, Statistics)>,
    pub sensitivity: Vec<Sensitivity>,
    pub convergence: Vec<Convergence>,
    pub shapes: Vec<Shape>,
    pub quality: Quality,
    /// The sentence that must accompany every result. Not advice, not a
    /// forecast — an arithmetic consequence of the assumptions someone typed
    /// in. Section 15 is the reason it is returned by the engine rather than
    /// left to whichever screen happens to render the numbers.
    pub disclaimer: &'static str,
}

pub const DISCLAIMER: &str = "Resultatet är en simulering av de antaganden och \
    sannolikhetsfördelningar som angetts, inte en prognos och inte ett facit. \
    Sannolikheter beskriver modellen, inte verkligheten.";

/// Runs a compiled model.
///
/// `elapsed_ms` is passed in rather than measured here so the crate stays free
/// of a clock — which is also what lets the determinism test assert that two
/// runs are byte-identical.
pub fn run(
    compiled: &CompiledSpec,
    config: RunConfig,
    control: &RunControl,
) -> Result<RunOutcome, EngineError> {
    config.validate()?;

    let inputs = &compiled.spec.inputs;
    let outputs = &compiled.spec.outputs;
    let iterations = config.iterations;

    let cells = u64::from(iterations) * outputs.len() as u64;
    if cells > MAX_SAMPLE_CELLS {
        return Err(EngineError::TooLarge {
            iterations,
            outputs: outputs.len(),
            cells,
            limit: MAX_SAMPLE_CELLS,
        });
    }

    let started = std::time::Instant::now();

    // One generator per input, each seeded from the run's seed and the input's
    // identifier.
    let mut generators: Vec<Rng> = inputs
        .iter()
        .map(|input| Rng::for_stream(config.seed, &input.id))
        .collect();

    let mut output_samples: Vec<Vec<f64>> = outputs
        .iter()
        .map(|_| Vec::with_capacity(iterations as usize))
        .collect();

    let retained = iterations.min(SENSITIVITY_SAMPLE) as usize;
    let mut input_samples: Vec<Vec<f64>> = inputs
        .iter()
        .map(|_| Vec::with_capacity(retained))
        .collect();

    let mut values = vec![0.0_f64; compiled.slots()];
    let mut quality = Quality::default();

    let mut iteration = 0u32;
    while iteration < iterations {
        let batch_end = (iteration + BATCH).min(iterations);

        while iteration < batch_end {
            for (index, input) in inputs.iter().enumerate() {
                let rng = &mut generators[index];
                let mut sample = input.distribution.sample(rng);

                if let Some(constraints) = input.constraints {
                    match constraints.mode {
                        ConstraintMode::Resample => {
                            let mut attempts = 0;
                            while !constraints.permits(sample) {
                                attempts += 1;
                                if attempts >= MAX_CONSTRAINT_ATTEMPTS {
                                    return Err(EngineError::ConstraintUnsatisfiable {
                                        input: input.id.clone(),
                                        iteration,
                                        attempts,
                                    });
                                }
                                sample = input.distribution.sample(rng);
                            }
                            quality.constraint_resamples += u64::from(attempts);
                        }
                        ConstraintMode::Clamp => {
                            if !constraints.permits(sample) {
                                sample = constraints.clamp(sample);
                                quality.clamped_samples += 1;
                            }
                        }
                    }
                }

                values[index] = sample;
                if (iteration as usize) < retained {
                    input_samples[index].push(sample);
                }
            }

            for (index, expression) in compiled.expressions().iter().enumerate() {
                let value = expression.evaluate(&values);
                if !value.is_finite() {
                    return Err(EngineError::NonFinite {
                        output: outputs[index].id.clone(),
                        iteration,
                        kind: if value.is_nan() {
                            "a value that is not a number"
                        } else {
                            "an infinite value"
                        },
                    });
                }
                values[compiled.output_slot(index)] = value;
                output_samples[index].push(value);
            }

            iteration += 1;
        }

        control.record(iteration);
        if control.is_cancelled() && iteration < iterations {
            return Err(EngineError::Cancelled {
                completed: iteration,
                total: iterations,
            });
        }
    }

    if quality.constraint_resamples > u64::from(iterations) / 2 {
        quality.warnings.push(format!(
            "{} dragningar förkastades av villkoren, vilket är fler än en per två \
             iterationer. Den faktiska fördelningen avviker då kraftigt från den \
             angivna — överväg att ange fördelningen så att villkoren sällan behövs.",
            quality.constraint_resamples
        ));
    }
    if quality.clamped_samples * 10 > u64::from(iterations) {
        quality.warnings.push(format!(
            "{} dragningar flyttades till en gräns. Sannolikhetsmassa har därmed \
             samlats på gränsvärdena, vilket syns som staplar i ytterkanterna.",
            quality.clamped_samples
        ));
    }

    // Everything below works on finished vectors. Sorting a copy leaves the
    // originals in iteration order, which the sensitivity pairing and the
    // convergence prefixes both depend on.
    let mut statistics = Vec::with_capacity(outputs.len());
    let mut shapes = Vec::with_capacity(outputs.len());
    let mut convergence_reports = Vec::with_capacity(outputs.len());
    let mut sensitivity_reports = Vec::with_capacity(outputs.len());

    let input_identity: Vec<(String, String)> = inputs
        .iter()
        .map(|input| (input.id.clone(), input.name.clone()))
        .collect();

    for (index, output) in outputs.iter().enumerate() {
        let samples = &output_samples[index];

        let mut sorted = samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        statistics.push((output.id.clone(), Statistics::compute(&sorted, output)));
        shapes.push(shape::build(&output.id, &sorted, shape::DEFAULT_BINS));
        convergence_reports.push(convergence::analyse(&output.id, samples));

        // Which inputs this output's expression can see. Transitive through
        // earlier outputs: `profit = revenue - costs` reads `revenue`, which
        // reads `customers`, so `customers` influences `profit`.
        let referenced = reachable_inputs(compiled, index);
        sensitivity_reports.push(sensitivity::analyse(
            &output.id,
            &input_identity,
            &input_samples,
            &samples[..retained.min(samples.len())],
            &referenced,
        ));
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    let iterations_per_second = if duration_ms > 0 {
        f64::from(iterations) * 1000.0 / duration_ms as f64
    } else {
        f64::from(iterations) * 1000.0
    };

    for report in &convergence_reports {
        if let Some(warning) = &report.warning {
            quality.warnings.push(warning.clone());
        }
    }

    Ok(RunOutcome {
        engine_version: ENGINE_VERSION,
        spec_hash: compiled.spec.hash(),
        seed: config.seed,
        iterations,
        duration_ms,
        iterations_per_second,
        statistics,
        sensitivity: sensitivity_reports,
        convergence: convergence_reports,
        shapes,
        quality,
        disclaimer: DISCLAIMER,
    })
}

/// Which inputs an output depends on, following references through earlier
/// outputs.
fn reachable_inputs(compiled: &CompiledSpec, output_index: usize) -> Vec<bool> {
    let input_count = compiled.spec.inputs.len();
    let mut reached = vec![false; input_count];
    let mut pending = vec![output_index];
    let mut seen_outputs = vec![false; compiled.expressions().len()];

    while let Some(index) = pending.pop() {
        if seen_outputs[index] {
            continue;
        }
        seen_outputs[index] = true;
        for slot in compiled.expressions()[index].referenced_slots() {
            if *slot < input_count {
                reached[*slot] = true;
            } else {
                pending.push(slot - input_count);
            }
        }
    }
    reached
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distribution::Distribution;
    use crate::spec::{Constraints, Input, Output, SimulationSpec, TargetDirection};

    fn input(id: &str, distribution: Distribution) -> Input {
        Input {
            id: id.to_string(),
            name: id.to_string(),
            distribution,
            unit: None,
            source: None,
            confidence: None,
            description: None,
            constraints: None,
        }
    }

    fn output(id: &str, expression: &str) -> Output {
        Output {
            id: id.to_string(),
            name: id.to_string(),
            expression: expression.to_string(),
            unit: None,
            description: None,
            target: None,
            target_direction: TargetDirection::AtLeast,
            critical_threshold: None,
        }
    }

    fn business_model() -> SimulationSpec {
        SimulationSpec {
            name: "Resultat".into(),
            description: None,
            inputs: vec![
                input(
                    "customers",
                    Distribution::Normal {
                        mean: 1_000.0,
                        std_dev: 120.0,
                    },
                ),
                input(
                    "average_revenue",
                    Distribution::Triangular {
                        low: 700.0,
                        mode: 850.0,
                        high: 1_100.0,
                    },
                ),
                input(
                    "costs",
                    Distribution::Uniform {
                        low: 500_000.0,
                        high: 700_000.0,
                    },
                ),
            ],
            outputs: vec![
                output("revenue", "customers * average_revenue"),
                Output {
                    target: Some(200_000.0),
                    critical_threshold: Some(0.0),
                    ..output("profit", "revenue - costs")
                },
            ],
        }
    }

    fn run_default(spec: &SimulationSpec, iterations: u32, seed: u64) -> RunOutcome {
        let compiled = spec.compile().expect("compiles");
        let control = RunControl::new();
        run(&compiled, RunConfig { iterations, seed }, &control).expect("runs")
    }

    #[test]
    fn a_run_produces_a_result_for_every_output() {
        let outcome = run_default(&business_model(), 20_000, 1);
        assert_eq!(outcome.statistics.len(), 2);
        assert_eq!(outcome.shapes.len(), 2);
        assert_eq!(outcome.sensitivity.len(), 2);
        assert_eq!(outcome.convergence.len(), 2);
        assert_eq!(outcome.iterations, 20_000);
        assert_eq!(outcome.engine_version, ENGINE_VERSION);
        assert!(!outcome.disclaimer.is_empty());
    }

    /// Section 12. The whole reproducibility promise, in one assertion.
    #[test]
    fn the_same_seed_reproduces_the_same_result() {
        let spec = business_model();
        let first = run_default(&spec, 20_000, 4242);
        let second = run_default(&spec, 20_000, 4242);
        assert_eq!(first.spec_hash, second.spec_hash);
        assert_eq!(
            serde_json::to_string(&first.statistics).unwrap(),
            serde_json::to_string(&second.statistics).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&first.shapes).unwrap(),
            serde_json::to_string(&second.shapes).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&first.sensitivity).unwrap(),
            serde_json::to_string(&second.sensitivity).unwrap()
        );
    }

    #[test]
    fn a_different_seed_gives_a_different_result() {
        let spec = business_model();
        let first = run_default(&spec, 20_000, 1);
        let second = run_default(&spec, 20_000, 2);
        assert_ne!(first.statistics[1].1.mean, second.statistics[1].1.mean);
    }

    /// The property that lets a model be edited without invalidating history.
    #[test]
    fn adding_an_input_does_not_move_the_others() {
        let spec = business_model();
        let before = run_default(&spec, 5_000, 77);

        let mut edited = spec.clone();
        edited
            .inputs
            .insert(0, input("staff", Distribution::Poisson { lambda: 12.0 }));
        let after = run_default(&edited, 5_000, 77);

        // `revenue` reads only customers and average_revenue, whose streams are
        // named rather than positional.
        assert_eq!(before.statistics[0].1.mean, after.statistics[0].1.mean);
        assert_eq!(before.statistics[0].1.p90, after.statistics[0].1.p90);
    }

    #[test]
    fn the_arithmetic_is_the_arithmetic() {
        // A model with no uncertainty at all must reproduce the calculation
        // exactly, which is the check that the engine is not perturbing values
        // on its way through.
        let spec = SimulationSpec {
            name: "Exakt".into(),
            description: None,
            inputs: vec![
                input(
                    "a",
                    Distribution::Normal {
                        mean: 10.0,
                        std_dev: 0.0,
                    },
                ),
                input(
                    "b",
                    Distribution::Normal {
                        mean: 4.0,
                        std_dev: 0.0,
                    },
                ),
            ],
            outputs: vec![output("product", "a * b"), output("ratio", "product / b")],
        };
        let outcome = run_default(&spec, 1_000, 1);
        assert_eq!(outcome.statistics[0].1.mean, 40.0);
        assert_eq!(outcome.statistics[0].1.std_dev, 0.0);
        assert_eq!(outcome.statistics[1].1.mean, 10.0);
    }

    #[test]
    fn the_mean_of_a_product_matches_the_product_of_the_means() {
        // Independent inputs, so E[XY] = E[X]E[Y]. A run of a hundred thousand
        // should land within a fraction of a percent, and this is the check
        // that catches a sampler correlated with itself.
        let outcome = run_default(&business_model(), 200_000, 9);
        let expected = 1_000.0 * (700.0 + 850.0 + 1_100.0) / 3.0;
        let measured = outcome.statistics[0].1.mean;
        assert!(
            (measured / expected - 1.0).abs() < 0.005,
            "revenue mean was {measured}, expected about {expected}"
        );
    }

    #[test]
    fn probabilities_are_reported_and_are_probabilities() {
        let outcome = run_default(&business_model(), 50_000, 3);
        let profit = &outcome.statistics[1].1;
        let target = profit.probability_of_target.expect("a target was set");
        assert!((0.0..=1.0).contains(&target));
        assert!((0.0..=1.0).contains(&profit.probability_of_loss));
        // The two threshold probabilities partition the outcomes.
        let below = profit.probability_below_threshold.unwrap();
        let above = profit.probability_above_threshold.unwrap();
        assert!((below + above - 1.0).abs() < 1e-9);
    }

    #[test]
    fn sensitivity_finds_the_input_that_actually_drives_the_output() {
        // `costs` has a range of 200_000; revenue's spread is far larger, so
        // profit should rank the revenue drivers above costs.
        let outcome = run_default(&business_model(), 100_000, 5);
        let profit = &outcome.sensitivity[1];
        let contributions: f64 = profit
            .inputs
            .iter()
            .map(|entry| entry.variance_contribution)
            .sum();
        assert!((contributions - 1.0).abs() < 1e-9);
        assert_eq!(profit.inputs[0].rank, 1);
        assert!(profit.inputs.iter().all(|entry| entry.referenced));
    }

    #[test]
    fn sensitivity_follows_a_dependency_through_an_intermediate_output() {
        // `profit` never names `customers`; it names `revenue`, which does.
        // Reporting customers as unreferenced would be wrong.
        let outcome = run_default(&business_model(), 20_000, 6);
        let profit = &outcome.sensitivity[1];
        let customers = profit
            .inputs
            .iter()
            .find(|entry| entry.input_id == "customers")
            .unwrap();
        assert!(customers.referenced);
        assert!(customers.rank_correlation.unwrap() > 0.1);
    }

    #[test]
    fn an_input_no_output_reads_is_not_credited_with_influence() {
        let mut spec = business_model();
        spec.inputs.push(input(
            "unused",
            Distribution::Uniform {
                low: 0.0,
                high: 1.0,
            },
        ));
        let outcome = run_default(&spec, 20_000, 7);
        for report in &outcome.sensitivity {
            let unused = report
                .inputs
                .iter()
                .find(|entry| entry.input_id == "unused")
                .unwrap();
            assert!(!unused.referenced);
            assert_eq!(unused.variance_contribution, 0.0);
        }
    }

    #[test]
    fn a_run_that_would_produce_an_infinity_fails_and_says_where() {
        let spec = SimulationSpec {
            name: "Division".into(),
            description: None,
            inputs: vec![input(
                "divisor",
                Distribution::Discrete {
                    values: vec![0.0, 2.0],
                    weights: vec![1.0, 1.0],
                },
            )],
            outputs: vec![output("ratio", "100 / divisor")],
        };
        let compiled = spec.compile().unwrap();
        let control = RunControl::new();
        let error = run(
            &compiled,
            RunConfig {
                iterations: 1_000,
                seed: 1,
            },
            &control,
        )
        .unwrap_err();
        match error {
            EngineError::NonFinite { output, kind, .. } => {
                assert_eq!(output, "ratio");
                assert_eq!(kind, "an infinite value");
            }
            other => panic!("expected a non-finite failure, got {other}"),
        }
    }

    #[test]
    fn a_cancelled_run_stops_and_reports_where_it_stopped() {
        let spec = business_model();
        let compiled = spec.compile().unwrap();
        let control = RunControl::new();
        control.cancel();
        let error = run(
            &compiled,
            RunConfig {
                iterations: 100_000,
                seed: 1,
            },
            &control,
        )
        .unwrap_err();
        match error {
            EngineError::Cancelled { completed, total } => {
                assert_eq!(total, 100_000);
                assert!(completed < total);
                assert_eq!(completed % BATCH, 0);
            }
            other => panic!("expected cancellation, got {other}"),
        }
    }

    #[test]
    fn progress_is_visible_while_a_run_is_in_flight() {
        // The control is shared, and the worker's heartbeat reads it from
        // another thread. Here the assertion is simply that it advances.
        let spec = business_model();
        let compiled = spec.compile().unwrap();
        let control = RunControl::new();
        assert_eq!(control.completed(), 0);
        run(
            &compiled,
            RunConfig {
                iterations: 20_000,
                seed: 1,
            },
            &control,
        )
        .unwrap();
        assert_eq!(control.completed(), 20_000);
    }

    #[test]
    fn a_run_too_large_to_hold_is_refused_before_it_starts() {
        let mut spec = business_model();
        for index in 0..14 {
            spec.outputs
                .push(output(&format!("copy_{index}"), "revenue - costs"));
        }
        let compiled = spec.compile().unwrap();
        let control = RunControl::new();
        let error = run(
            &compiled,
            RunConfig {
                iterations: 10_000_000,
                seed: 1,
            },
            &control,
        )
        .unwrap_err();
        assert!(matches!(error, EngineError::TooLarge { .. }));
    }

    #[test]
    fn a_constraint_truncates_the_distribution() {
        let mut spec = business_model();
        spec.inputs[0].constraints = Some(Constraints {
            min: Some(1_000.0),
            max: None,
            mode: ConstraintMode::Resample,
        });
        let outcome = run_default(&spec, 20_000, 8);
        // Every revenue draw used at least 1000 customers, so the minimum
        // revenue cannot be below 1000 × the lowest average revenue.
        assert!(outcome.statistics[0].1.min >= 1_000.0 * 700.0);
        assert!(outcome.quality.constraint_resamples > 0);
    }

    #[test]
    fn a_clamping_constraint_censors_rather_than_truncates() {
        let mut spec = business_model();
        spec.inputs[0].constraints = Some(Constraints {
            min: Some(1_000.0),
            max: None,
            mode: ConstraintMode::Clamp,
        });
        let outcome = run_default(&spec, 20_000, 8);
        assert!(outcome.quality.clamped_samples > 0);
        assert_eq!(outcome.quality.constraint_resamples, 0);
        assert!(outcome
            .quality
            .warnings
            .iter()
            .any(|warning| warning.contains("gräns")));
    }

    #[test]
    fn convergence_is_reported_for_every_output() {
        let outcome = run_default(&business_model(), 50_000, 10);
        for report in &outcome.convergence {
            assert!(report.checkpoints.len() >= 2);
            assert_eq!(report.checkpoints.last().unwrap().iterations, 50_000);
        }
    }

    #[test]
    fn an_unstable_run_puts_its_warning_where_a_caller_will_see_it() {
        // A lognormal with a fat tail at only a thousand iterations.
        let spec = SimulationSpec {
            name: "Tung svans".into(),
            description: None,
            inputs: vec![input(
                "x",
                Distribution::Lognormal {
                    log_mean: 0.0,
                    log_std_dev: 4.0,
                },
            )],
            outputs: vec![output("y", "x")],
        };
        let outcome = run_default(&spec, 1_000, 11);
        assert!(!outcome.convergence[0].stable);
        assert!(!outcome.quality.warnings.is_empty());
    }

    #[test]
    fn the_minimum_iteration_count_is_enforced() {
        let compiled = business_model().compile().unwrap();
        let control = RunControl::new();
        let error = run(
            &compiled,
            RunConfig {
                iterations: 10,
                seed: 1,
            },
            &control,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            EngineError::Spec(SpecError::Iterations { .. })
        ));
    }

    #[test]
    fn every_supported_distribution_survives_a_whole_run() {
        let spec = SimulationSpec {
            name: "Alla fördelningar".into(),
            description: None,
            inputs: vec![
                input(
                    "d_normal",
                    Distribution::Normal {
                        mean: 10.0,
                        std_dev: 2.0,
                    },
                ),
                input(
                    "d_lognormal",
                    Distribution::Lognormal {
                        log_mean: 1.0,
                        log_std_dev: 0.4,
                    },
                ),
                input(
                    "d_uniform",
                    Distribution::Uniform {
                        low: 1.0,
                        high: 5.0,
                    },
                ),
                input(
                    "d_triangular",
                    Distribution::Triangular {
                        low: 0.0,
                        mode: 2.0,
                        high: 9.0,
                    },
                ),
                input(
                    "d_beta",
                    Distribution::Beta {
                        alpha: 2.0,
                        beta: 3.0,
                        low: 0.0,
                        high: 10.0,
                    },
                ),
                input("d_exponential", Distribution::Exponential { rate: 0.5 }),
                input("d_poisson", Distribution::Poisson { lambda: 4.0 }),
                input("d_bernoulli", Distribution::Bernoulli { p: 0.4 }),
                input("d_binomial", Distribution::Binomial { trials: 20, p: 0.3 }),
                input(
                    "d_discrete",
                    Distribution::Discrete {
                        values: vec![1.0, 5.0, 9.0],
                        weights: vec![2.0, 1.0, 1.0],
                    },
                ),
                input(
                    "d_custom",
                    Distribution::Custom {
                        points: vec![(0.0, 0.0), (5.0, 0.4), (20.0, 1.0)],
                    },
                ),
            ],
            outputs: vec![output(
                "total",
                "d_normal + d_lognormal + d_uniform + d_triangular + d_beta \
                 + d_exponential + d_poisson + d_bernoulli + d_binomial \
                 + d_discrete + d_custom",
            )],
        };
        let outcome = run_default(&spec, 20_000, 12);
        let stats = &outcome.statistics[0].1;
        assert!(stats.mean.is_finite());
        assert!(stats.std_dev > 0.0);
        assert_eq!(outcome.sensitivity[0].inputs.len(), 11);
        assert!(outcome.sensitivity[0]
            .inputs
            .iter()
            .all(|entry| entry.referenced));
    }
}

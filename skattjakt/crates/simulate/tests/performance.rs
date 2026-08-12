//! Performance and scale, section 21.
//!
//! Ignored by default: a million-iteration run in a debug build takes long
//! enough to make `cargo test` unpleasant, and the numbers from a debug build
//! would be meaningless anyway. Run them deliberately:
//!
//! ```text
//! cargo test -p skattjakt-simulate --release --test performance -- --ignored --nocapture
//! ```
//!
//! The assertions are floors rather than targets. A throughput assertion tuned
//! to the machine it was written on fails on every other machine and gets
//! deleted; these are set low enough to pass anywhere and high enough to catch
//! an accidental order of magnitude — a hash lookup back in the inner loop, a
//! sort inside the batch, an allocation per iteration.

use skattjakt_simulate::{
    Distribution, Input, Output, RunConfig, RunControl, SimulationSpec, TargetDirection,
};

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

/// A model of realistic width: eight uncertain inputs across six distribution
/// families, three chained outputs, one of them with a branch.
fn realistic_model() -> SimulationSpec {
    SimulationSpec {
        name: "Prestandamodell".into(),
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
                "churn",
                Distribution::Beta {
                    alpha: 2.0,
                    beta: 18.0,
                    low: 0.0,
                    high: 1.0,
                },
            ),
            input(
                "fixed_costs",
                Distribution::Uniform {
                    low: 400_000.0,
                    high: 600_000.0,
                },
            ),
            input(
                "variable_cost_rate",
                Distribution::Lognormal {
                    log_mean: -1.6,
                    log_std_dev: 0.25,
                },
            ),
            input("incidents", Distribution::Poisson { lambda: 4.0 }),
            input(
                "incident_cost",
                Distribution::Exponential {
                    rate: 1.0 / 25_000.0,
                },
            ),
            input("wins_a_contract", Distribution::Bernoulli { p: 0.35 }),
        ],
        outputs: vec![
            output(
                "revenue",
                "customers * (1 - churn) * average_revenue + if(wins_a_contract, 250000, 0)",
            ),
            output(
                "costs",
                "fixed_costs + revenue * variable_cost_rate + incidents * incident_cost",
            ),
            Output {
                target: Some(100_000.0),
                critical_threshold: Some(0.0),
                ..output("profit", "revenue - costs")
            },
        ],
    }
}

fn measure(iterations: u32) -> (f64, u64) {
    let spec = realistic_model();
    let compiled = spec.compile().expect("the model compiles");
    let control = RunControl::new();
    let outcome = skattjakt_simulate::run(
        &compiled,
        RunConfig {
            iterations,
            seed: 1,
        },
        &control,
    )
    .expect("the run completes");

    println!(
        "{:>9} iterations  {:>7} ms  {:>12.0} iterations/s  profit P50 {:>12.0}",
        iterations, outcome.duration_ms, outcome.iterations_per_second, outcome.statistics[2].1.p50
    );
    (outcome.iterations_per_second, outcome.duration_ms)
}

#[test]
#[ignore = "measured deliberately, in release"]
fn one_thousand() {
    measure(1_000);
}

#[test]
#[ignore = "measured deliberately, in release"]
fn ten_thousand() {
    measure(10_000);
}

#[test]
#[ignore = "measured deliberately, in release"]
fn one_hundred_thousand() {
    let (rate, _) = measure(100_000);
    assert!(rate > 20_000.0, "only {rate:.0} iterations/s");
}

#[test]
#[ignore = "measured deliberately, in release"]
fn one_million() {
    let (rate, duration) = measure(1_000_000);
    assert!(rate > 20_000.0, "only {rate:.0} iterations/s");
    // A minute is the point at which a synchronous API request is the wrong
    // shape and the work belongs on the queue; the engine must be well inside
    // it so that the queue is a choice rather than a necessity.
    assert!(
        duration < 120_000,
        "a million iterations took {duration} ms"
    );
}

/// Scale in the other direction: the widest model the engine accepts, at a
/// size that fits its memory bound.
#[test]
#[ignore = "measured deliberately, in release"]
fn sixteen_outputs_at_a_million() {
    let mut spec = realistic_model();
    while spec.outputs.len() < 16 {
        let index = spec.outputs.len();
        spec.outputs
            .push(output(&format!("scenario_{index}"), "profit * 1.05 - 1000"));
    }
    let compiled = spec.compile().expect("compiles");
    let control = RunControl::new();
    let outcome = skattjakt_simulate::run(
        &compiled,
        RunConfig {
            iterations: 1_000_000,
            seed: 1,
        },
        &control,
    )
    .expect("runs");
    println!(
        "1 000 000 iterations × 16 outputs  {:>7} ms",
        outcome.duration_ms
    );
    assert_eq!(outcome.statistics.len(), 16);
}

/// The visualisation payload must not grow with the run. This is the assertion
/// behind section 16's separation of raw data from what a chart needs.
#[test]
#[ignore = "measured deliberately, in release"]
fn the_stored_result_does_not_grow_with_the_iteration_count() {
    let spec = realistic_model();
    let compiled = spec.compile().expect("compiles");

    let mut sizes = Vec::new();
    for iterations in [10_000u32, 1_000_000] {
        let control = RunControl::new();
        let outcome = skattjakt_simulate::run(
            &compiled,
            RunConfig {
                iterations,
                seed: 1,
            },
            &control,
        )
        .expect("runs");
        let payload = serde_json::json!({
            "statistics": outcome.statistics,
            "shapes": outcome.shapes,
            "sensitivity": outcome.sensitivity,
            "convergence": outcome.convergence,
        });
        let bytes = serde_json::to_vec(&payload).unwrap().len();
        println!("{iterations:>9} iterations → {bytes} bytes stored");
        sizes.push(bytes);
    }

    // A hundredfold more iterations must not double the stored result. The
    // convergence report gains a checkpoint or two; nothing else changes.
    assert!(
        sizes[1] < sizes[0] * 2,
        "the payload grew from {} to {} bytes",
        sizes[0],
        sizes[1]
    );
}

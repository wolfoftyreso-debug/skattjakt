//! Running one queued Monte Carlo simulation.
//!
//! In this worker rather than a fourth process, and that is a considered
//! choice rather than convenience. An analysis and a simulation are the same
//! kind of workload — minutes of CPU, no user waiting on a socket, a result
//! written to the database at the end — so they want the same node pool, the
//! same memory limits and the same disruption budget. A notification is
//! different in every one of those respects, which is why *that* got its own
//! Deployment.
//!
//! Two problems here that the engine deliberately does not solve, because
//! solving them would have required it to know about Tokio and Postgres:
//!
//! **The engine blocks.** It is a few hundred million floating-point operations
//! with no yield point. Run on an async worker thread it would starve the
//! heartbeat that keeps the job's lease alive, and the job would be reclaimed
//! and run again by somebody else while it was still running here. So it goes
//! on a blocking thread and communicates through the shared `RunControl`.
//!
//! **Cancellation lives in the database.** The API marks `cancel_requested` on
//! a row; the engine knows only an atomic flag. A watcher task bridges the two
//! — it polls the row while the run is in flight, writes the progress back so a
//! browser can show it, and flips the flag when a cancellation arrives.

use std::sync::Arc;
use std::time::Duration;

use skattjakt_core::CompanyId;
use skattjakt_jobs::Job;
use skattjakt_simulate::{EngineError, RunConfig, RunControl, SimulationSpec};
use skattjakt_store::simulations::RunState;
use skattjakt_store::Store;
use skattjakt_telemetry::{names, LabelSet, LogRecord, Registry};

use crate::runner::RunFailure;

/// How often the watcher publishes progress and checks for a cancellation.
///
/// Two seconds: fast enough that a cancelled run stops while the person who
/// cancelled it is still looking at the screen, slow enough that a ten-minute
/// run costs three hundred small updates rather than thirty thousand.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// Runs the simulation named by a job.
pub async fn run(store: &Store, metrics: &Registry, job: &Job) -> Result<(), RunFailure> {
    let company = CompanyId::from_uuid(job.company_id);
    let run_id = job.subject_id;

    // Load the run and the exact version it names. Not the current version:
    // a run started before a model was edited must produce what that model
    // said, or the seed recorded beside it means nothing.
    let mut tenant = store
        .tenant(company)
        .await
        .map_err(|_| RunFailure::transient("store_unavailable"))?;

    let stored = match tenant.run(run_id).await {
        Ok(stored) => stored,
        Err(_) => {
            // The row is gone — the company was erased, or the run was deleted
            // between enqueue and claim. Nothing to do and nothing to retry.
            let _ = tenant.commit().await;
            return Err(RunFailure::permanent(
                "run_missing",
                "Simuleringen finns inte längre.",
            ));
        }
    };

    if !should_run(stored.state, stored.cancel_requested) {
        if stored.state.is_terminal() {
            // Already finished — a duplicate claim after a lease expired on an
            // attempt that had actually succeeded. Reporting success is right:
            // the work the job describes is done.
            let _ = tenant.commit().await;
            return Ok(());
        }
        // Cancelled between being enqueued and being picked up. Nothing ran, so
        // there is no progress to record.
        tenant
            .cancel_run(run_id, 0)
            .await
            .map_err(|_| RunFailure::transient("store_unavailable"))?;
        tenant
            .commit()
            .await
            .map_err(|_| RunFailure::transient("store_unavailable"))?;
        metrics.increment(
            names::SIMULATIONS_FINISHED,
            LabelSet::new().enumerated("outcome", "cancelled"),
        );
        return Ok(());
    }

    let document = tenant
        .simulation_version_spec(stored.version_id)
        .await
        .map_err(|_| {
            RunFailure::permanent(
                "version_missing",
                "Modellversionen som körningen avser finns inte längre.",
            )
        })?;
    tenant
        .mark_run_running(run_id)
        .await
        .map_err(|_| RunFailure::transient("store_unavailable"))?;
    tenant
        .commit()
        .await
        .map_err(|_| RunFailure::transient("store_unavailable"))?;

    let spec: SimulationSpec = serde_json::from_value(document).map_err(|error| {
        RunFailure::permanent(
            "spec_unreadable",
            format!("Modellen kunde inte läsas: {error}"),
        )
    })?;
    let compiled = spec.compile().map_err(|error| {
        // Stored specifications are compiled before they are written, so this
        // is only reachable if the engine's validation tightened between the
        // two. Permanent either way: retrying will reject it identically.
        RunFailure::permanent("spec_invalid", format!("Modellen kan inte köras: {error}"))
    })?;

    let config = RunConfig {
        iterations: stored.iterations.max(0) as u32,
        seed: stored.seed,
    };

    LogRecord::info("simulation started")
        .correlate(job.correlation_id)
        .internal("run_id", run_id.to_string())
        .internal("iterations", i64::from(stored.iterations))
        .emit();

    let control = RunControl::new();
    let watcher = spawn_watcher(store.clone(), company, run_id, control.clone());

    let engine_control = control.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        skattjakt_simulate::run(&compiled, config, &engine_control)
    })
    .await;

    watcher.abort();

    let outcome = match outcome {
        Ok(result) => result,
        // The blocking task panicked. That is a bug rather than a condition,
        // and it is retryable only in the sense that a second attempt will
        // reproduce it — so it is not.
        Err(error) => {
            let _ = fail(
                store,
                company,
                run_id,
                &format!("intern körningsfel: {error}"),
            )
            .await;
            metrics.increment(
                names::SIMULATIONS_FINISHED,
                LabelSet::new().enumerated("outcome", "failed"),
            );
            return Err(RunFailure::permanent(
                "engine_panic",
                "Simuleringen avbröts av ett internt fel.",
            ));
        }
    };

    let mut tenant = store
        .tenant(company)
        .await
        .map_err(|_| RunFailure::transient("store_unavailable"))?;

    match outcome {
        Ok(outcome) => {
            let unstable = outcome
                .convergence
                .iter()
                .filter(|report| !report.stable)
                .count();

            tenant
                .complete_run(run_id, &outcome)
                .await
                .map_err(|_| RunFailure::transient("store_unavailable"))?;
            tenant
                .audit_as(
                    format!("worker:{}", job.id),
                    "simulation.run_completed",
                    Some(stored.simulation_id),
                    serde_json::json!({
                        "run_id": run_id,
                        "seed": outcome.seed.to_string(),
                        "iterations": outcome.iterations,
                        "engine_version": outcome.engine_version,
                        "spec_hash": outcome.spec_hash,
                        "duration_ms": outcome.duration_ms,
                        "unstable_outputs": unstable,
                    }),
                )
                .await
                .map_err(|_| RunFailure::transient("store_unavailable"))?;
            tenant
                .commit()
                .await
                .map_err(|_| RunFailure::transient("store_unavailable"))?;

            metrics.increment(
                names::SIMULATIONS_FINISHED,
                LabelSet::new().enumerated("outcome", "succeeded"),
            );
            metrics.observe(
                names::SIMULATION_DURATION,
                LabelSet::new(),
                outcome.duration_ms,
            );
            metrics.add(
                names::SIMULATION_ITERATIONS,
                LabelSet::new(),
                u64::from(outcome.iterations),
            );
            metrics.observe(
                names::SIMULATION_THROUGHPUT,
                LabelSet::new(),
                outcome.iterations_per_second.max(0.0) as u64,
            );
            if unstable > 0 {
                metrics.add(
                    names::SIMULATION_CONVERGENCE_FAILURES,
                    LabelSet::new(),
                    unstable as u64,
                );
            }

            LogRecord::info("simulation finished")
                .correlate(job.correlation_id)
                .internal("run_id", run_id.to_string())
                .internal("duration_ms", outcome.duration_ms as i64)
                .emit();
            Ok(())
        }
        Err(EngineError::Cancelled { completed, .. }) => {
            tenant
                .cancel_run(run_id, completed as i32)
                .await
                .map_err(|_| RunFailure::transient("store_unavailable"))?;
            tenant
                .commit()
                .await
                .map_err(|_| RunFailure::transient("store_unavailable"))?;
            metrics.increment(
                names::SIMULATIONS_FINISHED,
                LabelSet::new().enumerated("outcome", "cancelled"),
            );
            LogRecord::info("simulation cancelled")
                .correlate(job.correlation_id)
                .internal("run_id", run_id.to_string())
                .internal("completed", i64::from(completed))
                .emit();
            // A cancellation is a completed job. Failing it here would put the
            // job back on the queue to be cancelled again.
            Ok(())
        }
        Err(error) => {
            tenant
                .fail_run(run_id, &error.to_string())
                .await
                .map_err(|_| RunFailure::transient("store_unavailable"))?;
            tenant
                .commit()
                .await
                .map_err(|_| RunFailure::transient("store_unavailable"))?;
            metrics.increment(
                names::SIMULATIONS_FINISHED,
                LabelSet::new().enumerated("outcome", "failed"),
            );
            // Deterministic: the same seed and model produce the same failure,
            // so there is nothing for a retry to discover.
            Err(RunFailure::permanent(
                "simulation_failed",
                format!("Simuleringen kunde inte slutföras: {error}"),
            ))
        }
    }
}

/// Publishes progress and relays cancellation, on its own task.
fn spawn_watcher(
    store: Store,
    company: CompanyId,
    run_id: uuid::Uuid,
    control: Arc<RunControl>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(WATCH_INTERVAL);
        // The first tick fires immediately; skipping it avoids a write before
        // the run has done anything.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let completed = control.completed();

            let Ok(mut tenant) = store.tenant(company).await else {
                // A database blip must not cancel a run in flight. Progress
                // simply stops updating until the next tick succeeds.
                continue;
            };
            match tenant.report_run_progress(run_id, completed as i32).await {
                Ok(true) => {
                    control.cancel();
                    let _ = tenant.commit().await;
                    return;
                }
                Ok(false) => {
                    let _ = tenant.commit().await;
                }
                Err(_) => {
                    let _ = tenant.commit().await;
                }
            }
        }
    })
}

async fn fail(store: &Store, company: CompanyId, run_id: uuid::Uuid, message: &str) {
    if let Ok(mut tenant) = store.tenant(company).await {
        let _ = tenant.fail_run(run_id, message).await;
        let _ = tenant.commit().await;
    }
}

/// Whether a run row is still worth working on.
///
/// A claim can legitimately find a run that has already finished — a lease
/// expired after the work was done — or one that was cancelled between being
/// enqueued and being picked up. Both are jobs to report complete rather than
/// jobs to run.
fn should_run(state: RunState, cancel_requested: bool) -> bool {
    !state.is_terminal() && !cancel_requested
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_run_is_not_run_again() {
        assert!(!should_run(RunState::Succeeded, false));
        assert!(!should_run(RunState::Failed, false));
        assert!(!should_run(RunState::Cancelled, false));
    }

    #[test]
    fn a_cancelled_request_stops_a_run_that_never_started() {
        assert!(!should_run(RunState::Queued, true));
        assert!(should_run(RunState::Queued, false));
        assert!(should_run(RunState::Running, false));
    }
}

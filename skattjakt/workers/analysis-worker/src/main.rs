//! The analysis worker.
//!
//! A separate process and a separate Deployment from the API (section 8). The
//! reason is not tidiness. An analysis takes minutes of model latency and holds
//! memory while it does; the API serves requests in milliseconds. Sharing a
//! process means one autoscaling signal for two workloads with nothing in
//! common, an API pod that cannot be rolled without killing analyses, and a
//! memory spike in extraction taking down request serving.
//!
//! The loop is deliberately dull: claim, heartbeat, run, report, repeat. All of
//! the difficulty — leases, retries, backoff, dead letters — is in
//! `skattjakt-jobs`, where it is testable without a worker.

mod runner;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use skattjakt_core::{AnalysisId, AnalysisState, CompanyId};
use skattjakt_gateway::{GatewayConfig, ModelGateway};
use skattjakt_jobs::{JobKind, Queue};
use skattjakt_model::{AnthropicConfig, AnthropicProvider};
use skattjakt_pipeline::{AnalysisPipeline, PipelineConfig};
use skattjakt_rules::RuleEngine;
use skattjakt_store::{FilesystemBlobStore, Store};
use skattjakt_telemetry::{logging, metrics, LogRecord, Registry};

use crate::runner::{spawn_heartbeat, Runner};

/// How long to wait when the queue is empty.
///
/// Polling rather than `LISTEN`/`NOTIFY`. A notification is lost if no worker
/// is connected when it fires, so a `LISTEN`-only design needs this poll as a
/// backstop anyway — and at this volume the poll alone is sufficient and has
/// one failure mode instead of two.
const IDLE_POLL: Duration = Duration::from_secs(2);
/// How often to return elapsed backoffs and reap lost leases.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logging::init("skattjakt=info,sqlx=warn");

    let metrics_registry = Registry::new();
    metrics::register_all(&metrics_registry);

    let worker_id = std::env::var("HOSTNAME").unwrap_or_else(|_| "analysis-worker".to_string());

    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL is required; the worker has nothing to do without a queue")?;
    let store = Store::connect(&database_url)
        .await
        .context("could not connect to the database")?;

    let blob_root = std::env::var("SKATTJAKT_BLOB_ROOT")
        .unwrap_or_else(|_| "/var/lib/skattjakt/blobs".to_string());
    let blobs = Arc::new(FilesystemBlobStore::new(&blob_root));

    let engine = Arc::new(RuleEngine::load_embedded().context("the embedded rule set is invalid")?);

    // A missing model provider is not fatal, for the same reason it is not
    // fatal in the API: the rule engine produces evidence-backed findings on
    // its own, and a rules-only analysis is more useful than no service. The
    // two processes must agree on this — a deployment where the API accepts
    // work that the worker refuses to run is a deployment that queues
    // analyses forever.
    let (provider, model_configured): (Arc<dyn skattjakt_model::ModelProvider>, bool) =
        match AnthropicConfig::from_env().and_then(AnthropicProvider::new) {
            Ok(provider) => (Arc::new(provider), true),
            Err(error) => {
                LogRecord::warn("model provider not configured; running rules-only")
                    .internal("reason", error.to_string())
                    .emit();
                (Arc::new(skattjakt_model::ScriptedProvider::new()), false)
            }
        };

    let gateway_config = GatewayConfig::from_env().context("model pricing is misconfigured")?;
    let gateway = Arc::new(ModelGateway::new(
        provider.clone(),
        gateway_config,
        metrics_registry.clone(),
    ));

    // A configured model with no price is a different matter, and it is fatal.
    // An unpriced call is an unbounded call: the budget check would pass for
    // it, and the ceiling of section 69 would not exist. Failing here makes it
    // a failed rollout that a readiness probe catches, rather than a worker
    // that starts and dead-letters everything it claims.
    if model_configured && !gateway.is_callable() {
        anyhow::bail!(
            "no price is configured for model {} — set SKATTJAKT_MODEL_PRICES before starting",
            gateway.model_id()
        );
    }

    // The pipeline holds the gateway, not the provider. Every model call it
    // makes is therefore priced, budgeted, fence-checked and fallback-checked.
    let pipeline = Arc::new(AnalysisPipeline::new(
        engine,
        gateway.clone(),
        PipelineConfig::default(),
    ));

    let queue = Queue::new(store.pool().clone(), metrics_registry.clone(), &worker_id);

    let runner = Arc::new(Runner {
        store: store.clone(),
        blobs,
        gateway,
        pipeline,
        queue: queue.clone(),
        metrics: metrics_registry.clone(),
    });

    LogRecord::info("analysis worker started")
        .internal("worker_id", worker_id.clone())
        .internal("model", runner.gateway.model_id().to_string())
        .emit();

    // Maintenance runs on its own timer so a long analysis does not stop lost
    // leases from being reaped.
    let maintenance = spawn_maintenance(queue.clone());

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                LogRecord::info("shutdown requested; finishing the current job and stopping").emit();
                break;
            }
            claimed = queue.claim(JobKind::Analysis) => {
                match claimed {
                    Ok(Some(job)) => {
                        let heartbeat = spawn_heartbeat(queue.clone(), job.clone());
                        let outcome = runner.run(&job).await;
                        heartbeat.abort();

                        let report = match &outcome {
                            Ok(()) => queue.succeed(&job).await.map(|_| ()),
                            Err(failure) => match queue
                                .fail(&job, failure.retryable, failure.kind)
                                .await
                            {
                                // A dead-lettered analysis has run out of
                                // attempts. Nothing in `runner` told the
                                // customer, because until this moment the run
                                // was still in flight — but there will be no
                                // further attempt, so the progress bar must
                                // stop rather than spin forever.
                                Ok(AnalysisState::DeadLettered) => {
                                    let message = failure.message.clone().unwrap_or_else(|| {
                                        "Analysen kunde inte slutföras efter flera försök. \
                                         Vi har registrerat felet och tittar på det."
                                            .to_string()
                                    });
                                    let company = CompanyId::from_uuid(job.company_id);
                                    let analysis = AnalysisId::from_uuid(job.subject_id);
                                    if let Ok(mut tenant) = store.tenant(company).await {
                                        let _ = tenant.fail_analysis(analysis, &message).await;
                                        let _ = tenant.commit().await;
                                    }
                                    Ok(())
                                }
                                Ok(_) => Ok(()),
                                Err(error) => Err(error),
                            },
                        };
                        if let Err(error) = report {
                            // The work is done but the queue does not know. The
                            // lease will expire and the job will be retried;
                            // the analysis is idempotent at the result level
                            // because `complete_analysis` writes by id.
                            LogRecord::error("could not report job outcome")
                                .correlate(job.correlation_id)
                                .internal("error", error.to_string())
                                .emit();
                        }
                    }
                    Ok(None) => {
                        tokio::time::sleep(IDLE_POLL).await;
                    }
                    Err(error) => {
                        LogRecord::error("could not claim a job")
                            .internal("error", error.to_string())
                            .emit();
                        tokio::time::sleep(IDLE_POLL).await;
                    }
                }
            }
        }
    }

    maintenance.abort();
    LogRecord::info("analysis worker stopped").emit();
    Ok(())
}

fn spawn_maintenance(queue: Queue) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(MAINTENANCE_INTERVAL);
        loop {
            ticker.tick().await;

            match queue.release_elapsed_backoffs().await {
                Ok(n) if n > 0 => LogRecord::info("returned jobs to the queue after backoff")
                    .internal("jobs", n)
                    .emit(),
                Ok(_) => {}
                Err(error) => LogRecord::warn("could not release backoffs")
                    .internal("error", error.to_string())
                    .emit(),
            }

            match queue.reap_expired_leases().await {
                Ok(n) if n > 0 => LogRecord::warn("reaped jobs whose worker stopped reporting")
                    .internal("jobs", n)
                    .emit(),
                Ok(_) => {}
                Err(error) => LogRecord::warn("could not reap expired leases")
                    .internal("error", error.to_string())
                    .emit(),
            }

            let _ = queue.publish_depth().await;
        }
    })
}

/// Waits for SIGTERM or Ctrl-C.
///
/// SIGTERM is what Kubernetes sends before it kills a pod. Catching it is what
/// turns a rolling deploy from "analyses in flight are lost" into "analyses in
/// flight finish, or are retried by whoever claims them next".
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            // Without the signal handler the worker still stops on Ctrl-C; it
            // just loses the graceful path, which is worth a log line and not a
            // panic.
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

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

mod payments;
mod runner;
mod simulation;
mod sources;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use skattjakt_core::{AnalysisId, AnalysisState, CompanyId};
use skattjakt_gateway::{GatewayConfig, ModelGateway};
use skattjakt_jobs::{JobKind, Queue};
use skattjakt_model::{AnthropicConfig, AnthropicProvider};
use skattjakt_pipeline::{AnalysisPipeline, PipelineConfig};
use skattjakt_rules::RuleEngine;
use skattjakt_store::Store;
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
    // One subcommand, and it is here rather than in a tool of its own for a
    // reason worth stating: the source check that runs on a schedule inside
    // this process and the source check an operator runs by hand have to be the
    // *same* check. Two implementations of "does this paragraph still say 25
    // per cent" drift, and the one that drifts is always the one nobody runs.
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("verify-sources") {
        return verify_sources_command(&arguments[1..]).await;
    }

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
    // S3 when it is configured, the filesystem otherwise — and fatal if it is
    // half-configured, because silently writing a customer's documents to a
    // local disk that is not backed up and not shared between replicas is a
    // worse outcome than refusing to start.
    let blobs = skattjakt_store::blob::from_env(&blob_root).map_err(anyhow::Error::msg)?;

    let engine = Arc::new(RuleEngine::load_embedded().context("the embedded rule set is invalid")?);

    // The worker needs the provider to *ask* about payments, never to create
    // them: creating is a request-path action. An unconfigured deployment gets
    // a provider that refuses, and the sweep then finds nothing to resolve
    // because nothing was ever started.
    let payment_provider: Arc<dyn skattjakt_payments::PaymentProvider> =
        match skattjakt_payments::swish::SwishConfig::from_env().map_err(anyhow::Error::msg)? {
            Some(config) => Arc::new(
                skattjakt_payments::swish::SwishProvider::new(config)
                    .map_err(anyhow::Error::msg)?,
            ),
            None => Arc::new(skattjakt_payments::UnconfiguredProvider),
        };

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
        engine.clone(),
        gateway.clone(),
        PipelineConfig::default(),
    ));

    let queue = Queue::new(store.pool().clone(), metrics_registry.clone(), &worker_id);

    let spans = match skattjakt_telemetry::otlp::OtlpConfig::from_env("skattjakt-analysis-worker") {
        Some(config) => {
            let exporter = skattjakt_telemetry::otlp::SpanExporter::new(config);
            exporter.spawn_flush_loop();
            LogRecord::info("trace export configured").emit();
            exporter
        }
        None => skattjakt_telemetry::otlp::SpanExporter::disabled(),
    };

    let runner = Arc::new(Runner {
        store: store.clone(),
        blobs,
        gateway,
        pipeline,
        queue: queue.clone(),
        metrics: metrics_registry.clone(),
        spans,
    });

    LogRecord::info("analysis worker started")
        .internal("worker_id", worker_id.clone())
        .internal("model", runner.gateway.model_id().to_string())
        .emit();

    // Maintenance runs on its own timer so a long analysis does not stop lost
    // leases from being reaped.
    let maintenance = spawn_maintenance(queue.clone(), store.clone());
    // Source verification runs on a timer of its own rather than inside
    // maintenance: a full sweep spaces 24 requests out over half a minute,
    // and a lease that goes unreaped for that long is a job nobody is
    // working on.
    let source_sweep = spawn_source_sweep(store.clone(), engine, metrics_registry.clone());
    // Payments resolve themselves whether or not the callback arrives.
    let payment_sweep = spawn_payment_sweep(
        store.clone(),
        payment_provider.clone(),
        metrics_registry.clone(),
    );

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                LogRecord::info("shutdown requested; finishing the current job and stopping").emit();
                break;
            }
            claimed = claim_any(&queue) => {
                match claimed {
                    Ok(Some(job)) => {
                        let heartbeat = spawn_heartbeat(queue.clone(), job.clone());
                        // Two kinds of work, one loop. They share a node pool
                        // and a memory limit because they are the same shape of
                        // workload: minutes of CPU with nobody holding a socket
                        // open. A notification is not, which is why that has a
                        // Deployment of its own.
                        let outcome = match job.kind {
                            JobKind::Simulation => {
                                simulation::run(&store, &metrics_registry, &job).await
                            }
                            _ => runner.run(&job).await,
                        };
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
    source_sweep.abort();
    payment_sweep.abort();
    LogRecord::info("analysis worker stopped").emit();
    Ok(())
}

/// Claims whichever kind of work is waiting.
///
/// Analyses first, deliberately. A customer is watching a progress bar for one
/// of those; a queued simulation is large by definition and its requester has
/// already been told it is running in the background. Under saturation the
/// interactive work should win.
async fn claim_any(
    queue: &Queue,
) -> Result<Option<skattjakt_jobs::Job>, skattjakt_jobs::QueueError> {
    if let Some(job) = queue.claim(JobKind::Analysis).await? {
        return Ok(Some(job));
    }
    queue.claim(JobKind::Simulation).await
}

fn spawn_maintenance(queue: Queue, store: Store) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(MAINTENANCE_INTERVAL);
        // Housekeeping runs far less often than the queue maintenance it rides
        // on: an expired session row and an unredeemed upload ticket are not
        // urgent, and sweeping them every fifteen seconds would be a pointless
        // write against a table nobody is waiting on.
        let mut ticks: u64 = 0;
        loop {
            ticker.tick().await;
            ticks += 1;

            match queue.release_elapsed_backoffs().await {
                Ok(n) if n > 0 => LogRecord::info("returned jobs to the queue after backoff")
                    .internal("jobs", n)
                    .emit(),
                Ok(_) => {}
                Err(error) => LogRecord::warn("could not release backoffs")
                    .internal("error", error.to_string())
                    .emit(),
            }

            // Roughly hourly at a fifteen-second tick.
            if ticks % 240 == 0 {
                match store.expire_upload_tickets().await {
                    Ok(n) if n > 0 => LogRecord::info("marked upload tickets that were never used")
                        .internal("tickets", n as i64)
                        .emit(),
                    Ok(_) => {}
                    Err(error) => LogRecord::warn("could not expire upload tickets")
                        .internal("error", error.to_string())
                        .emit(),
                }

                // Retention, not tidiness: an expired session row records when
                // someone was signed in and from roughly where, and it should
                // not outlive its usefulness. Ninety days after the refresh
                // token died is long enough to investigate an incident.
                match store.purge_expired_sessions(90).await {
                    Ok(n) if n > 0 => LogRecord::info("purged long-expired sessions")
                        .internal("sessions", n as i64)
                        .emit(),
                    Ok(_) => {}
                    Err(error) => LogRecord::warn("could not purge sessions")
                        .internal("error", error.to_string())
                        .emit(),
                }
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

/// Resolves payments the callback did not, forever.
///
/// Its own timer for the same reason the source sweep has one: a customer who
/// has paid must not wait on an analysis finishing, and an analysis must not
/// wait on a payment provider answering.
fn spawn_payment_sweep(
    store: Store,
    provider: std::sync::Arc<dyn skattjakt_payments::PaymentProvider>,
    metrics: Registry,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(payments::SWEEP_INTERVAL);
        loop {
            ticker.tick().await;
            if let Err(error) = payments::sweep(&store, provider.as_ref(), &metrics).await {
                LogRecord::warn("the payment sweep failed")
                    .internal("error", error.to_string())
                    .emit();
            }
        }
    })
}

/// Re-checks the cited legal sources, forever.
///
/// The interval and the locking live in `sources`; this is only the timer and
/// the failure handling. A sweep that errors is logged and retried on the next
/// tick rather than taking the worker down: the product runs perfectly well on
/// a stale verification — every finding simply keeps the ceiling that an
/// unchecked source earns it — and a crash loop here would stop analyses too.
fn spawn_source_sweep(
    store: Store,
    engine: Arc<RuleEngine>,
    metrics: Registry,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(sources::SWEEP_POLL);
        loop {
            ticker.tick().await;
            match sources::sweep_if_due(&store, &engine, &metrics).await {
                Ok(Some(outcome)) if outcome.mismatched > 0 => {
                    LogRecord::error("the rule set no longer agrees with its sources")
                        .internal("mismatched", outcome.mismatched.to_string())
                        .internal("of", outcome.total().to_string())
                        .emit();
                }
                // `None` is the common case: not due, or another worker has it.
                Ok(_) => {}
                Err(error) => LogRecord::warn("could not sweep the source registry")
                    .internal("error", error.to_string())
                    .emit(),
            }
        }
    })
}

/// `skattjakt-analysis-worker verify-sources [--ruleset PATH] [--write]`
///
/// Fetches every source the rule set cites and reports whether it still says
/// what the rules assume. Without `--write` it changes nothing, which is the
/// mode to run when you want an answer rather than a decision.
///
/// Exit codes are meant for a pipeline: 0 everything verified, 1 a source
/// contradicted the rule set, 2 nothing could be retrieved. The middle one is
/// the interesting failure — it means the law moved or the rule was wrong, and
/// a build should stop rather than ship findings that cite it.
async fn verify_sources_command(arguments: &[String]) -> anyhow::Result<()> {
    let mut ruleset: Option<String> = None;
    let mut write = false;
    let mut rest = arguments.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--write" => write = true,
            "--ruleset" => ruleset = Some(rest.next().context("--ruleset needs a path")?.clone()),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }

    let engine = match &ruleset {
        Some(path) => {
            let json =
                std::fs::read_to_string(path).with_context(|| format!("could not read {path}"))?;
            RuleEngine::from_json(&json).context("the rule set is not valid")?
        }
        None => RuleEngine::load_embedded().context("the embedded rule set is invalid")?,
    };

    let metrics_registry = Registry::new();
    metrics::register_all(&metrics_registry);

    // Writing needs somewhere to write. Reporting does not, so an operator can
    // ask the question from a laptop with no database in reach.
    let store = match (write, std::env::var("DATABASE_URL")) {
        (true, Ok(url)) if !url.is_empty() => Some(
            Store::connect(&url)
                .await
                .context("could not connect to the database")?,
        ),
        (true, _) => anyhow::bail!("--write needs DATABASE_URL"),
        (false, _) => None,
    };

    let report = sources::check_all(&engine, store.as_ref(), &metrics_registry).await?;
    for line in &report.lines {
        println!("{line}");
    }
    println!(
        "\n{} verified, {} contradicted, {} unreachable, of {} sources",
        report.outcome.verified,
        report.outcome.mismatched,
        report.outcome.unreachable,
        report.outcome.total()
    );
    if let Some(store) = &store {
        let _ = store;
        println!("recorded to source_retrievals");
    }

    if report.outcome.mismatched > 0 {
        println!(
            "\nA source contradicted the rule set. Either the law has changed or the rule \
             set was wrong when it was written; both need a person before anything ships."
        );
        std::process::exit(1);
    }
    if report.outcome.verified == 0 {
        println!(
            "\nNothing could be retrieved, so nothing is verified. The rules keep the \
             status ceiling that unchecked sources earn them."
        );
        std::process::exit(2);
    }
    Ok(())
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

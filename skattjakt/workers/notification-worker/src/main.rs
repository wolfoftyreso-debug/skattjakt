//! The notification delivery worker.
//!
//! A third process, and the reason is latency rather than tidiness. A
//! notification is cheap — one HTTP call or one SMTP session — and an analysis
//! is minutes of model latency. Sharing a worker would put "your analysis is
//! ready" behind somebody else's four-minute analysis, which defeats the point
//! of sending it.
//!
//! The loop is the same shape as the analysis worker's, because the problem is
//! the same: claim, do, record, back off. What differs is that a notification
//! has several channels and a partial success is a real outcome.

use std::time::Duration;

use anyhow::Context;
use skattjakt_notify::{Dispatcher, SmtpConfig, SmtpSender};
use skattjakt_store::notifications::PendingNotification;
use skattjakt_store::Store;
use skattjakt_telemetry::{logging, metrics, LogRecord, Registry};

/// How long to wait when the outbox is empty.
const IDLE_POLL: Duration = Duration::from_secs(5);
/// How many to claim at once.
///
/// Small. A large batch means a crash loses more in-flight work, and there is
/// no throughput problem to solve at this volume.
const BATCH: i64 = 10;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logging::init("skattjakt=info,sqlx=warn");

    let registry = Registry::new();
    metrics::register_all(&registry);

    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL is required; the worker has nothing to deliver without it")?;
    let store = Store::connect(&database_url)
        .await
        .context("could not connect to the database")?;

    // A missing relay is not fatal. In-app notifications still work, and a
    // deployment that has not configured mail yet should degrade rather than
    // refuse to start — the same judgement as a missing model provider.
    let email: Option<Box<dyn skattjakt_notify::ChannelSender>> =
        match SmtpConfig::from_env().map_err(anyhow::Error::msg)? {
            Some(config) => {
                LogRecord::info("email delivery configured")
                    .internal("host", config.host.clone())
                    .emit();
                Some(Box::new(SmtpSender::new(config)))
            }
            None => {
                LogRecord::warn("no mail relay configured; email notifications will not be sent")
                    .emit();
                None
            }
        };

    let dispatcher = Dispatcher::new(email);

    LogRecord::info("notification worker started").emit();

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                LogRecord::info("shutdown requested; stopping").emit();
                break;
            }
            claimed = store.claim_notifications(BATCH) => {
                match claimed {
                    Ok(batch) if batch.is_empty() => {
                        tokio::time::sleep(IDLE_POLL).await;
                    }
                    Ok(batch) => {
                        for notification in batch {
                            deliver_one(&store, &dispatcher, &registry, notification).await;
                        }
                    }
                    Err(error) => {
                        LogRecord::error("could not claim notifications")
                            .internal("error", error.to_string())
                            .emit();
                        tokio::time::sleep(IDLE_POLL).await;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn deliver_one(
    store: &Store,
    dispatcher: &Dispatcher,
    registry: &Registry,
    notification: PendingNotification,
) {
    let recipient = match store.recipient_for(&notification).await {
        Ok(recipient) => recipient,
        Err(error) => {
            LogRecord::error("could not resolve a notification recipient")
                .internal("error", error.to_string())
                .internal("correlation_id", notification.correlation_id.to_string())
                .emit();
            let _ = store
                .record_delivery(notification.id, &[], Some("recipient_unresolvable"))
                .await;
            return;
        }
    };

    let outcome = dispatcher.deliver(&notification, &recipient).await;

    for channel in &outcome.delivered {
        registry.increment(
            skattjakt_telemetry::names::NOTIFICATIONS_DELIVERED,
            skattjakt_telemetry::LabelSet::new()
                .enumerated("channel", static_channel(channel))
                .enumerated("kind", static_kind(&notification.kind)),
        );
    }

    // A permanent failure must not be retried. `record_delivery` decides that
    // from whether an error is present and how many attempts have been made;
    // passing `None` for a permanent failure would mark it delivered, and
    // passing an error for one would retry it until the cap.
    let error = match (&outcome.error, outcome.retry) {
        (None, _) => None,
        (Some(error), true) => Some(error.as_str()),
        (Some(error), false) => {
            // Permanent. The row is exhausted rather than rescheduled — but the
            // channels that *did* deliver are kept, because an email that
            // reached the customer alongside a push that could not be sent is a
            // notification that arrived.
            let _ = store
                .exhaust_notification(notification.id, &outcome.delivered, error)
                .await;
            registry.increment(
                skattjakt_telemetry::names::NOTIFICATIONS_FAILED,
                skattjakt_telemetry::LabelSet::new().enumerated(
                    "class",
                    if outcome.delivered.is_empty() {
                        "permanent"
                    } else {
                        "partial"
                    },
                ),
            );
            return;
        }
    };

    if let Err(e) = store
        .record_delivery(notification.id, &outcome.delivered, error)
        .await
    {
        LogRecord::error("could not record a delivery")
            .internal("error", e.to_string())
            .internal("correlation_id", notification.correlation_id.to_string())
            .emit();
    }
}

/// Metric labels must be a closed set, or the label becomes unbounded.
fn static_channel(value: &str) -> &'static str {
    match value {
        "email" => "email",
        "push" => "push",
        "in_app" => "in_app",
        _ => "other",
    }
}

fn static_kind(value: &str) -> &'static str {
    match value {
        "analysis_completed" => "analysis_completed",
        "analysis_failed" => "analysis_failed",
        "document_processed" => "document_processed",
        "member_invited" => "member_invited",
        "security_alert" => "security_alert",
        _ => "other",
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

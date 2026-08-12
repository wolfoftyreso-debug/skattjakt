//! Exporting spans over OTLP.
//!
//! ## What this adds, and what already existed
//!
//! Trace context was already minted, parsed, carried across the job queue and
//! stamped onto every log line. What it was not doing is leaving the process,
//! so a trace could be reconstructed by grepping logs and not by looking at it.
//! This is the part that sends it.
//!
//! ## OTLP/HTTP with JSON, not gRPC with protobuf
//!
//! The specification defines both, and every collector accepts JSON on
//! `/v1/traces`. Section 33 asks what each costs:
//!
//!   * gRPC/protobuf means `tonic`, `prost`, a build-time protobuf compiler and
//!     a generated-code step — roughly forty crates and a `build.rs` — in a
//!     product that publishes an SBOM and gates every commit on `cargo audit`.
//!   * JSON means `serde_json` and the `reqwest` client that is already here.
//!
//! What is given up is wire efficiency, and it does not matter at this volume:
//! a few hundred spans a minute is kilobytes. If it ever does, the exporter is
//! one module behind one trait.
//!
//! ## Sampling and batching
//!
//! Spans are batched and dropped rather than blocking. Telemetry that can stall
//! the request it is measuring is worse than no telemetry — an exporter waiting
//! on a collector that has stopped answering would turn an observability outage
//! into a product outage.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::tracing_context::{SpanContext, SpanId, TraceId};

/// How many spans may wait to be sent before the oldest are dropped.
///
/// Bounded because an unbounded buffer is a memory leak with a collector
/// outage for a trigger.
const MAX_BUFFERED: usize = 2_048;

/// How often the buffer is flushed.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// How a span ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanStatus {
    Unset,
    Ok,
    Error,
}

impl SpanStatus {
    /// OTLP's numeric status codes.
    fn code(self) -> u8 {
        match self {
            SpanStatus::Unset => 0,
            SpanStatus::Ok => 1,
            SpanStatus::Error => 2,
        }
    }
}

/// A finished span, ready to export.
///
/// Attributes are `(&'static str, String)` and the constructor is the only way
/// to add one — the same shape as `LogRecord`, and for the same reason. A span
/// that is exported to a third-party collector must not be the place a company
/// id or an amount escapes, so attribute *names* are static and every value
/// goes through [`FinishedSpan::attribute`], which is documented as PUBLIC or
/// INTERNAL only.
#[derive(Debug, Clone)]
pub struct FinishedSpan {
    trace_id: TraceId,
    span_id: SpanId,
    parent_span_id: Option<SpanId>,
    name: &'static str,
    start_unix_nanos: u128,
    end_unix_nanos: u128,
    status: SpanStatus,
    attributes: Vec<(&'static str, String)>,
}

impl FinishedSpan {
    /// Starts timing a span.
    pub fn start(context: SpanContext) -> InFlightSpan {
        InFlightSpan {
            context,
            started: SystemTime::now(),
            attributes: Vec::new(),
        }
    }

    pub fn trace_id_hex(&self) -> String {
        self.trace_id.to_hex()
    }

    /// The OTLP JSON for one span.
    fn to_json(&self) -> Value {
        json!({
            "traceId": self.trace_id.to_hex(),
            "spanId": self.span_id.to_hex(),
            "parentSpanId": self.parent_span_id.map(|id| id.to_hex()).unwrap_or_default(),
            "name": self.name,
            // 1 = SERVER. Everything exported here is work this process did on
            // behalf of a caller; a finer breakdown would be a guess.
            "kind": 1,
            // OTLP wants nanoseconds since the epoch, as a *string* — the
            // values exceed what JSON numbers represent exactly, and a
            // collector that parses them as f64 loses microseconds.
            "startTimeUnixNano": self.start_unix_nanos.to_string(),
            "endTimeUnixNano": self.end_unix_nanos.to_string(),
            "attributes": self.attributes.iter().map(|(k, v)| json!({
                "key": k,
                "value": {"stringValue": v},
            })).collect::<Vec<_>>(),
            "status": {"code": self.status.code()},
        })
    }
}

/// A span being timed.
#[derive(Debug)]
pub struct InFlightSpan {
    context: SpanContext,
    started: SystemTime,
    attributes: Vec<(&'static str, String)>,
}

impl InFlightSpan {
    /// Adds an attribute.
    ///
    /// **PUBLIC or INTERNAL only.** A span leaves the cluster for a collector
    /// that may be shared, retained differently and read by more people than
    /// the database is. A company id, an org number, a filename or an amount
    /// must not be here — use the correlation id, which identifies the unit of
    /// work without identifying the customer.
    pub fn attribute(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.attributes.push((name, value.into()));
        self
    }

    pub fn finish(self, status: SpanStatus) -> FinishedSpan {
        let now = SystemTime::now();
        FinishedSpan {
            trace_id: self.context.trace_id,
            span_id: self.context.span_id,
            parent_span_id: self.context.parent_span_id,
            name: self.context.name,
            start_unix_nanos: unix_nanos(self.started),
            end_unix_nanos: unix_nanos(now),
            status,
            attributes: self.attributes,
        }
    }
}

fn unix_nanos(at: SystemTime) -> u128 {
    at.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct OtlpConfig {
    /// The collector's trace endpoint, e.g.
    /// `http://otel-collector.observability.svc:4318/v1/traces`.
    pub endpoint: String,
    pub service_name: String,
    pub environment: String,
}

impl OtlpConfig {
    /// Reads the configuration, or `None` when no collector is configured.
    ///
    /// Absent is a supported state: a deployment without a collector still
    /// carries trace ids in its logs, which is what the scheme was doing
    /// before this module existed.
    pub fn from_env(service_name: &str) -> Option<Self> {
        // The conventional variable name, so an operator who has configured
        // OpenTelemetry anywhere else does not have to learn a new one.
        let base = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?;
        let base = base.trim_end_matches('/');
        Some(Self {
            endpoint: if base.ends_with("/v1/traces") {
                base.to_string()
            } else {
                format!("{base}/v1/traces")
            },
            service_name: std::env::var("OTEL_SERVICE_NAME")
                .unwrap_or_else(|_| service_name.to_string()),
            environment: std::env::var("SKATTJAKT_ENVIRONMENT")
                .unwrap_or_else(|_| "unknown".to_string()),
        })
    }
}

/// Buffers spans and ships them to a collector.
#[derive(Debug, Clone)]
pub struct SpanExporter {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: Option<OtlpConfig>,
    buffer: Mutex<Vec<FinishedSpan>>,
    dropped: Mutex<u64>,
}

impl SpanExporter {
    /// An exporter that sends nothing.
    ///
    /// The no-collector case is this rather than an `Option<SpanExporter>` at
    /// every call site: a caller that has to remember to check is a caller that
    /// eventually does not.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(Inner {
                config: None,
                buffer: Mutex::new(Vec::new()),
                dropped: Mutex::new(0),
            }),
        }
    }

    pub fn new(config: OtlpConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config: Some(config),
                buffer: Mutex::new(Vec::new()),
                dropped: Mutex::new(0),
            }),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.config.is_some()
    }

    /// Queues a span.
    ///
    /// Never blocks and never fails. When the buffer is full the *oldest* span
    /// is dropped rather than the newest: during an incident the spans worth
    /// having are the ones being produced now.
    pub fn record(&self, span: FinishedSpan) {
        if self.inner.config.is_none() {
            return;
        }
        let Ok(mut buffer) = self.inner.buffer.lock() else {
            // A poisoned lock means another thread panicked while holding it.
            // Losing telemetry is the correct response; propagating the panic
            // into a request handler is not.
            return;
        };
        if buffer.len() >= MAX_BUFFERED {
            buffer.remove(0);
            if let Ok(mut dropped) = self.inner.dropped.lock() {
                *dropped += 1;
            }
        }
        buffer.push(span);
    }

    /// How many spans have been dropped for want of buffer space.
    ///
    /// Published as a metric, because an exporter that silently discards is an
    /// exporter nobody knows is broken.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.lock().map(|d| *d).unwrap_or(0)
    }

    pub fn buffered(&self) -> usize {
        self.inner.buffer.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// Takes everything buffered.
    fn drain(&self) -> Vec<FinishedSpan> {
        self.inner
            .buffer
            .lock()
            .map(|mut b| std::mem::take(&mut *b))
            .unwrap_or_default()
    }

    /// Builds the OTLP request body for a batch.
    pub fn request_body(&self, spans: &[FinishedSpan]) -> Value {
        let config = self.inner.config.as_ref();
        json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [
                        {"key": "service.name", "value": {"stringValue":
                            config.map(|c| c.service_name.clone()).unwrap_or_default()}},
                        {"key": "deployment.environment", "value": {"stringValue":
                            config.map(|c| c.environment.clone()).unwrap_or_default()}},
                    ]
                },
                "scopeSpans": [{
                    "scope": {"name": "skattjakt"},
                    "spans": spans.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
                }]
            }]
        })
    }

    /// Sends one batch. Returns how many spans were sent.
    pub async fn flush(&self, client: &reqwest::Client) -> usize {
        let Some(config) = self.inner.config.as_ref() else {
            return 0;
        };
        let spans = self.drain();
        if spans.is_empty() {
            return 0;
        }

        let body = self.request_body(&spans);
        match client
            .post(&config.endpoint)
            .header("content-type", "application/json")
            // Short. A collector that is slow must not become a memory problem
            // here, and the spans are worth less the older they are.
            .timeout(Duration::from_secs(10))
            .json(&body)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => spans.len(),
            Ok(response) => {
                // Dropped, not re-queued. Re-queuing on failure is how a
                // collector outage becomes an unbounded buffer, and the spans
                // are the least valuable thing in the process.
                crate::LogRecord::warn("the collector rejected a span batch")
                    .public("status", u64::from(response.status().as_u16()))
                    .internal("spans", spans.len() as i64)
                    .emit();
                0
            }
            Err(error) => {
                crate::LogRecord::warn("spans could not be sent to the collector")
                    .internal("error", error.to_string())
                    .internal("spans", spans.len() as i64)
                    .emit();
                0
            }
        }
    }

    /// Runs the flush loop until the process ends.
    pub fn spawn_flush_loop(&self) -> tokio::task::JoinHandle<()> {
        let exporter = self.clone();
        tokio::spawn(async move {
            if !exporter.is_enabled() {
                return;
            }
            let client = reqwest::Client::new();
            loop {
                tokio::time::sleep(FLUSH_INTERVAL).await;
                exporter.flush(&client).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracing_context::TraceContext;

    fn a_span(name: &'static str) -> FinishedSpan {
        FinishedSpan::start(TraceContext::root().start_span(name)).finish(SpanStatus::Ok)
    }

    #[test]
    fn a_disabled_exporter_buffers_nothing() {
        // No collector is a supported state, not a broken one: trace ids still
        // reach the log stream.
        let exporter = SpanExporter::disabled();
        exporter.record(a_span("http.request"));
        assert_eq!(exporter.buffered(), 0);
        assert!(!exporter.is_enabled());
    }

    fn enabled() -> SpanExporter {
        SpanExporter::new(OtlpConfig {
            endpoint: "http://collector:4318/v1/traces".into(),
            service_name: "skattjakt-api".into(),
            environment: "test".into(),
        })
    }

    #[test]
    fn the_buffer_is_bounded_and_drops_the_oldest() {
        // An unbounded buffer is a memory leak with a collector outage for a
        // trigger. The newest spans are the ones worth having in an incident.
        let exporter = enabled();
        for _ in 0..(MAX_BUFFERED + 10) {
            exporter.record(a_span("http.request"));
        }
        assert_eq!(exporter.buffered(), MAX_BUFFERED);
        assert_eq!(exporter.dropped(), 10);
    }

    #[test]
    fn timestamps_are_strings_because_nanoseconds_exceed_a_json_number() {
        // A collector that parses these as f64 loses microseconds.
        let exporter = enabled();
        let body = exporter.request_body(&[a_span("analysis.pipeline")]);
        let span = &body["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert!(span["startTimeUnixNano"].is_string());
        assert!(span["endTimeUnixNano"].is_string());
        assert!(span["startTimeUnixNano"].as_str().unwrap().len() >= 19);
    }

    #[test]
    fn the_body_carries_the_service_and_environment() {
        let exporter = enabled();
        let body = exporter.request_body(&[a_span("http.request")]);
        let attributes = &body["resourceSpans"][0]["resource"]["attributes"];
        let rendered = attributes.to_string();
        assert!(rendered.contains("service.name"));
        assert!(rendered.contains("skattjakt-api"));
        assert!(rendered.contains("deployment.environment"));
    }

    #[test]
    fn a_child_span_names_its_parent() {
        // The property that makes a trace a tree rather than a pile.
        let root = TraceContext::root();
        let parent = root.start_span("http.request");
        let child = parent.child_context().start_span("analysis.pipeline");

        let exporter = enabled();
        let body = exporter.request_body(&[
            FinishedSpan::start(parent).finish(SpanStatus::Ok),
            FinishedSpan::start(child).finish(SpanStatus::Ok),
        ]);
        let spans = &body["resourceSpans"][0]["scopeSpans"][0]["spans"];

        assert_eq!(spans[0]["traceId"], spans[1]["traceId"]);
        assert_eq!(spans[1]["parentSpanId"], spans[0]["spanId"]);
        // A root span has no parent, and OTLP wants an empty string for that.
        assert_eq!(spans[0]["parentSpanId"], "");
    }

    #[test]
    fn a_failed_span_carries_the_error_status() {
        let exporter = enabled();
        let failed = FinishedSpan::start(TraceContext::root().start_span("model.call"))
            .finish(SpanStatus::Error);
        let body = exporter.request_body(&[failed]);
        assert_eq!(
            body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"]["code"],
            2
        );
    }

    #[test]
    fn attributes_are_carried_as_strings() {
        let exporter = enabled();
        let span = FinishedSpan::start(TraceContext::root().start_span("http.request"))
            .attribute("http.route", "/v1/analyses")
            .attribute("http.status_class", "2xx")
            .finish(SpanStatus::Ok);
        let body = exporter.request_body(&[span]);
        let rendered =
            body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["attributes"].to_string();
        assert!(rendered.contains("http.route"));
        assert!(rendered.contains("/v1/analyses"));
    }

    #[test]
    fn the_endpoint_gets_the_path_appended_once() {
        // An operator may configure either form. Appending blindly produces
        // `/v1/traces/v1/traces`, which 404s in a way that looks like a network
        // problem.
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318");
        assert_eq!(
            OtlpConfig::from_env("x").unwrap().endpoint,
            "http://collector:4318/v1/traces"
        );
        std::env::set_var(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "http://collector:4318/v1/traces",
        );
        assert_eq!(
            OtlpConfig::from_env("x").unwrap().endpoint,
            "http://collector:4318/v1/traces"
        );
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
    }

    #[test]
    fn draining_empties_the_buffer() {
        let exporter = enabled();
        exporter.record(a_span("http.request"));
        exporter.record(a_span("http.request"));
        assert_eq!(exporter.drain().len(), 2);
        assert_eq!(exporter.buffered(), 0);
    }
}

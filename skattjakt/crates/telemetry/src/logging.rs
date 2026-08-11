//! Structured logging with classification enforced at the emitter (section 9,
//! section 43).
//!
//! Sections 20 and 45 both say the same thing from different directions: no
//! document data in logs, no customer data in metrics. That instruction cannot
//! be delivered as a convention, because the failure mode is a `tracing::info!`
//! written at 02:00 during an incident with the offending value already in
//! scope and an obvious reason to print it.
//!
//! So the log record is a value with typed fields. A field carries its
//! classification; `render` drops anything above `LOG_CEILING` and writes the
//! redaction marker in its place. The escape hatch is deliberate and awkward:
//! there isn't one.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use skattjakt_core::{Classification, REDACTED};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// One structured line.
///
/// `message` is `&'static str` on purpose. A formatted message is the other
/// route by which an amount reaches a log file; a static string plus typed
/// fields cannot carry one accidentally.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub level: Level,
    pub message: &'static str,
    pub at: DateTime<Utc>,
    pub correlation: Option<CorrelationId>,
    fields: BTreeMap<String, (Classification, Value)>,
}

impl LogRecord {
    pub fn new(level: Level, message: &'static str) -> Self {
        Self {
            level,
            message,
            at: Utc::now(),
            correlation: None,
            fields: BTreeMap::new(),
        }
    }

    pub fn info(message: &'static str) -> Self {
        Self::new(Level::Info, message)
    }

    pub fn warn(message: &'static str) -> Self {
        Self::new(Level::Warn, message)
    }

    pub fn error(message: &'static str) -> Self {
        Self::new(Level::Error, message)
    }

    pub fn correlate(mut self, id: CorrelationId) -> Self {
        self.correlation = Some(id);
        self
    }

    /// Adds a field at a stated classification. Accepting anything means the
    /// call site is never blocked mid-incident; `render` is where the decision
    /// is applied, uniformly, without the author having to remember.
    pub fn field(
        mut self,
        name: &str,
        value: impl Into<Value>,
        level: Classification,
    ) -> Self {
        self.fields
            .insert(name.to_string(), (level, value.into()));
        self
    }

    /// A field that is safe anywhere: a rule id, a state name, a status code.
    pub fn public(self, name: &str, value: impl Into<Value>) -> Self {
        self.field(name, value, Classification::Public)
    }

    /// Operational detail: durations, counts, error kinds, queue names.
    pub fn internal(self, name: &str, value: impl Into<Value>) -> Self {
        self.field(name, value, Classification::Internal)
    }

    /// Tenant-identifying. Recorded so the intent is visible in the code, and
    /// redacted on the way out.
    pub fn confidential(self, name: &str, value: impl Into<Value>) -> Self {
        self.field(name, value, Classification::Confidential)
    }

    /// The customer's economy. Recorded for the same reason, redacted harder.
    pub fn sensitive(self, name: &str, value: impl Into<Value>) -> Self {
        self.field(name, value, Classification::HighlyConfidential)
    }

    /// The JSON line that is actually written.
    pub fn render(&self) -> Value {
        self.render_at(Classification::LOG_CEILING)
    }

    /// Rendering at an explicit ceiling, so a destination with a different
    /// clearance — a trace exporter, an authorised error body — reuses the
    /// same record type rather than growing its own redaction logic.
    pub fn render_at(&self, ceiling: Classification) -> Value {
        let mut object = Map::new();
        object.insert("ts".into(), Value::String(self.at.to_rfc3339()));
        object.insert("level".into(), Value::String(self.level.as_str().into()));
        object.insert("msg".into(), Value::String(self.message.into()));
        if let Some(correlation) = &self.correlation {
            object.insert("correlation_id".into(), Value::String(correlation.to_string()));
        }
        for (name, (level, value)) in &self.fields {
            let rendered = if level.permitted_at(ceiling) {
                value.clone()
            } else {
                Value::String(REDACTED.to_string())
            };
            object.insert(name.clone(), rendered);
        }
        Value::Object(object)
    }

    pub fn to_line(&self) -> String {
        self.render().to_string()
    }

    /// Emits through `tracing`, so the record joins whatever subscriber the
    /// binary installed. The payload is the already-redacted JSON.
    pub fn emit(&self) {
        let line = self.to_line();
        match self.level {
            Level::Debug => tracing::debug!(record = %line),
            Level::Info => tracing::info!(record = %line),
            Level::Warn => tracing::warn!(record = %line),
            Level::Error => tracing::error!(record = %line),
        }
    }
}

/// A correlation id (section 13, section 43).
///
/// Minted at the edge, carried through the job row, the worker, every model
/// call and every log line. It is the only identifier that appears in logs, and
/// that is the trade: an operator can follow one request end to end without any
/// log store ever holding a company id.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct CorrelationId(uuid::Uuid);

impl CorrelationId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    pub const fn from_uuid(id: uuid::Uuid) -> Self {
        Self(id)
    }

    pub const fn as_uuid(&self) -> &uuid::Uuid {
        &self.0
    }

    /// Accepts a caller-supplied id so a client can correlate its own trace,
    /// but only if it parses. An arbitrary header value would be an injection
    /// point into the log store.
    pub fn parse(value: &str) -> Option<Self> {
        uuid::Uuid::parse_str(value).ok().map(Self)
    }
}

impl Default for CorrelationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The header the id travels in.
pub const CORRELATION_HEADER: &str = "x-correlation-id";

/// Installs the JSON subscriber. Called once per binary.
pub fn init(default_filter: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_filter));
    // `try_init` rather than `init`: a test binary that initialises twice
    // should not abort.
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .with_span_list(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_financial_amount_is_redacted_from_the_log_line() {
        let record = LogRecord::info("analysis finished").sensitive("estimated_total_ore", 18_600_000i64);
        let line = record.to_line();
        assert!(!line.contains("18600000"));
        assert!(line.contains(REDACTED));
    }

    #[test]
    fn a_company_id_is_redacted_from_the_log_line() {
        let record =
            LogRecord::info("analysis started").confidential("company_id", "8a5e1e6c-0000-0000-0000-000000000000");
        assert!(!record.to_line().contains("8a5e1e6c"));
    }

    #[test]
    fn document_text_is_redacted_from_the_log_line() {
        let excerpt = "Personalkostnader -4 210 000";
        let record = LogRecord::warn("could not parse line").sensitive("excerpt", excerpt);
        assert!(!record.to_line().contains("4 210 000"));
    }

    #[test]
    fn operational_fields_survive_so_the_line_is_still_useful() {
        let record = LogRecord::error("analysis failed")
            .public("state", "failed")
            .internal("duration_ms", 41_233u64)
            .internal("error_kind", "provider_timeout")
            .confidential("company_id", "8a5e")
            .sensitive("revenue_ore", 1_000_000i64);
        let value = record.render();
        assert_eq!(value["state"], "failed");
        assert_eq!(value["duration_ms"], 41_233);
        assert_eq!(value["error_kind"], "provider_timeout");
        assert_eq!(value["company_id"], REDACTED);
        assert_eq!(value["revenue_ore"], REDACTED);
    }

    #[test]
    fn the_correlation_id_is_present_so_a_request_can_still_be_followed() {
        let id = CorrelationId::new();
        let record = LogRecord::info("request").correlate(id);
        assert_eq!(record.render()["correlation_id"], id.to_string());
    }

    #[test]
    fn a_caller_supplied_correlation_id_must_parse_as_a_uuid() {
        assert!(CorrelationId::parse("not-a-uuid").is_none());
        assert!(CorrelationId::parse("\" injected }{").is_none());
        assert!(CorrelationId::parse("018f4d5e-0000-7000-8000-000000000000").is_some());
    }

    #[test]
    fn rendering_at_a_higher_ceiling_reveals_more_without_a_second_code_path() {
        let record = LogRecord::info("x").confidential("company_id", "8a5e");
        assert_eq!(record.render()["company_id"], REDACTED);
        assert_eq!(
            record.render_at(Classification::ERROR_BODY_CEILING)["company_id"],
            "8a5e"
        );
    }

    #[test]
    fn every_line_is_valid_json_with_the_standard_envelope() {
        let line = LogRecord::warn("something").internal("n", 1u64).to_line();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(parsed["ts"].is_string());
        assert_eq!(parsed["level"], "warn");
        assert_eq!(parsed["msg"], "something");
    }
}

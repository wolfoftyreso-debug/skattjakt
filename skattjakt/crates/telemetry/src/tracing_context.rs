//! W3C Trace Context propagation (section 43).
//!
//! A request enters the API, becomes a job row, is claimed by a worker in a
//! different pod minutes later, and makes several model calls. "Which spans
//! belong to that request?" has to survive a queue and a process boundary, so
//! the thing that gets propagated is the standard `traceparent` header —
//! parsed, validated and re-emitted, not a bespoke id scheme.
//!
//! What this module does: mint and propagate trace and span ids, in the format
//! every collector already understands, and attach them to log records so a
//! trace can be assembled from the log stream.
//!
//! What it does not do: export spans over OTLP. There is no exporter here and
//! no collector configured. `SKATTJAKT_DEPLOYMENT.md` records this as an open
//! gap rather than implying a pipeline that does not exist.

use std::fmt;

use crate::logging::LogRecord;

pub const TRACEPARENT_HEADER: &str = "traceparent";

/// 16 bytes, never all zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId([u8; 16]);

/// 8 bytes, never all zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId([u8; 8]);

impl TraceId {
    pub fn new() -> Self {
        // A v4 UUID is 16 bytes of the same randomness a trace id wants, and
        // the crate is already a dependency. The version and variant bits cost
        // 6 bits of entropy out of 128, which does not matter here.
        Self(*uuid::Uuid::new_v4().as_bytes())
    }

    pub fn parse(hex: &str) -> Option<Self> {
        let bytes: [u8; 16] = parse_hex::<16>(hex)?;
        (bytes != [0u8; 16]).then_some(Self(bytes))
    }

    pub fn to_hex(self) -> String {
        hex_of(&self.0)
    }
}

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl SpanId {
    pub fn new() -> Self {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let mut id = [0u8; 8];
        id.copy_from_slice(&bytes[..8]);
        // Astronomically unlikely, but an all-zero span id is invalid and the
        // check is one comparison.
        if id == [0u8; 8] {
            id[0] = 1;
        }
        Self(id)
    }

    pub fn parse(hex: &str) -> Option<Self> {
        let bytes: [u8; 8] = parse_hex::<8>(hex)?;
        (bytes != [0u8; 8]).then_some(Self(bytes))
    }

    pub fn to_hex(self) -> String {
        hex_of(&self.0)
    }
}

impl Default for SpanId {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_hex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Lower case only: the specification says so, and accepting upper case
    // would make two spellings of one id.
    if hex.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let mut out = [0u8; N];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// The trace a unit of work belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceContext {
    pub trace_id: TraceId,
    pub parent_span_id: Option<SpanId>,
    /// The single flag anyone uses: bit 0, "this trace is sampled".
    pub sampled: bool,
}

impl TraceContext {
    pub fn root() -> Self {
        Self {
            trace_id: TraceId::new(),
            parent_span_id: None,
            sampled: true,
        }
    }

    /// Reads an inbound header. Malformed values are ignored rather than
    /// rejected — a broken trace header should not fail a request — and the
    /// caller starts a new trace instead.
    pub fn parse(header: &str) -> Option<Self> {
        let mut parts = header.split('-');
        let version = parts.next()?;
        let trace_id = parts.next()?;
        let span_id = parts.next()?;
        let flags = parts.next()?;
        // Version ff is explicitly forbidden; higher versions must still parse
        // their first four fields, so unknown versions are accepted.
        if version.len() != 2 || version == "ff" || !version.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        if version == "00" && parts.next().is_some() {
            return None;
        }
        let flags = u8::from_str_radix(flags, 16).ok()?;
        Some(Self {
            trace_id: TraceId::parse(trace_id)?,
            parent_span_id: SpanId::parse(span_id),
            sampled: flags & 0x01 != 0,
        })
    }

    /// Reads the header if it is valid, and starts a fresh trace if it is not.
    pub fn from_header_or_new(header: Option<&str>) -> Self {
        header.and_then(Self::parse).unwrap_or_else(Self::root)
    }

    /// Starts a span in this trace.
    pub fn start_span(&self, name: &'static str) -> SpanContext {
        SpanContext {
            trace_id: self.trace_id,
            span_id: SpanId::new(),
            parent_span_id: self.parent_span_id,
            sampled: self.sampled,
            name,
        }
    }
}

/// One span. Cheap and copyable: it holds ids and a static name, no attributes.
/// Attributes go on the log record, where the classification rules apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanContext {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_span_id: Option<SpanId>,
    pub sampled: bool,
    pub name: &'static str,
}

impl SpanContext {
    /// The header to send downstream, so the next service continues the trace
    /// with this span as its parent.
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            self.trace_id.to_hex(),
            self.span_id.to_hex(),
            u8::from(self.sampled)
        )
    }

    /// A context for work that continues elsewhere — the job row a worker will
    /// pick up, or a nested call.
    pub fn child_context(&self) -> TraceContext {
        TraceContext {
            trace_id: self.trace_id,
            parent_span_id: Some(self.span_id),
            sampled: self.sampled,
        }
    }

    /// Stamps a log record with the span, so the log stream carries the trace.
    ///
    /// Trace and span ids are `Internal`: they identify a unit of work, not a
    /// customer, so they survive the log ceiling. That is the whole reason the
    /// scheme works without putting tenant ids in logs.
    pub fn annotate(&self, record: LogRecord) -> LogRecord {
        let record = record
            .internal("trace_id", self.trace_id.to_hex())
            .internal("span_id", self.span_id.to_hex())
            .public("span", self.name);
        match self.parent_span_id {
            Some(parent) => record.internal("parent_span_id", parent.to_hex()),
            None => record,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skattjakt_core::Classification;

    #[test]
    fn a_valid_traceparent_round_trips() {
        let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let context = TraceContext::parse(header).unwrap();
        assert_eq!(
            context.trace_id.to_hex(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(context.parent_span_id.unwrap().to_hex(), "00f067aa0ba902b7");
        assert!(context.sampled);
    }

    #[test]
    fn an_all_zero_trace_id_is_rejected() {
        assert!(
            TraceContext::parse("00-00000000000000000000000000000000-00f067aa0ba902b7-01")
                .is_none()
        );
    }

    #[test]
    fn malformed_headers_are_ignored_rather_than_failing_the_request() {
        for header in [
            "",
            "garbage",
            "00-tooshort-00f067aa0ba902b7-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        ] {
            assert!(TraceContext::parse(header).is_none(), "{header} parsed");
            // And the fallback still produces a usable trace.
            let context = TraceContext::from_header_or_new(Some(header));
            assert!(context.parent_span_id.is_none());
        }
    }

    #[test]
    fn a_future_version_still_parses_its_first_four_fields() {
        let context =
            TraceContext::parse("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x:y");
        assert!(context.is_some());
    }

    #[test]
    fn a_child_span_keeps_the_trace_and_takes_the_parent() {
        let root = TraceContext::root();
        let span = root.start_span("http.request");
        let child = span.child_context().start_span("analysis.run");
        assert_eq!(child.trace_id, root.trace_id);
        assert_eq!(child.parent_span_id, Some(span.span_id));
        assert_ne!(child.span_id, span.span_id);
    }

    #[test]
    fn the_outbound_header_is_well_formed_and_reparses() {
        let span = TraceContext::root().start_span("model.call");
        let header = span.traceparent();
        let parsed = TraceContext::parse(&header).unwrap();
        assert_eq!(parsed.trace_id, span.trace_id);
        assert_eq!(parsed.parent_span_id, Some(span.span_id));
    }

    #[test]
    fn span_ids_reach_the_logs_and_tenant_data_still_does_not() {
        let span = TraceContext::root().start_span("analysis.run");
        let record = span
            .annotate(LogRecord::info("stage finished"))
            .confidential("company_id", "8a5e");
        let rendered = record.render();
        assert_eq!(rendered["trace_id"], span.trace_id.to_hex());
        assert_eq!(rendered["span"], "analysis.run");
        assert_eq!(rendered["company_id"], skattjakt_core::REDACTED);
    }

    #[test]
    fn trace_ids_sit_below_the_log_ceiling_by_design() {
        assert!(Classification::Internal.permitted_at(Classification::LOG_CEILING));
    }
}

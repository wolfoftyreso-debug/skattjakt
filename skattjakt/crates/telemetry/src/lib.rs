//! # skattjakt-telemetry
//!
//! Metrics, correlation ids and structured logging, with the data
//! classification rules of section 9 applied at the emitter rather than left to
//! the caller's memory.
//!
//! Two things live here that are usually taken off the shelf — a metrics
//! registry and a log record type — and both are hand-written for the same
//! reason. The off-the-shelf versions accept any string as a label or a field,
//! and this product has a hard rule that some strings must never reach a log
//! store or a time-series database. A rule enforced by the API is a rule; a
//! rule enforced by review is a hope.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod logging;
pub mod metrics;
pub mod otlp;
pub mod tracing_context;

pub use logging::{CorrelationId, Level, LogRecord, CORRELATION_HEADER};
pub use metrics::{names, LabelError, LabelSet, Registry};
pub use tracing_context::{SpanContext, TraceContext, TRACEPARENT_HEADER};

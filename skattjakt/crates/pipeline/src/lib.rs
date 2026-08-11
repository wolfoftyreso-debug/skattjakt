//! # skattjakt-pipeline
//!
//! Orchestrates one analysis, in the order section 9 lays down:
//!
//! ```text
//! documents → extraction → financial facts → model discovery → rule engine
//!   → calculation → skeptic/falsification → evidence validation → opportunities
//! ```
//!
//! The division of labour is the point. The model proposes; the rule engine
//! disposes. Money is computed by code. A finding that cannot show a document
//! value and a versioned rule never reaches the user as something to act on.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod facts;
pub mod pipeline;
pub mod report;

pub use facts::{build_fact_set, DocumentInput};
pub use pipeline::{AnalysisInput, AnalysisPipeline, PipelineConfig, PipelineError};
pub use report::{build as build_report, to_markdown, Report};

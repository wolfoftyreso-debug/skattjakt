//! # skattjakt-model
//!
//! The reasoning layer: a provider abstraction (section 8), versioned prompts,
//! and mandatory structured-output validation.
//!
//! The model interprets, reasons, spots patterns and forms hypotheses. It does
//! not decide whether a rule applies, and it does not compute money — those
//! belong to `skattjakt-rules` (section 9). Nothing here stores chain-of-thought.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod anthropic;
pub mod prompts;
pub mod provider;
pub mod schema;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use prompts::{output_schema, system_prompt, PROMPT_VERSION};
pub use provider::{
    Effort, ModelProvider, ModelRequest, ModelResponse, ModelRunRecord, ModelRunStatus,
    ProviderError, ProviderResult, ReasoningTask, ScriptedProvider, TokenUsage,
};
pub use schema::{validate, SchemaError};

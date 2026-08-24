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

// Behind `native` because it opens sockets, and the engine has to build for
// wasm32 where nothing can. Turning the feature off removes the network, not
// the analysis: the rules, the extraction and the report are untouched.
#[cfg(feature = "native")]
pub mod anthropic;
pub mod prompts;
pub mod provider;
pub mod schema;

#[cfg(feature = "native")]
pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use prompts::{output_schema, system_prompt, PROMPT_VERSION};
pub use provider::{
    fallback_enabled, Effort, ModelProvider, ModelRequest, ModelResponse, ModelRunRecord,
    ModelRunStatus, ProviderError, ProviderResult, ReasoningTask, ScriptedProvider, TokenUsage,
};
pub use schema::{validate, SchemaError};

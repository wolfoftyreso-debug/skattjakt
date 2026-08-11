//! The model provider abstraction.
//!
//! Section 8 is explicit that Skattjakt must not be hard-wired to one model
//! version. Everything above this layer speaks in `ReasoningTask`s and JSON
//! schemas; which model answers them is a deployment decision.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The reasoning tasks Skattjakt asks a model to perform (section 8).
///
/// Each task has its own prompt, its own output schema, and its own place in
/// the pipeline. Keeping them as a closed enum means a stored model run can
/// always be attributed to a known step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningTask {
    DocumentUnderstanding,
    FinancialExtraction,
    OpportunityDiscovery,
    TaxReasoning,
    ContradictionCheck,
    EvidenceValidation,
    CalculationReview,
    FinalSynthesis,
}

impl ReasoningTask {
    pub fn key(self) -> &'static str {
        match self {
            ReasoningTask::DocumentUnderstanding => "document_understanding",
            ReasoningTask::FinancialExtraction => "financial_extraction",
            ReasoningTask::OpportunityDiscovery => "opportunity_discovery",
            ReasoningTask::TaxReasoning => "tax_reasoning",
            ReasoningTask::ContradictionCheck => "contradiction_check",
            ReasoningTask::EvidenceValidation => "evidence_validation",
            ReasoningTask::CalculationReview => "calculation_review",
            ReasoningTask::FinalSynthesis => "final_synthesis",
        }
    }
}

impl fmt::Display for ReasoningTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// How much reasoning depth to spend. Maps to the provider's effort control
/// where one exists, and is ignored where it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

/// One request to a model.
///
/// The prompt is split into a versioned system prompt and the task payload, so
/// a stored run records exactly which prompt version produced it (section 21).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub task: ReasoningTask,
    pub prompt_version: String,
    pub system: String,
    pub user_content: String,
    /// JSON Schema the response must satisfy. Structured output is mandatory:
    /// free text from a model has no place in an evidence chain.
    pub schema: serde_json::Value,
    pub max_tokens: u32,
    pub effort: Effort,
}

/// One response from a model.
///
/// Deliberately excludes chain-of-thought. Section 21 lists what may be stored
/// — conclusions, evidence, calculations, a rationale summary, validation state
/// — and reasoning traces are not on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub task: ReasoningTask,
    /// The validated structured output.
    pub output: serde_json::Value,
    pub model: String,
    pub prompt_version: String,
    pub usage: TokenUsage,
    pub latency_ms: u64,
    pub finished_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("the model declined the request ({category}): {explanation}")]
    Refused {
        category: String,
        explanation: String,
    },

    #[error("the response was cut off before it was complete; raise max_tokens")]
    Truncated,

    #[error("the model returned output that does not satisfy the schema: {0}")]
    SchemaViolation(String),

    #[error("the provider is not configured: {0}")]
    NotConfigured(String),

    #[error("provider returned {status}: {message}")]
    Http { status: u16, message: String },

    #[error("transport failure: {0}")]
    Transport(String),

    #[error("could not parse the provider response: {0}")]
    Malformed(String),
}

impl ProviderError {
    /// Whether retrying the identical request could plausibly succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Transport(_) => true,
            ProviderError::Http { status, .. } => {
                *status == 429 || *status == 529 || (500..600).contains(status)
            }
            _ => false,
        }
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;

/// A model that can perform reasoning tasks.
#[async_trait]
pub trait ModelProvider: Send + Sync + fmt::Debug {
    /// Stable identifier for the provider, e.g. `anthropic`.
    fn name(&self) -> &str;

    /// The model this provider will use. Recorded on every run.
    fn model_id(&self) -> &str;

    async fn run(&self, request: &ModelRequest) -> ProviderResult<ModelResponse>;
}

/// The record persisted for every model call (section 21).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRunRecord {
    pub id: skattjakt_core::ModelRunId,
    pub analysis_id: skattjakt_core::AnalysisId,
    pub company_id: skattjakt_core::CompanyId,
    pub provider: String,
    pub model: String,
    pub task: ReasoningTask,
    pub prompt_version: String,
    /// Document versions the request was built from, for reproducibility.
    pub document_version_ids: Vec<skattjakt_core::DocumentVersionId>,
    pub status: ModelRunStatus,
    pub usage: TokenUsage,
    pub latency_ms: u64,
    /// The structured conclusion. Never the reasoning trace.
    pub output: serde_json::Value,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRunStatus {
    Succeeded,
    Refused,
    Failed,
}

/// A deterministic provider for tests and for the golden dataset.
///
/// Real model calls make the pipeline untestable: the golden dataset in section
/// 29 measures the *pipeline's* precision, which requires the model's
/// contribution to be fixed. Responses are keyed by task.
#[derive(Debug, Default, Clone)]
pub struct ScriptedProvider {
    responses: BTreeMap<ReasoningTask, serde_json::Value>,
    model_id: String,
}

impl ScriptedProvider {
    pub fn new() -> Self {
        Self {
            responses: BTreeMap::new(),
            model_id: "scripted-1".to_string(),
        }
    }

    pub fn with(mut self, task: ReasoningTask, output: serde_json::Value) -> Self {
        self.responses.insert(task, output);
        self
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn run(&self, request: &ModelRequest) -> ProviderResult<ModelResponse> {
        let output = self.responses.get(&request.task).cloned().ok_or_else(|| {
            ProviderError::NotConfigured(format!("no scripted response for {}", request.task))
        })?;

        Ok(ModelResponse {
            task: request.task,
            output,
            model: self.model_id.clone(),
            prompt_version: request.prompt_version.clone(),
            usage: TokenUsage::default(),
            latency_ms: 0,
            finished_at: Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_keys_are_stable() {
        assert_eq!(
            ReasoningTask::OpportunityDiscovery.key(),
            "opportunity_discovery"
        );
        assert_eq!(
            ReasoningTask::ContradictionCheck.to_string(),
            "contradiction_check"
        );
    }

    #[test]
    fn retryable_classification_matches_provider_semantics() {
        assert!(ProviderError::Transport("reset".into()).is_retryable());
        assert!(ProviderError::Http {
            status: 429,
            message: "rate limited".into()
        }
        .is_retryable());
        assert!(ProviderError::Http {
            status: 529,
            message: "overloaded".into()
        }
        .is_retryable());
        assert!(ProviderError::Http {
            status: 503,
            message: "down".into()
        }
        .is_retryable());

        assert!(!ProviderError::Http {
            status: 400,
            message: "bad".into()
        }
        .is_retryable());
        assert!(!ProviderError::Refused {
            category: "cyber".into(),
            explanation: "no".into()
        }
        .is_retryable());
        assert!(!ProviderError::SchemaViolation("missing field".into()).is_retryable());
        assert!(!ProviderError::Truncated.is_retryable());
    }

    #[tokio::test]
    async fn the_scripted_provider_returns_the_configured_output() {
        let provider = ScriptedProvider::new().with(
            ReasoningTask::OpportunityDiscovery,
            serde_json::json!({"candidates": []}),
        );

        let request = ModelRequest {
            task: ReasoningTask::OpportunityDiscovery,
            prompt_version: "v1".into(),
            system: "s".into(),
            user_content: "u".into(),
            schema: serde_json::json!({}),
            max_tokens: 100,
            effort: Effort::High,
        };

        let response = provider.run(&request).await.unwrap();
        assert_eq!(response.output, serde_json::json!({"candidates": []}));
        assert_eq!(response.model, "scripted-1");
    }

    #[tokio::test]
    async fn an_unscripted_task_fails_loudly_rather_than_returning_nothing() {
        let provider = ScriptedProvider::new();
        let request = ModelRequest {
            task: ReasoningTask::FinalSynthesis,
            prompt_version: "v1".into(),
            system: "s".into(),
            user_content: "u".into(),
            schema: serde_json::json!({}),
            max_tokens: 100,
            effort: Effort::Low,
        };
        assert!(matches!(
            provider.run(&request).await,
            Err(ProviderError::NotConfigured(_))
        ));
    }

    #[test]
    fn the_response_type_has_no_place_to_put_chain_of_thought() {
        // Guards section 21 structurally: if someone adds a `reasoning` field
        // to ModelResponse, this serialisation check is where it shows up.
        let response = ModelResponse {
            task: ReasoningTask::TaxReasoning,
            output: serde_json::json!({"ok": true}),
            model: "m".into(),
            prompt_version: "v".into(),
            usage: TokenUsage::default(),
            latency_ms: 1,
            finished_at: Utc::now(),
        };
        let value = serde_json::to_value(&response).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "finished_at",
                "latency_ms",
                "model",
                "output",
                "prompt_version",
                "task",
                "usage"
            ],
            "a field was added to ModelResponse — check it is not a reasoning trace"
        );
    }
}

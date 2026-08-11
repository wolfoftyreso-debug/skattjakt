//! Anthropic Messages API adapter.
//!
//! Talks raw HTTP rather than through an SDK: Rust has no first-party
//! Anthropic SDK, and the request surface Skattjakt needs is small and stable.
//!
//! Section 8 forbids hard-wiring the product to a model version, so the model
//! identifier is required configuration with no compiled-in default. Deployment
//! picks the strongest suitable model available at the time; the code does not
//! decide for it.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::provider::{
    ModelProvider, ModelRequest, ModelResponse, ProviderError, ProviderResult, TokenUsage,
};
use crate::schema;

/// The Messages API version header. Pinned: it is a contract version, not a
/// model version.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Opts the request into server-side fallback, so a request the model's safety
/// classifiers decline is re-run on a substitute rather than failing. Financial
/// documents occasionally trip classifiers on benign content.
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    /// Model identifier. Required — see the module note on section 8.
    pub model_id: String,
    pub base_url: String,
    pub timeout: Duration,
    pub max_retries: u32,
    /// Whether to request server-side fallback on a safety refusal.
    pub enable_fallback: bool,
}

impl AnthropicConfig {
    /// Reads configuration from the environment.
    ///
    /// `SKATTJAKT_MODEL_ID` has no default on purpose: an operator must state
    /// which model the analysis ran on, because that is part of the audit
    /// trail.
    pub fn from_env() -> Result<Self, ProviderError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ProviderError::NotConfigured("ANTHROPIC_API_KEY is not set".into()))?;
        let model_id = std::env::var("SKATTJAKT_MODEL_ID").map_err(|_| {
            ProviderError::NotConfigured(
                "SKATTJAKT_MODEL_ID is not set; name the model this deployment should use".into(),
            )
        })?;

        if api_key.trim().is_empty() {
            return Err(ProviderError::NotConfigured("ANTHROPIC_API_KEY is empty".into()));
        }
        if model_id.trim().is_empty() {
            return Err(ProviderError::NotConfigured("SKATTJAKT_MODEL_ID is empty".into()));
        }

        Ok(Self {
            api_key,
            model_id,
            base_url: std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string()),
            timeout: Duration::from_secs(
                std::env::var("SKATTJAKT_MODEL_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(600),
            ),
            max_retries: std::env::var("SKATTJAKT_MODEL_MAX_RETRIES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
            enable_fallback: std::env::var("SKATTJAKT_MODEL_FALLBACK")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
        })
    }
}

#[derive(Debug)]
pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> ProviderResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        Ok(Self { config, client })
    }

    fn body(&self, request: &ModelRequest) -> serde_json::Value {
        let mut body = json!({
            "model": self.config.model_id,
            "max_tokens": request.max_tokens,
            "system": request.system,
            "messages": [{ "role": "user", "content": request.user_content }],
            "output_config": {
                "effort": request.effort.as_str(),
                "format": {
                    "type": "json_schema",
                    "schema": request.schema,
                }
            }
        });

        if self.config.enable_fallback {
            body["fallbacks"] = json!("default");
        }
        body
    }

    async fn send_once(&self, request: &ModelRequest) -> ProviderResult<ApiResponse> {
        let mut builder = self
            .client
            .post(format!("{}/v1/messages", self.config.base_url.trim_end_matches('/')))
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");

        if self.config.enable_fallback {
            builder = builder.header("anthropic-beta", SERVER_SIDE_FALLBACK_BETA);
        }

        let response = builder
            .json(&self.body(request))
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());

        let text = response
            .text()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !status.is_success() {
            // The body may carry the provider's own error description; it is
            // relayed but never logged alongside document content.
            let message = serde_json::from_str::<ApiError>(&text)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| status.canonical_reason().unwrap_or("request failed").to_string());
            return Err(ProviderError::Http { status: status.as_u16(), message })
                .map_err(|e| attach_retry_after(e, retry_after));
        }

        serde_json::from_str(&text).map_err(|e| ProviderError::Malformed(e.to_string()))
    }
}

/// Retry hints ride on the error message so callers do not need a second
/// channel to learn how long to wait.
fn attach_retry_after(error: ProviderError, retry_after: Option<u64>) -> ProviderError {
    match (error, retry_after) {
        (ProviderError::Http { status, message }, Some(seconds)) => ProviderError::Http {
            status,
            message: format!("{message} (retry after {seconds}s)"),
        },
        (error, _) => error,
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn model_id(&self) -> &str {
        &self.config.model_id
    }

    async fn run(&self, request: &ModelRequest) -> ProviderResult<ModelResponse> {
        let started = std::time::Instant::now();
        let mut attempt = 0;

        let api = loop {
            match self.send_once(request).await {
                Ok(response) => break response,
                Err(e) if e.is_retryable() && attempt < self.config.max_retries => {
                    // Exponential backoff. The provider's own retry-after would
                    // be more precise; this floor keeps a burst of analyses
                    // from compounding a rate limit.
                    let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        };

        let output = api.into_structured_output(&request.schema)?;

        Ok(ModelResponse {
            task: request.task,
            output,
            model: self.config.model_id.clone(),
            prompt_version: request.prompt_version.clone(),
            usage: api_usage(&api.usage),
            latency_ms: started.elapsed().as_millis() as u64,
            finished_at: Utc::now(),
        })
    }
}

fn api_usage(usage: &ApiUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
        cache_creation_input_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
    }
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    stop_details: Option<StopDetails>,
    #[serde(default)]
    usage: ApiUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct StopDetails {
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    explanation: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: ApiErrorBody,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    message: String,
}

impl ApiResponse {
    /// Extracts and validates the structured output.
    ///
    /// The stop reason is checked *before* the content: a refusal returns HTTP
    /// 200 with empty or partial content, so reading the first block without
    /// checking would treat a decline as an answer.
    fn into_structured_output(&self, expected: &serde_json::Value) -> ProviderResult<serde_json::Value> {
        match self.stop_reason.as_deref() {
            Some("refusal") => {
                let details = self.stop_details.as_ref();
                return Err(ProviderError::Refused {
                    category: details
                        .and_then(|d| d.category.clone())
                        .unwrap_or_else(|| "unspecified".to_string()),
                    explanation: details
                        .and_then(|d| d.explanation.clone())
                        .unwrap_or_else(|| "the provider gave no explanation".to_string()),
                });
            }
            Some("max_tokens") => return Err(ProviderError::Truncated),
            _ => {}
        }

        let text = self
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Other => None,
            })
            .ok_or_else(|| ProviderError::Malformed("no text block in the response".into()))?;

        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| ProviderError::SchemaViolation(format!("output was not valid JSON: {e}")))?;

        schema::validate(&value, expected).map_err(|errors| {
            ProviderError::SchemaViolation(
                errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "),
            )
        })?;

        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Effort;

    fn parse(body: serde_json::Value) -> ApiResponse {
        serde_json::from_value(body).unwrap()
    }

    fn schema() -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["ok"],
            "additionalProperties": false,
            "properties": {"ok": {"type": "boolean"}}
        })
    }

    #[test]
    fn a_well_formed_response_yields_validated_output() {
        let response = parse(json!({
            "content": [{"type": "text", "text": "{\"ok\": true}"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 4}
        }));
        assert_eq!(
            response.into_structured_output(&schema()).unwrap(),
            json!({"ok": true})
        );
    }

    #[test]
    fn a_refusal_is_detected_before_the_content_is_read() {
        // A refusal is HTTP 200 with empty content — reading content[0] first
        // would panic or, worse, silently produce nothing.
        let response = parse(json!({
            "content": [],
            "stop_reason": "refusal",
            "stop_details": {"category": "cyber", "explanation": "declined"}
        }));
        match response.into_structured_output(&schema()) {
            Err(ProviderError::Refused { category, .. }) => assert_eq!(category, "cyber"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_refusal_with_partial_content_is_still_a_refusal() {
        let response = parse(json!({
            "content": [{"type": "text", "text": "{\"ok\": true}"}],
            "stop_reason": "refusal"
        }));
        assert!(matches!(
            response.into_structured_output(&schema()),
            Err(ProviderError::Refused { .. })
        ));
    }

    #[test]
    fn truncation_is_reported_rather_than_parsed_as_a_result() {
        let response = parse(json!({
            "content": [{"type": "text", "text": "{\"ok\": tr"}],
            "stop_reason": "max_tokens"
        }));
        assert!(matches!(
            response.into_structured_output(&schema()),
            Err(ProviderError::Truncated)
        ));
    }

    #[test]
    fn output_violating_the_schema_is_rejected() {
        let response = parse(json!({
            "content": [{"type": "text", "text": "{\"ok\": \"yes\"}"}],
            "stop_reason": "end_turn"
        }));
        assert!(matches!(
            response.into_structured_output(&schema()),
            Err(ProviderError::SchemaViolation(_))
        ));
    }

    #[test]
    fn non_json_output_is_rejected_as_a_schema_violation() {
        let response = parse(json!({
            "content": [{"type": "text", "text": "Här är analysen:"}],
            "stop_reason": "end_turn"
        }));
        assert!(matches!(
            response.into_structured_output(&schema()),
            Err(ProviderError::SchemaViolation(_))
        ));
    }

    #[test]
    fn unknown_content_block_types_are_skipped_not_fatal() {
        let response = parse(json!({
            "content": [
                {"type": "thinking", "thinking": ""},
                {"type": "text", "text": "{\"ok\": false}"}
            ],
            "stop_reason": "end_turn"
        }));
        assert_eq!(
            response.into_structured_output(&schema()).unwrap(),
            json!({"ok": false})
        );
    }

    #[test]
    fn config_requires_an_explicit_model_id() {
        // Section 8: no compiled-in model version.
        let source = include_str!("anthropic.rs");
        assert!(
            !source.contains("model_id: \"claude"),
            "the model identifier must come from configuration, not a literal"
        );
    }

    #[test]
    fn the_request_body_carries_the_schema_and_effort() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "k".into(),
            model_id: "configured-model".into(),
            base_url: DEFAULT_BASE_URL.into(),
            timeout: Duration::from_secs(1),
            max_retries: 0,
            enable_fallback: true,
        })
        .unwrap();

        let request = ModelRequest {
            task: crate::provider::ReasoningTask::OpportunityDiscovery,
            prompt_version: "v1".into(),
            system: "system".into(),
            user_content: "content".into(),
            schema: schema(),
            max_tokens: 4096,
            effort: Effort::High,
        };

        let body = provider.body(&request);
        assert_eq!(body["model"], "configured-model");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"], schema());
        assert_eq!(body["fallbacks"], "default");
        // Parameters removed on current models must not be sent.
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn fallback_can_be_disabled() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "k".into(),
            model_id: "m".into(),
            base_url: DEFAULT_BASE_URL.into(),
            timeout: Duration::from_secs(1),
            max_retries: 0,
            enable_fallback: false,
        })
        .unwrap();
        let request = ModelRequest {
            task: crate::provider::ReasoningTask::FinalSynthesis,
            prompt_version: "v1".into(),
            system: "s".into(),
            user_content: "c".into(),
            schema: schema(),
            max_tokens: 1,
            effort: Effort::Low,
        };
        assert!(provider.body(&request).get("fallbacks").is_none());
    }
}

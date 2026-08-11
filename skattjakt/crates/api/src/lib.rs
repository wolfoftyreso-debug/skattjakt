//! # skattjakt-api
//!
//! The HTTP surface. `api/openapi.yaml` is the contract (section 17); this
//! implements it and serves it.
//!
//! Everything the API can do, it does through the pipeline — there is no
//! analysis logic here, only transport, authentication and shape.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use skattjakt_core::analysis::AnalysisResult;
use skattjakt_core::document::{AccountsState, MimeType};
use skattjakt_core::{AnalysisId, CompanyId, CompanyProfile, FiscalYear, OrgNumber};
use skattjakt_model::{ModelProvider, ScriptedProvider};
use skattjakt_pipeline::pipeline::{AnalysisPipeline, PipelineConfig, PipelineError, SilentObserver};
use skattjakt_pipeline::{AnalysisInput, DocumentInput};
use skattjakt_rules::{ReviewState, RuleEngine};

/// The contract, compiled in so a deployed build can always serve the exact
/// contract it was built against.
const OPENAPI: &str = include_str!("../../../api/openapi.yaml");

/// Uploads are bounded; an unbounded body is a denial-of-service surface and a
/// very large prompt.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<RuleEngine>,
    pub provider: Arc<dyn ModelProvider>,
    pub config: PipelineConfig,
    /// Bearer token required on `/v1` routes. When `None`, those routes are
    /// closed entirely rather than left open.
    pub api_token: Option<String>,
    pub model_configured: bool,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("rule_set", &self.engine.version())
            .field("provider", &self.provider.name())
            .field("authenticated", &self.api_token.is_some())
            .finish()
    }
}

impl AppState {
    /// Builds state from the environment.
    ///
    /// A missing model provider is not fatal: the rule engine produces
    /// evidence-backed findings on its own, and a rules-only analysis is more
    /// useful than no service. Readiness reports the degraded state.
    pub fn from_env() -> Result<Self, String> {
        let engine = Arc::new(RuleEngine::load_embedded().map_err(|e| e.to_string())?);

        let (provider, model_configured): (Arc<dyn ModelProvider>, bool) =
            match skattjakt_model::AnthropicConfig::from_env() {
                Ok(config) => match skattjakt_model::AnthropicProvider::new(config) {
                    Ok(provider) => (Arc::new(provider), true),
                    Err(e) => {
                        tracing::warn!(error = %e, "model provider unavailable; running rules-only");
                        (Arc::new(ScriptedProvider::new()), false)
                    }
                },
                Err(e) => {
                    tracing::warn!(reason = %e, "model provider not configured; running rules-only");
                    (Arc::new(ScriptedProvider::new()), false)
                }
            };

        Ok(Self {
            engine,
            provider,
            config: PipelineConfig::default(),
            api_token: std::env::var("SKATTJAKT_API_TOKEN").ok().filter(|t| !t.is_empty()),
            model_configured,
        })
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/openapi.yaml", get(openapi))
        .route("/v1/rules", get(rules))
        .route("/v1/analyses", post(analyse))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Problem {
    status: StatusCode,
    title: String,
    detail: String,
}

impl Problem {
    fn bad_request(title: &str, detail: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, title: title.into(), detail: detail.into() }
    }

    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            title: "unauthorized".into(),
            // Deliberately uninformative: whether a token exists is not a
            // client's business.
            detail: "a valid bearer token is required".into(),
        }
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"title": self.title, "detail": self.detail}))).into_response()
    }
}

/// Checks the bearer token in constant time, so a timing difference cannot be
/// used to recover it byte by byte.
fn authorise(state: &AppState, headers: &HeaderMap) -> Result<(), Problem> {
    let Some(expected) = state.api_token.as_deref() else {
        // No token configured means the authenticated surface is closed, not open.
        return Err(Problem::unauthorized());
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(Problem::unauthorized)?;

    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(Problem::unauthorized())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}))
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let mut reasons = Vec::new();
    if !state.model_configured {
        reasons.push(
            "no model provider is configured; analyses will run on the rule engine alone"
                .to_string(),
        );
    }
    if state.api_token.is_none() {
        reasons.push("SKATTJAKT_API_TOKEN is not set; the /v1 routes are closed".to_string());
    }

    let body = json!({
        "ready": reasons.is_empty(),
        "rule_set_version": state.engine.version(),
        "model_provider": state.model_configured.then(|| state.provider.name().to_string()),
        "reasons": reasons,
    });

    let status = if reasons.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body))
}

async fn openapi() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/yaml")], OPENAPI)
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

async fn rules(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, Problem> {
    authorise(&state, &headers)?;

    let rules: Vec<serde_json::Value> = state
        .engine
        .rules()
        .iter()
        .map(|rule| {
            let (reviewed, note) = match &rule.review {
                ReviewState::Reviewed { .. } => (true, None),
                ReviewState::AwaitingProfessionalReview { note } => (false, Some(note.clone())),
            };
            json!({
                "rule_id": rule.rule_id,
                "version": rule.version,
                "title": rule.title,
                "category": rule.category,
                "tax_year_from": rule.tax_year_from,
                "tax_year_to": rule.tax_year_to,
                "source": rule.source.citation,
                "reviewed": reviewed,
                "review_note": note,
            })
        })
        .collect();

    let unreviewed = state.engine.rules().iter().filter(|r| !r.review.is_reviewed()).count();

    Ok(Json(json!({
        "version": state.engine.version(),
        "jurisdiction": "SE",
        "unreviewed_count": unreviewed,
        "rules": rules,
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AnalysisRequest {
    pub company: CompanyProfileRequest,
    pub documents: Vec<DocumentUpload>,
    #[serde(default)]
    pub accounts_state: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CompanyProfileRequest {
    pub name: String,
    pub org_number: String,
    pub fiscal_year: FiscalYearRequest,
    #[serde(default)]
    pub industry: Option<String>,
    #[serde(default)]
    pub sni_code: Option<String>,
    #[serde(default)]
    pub employee_count: Option<u32>,
    #[serde(default)]
    pub owner_count: Option<u32>,
    #[serde(default)]
    pub in_group: Option<bool>,
    #[serde(default)]
    pub operations_outside_sweden: Option<bool>,
    #[serde(default)]
    pub does_development_work: Option<bool>,
    #[serde(default)]
    pub owns_premises: Option<bool>,
    #[serde(default)]
    pub has_vehicles: Option<bool>,
    #[serde(default)]
    pub owners_active_in_company: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct FiscalYearRequest {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

#[derive(Debug, Deserialize)]
pub struct DocumentUpload {
    pub filename: String,
    pub mime_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub content_base64: Option<String>,
}

#[derive(Debug, Serialize)]
struct AnalysisResponse(AnalysisResult);

async fn analyse(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AnalysisRequest>,
) -> Result<Response, Problem> {
    authorise(&state, &headers)?;

    if request.documents.is_empty() {
        return Err(Problem::bad_request(
            "no documents",
            "at least one document is required for an analysis",
        ));
    }

    let org_number = OrgNumber::parse(&request.company.org_number)
        .map_err(|e| Problem::bad_request("invalid organisationsnummer", e.to_string()))?;

    let fiscal_year = FiscalYear::new(request.company.fiscal_year.start, request.company.fiscal_year.end)
        .map_err(|e| Problem::bad_request("invalid fiscal year", e.to_string()))?;

    let company = CompanyProfile {
        id: CompanyId::new(),
        name: request.company.name,
        org_number,
        fiscal_year,
        industry: request.company.industry,
        sni_code: request.company.sni_code,
        employee_count: request.company.employee_count,
        owner_count: request.company.owner_count,
        in_group: request.company.in_group,
        operations_outside_sweden: request.company.operations_outside_sweden,
        does_development_work: request.company.does_development_work,
        owns_premises: request.company.owns_premises,
        has_vehicles: request.company.has_vehicles,
        owners_active_in_company: request.company.owners_active_in_company,
    };

    let mut documents = Vec::with_capacity(request.documents.len());
    for upload in request.documents {
        documents.push(prepare_document(upload)?);
    }

    let accounts_state = match request.accounts_state.as_deref() {
        Some("final") => AccountsState::Final,
        Some("unknown") => AccountsState::Unknown,
        Some("preliminary") | None => AccountsState::Preliminary,
        Some(other) => {
            return Err(Problem::bad_request(
                "invalid accounts_state",
                format!("`{other}` is not one of preliminary, final, unknown"),
            ))
        }
    };

    let pipeline = AnalysisPipeline::new(
        state.engine.clone(),
        state.provider.clone(),
        state.config.clone(),
    );

    let input = AnalysisInput {
        analysis_id: AnalysisId::new(),
        company,
        documents,
        accounts_state,
    };

    match pipeline.run(&input, &SilentObserver).await {
        Ok((result, _runs)) => Ok(Json(result).into_response()),
        Err(PipelineError::TaxYearNotCovered(year)) => Err(Problem::bad_request(
            "tax year not covered",
            format!(
                "the rule set in force ({}) has no version for tax year {year}; \
                 an analysis would be misleading rather than merely incomplete",
                state.engine.version()
            ),
        )),
        Err(PipelineError::NoDocuments) => Err(Problem::bad_request(
            "no documents",
            "at least one document is required for an analysis",
        )),
        Err(e) => Err(Problem {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            title: "analysis failed".into(),
            detail: e.to_string(),
        }),
    }
}

fn prepare_document(upload: DocumentUpload) -> Result<DocumentInput, Problem> {
    let mime = MimeType::from_content_type(&upload.mime_type).ok_or_else(|| {
        Problem::bad_request(
            "unsupported document type",
            format!("`{}` is not a supported content type", upload.mime_type),
        )
    })?;

    let bytes = match (upload.text, upload.content_base64) {
        (Some(_), Some(_)) => {
            return Err(Problem::bad_request(
                "ambiguous document",
                format!("{}: supply either text or content_base64, not both", upload.filename),
            ))
        }
        (Some(text), None) => text.into_bytes(),
        (None, Some(encoded)) => decode_base64(&encoded).ok_or_else(|| {
            Problem::bad_request(
                "invalid base64",
                format!("{} could not be decoded", upload.filename),
            )
        })?,
        (None, None) => {
            return Err(Problem::bad_request(
                "empty document",
                format!("{}: supply either text or content_base64", upload.filename),
            ))
        }
    };

    // The declared type is a claim; check it against the bytes before parsing.
    if !mime.matches_content(&bytes) {
        return Err(Problem::bad_request(
            "content does not match its declared type",
            format!("{} does not look like {}", upload.filename, mime.as_content_type()),
        ));
    }

    let extracted = skattjakt_extract::extract(&bytes, mime).map_err(|e| {
        Problem::bad_request("unreadable document", format!("{}: {e}", upload.filename))
    })?;

    Ok(DocumentInput {
        document_id: skattjakt_core::DocumentId::new(),
        document_version_id: skattjakt_core::DocumentVersionId::new(),
        extracted,
    })
}

/// Standard base64 decoding. Hand-rolled to keep the dependency surface of a
/// service that handles financial documents as small as it reasonably can be.
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, c) in TABLE.iter().enumerate() {
        lookup[*c as usize] = i as u8;
    }

    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();

    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;

    for byte in cleaned {
        let value = lookup[byte as usize];
        if value == 255 {
            return None;
        }
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    // Leftover bits must be zero padding, never data.
    if bits >= 6 || (buffer & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;

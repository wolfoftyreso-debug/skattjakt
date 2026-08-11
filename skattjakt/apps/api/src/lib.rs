//! # skattjakt-api
//!
//! The HTTP surface. `api/openapi.yaml` is the contract (section 17); this
//! implements it and serves it.
//!
//! Everything the API can do, it does through the pipeline — there is no
//! analysis logic here, only transport, authentication and shape.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod observe;
pub mod routes;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::json;
use skattjakt_core::document::{AccountsState, MimeType};
use skattjakt_core::{AnalysisId, CompanyId, CompanyProfile, FiscalYear, OrgNumber};
use skattjakt_model::{ModelProvider, ScriptedProvider};
use skattjakt_pipeline::pipeline::{
    AnalysisPipeline, PipelineConfig, PipelineError, SilentObserver,
};
use skattjakt_pipeline::{AnalysisInput, DocumentInput};
use skattjakt_rules::{ReviewState, RuleEngine};
use skattjakt_store::{BlobStore, FilesystemBlobStore, Store};
use skattjakt_telemetry::{metrics, CorrelationId, Registry, CORRELATION_HEADER};

/// The contract, compiled in so a deployed build can always serve the exact
/// contract it was built against.
const OPENAPI: &str = include_str!("../openapi.yaml");

/// The beta interface. One file, no build step, no dependencies — section 25
/// asks for a minimal beta, and a bundler would be the largest thing in it.
const UI: &str = include_str!("../ui/index.html");

/// Uploads are bounded; an unbounded body is a denial-of-service surface and a
/// very large prompt.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<RuleEngine>,
    pub provider: Arc<dyn ModelProvider>,
    pub config: PipelineConfig,
    /// Static bearer token for the stateless surface. When `None` *and* no
    /// database is configured, the `/v1` routes are closed entirely rather
    /// than left open.
    pub api_token: Option<String>,
    /// Token that may create companies. Never grants access to any company's data.
    pub admin_token: Option<String>,
    pub model_configured: bool,
    /// Persistence. `None` runs the service statelessly: analyses are computed
    /// and returned, never stored.
    pub store: Option<Store>,
    pub blobs: Arc<dyn BlobStore>,
    /// The durable queue. Present exactly when persistence is: a queue without
    /// a database has nowhere to put a job.
    pub queue: Option<skattjakt_jobs::Queue>,
    pub metrics: Registry,
}

/// Reads the correlation id from the request, or mints one.
///
/// Accepted from the client only when it parses as a UUID: an arbitrary header
/// value would be an injection point into the log store, which is exactly the
/// kind of thing that is only noticed after it has been exploited.
pub fn correlation_id(headers: &HeaderMap) -> CorrelationId {
    headers
        .get(CORRELATION_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(CorrelationId::parse)
        .unwrap_or_default()
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
    pub async fn from_env() -> Result<Self, String> {
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

        let store = match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => match Store::connect(&url).await {
                Ok(store) => Some(store),
                Err(e) => {
                    // A configured-but-unreachable database is a misconfiguration,
                    // not a reason to silently run without persistence.
                    return Err(format!("DATABASE_URL is set but unusable: {e}"));
                }
            },
            _ => {
                tracing::warn!("DATABASE_URL is not set; running statelessly");
                None
            }
        };

        let blob_root =
            std::env::var("SKATTJAKT_BLOB_ROOT").unwrap_or_else(|_| "./data/documents".to_string());

        let registry = Registry::new();
        metrics::register_all(&registry);

        // The API enqueues; it never claims. The worker id is recorded on the
        // enqueue path only for provenance, and is never used to hold a lease.
        let queue = store.as_ref().map(|store| {
            skattjakt_jobs::Queue::new(
                store.pool().clone(),
                registry.clone(),
                std::env::var("HOSTNAME").unwrap_or_else(|_| "skattjakt-api".to_string()),
            )
        });

        Ok(Self {
            engine,
            provider,
            config: PipelineConfig::default(),
            api_token: std::env::var("SKATTJAKT_API_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            admin_token: std::env::var("SKATTJAKT_ADMIN_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            model_configured,
            store,
            blobs: Arc::new(FilesystemBlobStore::new(blob_root)),
            queue,
            metrics: registry,
        })
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui))
        .route("/favicon.svg", get(favicon))
        .route("/favicon.ico", get(favicon))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/openapi.yaml", get(openapi))
        .route("/v1/rules", get(rules))
        .route("/v1/analyses", post(analyse))
        .route("/v1/companies", post(routes::create_company))
        .route("/v1/companies/me", get(routes::get_company))
        .route("/v1/documents", post(routes::upload_document))
        .route("/v1/documents", get(routes::list_documents))
        .route("/v1/analyses/stored", post(routes::start_analysis))
        .route("/v1/analyses/stored", get(routes::list_analyses))
        .route("/v1/analyses/{id}", get(routes::get_analysis))
        .route(
            "/v1/analyses/{id}/opportunities",
            get(routes::list_opportunities),
        )
        .route("/v1/analyses/{id}/report", get(routes::get_report))
        .route("/v1/opportunities/{id}", get(routes::get_opportunity))
        .route("/metrics", get(observe::metrics))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            observe::observe,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Problem {
    pub status: StatusCode,
    pub title: String,
    pub detail: String,
}

impl Problem {
    pub fn bad_request(title: &str, detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            title: title.into(),
            detail: detail.into(),
        }
    }

    /// 429, with the two things a client needs to back off correctly.
    pub fn rate_limited(limit: i32, resets_at: chrono::DateTime<chrono::Utc>) -> Self {
        let seconds = (resets_at - chrono::Utc::now()).num_seconds().max(1);
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            title: "rate limited".into(),
            detail: format!(
                "the quota for this operation is {limit} per window; try again in {seconds} seconds"
            ),
        }
    }

    pub fn unauthorized() -> Self {
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
        (
            self.status,
            Json(json!({"title": self.title, "detail": self.detail})),
        )
            .into_response()
    }
}

/// What a credential is allowed to do.
///
/// The tenant comes from the token, never from the request body: a company id
/// a client sends cannot widen what that client can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// May create companies. Grants access to no company's data.
    Admin,
    Company(skattjakt_core::CompanyId),
    /// The static token, used when no database is configured.
    Stateless,
}

/// Resolves the bearer token to a scope.
///
/// Token comparisons are constant-time so a timing difference cannot be used to
/// recover a token byte by byte. The database lookup is by SHA-256, which is
/// constant-time by construction.
pub async fn authorise(state: &AppState, headers: &HeaderMap) -> Result<Scope, Problem> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(Problem::unauthorized)?;

    if let Some(admin) = state.admin_token.as_deref() {
        if constant_time_eq(presented.as_bytes(), admin.as_bytes()) {
            return Ok(Scope::Admin);
        }
    }

    if let Some(store) = state.store.as_ref() {
        match store.authenticate(presented).await {
            Ok(Some(company_id)) => return Ok(Scope::Company(company_id)),
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, "token lookup failed");
                return Err(Problem {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    title: "authentication unavailable".into(),
                    detail: "the credential store could not be reached".into(),
                });
            }
        }
    }

    if let Some(expected) = state.api_token.as_deref() {
        if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            return Ok(Scope::Stateless);
        }
    }

    // Nothing matched — including the case where no token is configured at all,
    // which closes the authenticated surface rather than opening it.
    Err(Problem::unauthorized())
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

async fn ui() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], UI)
}

/// An inline mark, so the browser does not request one that does not exist.
/// Inline rather than a file: the interface loads nothing from anywhere else.
async fn favicon() -> impl IntoResponse {
    const MARK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">
<rect width="32" height="32" rx="7" fill="#1f5d4c"/>
<path d="M9 21c3-1 5-3 6-6 1 3 3 5 6 6" stroke="#fbfaf8" stroke-width="2.4" fill="none" stroke-linecap="round"/>
<circle cx="16" cy="9" r="2.4" fill="#fbfaf8"/></svg>"##;
    ([(header::CONTENT_TYPE, "image/svg+xml")], MARK)
}

async fn openapi() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/yaml")], OPENAPI)
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

async fn rules(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, Problem> {
    authorise(&state, &headers).await?;

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

    let unreviewed = state
        .engine
        .rules()
        .iter()
        .filter(|r| !r.review.is_reviewed())
        .count();

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

async fn analyse(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AnalysisRequest>,
) -> Result<Response, Problem> {
    authorise(&state, &headers).await?;

    if request.documents.is_empty() {
        return Err(Problem::bad_request(
            "no documents",
            "at least one document is required for an analysis",
        ));
    }

    let company = build_profile(request.company)?;

    let mut documents = Vec::with_capacity(request.documents.len());
    for upload in request.documents {
        documents.push(prepare_document(upload)?);
    }

    let accounts_state = parse_accounts_state(request.accounts_state.as_deref())?;

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

/// Builds a validated company profile from a request body.
pub fn build_profile(request: CompanyProfileRequest) -> Result<CompanyProfile, Problem> {
    let org_number = OrgNumber::parse(&request.org_number)
        .map_err(|e| Problem::bad_request("invalid organisationsnummer", e.to_string()))?;
    let fiscal_year = FiscalYear::new(request.fiscal_year.start, request.fiscal_year.end)
        .map_err(|e| Problem::bad_request("invalid fiscal year", e.to_string()))?;

    Ok(CompanyProfile {
        id: CompanyId::new(),
        name: request.name,
        org_number,
        fiscal_year,
        industry: request.industry,
        sni_code: request.sni_code,
        employee_count: request.employee_count,
        owner_count: request.owner_count,
        in_group: request.in_group,
        operations_outside_sweden: request.operations_outside_sweden,
        does_development_work: request.does_development_work,
        owns_premises: request.owns_premises,
        has_vehicles: request.has_vehicles,
        owners_active_in_company: request.owners_active_in_company,
    })
}

pub fn parse_accounts_state(value: Option<&str>) -> Result<AccountsState, Problem> {
    match value {
        Some("final") => Ok(AccountsState::Final),
        Some("unknown") => Ok(AccountsState::Unknown),
        Some("preliminary") | None => Ok(AccountsState::Preliminary),
        Some(other) => Err(Problem::bad_request(
            "invalid accounts_state",
            format!("`{other}` is not one of preliminary, final, unknown"),
        )),
    }
}

pub fn parse_mime(content_type: &str) -> Result<MimeType, Problem> {
    MimeType::from_content_type(content_type).ok_or_else(|| {
        Problem::bad_request(
            "unsupported document type",
            format!("`{content_type}` is not a supported content type"),
        )
    })
}

/// Reads the bytes out of an upload, refusing an ambiguous or empty one.
pub fn upload_bytes(upload: &DocumentUpload) -> Result<Vec<u8>, Problem> {
    match (&upload.text, &upload.content_base64) {
        (Some(_), Some(_)) => Err(Problem::bad_request(
            "ambiguous document",
            format!(
                "{}: supply either text or content_base64, not both",
                upload.filename
            ),
        )),
        (Some(text), None) => Ok(text.clone().into_bytes()),
        (None, Some(encoded)) => decode_base64(encoded).ok_or_else(|| {
            Problem::bad_request(
                "invalid base64",
                format!("{} could not be decoded", upload.filename),
            )
        }),
        (None, None) => Err(Problem::bad_request(
            "empty document",
            format!("{}: supply either text or content_base64", upload.filename),
        )),
    }
}

fn prepare_document(upload: DocumentUpload) -> Result<DocumentInput, Problem> {
    let mime = parse_mime(&upload.mime_type)?;
    let bytes = upload_bytes(&upload)?;

    // The declared type is a claim; check it against the bytes before parsing.
    if !mime.matches_content(&bytes) {
        return Err(Problem::bad_request(
            "content does not match its declared type",
            format!(
                "{} does not look like {}",
                upload.filename,
                mime.as_content_type()
            ),
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

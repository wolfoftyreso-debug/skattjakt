//! Routes that require persistence.
//!
//! These are the endpoints of section 17 that need somewhere to put things:
//! companies, documents, analyses that run in the background, and the report.
//! When no database is configured they return 501 rather than pretending, and
//! the stateless inline analysis in `lib.rs` remains available.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use skattjakt_core::analysis::{AnalysisStage, AnalysisStatus};
use skattjakt_core::document::DocumentKind;
use skattjakt_core::{AnalysisId, CompanyId, DocumentVersionId, OpportunityId};
use skattjakt_jobs::{IdempotencyKey, JobKind, NewJob, Queue};
use skattjakt_pipeline::pipeline::SilentObserver;
use skattjakt_store::RateBucket;
use skattjakt_telemetry::{names, LabelSet};
use uuid::Uuid;

use skattjakt_identity::Permission;

use crate::{authorise, correlation_id, AppState, DocumentUpload, Problem, Scope};

/// Generates a bearer token with 256 bits of entropy from the OS.
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS must provide randomness");
    bytes.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

pub(crate) fn store(state: &AppState) -> Result<&skattjakt_store::Store, Problem> {
    state.store.as_ref().ok_or_else(|| Problem {
        status: StatusCode::NOT_IMPLEMENTED,
        title: "persistence is not configured".into(),
        detail: "DATABASE_URL is not set; this endpoint needs one. The stateless \
                 POST /v1/analyses with inline documents is available without it."
            .into(),
    })
}

fn queue(state: &AppState) -> Result<&Queue, Problem> {
    state.queue.as_ref().ok_or_else(|| Problem {
        status: StatusCode::NOT_IMPLEMENTED,
        title: "the job queue is not configured".into(),
        detail: "DATABASE_URL is not set; background analyses need one.".into(),
    })
}

/// Turns a queue failure into a 500 without leaking its message to the client.
fn queue_error(error: skattjakt_jobs::QueueError) -> Problem {
    tracing::error!(error = %error, "queue operation failed");
    Problem {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        title: "internal error".into(),
        detail: "the analysis could not be queued".into(),
    }
}

/// Resolves a scope to a tenant, checking that it may do the thing it is about
/// to do.
///
/// The permission is a required argument rather than an optional check, so a
/// new route cannot be written without stating what it needs. That is the
/// difference between a permission model and a permission convention.
pub(crate) fn company_scope(scope: &Scope, permission: Permission) -> Result<CompanyId, Problem> {
    let company = match scope {
        Scope::Company(id) => *id,
        Scope::User(user) => user.company_id,
        Scope::Admin => {
            return Err(Problem {
                status: StatusCode::FORBIDDEN,
                title: "wrong credential".into(),
                detail: "this endpoint needs a company credential, not the admin token".into(),
            })
        }
        Scope::Stateless => {
            return Err(Problem {
                status: StatusCode::NOT_IMPLEMENTED,
                title: "persistence is not configured".into(),
                detail: "the static API token has no company behind it".into(),
            })
        }
    };

    if !scope.may(permission) {
        // Says which role is held and what was needed. Unlike a 404 for
        // another tenant's data — which must stay uninformative — this tells a
        // legitimate colleague why they were refused and who can fix it.
        return Err(Problem {
            status: StatusCode::FORBIDDEN,
            title: "insufficient permission".into(),
            detail: "this action needs a role your account does not have; \
                     ask an owner of the company"
                .into(),
        });
    }

    Ok(company)
}

pub(crate) fn internal(error: impl std::fmt::Display) -> Problem {
    Problem {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        title: "storage failure".into(),
        detail: error.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Companies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateCompanyRequest {
    pub company: crate::CompanyProfileRequest,
    #[serde(default)]
    pub token_label: Option<String>,
}

/// Creates a company and issues its first token.
///
/// The token is returned once and never again: only its SHA-256 is stored, so
/// there is nothing to retrieve later.
pub async fn create_company(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateCompanyRequest>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    if !matches!(scope, Scope::Admin) {
        return Err(Problem {
            status: StatusCode::FORBIDDEN,
            title: "admin credential required".into(),
            detail: "creating a company requires the admin token".into(),
        });
    }
    let store = store(&state)?;

    let profile = crate::build_profile(request.company)?;
    let token = generate_token();
    let label = request.token_label.unwrap_or_else(|| "initial".to_string());

    store
        .create_company(&profile, &token, &label)
        .await
        .map_err(internal)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "company_id": profile.id,
            "name": profile.name,
            "org_number": profile.org_number.formatted(),
            "api_token": token,
            "token_label": label,
            "note": "Spara token nu. Endast dess hash lagras, så den kan inte hämtas igen.",
        })),
    )
        .into_response())
}

pub async fn get_company(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let company_id = company_scope(&authorise(&state, &headers).await?, Permission::ReadCompany)?;
    let store = store(&state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let profile = tenant.company().await.map_err(|e| match e {
        skattjakt_store::StoreError::NotFound => Problem {
            status: StatusCode::NOT_FOUND,
            title: "not found".into(),
            detail: "no such company".into(),
        },
        other => internal(other),
    })?;
    tenant.commit().await.map_err(internal)?;

    Ok(Json(json!({
        "id": profile.id,
        "name": profile.name,
        "org_number": profile.org_number.formatted(),
        "fiscal_year": {"start": profile.fiscal_year.start, "end": profile.fiscal_year.end},
        "profile": profile,
        "unanswered": profile.unanswered_fields(),
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UploadDocumentRequest {
    #[serde(flatten)]
    pub document: DocumentUpload,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub accounts_state: Option<String>,
}

/// Stores a document and its first immutable version.
pub async fn upload_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<UploadDocumentRequest>,
) -> Result<Response, Problem> {
    let company_id = company_scope(
        &authorise(&state, &headers).await?,
        Permission::UploadDocument,
    )?;
    let store = store(&state)?;

    let filename = request.document.filename.clone();
    // Parsing happens before storage: a file that cannot be read is rejected
    // rather than kept as an unusable blob.
    let (bytes, mime, extracted) = prepare_document_bytes(request.document)?;

    let kind = match request.kind.as_deref() {
        Some("income_statement") => DocumentKind::IncomeStatement,
        Some("balance_sheet") => DocumentKind::BalanceSheet,
        Some("general_ledger") => DocumentKind::GeneralLedger,
        Some("tax_return") => DocumentKind::TaxReturn,
        Some("annual_accounts") | None => DocumentKind::AnnualAccounts,
        Some(_) => DocumentKind::Unknown,
    };
    let accounts_state = crate::parse_accounts_state(request.accounts_state.as_deref())?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let version = tenant
        .create_document(
            kind,
            &filename,
            mime,
            &bytes,
            Some(extracted.page_count() as i32),
            accounts_state,
        )
        .await
        .map_err(internal)?;

    // Blob first, then commit: a committed row pointing at bytes that were
    // never written would be a dangling reference, while an orphaned blob is
    // merely garbage.
    state
        .blobs
        .put(&version.storage_key, &bytes)
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "document_id": version.document_id,
            "document_version_id": version.id,
            "sha256": version.sha256,
            "byte_size": version.byte_size,
            "page_count": version.page_count,
            "unreadable_pages": extracted.unreadable_pages,
        })),
    )
        .into_response())
}

fn prepare_document_bytes(
    upload: DocumentUpload,
) -> Result<
    (
        Vec<u8>,
        skattjakt_core::document::MimeType,
        skattjakt_extract::ExtractedDocument,
    ),
    Problem,
> {
    let mime = crate::parse_mime(&upload.mime_type)?;
    let bytes = crate::upload_bytes(&upload)?;
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
    Ok((bytes, mime, extracted))
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub cursor: Option<String>,
}

pub async fn list_documents(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<PageQuery>,
) -> Result<Response, Problem> {
    let company_id = company_scope(
        &authorise(&state, &headers).await?,
        Permission::ReadDocument,
    )?;
    let store = store(&state)?;

    // A malformed cursor is refused rather than silently treated as "start from
    // the beginning" — which would send a client that mangled its cursor back
    // to page one for ever without ever telling it.
    let cursor = match page.cursor.as_deref() {
        Some(raw) => Some(
            skattjakt_store::page::Cursor::decode(raw).ok_or_else(|| Problem {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                title: "invalid cursor".into(),
                detail: "the cursor is not one this API issued".into(),
            })?,
        ),
        None => None,
    };

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let result = tenant
        .list_document_versions_page(cursor, page.limit.unwrap_or(50))
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    let next_cursor = result.next.as_ref().map(|c| c.encode());
    Ok(Json(json!({
        "next_cursor": next_cursor,
        "documents": result.items
            .into_iter()
            .map(|(id, filename, sha256)| json!({
                "document_version_id": id, "filename": filename, "sha256": sha256
            }))
            .collect::<Vec<_>>()
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Analyses
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StartAnalysisRequest {
    pub document_version_ids: Vec<Uuid>,
    #[serde(default)]
    pub accounts_state: Option<String>,
}

/// Starts an analysis over stored documents and returns immediately.
///
/// An analysis can legitimately take minutes at high effort, which is far too
/// long to hold a request open. The client polls `GET /v1/analyses/{id}` and
/// sees the stage advance.
pub async fn start_analysis(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<StartAnalysisRequest>,
) -> Result<Response, Problem> {
    let company_id = company_scope(
        &authorise(&state, &headers).await?,
        Permission::StartAnalysis,
    )?;
    let store = store(&state)?.clone();

    if request.document_version_ids.is_empty() {
        return Err(Problem::bad_request(
            "no documents",
            "at least one document version is required",
        ));
    }

    let version_ids: Vec<DocumentVersionId> = request
        .document_version_ids
        .iter()
        .copied()
        .map(DocumentVersionId::from_uuid)
        .collect();

    let analysis_id = AnalysisId::new();
    let accounts_state = crate::parse_accounts_state(request.accounts_state.as_deref())?;
    let correlation = correlation_id(&headers);

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;

    // Rate limit before anything expensive happens (section 67). Counted per
    // tenant in the database rather than per process, because several API
    // replicas serve the same customer and an in-memory limiter would multiply
    // the quota by the replica count.
    let decision = tenant
        .check_rate_limit(RateBucket::Analysis)
        .await
        .map_err(internal)?;
    if !decision.allowed {
        tenant.commit().await.map_err(internal)?;
        state.metrics.increment(
            names::RATE_LIMITED,
            LabelSet::new().enumerated("bucket", "analysis"),
        );
        return Err(Problem::rate_limited(decision.limit, decision.resets_at));
    }

    // Resolve every document version up front, so a bad id is a 400 now rather
    // than a failed job later.
    for id in &version_ids {
        tenant.document_version(*id).await.map_err(|e| match e {
            skattjakt_store::StoreError::NotFound => Problem::bad_request(
                "unknown document",
                format!("no document version {id} for this company"),
            ),
            other => internal(other),
        })?;
    }
    tenant
        .create_analysis(
            analysis_id,
            &version_ids,
            state.engine.version(),
            accounts_state,
        )
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    // Hand the work to the durable queue rather than to a background task in
    // this process. A `tokio::spawn` here dies with the pod, and a rolling
    // deploy would silently lose every analysis in flight.
    let queue = queue(&state)?;
    let idempotency_key = match headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
        Some(raw) => IdempotencyKey::parse(raw)
            .map_err(|e| Problem::bad_request("invalid idempotency key", e.to_string()))?,
        // Derived from the work itself, so a client that retries a timed-out
        // request without a key still gets one analysis rather than two.
        None => IdempotencyKey::derived(
            JobKind::Analysis,
            company_id.0,
            &request.document_version_ids,
        ),
    };

    let enqueued = queue
        .enqueue(NewJob {
            kind: JobKind::Analysis,
            company_id: company_id.0,
            subject_id: analysis_id.0,
            idempotency_key,
            correlation_id: correlation,
            traceparent: headers
                .get(skattjakt_telemetry::TRACEPARENT_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            delay: None,
        })
        .await
        .map_err(queue_error)?;

    if !enqueued.is_new() {
        // The key matched a job that already exists. Return that one: a
        // duplicate request must not cost the customer a second model bill.
        let existing = queue.get(enqueued.job_id()).await.map_err(queue_error)?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "analysis_id": existing.subject_id,
                "status": existing.state.as_str(),
                "stage": "queued",
                "duplicate_of": existing.subject_id,
                "poll": format!("/v1/analyses/{}", existing.subject_id),
            })),
        )
            .into_response());
    }

    state
        .metrics
        .increment(names::ANALYSES_STARTED, LabelSet::new());

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "analysis_id": analysis_id,
            "status": "pending",
            "stage": "queued",
            "poll": format!("/v1/analyses/{analysis_id}"),
        })),
    )
        .into_response())
}

// Analyses are executed by `skattjakt-analysis-worker`, not here. The stage
// observer, the pipeline invocation and the result writing moved with it; see
// `workers/analysis-worker/src/runner.rs`. What remains in the API is the part
// that belongs to a request: validate, record, enqueue, answer.

pub async fn get_analysis(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, Problem> {
    let company_id = company_scope(
        &authorise(&state, &headers).await?,
        Permission::ReadAnalysis,
    )?;
    let store = store(&state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let analysis = tenant
        .analysis(AnalysisId::from_uuid(id))
        .await
        .map_err(not_found)?;
    let runs = tenant
        .model_runs(AnalysisId::from_uuid(id))
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok(Json(json!({
        "analysis_id": analysis.id,
        "status": status_key(analysis.status),
        "stage": stage_key(analysis.stage),
        "progress": analysis.stage.progress(),
        "stage_label": analysis.stage.label_sv(),
        "rule_set_version": analysis.rule_set_version,
        "error": analysis.error,
        "created_at": analysis.created_at,
        "finished_at": analysis.finished_at,
        "model_runs": runs,
        "result": analysis.result,
    }))
    .into_response())
}

pub async fn list_analyses(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let company_id = company_scope(
        &authorise(&state, &headers).await?,
        Permission::ReadAnalysis,
    )?;
    let store = store(&state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let analyses = tenant.list_analyses().await.map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok(Json(json!({
        "analyses": analyses
            .into_iter()
            .map(|(id, status, created)| json!({
                "analysis_id": id, "status": status, "created_at": created
            }))
            .collect::<Vec<_>>()
    }))
    .into_response())
}

pub async fn list_opportunities(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, Problem> {
    let company_id = company_scope(
        &authorise(&state, &headers).await?,
        Permission::ReadAnalysis,
    )?;
    let store = store(&state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let opportunities = tenant
        .list_opportunities(AnalysisId::from_uuid(id))
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok(Json(json!({
        "opportunities": opportunities,
        "disclaimer": skattjakt_core::DISCLAIMER_SV,
    }))
    .into_response())
}

pub async fn get_opportunity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, Problem> {
    let company_id = company_scope(
        &authorise(&state, &headers).await?,
        Permission::ReadAnalysis,
    )?;
    let store = store(&state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let opportunity = tenant
        .opportunity(OpportunityId::from_uuid(id))
        .await
        .map_err(not_found)?;
    tenant.commit().await.map_err(internal)?;

    Ok(Json(opportunity).into_response())
}

#[derive(Debug, Deserialize)]
pub struct ReportQuery {
    #[serde(default)]
    pub format: Option<String>,
}

pub async fn get_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<ReportQuery>,
) -> Result<Response, Problem> {
    let company_id = company_scope(&authorise(&state, &headers).await?, Permission::ReadReport)?;
    let store = store(&state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let analysis = tenant
        .analysis(AnalysisId::from_uuid(id))
        .await
        .map_err(not_found)?;
    let profile = tenant.company().await.map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    let Some(result) = analysis.result else {
        return Err(Problem {
            status: StatusCode::CONFLICT,
            title: "analysis is not finished".into(),
            detail: format!(
                "the analysis is {} at stage {}",
                status_key(analysis.status),
                stage_key(analysis.stage)
            ),
        });
    };

    let report = skattjakt_pipeline::build_report(
        &result,
        &profile.name,
        &profile.fiscal_year.label(),
        &analysis.rule_set_version,
    );

    if query.format.as_deref() == Some("markdown") {
        return Ok((
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            skattjakt_pipeline::to_markdown(&report),
        )
            .into_response());
    }

    Ok(Json(report).into_response())
}

fn not_found(error: skattjakt_store::StoreError) -> Problem {
    match error {
        skattjakt_store::StoreError::NotFound => Problem {
            status: StatusCode::NOT_FOUND,
            title: "not found".into(),
            detail: "no such resource for this company".into(),
        },
        other => internal(other),
    }
}

fn status_key(status: AnalysisStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn stage_key(stage: AnalysisStage) -> String {
    serde_json::to_value(stage)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Used by the stateless path, which has no observer to report to.
pub fn silent() -> SilentObserver {
    SilentObserver
}

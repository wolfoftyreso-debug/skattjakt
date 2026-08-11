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
use skattjakt_core::document::{AccountsState, DocumentKind};
use skattjakt_core::{AnalysisId, CompanyId, CompanyProfile, DocumentVersionId, OpportunityId};
use skattjakt_pipeline::pipeline::{AnalysisPipeline, SilentObserver, StageObserver};
use skattjakt_pipeline::{AnalysisInput, DocumentInput};
use uuid::Uuid;

use crate::{authorise, AppState, DocumentUpload, Problem, Scope};

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

fn store(state: &AppState) -> Result<&skattjakt_store::Store, Problem> {
    state.store.as_ref().ok_or_else(|| Problem {
        status: StatusCode::NOT_IMPLEMENTED,
        title: "persistence is not configured".into(),
        detail: "DATABASE_URL is not set; this endpoint needs one. The stateless \
                 POST /v1/analyses with inline documents is available without it."
            .into(),
    })
}

fn company_scope(scope: Scope) -> Result<CompanyId, Problem> {
    match scope {
        Scope::Company(id) => Ok(id),
        Scope::Admin => Err(Problem {
            status: StatusCode::FORBIDDEN,
            title: "wrong credential".into(),
            detail: "this endpoint needs a company token, not the admin token".into(),
        }),
        Scope::Stateless => Err(Problem {
            status: StatusCode::NOT_IMPLEMENTED,
            title: "persistence is not configured".into(),
            detail: "the static API token has no company behind it".into(),
        }),
    }
}

fn internal(error: impl std::fmt::Display) -> Problem {
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
    if scope != Scope::Admin {
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
    let company_id = company_scope(authorise(&state, &headers).await?)?;
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
    let company_id = company_scope(authorise(&state, &headers).await?)?;
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

pub async fn list_documents(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let company_id = company_scope(authorise(&state, &headers).await?)?;
    let store = store(&state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let versions = tenant.list_document_versions().await.map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok(Json(json!({
        "documents": versions
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
    let company_id = company_scope(authorise(&state, &headers).await?)?;
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

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let profile = tenant.company().await.map_err(internal)?;
    // Resolve every document version up front, so a bad id is a 400 now rather
    // than a failed job later.
    let mut documents = Vec::new();
    for id in &version_ids {
        let version = tenant.document_version(*id).await.map_err(|e| match e {
            skattjakt_store::StoreError::NotFound => Problem::bad_request(
                "unknown document",
                format!("no document version {id} for this company"),
            ),
            other => internal(other),
        })?;
        documents.push(version);
    }
    tenant
        .create_analysis(analysis_id, &version_ids, state.engine.version())
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    let background = state.clone();
    tokio::spawn(async move {
        if let Err(error) = run_analysis(
            background,
            company_id,
            analysis_id,
            profile,
            documents,
            accounts_state,
        )
        .await
        {
            tracing::error!(%analysis_id, error, "analysis failed");
        }
    });

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

/// Reports stage transitions into the database so a polling client can watch
/// the analysis progress (section 3, step 3).
struct DatabaseObserver {
    store: skattjakt_store::Store,
    company_id: CompanyId,
    analysis_id: AnalysisId,
    handle: tokio::runtime::Handle,
}

impl StageObserver for DatabaseObserver {
    fn stage(&self, stage: AnalysisStage) {
        let store = self.store.clone();
        let (company_id, analysis_id) = (self.company_id, self.analysis_id);
        // Progress reporting must never fail the analysis it is reporting on.
        self.handle.spawn(async move {
            if let Ok(mut tenant) = store.tenant(company_id).await {
                let _ = tenant.set_stage(analysis_id, stage).await;
                let _ = tenant.commit().await;
            }
        });
    }
}

async fn run_analysis(
    state: AppState,
    company_id: CompanyId,
    analysis_id: AnalysisId,
    profile: CompanyProfile,
    versions: Vec<skattjakt_core::document::DocumentVersion>,
    accounts_state: AccountsState,
) -> Result<(), String> {
    let store = state
        .store
        .clone()
        .expect("persistence was checked by the caller");

    let mut documents = Vec::new();
    for version in &versions {
        let bytes = state
            .blobs
            .get(&version.storage_key)
            .await
            .map_err(|e| e.to_string())?;

        // The stored hash is checked on read. A blob that no longer matches the
        // bytes we recorded must not silently become the basis of an analysis.
        if !version.verify_hash(&bytes) {
            let message = format!(
                "document version {} no longer matches its recorded hash",
                version.id
            );
            let mut tenant = store.tenant(company_id).await.map_err(|e| e.to_string())?;
            let _ = tenant.fail_analysis(analysis_id, &message).await;
            let _ = tenant.commit().await;
            return Err(message);
        }

        let extracted = skattjakt_extract::extract(&bytes, version.mime_type)
            .map_err(|e| format!("{}: {e}", version.id))?;
        documents.push(DocumentInput {
            document_id: version.document_id,
            document_version_id: version.id,
            extracted,
        });
    }

    let input = AnalysisInput {
        analysis_id,
        company: profile.clone(),
        documents,
        accounts_state,
    };

    let facts =
        skattjakt_pipeline::build_fact_set(company_id, profile.fiscal_year, &input.documents);
    let stored_facts: Vec<_> = facts.iter().cloned().collect();

    let pipeline = AnalysisPipeline::new(
        state.engine.clone(),
        state.provider.clone(),
        state.config.clone(),
    );

    let observer = DatabaseObserver {
        store: store.clone(),
        company_id,
        analysis_id,
        handle: tokio::runtime::Handle::current(),
    };

    match pipeline.run(&input, &observer).await {
        Ok((result, runs)) => {
            let mut tenant = store.tenant(company_id).await.map_err(|e| e.to_string())?;
            tenant
                .insert_facts(&stored_facts)
                .await
                .map_err(|e| e.to_string())?;
            tenant
                .complete_analysis(&result, &runs)
                .await
                .map_err(|e| e.to_string())?;
            tenant.commit().await.map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(error) => {
            let mut tenant = store.tenant(company_id).await.map_err(|e| e.to_string())?;
            let _ = tenant.fail_analysis(analysis_id, &error.to_string()).await;
            let _ = tenant.commit().await;
            Err(error.to_string())
        }
    }
}

pub async fn get_analysis(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, Problem> {
    let company_id = company_scope(authorise(&state, &headers).await?)?;
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
    let company_id = company_scope(authorise(&state, &headers).await?)?;
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
    let company_id = company_scope(authorise(&state, &headers).await?)?;
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
    let company_id = company_scope(authorise(&state, &headers).await?)?;
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
    let company_id = company_scope(authorise(&state, &headers).await?)?;
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

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

pub(crate) fn require_queue(state: &AppState) -> Result<&Queue, Problem> {
    state.queue.as_ref().ok_or_else(|| Problem {
        status: StatusCode::NOT_IMPLEMENTED,
        title: "the job queue is not configured".into(),
        detail: "DATABASE_URL is not set; background analyses need one.".into(),
    })
}

/// Turns a queue failure into a 500 without leaking its message to the client.
pub(crate) fn map_queue_error(error: skattjakt_jobs::QueueError) -> Problem {
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
    /// The paid order this analysis is drawn against.
    ///
    /// Required when the deployment takes payment. Redeemed inside the same
    /// transaction that creates the analysis, so an order buys exactly one.
    #[serde(default)]
    pub order_id: Option<uuid::Uuid>,
}

/// Starts an analysis over stored documents and returns immediately.
///
/// An analysis can legitimately take minutes at high effort, which is far too
/// long to hold a request open. The client polls `GET /v1/analyses/{id}` and
/// sees the stage advance.
pub async fn start_analysis(
    State(state): State<AppState>,
    headers: HeaderMap,
    span: Option<axum::extract::Extension<skattjakt_telemetry::SpanContext>>,
    Json(request): Json<StartAnalysisRequest>,
) -> Result<Response, Problem> {
    let span = span.map(|axum::extract::Extension(s)| s);
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

    // The payment gate, and it is here rather than at the top of the handler on
    // purpose: redeeming the order and creating the analysis happen in one
    // transaction, so an order cannot be spent on an analysis that then fails
    // to be created, and two requests racing on the same order cannot both win.
    // `redeem_order` is a single conditional UPDATE — see its documentation.
    if state.payments.required() {
        let Some(order_id) = request.order_id else {
            return Err(Problem {
                status: StatusCode::PAYMENT_REQUIRED,
                title: "payment_required".into(),
                detail: "this analysis must be drawn against a paid order".into(),
            });
        };
        match tenant.redeem_order(order_id, analysis_id).await {
            Ok(_) => {}
            Err(skattjakt_store::StoreError::NotFound) => {
                // Not payable — or already spent, which is a different thing
                // and must not be answered the same way.
                //
                // A customer whose request timed out and who pressed the button
                // again has an order that is already consumed. Refusing them
                // with 402 would take their money and hand back "that order
                // cannot be used", which is the worst answer available: the
                // order *was* used, and it bought them something. So an order
                // that already names an analysis answers with that analysis.
                //
                // This is the same invariant as before, read the other way
                // round: one order buys exactly one analysis, and asking twice
                // shows you the one you bought rather than selling a second.
                let already = tenant
                    .order(order_id)
                    .await
                    .ok()
                    .and_then(|o| o.analysis_id);
                if let Some(bought) = already {
                    // The analysis created a few lines above is discarded by
                    // dropping the transaction uncommitted — it was never
                    // enqueued and nothing else refers to it.
                    return Ok((
                        StatusCode::ACCEPTED,
                        Json(json!({
                            "analysis_id": bought,
                            "status": "pending",
                            "stage": "queued",
                            "duplicate_of": bought,
                            "poll": format!("/v1/analyses/{bought}"),
                        })),
                    )
                        .into_response());
                }
                return Err(Problem {
                    status: StatusCode::PAYMENT_REQUIRED,
                    title: "order_not_payable".into(),
                    detail: "that order does not exist, is not paid, or has already been used"
                        .into(),
                });
            }
            Err(other) => return Err(internal(other)),
        }
    }

    tenant.commit().await.map_err(internal)?;

    // Hand the work to the durable queue rather than to a background task in
    // this process. A `tokio::spawn` here dies with the pod, and a rolling
    // deploy would silently lose every analysis in flight.
    let queue = require_queue(&state)?;
    let idempotency_key = match headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
        Some(raw) => IdempotencyKey::parse(raw)
            .map_err(|e| Problem::bad_request("invalid idempotency key", e.to_string()))?,
        // Derived from the work itself, so a client that retries a timed-out
        // request without a key still gets one analysis rather than two.
        //
        // The order is part of the work. Without it the key is
        // (company, documents), so a paid analysis over the same documents as
        // an earlier one collides with it — and the customer is handed the
        // earlier analysis while their order is consumed against a new row
        // nobody will ever look at. They paid, and were given something else.
        // The order id makes every purchase its own piece of work; a retry of
        // the *same* purchase still derives the same key, which is the case
        // this derivation exists for.
        None => match request.order_id {
            Some(order_id) => IdempotencyKey::derived_for_order(
                JobKind::Analysis,
                company_id.0,
                &request.document_version_ids,
                order_id,
            ),
            None => IdempotencyKey::derived(
                JobKind::Analysis,
                company_id.0,
                &request.document_version_ids,
            ),
        },
    };

    let enqueued = queue
        .enqueue(NewJob {
            kind: JobKind::Analysis,
            company_id: company_id.0,
            subject_id: analysis_id.0,
            idempotency_key,
            correlation_id: correlation,
            // This request's own span, not the inbound header — see the note
            // in `observe.rs`. Falls back to the header when the middleware is
            // not in the stack, which is only the case in a unit test.
            traceparent: span.map(|s| s.traceparent()).or_else(|| {
                headers
                    .get(skattjakt_telemetry::TRACEPARENT_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            }),
            delay: None,
        })
        .await
        .map_err(map_queue_error)?;

    if !enqueued.is_new() {
        // The key matched a job that already exists. Return that one: a
        // duplicate request must not cost the customer a second model bill.
        let existing = queue
            .get(enqueued.job_id())
            .await
            .map_err(map_queue_error)?;
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
    /// Which presentation layer: `private`, `company` (the default) or
    /// `accountant`. The analysis behind all three is the same one — this
    /// selects how it is written up, not what was checked.
    #[serde(default)]
    pub audience: Option<String>,
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

    // An unknown audience is rejected rather than quietly served as the
    // default: a caller asking for `accountant` and silently getting the
    // company view would not notice, and would ship a review with no control
    // section in it.
    let requested = match query.audience.as_deref() {
        None => None,
        Some(value) => Some(
            skattjakt_pipeline::Audience::parse(value).ok_or_else(|| Problem {
                status: StatusCode::BAD_REQUEST,
                title: "unknown audience".into(),
                detail: format!("{value:?} is not one of: private, company, accountant"),
            })?,
        ),
    };

    // What was bought decides what is served.
    //
    // This used to be the query parameter alone, which meant a customer who
    // paid 29 kronor for Privatanalys could ask for `?audience=accountant` and
    // receive the 69-kronor Kontroll report. The payment was verified; the
    // entitlement was not. That is the same mistake as letting a client declare
    // its own payment settled — the client was deciding what it had bought.
    //
    // There is deliberately no ladder here. Bolagsanalys and Skattjakt Kontroll
    // cost the same and are different reports, not more and less of one report,
    // so "at or below what you paid for" has no meaning to implement. You get
    // the layer you bought.
    let audience = match analysis.audience.as_deref() {
        // Not bought — payments were not required — so the caller chooses, as
        // before.
        None => requested.unwrap_or(skattjakt_pipeline::Audience::Company),
        Some(bought) => {
            let entitled = skattjakt_pipeline::Audience::parse(bought).ok_or_else(|| {
                // Unreachable through the API — the column is constrained to
                // the three keys — but an unreadable entitlement must fail
                // closed rather than fall back to a default that might be
                // more than was paid for.
                internal(format!("analysis carries an unknown audience {bought:?}"))
            })?;
            if let Some(requested) = requested {
                if requested != entitled {
                    return Err(Problem {
                        status: StatusCode::FORBIDDEN,
                        title: "not_what_was_bought".into(),
                        detail: format!(
                            "this analysis was bought as {}, so it cannot be read as {}",
                            entitled.as_str(),
                            requested.as_str()
                        ),
                    });
                }
            }
            entitled
        }
    };

    let report = skattjakt_pipeline::build_report_for(
        audience,
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

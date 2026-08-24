//! Direct-to-storage uploads.
//!
//! The flow a phone uses, and the reason it exists: a 30 MB scanned annual
//! report posted as JSON crosses a mobile network, is buffered whole in an API
//! pod, and any drop in the last second means starting again. It also ties
//! upload throughput to API pod memory, so one customer photographing their
//! accounts can push a replica into an OOM kill.
//!
//! ```text
//!   POST /v1/documents/tickets            → ticket id + upload URL + expiry
//!   PUT  <upload URL>                     → the bytes, straight to storage
//!   POST /v1/documents/tickets/{id}/complete → the document version id
//! ```
//!
//! The API never handles the bytes on the middle step.
//!
//! The proxied `POST /v1/documents` stays. It is the right shape for a browser
//! posting a small text file, and removing it would break the web client for no
//! benefit.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use skattjakt_core::document::{AccountsState, DocumentKind, MimeType};
use skattjakt_core::DocumentId;
use skattjakt_identity::Permission;
use skattjakt_store::uploads::{is_supported_type, CompletionOutcome, MAX_DECLARED_BYTES};
use uuid::Uuid;

use crate::routes::{company_scope, internal, store};
use crate::{authorise, AppState, Problem};

#[derive(Debug, Deserialize)]
pub struct TicketRequest {
    pub filename: String,
    pub mime_type: String,
    /// What the client says it will send, in **bytes**.
    ///
    /// Bytes, not characters, and the distinction bites in this market: a
    /// Swedish document contains å, ä and ö, each two bytes in UTF-8, so a
    /// client that counts characters declares a size that is always too small
    /// and every upload is rejected. The mismatch error below names both
    /// numbers so that is diagnosable in one attempt rather than by guesswork.
    ///
    /// Checked against reality on completion. The declared size is what the
    /// size limit is applied to, so a ticket redeemable for more bytes than it
    /// declared is a ticket with no size limit.
    pub size: i64,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub accounts_state: Option<String>,
}

/// `POST /v1/documents/tickets`
pub async fn issue_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TicketRequest>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::UploadDocument)?;
    let store = store(&state)?;

    if !is_supported_type(&request.mime_type) {
        // An allowlist, not a denylist. A denylist is a list of the formats
        // someone thought of, and the interesting one is always the format
        // nobody thought of.
        return Err(Problem {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            title: "unsupported document type".into(),
            detail: "the declared type could not be read at all".into(),
        });
    }
    if request.size <= 0 || request.size > MAX_DECLARED_BYTES {
        return Err(Problem {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            title: "document too large".into(),
            detail: format!(
                "a document must be between 1 byte and {} GB",
                MAX_DECLARED_BYTES / 1024 / 1024 / 1024
            ),
        });
    }

    let document_id = DocumentId::new();
    let mut tenant = store.tenant(company_id).await.map_err(internal)?;

    // The same per-tenant quota the proxied upload path uses. A ticket is cheap
    // to issue and expensive to redeem, so the limit belongs here rather than
    // on completion — by then the bytes have already been written and the
    // storage has already been paid for.
    let decision = tenant
        .check_rate_limit(skattjakt_store::RateBucket::Upload)
        .await
        .map_err(internal)?;
    if !decision.allowed {
        tenant.commit().await.map_err(internal)?;
        return Err(Problem::rate_limited(decision.limit, decision.resets_at));
    }

    let ticket = tenant
        .issue_upload_ticket(
            document_id,
            &request.filename,
            &request.mime_type,
            request.size,
            scope.user_id(),
        )
        .await
        .map_err(map_store_error)?;
    tenant.commit().await.map_err(internal)?;

    // A presigned URL when object storage can produce one; otherwise the API's
    // own path. The client's code is the same either way, which is what lets a
    // single-node deployment run without S3 and a clustered one use it.
    let (upload_url, method) = match state.blobs.presign_put(&ticket.storage_key, 1800) {
        Some(url) => (url, "direct"),
        None => (
            format!("/v1/documents/tickets/{}/content", ticket.id),
            "proxied",
        ),
    };

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "ticket_id": ticket.id,
            "document_id": document_id,
            "upload_url": upload_url,
            "upload_method": method,
            "expires_at": ticket.expires_at,
            "kind": request.kind.unwrap_or_else(|| "annual_accounts".into()),
            "accounts_state": request.accounts_state.unwrap_or_else(|| "preliminary".into()),
        })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct CompleteRequest {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub accounts_state: Option<String>,
}

/// `POST /v1/documents/tickets/{id}/complete`
///
/// Reads what actually landed in storage and checks it against the ticket. The
/// size and hash come from storage, not from the client — asking the client
/// what it uploaded and believing the answer would make every check here
/// decorative.
pub async fn complete_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ticket_id): Path<Uuid>,
    body: Option<Json<CompleteRequest>>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::UploadDocument)?;
    let store = store(&state)?;
    let request = body.map(|Json(b)| b).unwrap_or(CompleteRequest {
        kind: None,
        accounts_state: None,
    });

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let details = tenant
        .ticket_for_completion(ticket_id)
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    // 404 for unknown, expired and already-used alike. A ticket belonging to
    // another tenant never resolves here at all, because the lookup runs inside
    // that tenant's transaction.
    let Some(details) = details else {
        return Err(Problem {
            status: StatusCode::NOT_FOUND,
            title: "not found".into(),
            detail: "no such upload ticket, or it has expired or been used".into(),
        });
    };

    let storage_key = details.storage_key;
    let bytes = match state.blobs.get(&storage_key).await {
        Ok(bytes) => bytes,
        Err(skattjakt_store::blob::BlobError::NotFound(_)) => {
            return Err(Problem {
                status: StatusCode::CONFLICT,
                title: "nothing was uploaded".into(),
                detail: "the ticket exists but no bytes were written to it".into(),
            })
        }
        Err(e) => return Err(internal(e)),
    };

    let kind: DocumentKind = parse_enum(request.kind.as_deref(), "annual_accounts")?;
    let accounts_state: AccountsState =
        parse_enum(request.accounts_state.as_deref(), "preliminary")?;
    let sha256 = skattjakt_core::document::sha256_hex(&bytes);

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;

    // The document version is created from the bytes that are actually there,
    // which re-derives the canonical storage key. The upload key was a landing
    // area; this is where the document lives, content-addressed like every
    // other document in the system.
    let version = tenant
        .create_document(
            kind,
            // What the customer called it, not a label we invented — this is
            // the name they will look for in their own document list.
            &details.declared_name,
            // Sniffed from what actually arrived in storage, not from what the
            // ticket declared half an hour earlier.
            MimeType::sniff(&bytes, Some(details.declared_name.as_str())),
            &bytes,
            None,
            accounts_state,
        )
        .await
        .map_err(map_store_error)?;

    let outcome = tenant
        .complete_upload_ticket(ticket_id, bytes.len() as i64, &sha256, version.id)
        .await
        .map_err(map_store_error)?;

    match outcome {
        CompletionOutcome::Accepted {
            document_version_id,
        } => {
            tenant.commit().await.map_err(internal)?;
            // The landing-area copy is removed once the document exists in its
            // canonical place. Leaving it would double the storage every
            // document costs, for a copy nothing can reach.
            let _ = state.blobs.delete(&storage_key).await;

            Ok((
                StatusCode::CREATED,
                Json(json!({
                    "document_version_id": document_version_id,
                    "sha256": sha256,
                    "byte_size": bytes.len(),
                })),
            )
                .into_response())
        }
        CompletionOutcome::Rejected {
            reason,
            declared,
            observed,
        } => {
            // The ticket row is already marked rejected inside the transaction,
            // so the commit records *why* it failed rather than discarding it.
            tenant.commit().await.map_err(internal)?;
            let _ = state.blobs.delete(&storage_key).await;
            Err(Problem {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                title: "the upload does not match its ticket".into(),
                detail: format!(
                    "{reason}: the ticket declared {declared} bytes and {observed} arrived. \
                     Sizes are in bytes, not characters — a Swedish document contains å, ä \
                     and ö, which are two bytes each."
                ),
            })
        }
    }
}

/// `PUT /v1/documents/tickets/{id}/content`
///
/// The fallback for a deployment with no object storage, where there is no
/// presigned URL to hand out. The bytes go through the API — which is what the
/// ticket flow exists to avoid — so this is a single-node convenience, not the
/// path a phone should take against a real deployment.
pub async fn upload_content(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ticket_id): Path<Uuid>,
    bytes: axum::body::Bytes,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::UploadDocument)?;
    let store = store(&state)?;

    if bytes.len() as i64 > MAX_DECLARED_BYTES {
        return Err(Problem {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            title: "document too large".into(),
            detail: format!("at most {} MB", MAX_DECLARED_BYTES / 1024 / 1024),
        });
    }

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let details = tenant
        .ticket_for_completion(ticket_id)
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    let Some(details) = details else {
        return Err(Problem {
            status: StatusCode::NOT_FOUND,
            title: "not found".into(),
            detail: "no such upload ticket, or it has expired or been used".into(),
        });
    };

    state
        .blobs
        .put(&details.storage_key, &bytes)
        .await
        .map_err(internal)?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /v1/notifications`
///
/// The in-app channel. Delivery for it *is* this endpoint: there is nothing to
/// transmit, so a client reads what the outbox holds.
pub async fn list_notifications(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::ReadCompany)?;
    let store = store(&state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let notifications = tenant
        .recent_notifications(scope.user_id(), 50)
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok(Json(json!({
        "notifications": notifications.iter().map(|n| json!({
            "id": n.id,
            "kind": n.kind,
            "subject_id": n.subject_id,
        })).collect::<Vec<_>>()
    }))
    .into_response())
}

/// Parses a string into a domain enum, falling back to a default.
fn parse_enum<T: serde::de::DeserializeOwned>(
    value: Option<&str>,
    default: &str,
) -> Result<T, Problem> {
    serde_json::from_value(serde_json::Value::String(
        value.unwrap_or(default).to_string(),
    ))
    .map_err(|_| Problem {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        title: "unknown value".into(),
        detail: format!(
            "'{}' is not a value this field accepts",
            value.unwrap_or("")
        ),
    })
}

/// Turns a store error into a status a client can act on.
///
/// `Invalid` is the caller's mistake and answers 422; everything else is ours
/// and answers 500. Collapsing them would report our faults as the customer's.
fn map_store_error(error: skattjakt_store::StoreError) -> Problem {
    match error {
        skattjakt_store::StoreError::Invalid(detail) => Problem {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            title: "invalid request".into(),
            detail,
        },
        skattjakt_store::StoreError::NotFound => Problem {
            status: StatusCode::NOT_FOUND,
            title: "not found".into(),
            detail: "no such object".into(),
        },
        other => internal(other),
    }
}

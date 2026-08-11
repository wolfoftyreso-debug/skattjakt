//! The session surface: sign in, refresh, sign out, devices, company switching.
//!
//! Written for three clients at once even though only the web client exists.
//! Everything a phone needs is here — a refresh token it can hold in the
//! Keychain, a device identity that survives sign-out, a push-token slot, and a
//! company switch that does not require signing in again — because adding any
//! of it after an app has shipped is a breaking change across every client.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use skattjakt_core::CompanyId;
use skattjakt_identity::{ClientKind, PasswordPolicy, Permission};
use skattjakt_store::identity::{IssuedSession, SignInError};
use skattjakt_telemetry::{names, LabelSet};
use uuid::Uuid;

use crate::routes::internal;
use crate::{authorise, AppState, Problem, Scope};

/// Hashes a client address for the "signed in from" list.
///
/// Stored as a hash rather than in the clear: it is enough to tell two
/// locations apart, which is all the feature needs, and it keeps an address out
/// of a table that many queries touch.
fn hash_ip(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)?;
    Some(skattjakt_core::document::sha256_hex(raw.as_bytes()))
}

/// Reads the client kind from the header the clients send.
///
/// Defaults to web rather than rejecting: an unknown client is a browser until
/// proven otherwise, and the only thing that turns on it is the session
/// lifetime — where web is the conservative choice.
fn client_kind(headers: &HeaderMap) -> ClientKind {
    headers
        .get("x-skattjakt-client")
        .and_then(|v| v.to_str().ok())
        .and_then(ClientKind::parse)
        .unwrap_or(ClientKind::Web)
}

fn session_body(issued: &IssuedSession) -> serde_json::Value {
    json!({
        "access_token": issued.access_token.expose(),
        "token_type": "Bearer",
        "expires_at": issued.access_expires_at,
        "refresh_token": issued.refresh_token.expose(),
        "refresh_expires_at": issued.refresh_expires_at,
        "company_id": issued.company_id,
        "role": issued.role.as_str(),
        "device_id": issued.device_id,
    })
}

#[derive(Debug, Deserialize)]
pub struct SignInRequest {
    pub email: String,
    pub password: String,
    /// A stable per-installation identifier the client generates and keeps.
    ///
    /// Lets a returning installation be recognised instead of accumulating a
    /// new device row on every sign-in. Not a security boundary — it is
    /// client-supplied and scoped to the user, so it can only ever collide with
    /// that user's own devices.
    #[serde(default)]
    pub install_id: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
}

/// `POST /v1/auth/sign-in`
pub async fn sign_in(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SignInRequest>,
) -> Result<Response, Problem> {
    let store = crate::routes::store(&state)?;
    let client = client_kind(&headers);

    // A client that sends no install id gets a per-session one, so it still
    // works — it simply accumulates a device row per sign-in, which is the
    // correct consequence of not identifying its installation.
    let install_id = request
        .install_id
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let device_name = request
        .device_name
        .unwrap_or_else(|| default_device_name(client));

    let ip_hash = hash_ip(&headers);
    match store
        .sign_in(
            skattjakt_store::identity::SignInAttempt {
                email: &request.email,
                password: &request.password,
                client,
                install_id: &install_id,
                device_name: &device_name,
                ip_hash: ip_hash.as_deref(),
            },
            &state.password_verifier,
        )
        .await
    {
        Ok(issued) => {
            state.metrics.increment(
                names::SIGN_INS,
                LabelSet::new()
                    .enumerated("client", client.as_str())
                    .enumerated("outcome", "succeeded"),
            );
            Ok((StatusCode::CREATED, Json(session_body(&issued))).into_response())
        }
        Err(SignInError::Locked { until }) => {
            state.metrics.increment(
                names::SIGN_INS,
                LabelSet::new()
                    .enumerated("client", client.as_str())
                    .enumerated("outcome", "locked"),
            );
            Err(Problem {
                status: StatusCode::TOO_MANY_REQUESTS,
                title: "account temporarily locked".into(),
                detail: format!(
                    "too many failed attempts; try again after {}",
                    until.to_rfc3339()
                ),
            })
        }
        Err(SignInError::NoCompany) => Err(Problem {
            status: StatusCode::FORBIDDEN,
            title: "no company".into(),
            detail: "this account is not a member of any company".into(),
        }),
        Err(SignInError::InvalidCredentials) => {
            state.metrics.increment(
                names::SIGN_INS,
                LabelSet::new()
                    .enumerated("client", client.as_str())
                    .enumerated("outcome", "rejected"),
            );
            // One message for every reason. Distinguishing "no such account"
            // from "wrong password" enumerates which businesses are customers.
            Err(Problem {
                status: StatusCode::UNAUTHORIZED,
                title: "invalid credentials".into(),
                detail: "the email address or password is not correct".into(),
            })
        }
        Err(SignInError::Store(e)) => Err(internal(e)),
    }
}

fn default_device_name(client: ClientKind) -> String {
    match client {
        ClientKind::Web => "Webbläsare",
        ClientKind::Ios => "iPhone",
        ClientKind::Android => "Android-enhet",
    }
    .to_string()
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// `POST /v1/auth/refresh`
///
/// Rotates. The old refresh token stops working, except within the short grace
/// window that keeps a lost response from signing a customer out — see
/// `skattjakt_identity::SessionPolicy`.
pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RefreshRequest>,
) -> Result<Response, Problem> {
    let store = crate::routes::store(&state)?;

    match store
        .refresh_session(&request.refresh_token, hash_ip(&headers).as_deref())
        .await
    {
        Ok(Some(issued)) => {
            state.metrics.increment(
                names::SESSION_REFRESHES,
                LabelSet::new().enumerated("outcome", "rotated"),
            );
            Ok(Json(session_body(&issued)).into_response())
        }
        Ok(None) => {
            // Expired, revoked, unknown and reuse-detected all answer the same
            // way. A client cannot act differently on the distinction — every
            // one of them means "sign in again" — and telling a thief which of
            // the four they hit is free intelligence.
            state.metrics.increment(
                names::SESSION_REFRESHES,
                LabelSet::new().enumerated("outcome", "rejected"),
            );
            Err(Problem {
                status: StatusCode::UNAUTHORIZED,
                title: "the session cannot be refreshed".into(),
                detail: "sign in again".into(),
            })
        }
        Err(e) => Err(internal(e)),
    }
}

/// `POST /v1/auth/sign-out`
pub async fn sign_out(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let store = crate::routes::store(&state)?;

    let Scope::User(user) = &scope else {
        return Err(Problem {
            status: StatusCode::BAD_REQUEST,
            title: "not a session".into(),
            detail: "only a signed-in session can be signed out; a company token is revoked \
                     through token management"
                .into(),
        });
    };

    store
        .revoke_session(user.session_id, "signed_out")
        .await
        .map_err(internal)?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /v1/auth/sign-out-everywhere`
///
/// What a customer reaches for when a device is lost. Ends every session for
/// the user, including this one.
pub async fn sign_out_everywhere(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let store = crate::routes::store(&state)?;

    let Scope::User(user) = &scope else {
        return Err(Problem {
            status: StatusCode::BAD_REQUEST,
            title: "not a session".into(),
            detail: "only a signed-in session can do this".into(),
        });
    };

    let ended = store
        .revoke_all_sessions(user.user_id, "signed_out")
        .await
        .map_err(internal)?;

    Ok(Json(json!({ "sessions_ended": ended })).into_response())
}

/// `GET /v1/auth/devices`
pub async fn list_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let store = crate::routes::store(&state)?;

    let Scope::User(user) = &scope else {
        return Err(Problem {
            status: StatusCode::BAD_REQUEST,
            title: "not a session".into(),
            detail: "device management needs a signed-in session".into(),
        });
    };

    let devices = store.list_devices(user.user_id).await.map_err(internal)?;

    Ok(Json(json!({
        "devices": devices.iter().map(|d| json!({
            "id": d.device_id,
            "platform": d.platform,
            "display_name": d.display_name,
            "last_seen_at": d.last_seen_at,
            "push_ready": d.push_ready,
            "live_sessions": d.live_sessions,
            "current": d.device_id == user.device_id,
        })).collect::<Vec<_>>()
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
pub struct PushTokenRequest {
    /// `null` clears the token — how a client says "notifications were turned
    /// off" without deleting the device the customer can still see.
    pub push_token: Option<String>,
    pub provider: Option<String>,
}

/// `PUT /v1/auth/devices/{id}/push-token`
pub async fn set_push_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(device_id): Path<Uuid>,
    Json(request): Json<PushTokenRequest>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let store = crate::routes::store(&state)?;

    let Scope::User(user) = &scope else {
        return Err(Problem {
            status: StatusCode::BAD_REQUEST,
            title: "not a session".into(),
            detail: "registering a push token needs a signed-in session".into(),
        });
    };

    if let Some(provider) = request.provider.as_deref() {
        if !matches!(provider, "apns" | "fcm" | "web_push") {
            return Err(Problem {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                title: "unknown push provider".into(),
                detail: "provider must be one of apns, fcm, web_push".into(),
            });
        }
    }
    if request.push_token.is_some() && request.provider.is_none() {
        return Err(Problem {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            title: "provider required".into(),
            detail: "a push token must name the provider that issued it".into(),
        });
    }

    let updated = store
        .set_push_token(
            user.user_id,
            device_id,
            request.push_token.as_deref(),
            request.provider.as_deref(),
        )
        .await
        .map_err(internal)?;

    if !updated {
        // 404 rather than 403: whether another customer's device exists is not
        // this caller's business.
        return Err(Problem {
            status: StatusCode::NOT_FOUND,
            title: "not found".into(),
            detail: "no such device".into(),
        });
    }

    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
pub struct SwitchCompanyRequest {
    pub company_id: Uuid,
}

/// `POST /v1/auth/switch-company`
///
/// For the accountant with several clients — the normal case in this market.
/// Membership is verified rather than trusted from the request, and the tokens
/// are not rotated: the session keeps its credentials and changes which tenant
/// it acts in.
pub async fn switch_company(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SwitchCompanyRequest>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let store = crate::routes::store(&state)?;

    let Scope::User(user) = &scope else {
        return Err(Problem {
            status: StatusCode::BAD_REQUEST,
            title: "not a session".into(),
            detail: "switching company needs a signed-in session".into(),
        });
    };

    let target = CompanyId::from_uuid(request.company_id);
    match store
        .switch_company(user.session_id, user.user_id, target)
        .await
        .map_err(internal)?
    {
        Some(role) => Ok(Json(json!({
            "company_id": target,
            "role": role.as_str(),
        }))
        .into_response()),
        // 404, not 403. A caller who is not a member must not learn whether the
        // company exists.
        None => Err(Problem {
            status: StatusCode::NOT_FOUND,
            title: "not found".into(),
            detail: "no such company, or you are not a member of it".into(),
        }),
    }
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// `POST /v1/auth/change-password`
///
/// Ends every other session. A password change after a suspected compromise
/// that leaves the attacker's session alive is theatre.
pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChangePasswordRequest>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let store = crate::routes::store(&state)?;

    let Scope::User(user) = &scope else {
        return Err(Problem {
            status: StatusCode::BAD_REQUEST,
            title: "not a session".into(),
            detail: "changing a password needs a signed-in session".into(),
        });
    };

    PasswordPolicy::default()
        .check(&request.new_password)
        .map_err(|e| Problem {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            title: "password rejected".into(),
            detail: e.to_string(),
        })?;

    // Re-authenticate with the current password. Holding a session is not
    // enough to change the credential that creates sessions: a borrowed
    // unlocked laptop would otherwise be a permanent account takeover.
    let verified = store
        .verify_current_password(
            user.user_id,
            &request.current_password,
            &state.password_verifier,
        )
        .await
        .map_err(internal)?;

    if !verified {
        return Err(Problem {
            status: StatusCode::UNAUTHORIZED,
            title: "invalid credentials".into(),
            detail: "the current password is not correct".into(),
        });
    }

    let new_hash = state
        .password_verifier
        .hash(&request.new_password)
        .map_err(|_| Problem {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            title: "internal error".into(),
            detail: "the password could not be stored".into(),
        })?;

    let ended = store
        .change_password(user.user_id, &new_hash, Some(user.session_id))
        .await
        .map_err(internal)?;

    Ok(Json(json!({ "other_sessions_ended": ended })).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub role: Option<String>,
}

/// `POST /v1/users`
///
/// Creates a person in the caller's company. Needs `ManageMembers`, which only
/// an owner has — so a member cannot quietly widen access and an advisor cannot
/// invite themselves a colleague.
pub async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateUserRequest>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = crate::routes::company_scope(&scope, Permission::ManageMembers)?;
    let store = crate::routes::store(&state)?;

    let role = request
        .role
        .as_deref()
        .map(|r| {
            skattjakt_identity::Role::parse(r).ok_or_else(|| Problem {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                title: "unknown role".into(),
                detail: "role must be one of owner, member, advisor".into(),
            })
        })
        .transpose()?
        .unwrap_or(skattjakt_identity::Role::Member);

    PasswordPolicy::default()
        .check(&request.password)
        .map_err(|e| Problem {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            title: "password rejected".into(),
            detail: e.to_string(),
        })?;

    let hash = state
        .password_verifier
        .hash(&request.password)
        .map_err(|_| Problem {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            title: "internal error".into(),
            detail: "the password could not be stored".into(),
        })?;

    let user_id = store
        .create_user_with_password(&request.email, &hash, company_id, role)
        .await
        .map_err(|e| match e {
            skattjakt_store::StoreError::Database(ref db)
                if db.to_string().contains("users_email_key") =>
            {
                // Deliberately the same answer as success would give a caller
                // who cannot see the result: an owner adding a colleague should
                // not be able to probe which addresses already have accounts
                // elsewhere in the product.
                Problem {
                    status: StatusCode::CONFLICT,
                    title: "already exists".into(),
                    detail: "an account with that email address already exists".into(),
                }
            }
            other => internal(other),
        })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "user_id": user_id,
            "email": request.email.trim().to_lowercase(),
            "role": role.as_str(),
            "company_id": company_id,
        })),
    )
        .into_response())
}

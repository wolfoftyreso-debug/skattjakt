//! Persistence for users, credentials, devices and sessions.
//!
//! The decisions live in `skattjakt-identity`, which has no I/O and is
//! therefore testable without a database. This module is the part that talks to
//! Postgres, and it is deliberately thin: where a question has an answer that
//! could be got wrong, the answer comes from the identity crate.
//!
//! These tables sit outside row-level security, for the same reason
//! `api_tokens` does — authentication happens before a tenant is known, so a
//! policy keyed on the tenant cannot guard the lookup that establishes it.

use chrono::{DateTime, Utc};
use skattjakt_core::CompanyId;
use skattjakt_identity::{
    credential::{lockout_duration, CredentialError, MAX_FAILED_ATTEMPTS},
    ClientKind, PasswordVerifier, RefreshOutcome, Role, SecretToken, SessionPolicy, SessionState,
    TokenHash, VerificationLevel,
};
use sqlx::Row;
use uuid::Uuid;

use crate::{Store, StoreError, StoreResult};

/// An authenticated caller, resolved from an access token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedUser {
    pub user_id: Uuid,
    pub company_id: CompanyId,
    pub session_id: Uuid,
    pub device_id: Uuid,
    pub role: Role,
    pub verification: VerificationLevel,
    pub client_kind: ClientKind,
}

/// A freshly issued pair. The clear tokens exist only long enough to be
/// returned to the caller.
///
/// `Debug` is derived rather than hand-written because `SecretToken`'s own
/// `Debug` prints a placeholder — so this cannot print a token even by
/// accident.
#[derive(Debug)]
pub struct IssuedSession {
    pub session_id: Uuid,
    pub device_id: Uuid,
    pub access_token: SecretToken,
    pub access_expires_at: DateTime<Utc>,
    pub refresh_token: SecretToken,
    pub refresh_expires_at: DateTime<Utc>,
    pub company_id: CompanyId,
    pub role: Role,
}

/// How a sign-in failed.
///
/// `InvalidCredentials` covers a missing user, a wrong password and a user with
/// no password set. Splitting them would let anyone enumerate which businesses
/// are customers.
#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    #[error("the credentials are not valid")]
    InvalidCredentials,
    #[error("the account is locked until {until}")]
    Locked { until: DateTime<Utc> },
    #[error("the user belongs to no company")]
    NoCompany,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// One sign-in attempt.
///
/// A struct rather than eight positional arguments: `sign_in(email, password,
/// client, install, name, ip, ...)` is a call whose fourth and fifth arguments
/// are both strings and can be swapped without the compiler noticing.
#[derive(Debug)]
pub struct SignInAttempt<'a> {
    pub email: &'a str,
    pub password: &'a str,
    pub client: ClientKind,
    pub install_id: &'a str,
    pub device_name: &'a str,
    pub ip_hash: Option<&'a str>,
}

impl Store {
    /// Signs a user in and issues a session bound to a device.
    pub async fn sign_in(
        &self,
        attempt: SignInAttempt<'_>,
        verifier: &PasswordVerifier,
    ) -> Result<IssuedSession, SignInError> {
        let SignInAttempt {
            email,
            password,
            client,
            install_id,
            device_name,
            ip_hash,
        } = attempt;
        // Email is matched case-insensitively and after trimming. A customer
        // who typed a trailing space is not an attacker, and telling them their
        // credentials are wrong when they are right is the kind of defect that
        // never gets reported, only abandoned.
        let normalised = email.trim().to_lowercase();

        let row = sqlx::query(
            "SELECT u.id, c.method, c.password_hash, c.failed_attempts, c.locked_until
             FROM users u
             LEFT JOIN user_credentials c ON c.user_id = u.id
             WHERE lower(u.email) = $1",
        )
        .bind(&normalised)
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::from)?;

        let Some(row) = row else {
            // No such user. Spend the same work a real verification costs, so
            // the response time does not disclose which addresses exist.
            verifier.spend_equivalent_work();
            return Err(SignInError::InvalidCredentials);
        };

        let user_id: Uuid = row.get("id");
        let locked_until: Option<DateTime<Utc>> = row.try_get("locked_until").ok().flatten();
        if let Some(until) = locked_until {
            if until > Utc::now() {
                return Err(SignInError::Locked { until });
            }
        }

        let stored: Option<String> = row.try_get("password_hash").ok().flatten();
        let Some(stored) = stored else {
            // A federated user, or one with no credential yet. Same answer,
            // same cost.
            verifier.spend_equivalent_work();
            return Err(SignInError::InvalidCredentials);
        };

        if verifier.verify(password, &stored).is_err() {
            self.record_failed_attempt(user_id).await?;
            return Err(SignInError::InvalidCredentials);
        }

        // A successful sign-in clears the counter. Without this, eight failures
        // spread over a year would lock out a customer who has done nothing
        // wrong.
        sqlx::query(
            "UPDATE user_credentials
             SET failed_attempts = 0, locked_until = NULL, updated_at = now()
             WHERE user_id = $1",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(StoreError::from)?;

        let company = self.default_company_for(user_id).await?;
        let Some((company_id, role)) = company else {
            return Err(SignInError::NoCompany);
        };

        let device_id = self
            .upsert_device(user_id, client, install_id, device_name)
            .await?;

        Ok(self
            .issue_session(user_id, company_id, device_id, role, client, ip_hash)
            .await?)
    }

    async fn record_failed_attempt(&self, user_id: Uuid) -> StoreResult<()> {
        // The counter and the lock are computed in one statement so two
        // concurrent attempts cannot both read the old count and each write
        // back the same increment.
        let row = sqlx::query(
            "UPDATE user_credentials
             SET failed_attempts = failed_attempts + 1, updated_at = now()
             WHERE user_id = $1
             RETURNING failed_attempts",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            let attempts: i32 = row.get("failed_attempts");
            if attempts >= MAX_FAILED_ATTEMPTS {
                let until = Utc::now() + lockout_duration(attempts);
                sqlx::query("UPDATE user_credentials SET locked_until = $2 WHERE user_id = $1")
                    .bind(user_id)
                    .bind(until)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Which company a session starts in.
    ///
    /// An accountant belongs to many companies — the normal case in this
    /// market, not an edge case. The first is chosen by role then by age, so a
    /// person who owns a company lands in their own rather than in a client's,
    /// and the client can switch afterwards.
    async fn default_company_for(&self, user_id: Uuid) -> StoreResult<Option<(CompanyId, Role)>> {
        // Reading membership is the one place the tenant cannot be set first,
        // because finding the tenant is the point of the query. It goes through
        // a narrow `SECURITY DEFINER` function that answers only this question —
        // rather than through a `BYPASSRLS` role, which would also bypass
        // isolation on every table holding the customer's economy. See the note
        // in migration 0004.
        let row = sqlx::query("SELECT company_id, role FROM memberships_for_user($1) LIMIT 1")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.and_then(|r| {
            let company_id = CompanyId::from_uuid(r.get("company_id"));
            Role::parse(r.get::<String, _>("role").as_str()).map(|role| (company_id, role))
        }))
    }

    async fn upsert_device(
        &self,
        user_id: Uuid,
        client: ClientKind,
        install_id: &str,
        display_name: &str,
    ) -> StoreResult<Uuid> {
        // One row per installation, not one per sign-in. A device outlives its
        // sessions: the push token and the name the customer recognises should
        // survive a sign-out.
        let truncated: String = display_name.chars().take(120).collect();
        let install: String = install_id.chars().take(200).collect();

        let row = sqlx::query(
            "INSERT INTO devices (user_id, platform, display_name, install_id, last_seen_at)
             VALUES ($1, $2, $3, $4, now())
             ON CONFLICT (user_id, install_id) DO UPDATE
                 SET last_seen_at = now(), display_name = EXCLUDED.display_name
             RETURNING id",
        )
        .bind(user_id)
        .bind(client.as_str())
        .bind(&truncated)
        .bind(&install)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.get("id"))
    }

    async fn issue_session(
        &self,
        user_id: Uuid,
        company_id: CompanyId,
        device_id: Uuid,
        role: Role,
        client: ClientKind,
        ip_hash: Option<&str>,
    ) -> StoreResult<IssuedSession> {
        let policy = SessionPolicy::for_client(client);
        let now = Utc::now();
        let access = SecretToken::generate();
        let refresh = SecretToken::generate();
        let family = Uuid::new_v4();

        let row = sqlx::query(
            "INSERT INTO sessions (
                 user_id, device_id, company_id,
                 access_token_hash, access_expires_at,
                 refresh_token_hash, refresh_expires_at,
                 family_id, generation, client_kind, ip_hash)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,0,$9,$10)
             RETURNING id",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(company_id.0)
        .bind(access.hash().as_str())
        .bind(policy.access_expiry(now))
        .bind(refresh.hash().as_str())
        .bind(policy.refresh_expiry(now))
        .bind(family)
        .bind(client.as_str())
        .bind(ip_hash)
        .fetch_one(&self.pool)
        .await?;

        Ok(IssuedSession {
            session_id: row.get("id"),
            device_id,
            access_expires_at: policy.access_expiry(now),
            refresh_expires_at: policy.refresh_expiry(now),
            access_token: access,
            refresh_token: refresh,
            company_id,
            role,
        })
    }

    /// Resolves an access token to a caller.
    ///
    /// Returns `None` for absent, expired and revoked alike. A caller cannot
    /// use the distinction, and a 401 that explains why is a 401 that helps an
    /// attacker.
    pub async fn authenticate_session(
        &self,
        presented: &str,
    ) -> StoreResult<Option<AuthenticatedUser>> {
        let hash = TokenHash::of(presented);

        let row = sqlx::query(
            "UPDATE sessions s
             SET last_used_at = now()
             WHERE s.access_token_hash = $1
               AND s.revoked_at IS NULL
               AND s.superseded_at IS NULL
               AND s.access_expires_at > now()
               AND membership_role(s.user_id, s.company_id) IS NOT NULL
             RETURNING s.id, s.user_id, s.company_id, s.device_id, s.client_kind,
                       membership_role(s.user_id, s.company_id) AS role",
        )
        .bind(hash.as_str())
        .fetch_optional(&self.pool)
        .await?;

        // The join on `company_members` is what makes removing someone from a
        // company take effect immediately: their access token stays valid for
        // its remaining minutes, but it no longer resolves to a role in a
        // company they were removed from, so it authenticates to nothing.
        Ok(row.and_then(|r| {
            let role = Role::parse(r.get::<String, _>("role").as_str())?;
            let client_kind = ClientKind::parse(r.get::<String, _>("client_kind").as_str())?;
            Some(AuthenticatedUser {
                user_id: r.get("user_id"),
                company_id: CompanyId::from_uuid(r.get("company_id")),
                session_id: r.get("id"),
                device_id: r.get("device_id"),
                role,
                // Verification is not yet sourced from anywhere: no provider is
                // integrated. Stated as unverified rather than assumed to be
                // adequate, so the day BankID lands the change is additive.
                verification: VerificationLevel::Unverified,
                client_kind,
            })
        }))
    }

    /// Exchanges a refresh token for a new pair.
    ///
    /// Rotation with reuse detection. The decision is
    /// `SessionPolicy::evaluate_refresh`; this performs it.
    pub async fn refresh_session(
        &self,
        presented: &str,
        ip_hash: Option<&str>,
    ) -> StoreResult<Option<IssuedSession>> {
        let hash = TokenHash::of(presented);
        let mut tx = self.pool.begin().await?;

        // `FOR UPDATE` on the family: two concurrent refreshes of one session
        // must not both rotate, or each would invalidate the other's brand-new
        // token and the customer would be signed out by their own retry.
        let row = sqlx::query(
            "SELECT s.id, s.user_id, s.company_id, s.device_id, s.family_id, s.generation,
                    s.refresh_expires_at, s.revoked_at, s.superseded_at, s.client_kind,
                    s.access_expires_at
             FROM sessions s
             WHERE s.refresh_token_hash = $1
             FOR UPDATE",
        )
        .bind(hash.as_str())
        .fetch_optional(&mut *tx)
        .await?;

        let Some(row) = row else {
            tx.rollback().await?;
            return Ok(None);
        };

        let session_id: Uuid = row.get("id");
        let family_id: Uuid = row.get("family_id");
        let presented_generation: i32 = row.get("generation");
        let user_id: Uuid = row.get("user_id");
        let company_id = CompanyId::from_uuid(row.get("company_id"));
        let device_id: Uuid = row.get("device_id");
        let client = ClientKind::parse(row.get::<String, _>("client_kind").as_str())
            .unwrap_or(ClientKind::Web);
        let policy = SessionPolicy::for_client(client);

        // The family's current generation, which is ahead of the presented row
        // whenever the presented token has already been rotated away.
        let current: i32 = sqlx::query_scalar(
            "SELECT coalesce(max(generation), 0) FROM sessions WHERE family_id = $1",
        )
        .bind(family_id)
        .fetch_one(&mut *tx)
        .await?;

        let state = SessionState {
            generation: current,
            refresh_expires_at: row.get("refresh_expires_at"),
            revoked_at: row.get("revoked_at"),
            rotated_at: row.get("superseded_at"),
        };

        match policy.evaluate_refresh(&state, presented_generation, Utc::now()) {
            RefreshOutcome::ReuseDetected => {
                // Tear the whole family down. This signs the customer out as
                // well as the thief, which is the intended outcome: the
                // alternative is issuing working tokens to both.
                sqlx::query(
                    "UPDATE sessions
                     SET revoked_at = now(), revoked_reason = 'refresh_reuse'
                     WHERE family_id = $1 AND revoked_at IS NULL",
                )
                .bind(family_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(None)
            }
            RefreshOutcome::Expired | RefreshOutcome::Revoked => {
                tx.rollback().await?;
                Ok(None)
            }
            RefreshOutcome::ReplayWithinGrace => {
                // A lost response, not a theft. The client is handed the
                // generation that superseded this one rather than a fresh
                // rotation, so a retry costs nothing and mints nothing.
                let live = sqlx::query(
                    "SELECT id, access_expires_at, refresh_expires_at
                     FROM sessions
                     WHERE family_id = $1 AND superseded_at IS NULL AND revoked_at IS NULL",
                )
                .bind(family_id)
                .fetch_optional(&mut *tx)
                .await?;

                let Some(live) = live else {
                    tx.rollback().await?;
                    return Ok(None);
                };

                // The stored tokens are hashes, so the live pair cannot be
                // handed back. Rotate onto a new generation instead: the effect
                // the client needs — a usable pair — without the retry being
                // punished as reuse.
                let issued = self
                    .rotate_onto_new_generation(
                        &mut tx,
                        live.get("id"),
                        family_id,
                        current,
                        user_id,
                        company_id,
                        device_id,
                        &policy,
                        ip_hash,
                    )
                    .await?;
                match issued {
                    Some(issued) => {
                        tx.commit().await?;
                        Ok(Some(issued))
                    }
                    None => {
                        tx.commit().await?;
                        Ok(None)
                    }
                }
            }
            RefreshOutcome::Rotate => {
                let issued = self
                    .rotate_onto_new_generation(
                        &mut tx, session_id, family_id, current, user_id, company_id, device_id,
                        &policy, ip_hash,
                    )
                    .await?;
                match issued {
                    Some(issued) => {
                        tx.commit().await?;
                        Ok(Some(issued))
                    }
                    None => {
                        tx.commit().await?;
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Supersedes one generation and inserts the next.
    ///
    /// The old row keeps its hashes so a replay of it is still *findable* —
    /// which is what lets reuse be told apart from an unknown token.
    #[allow(clippy::too_many_arguments)]
    async fn rotate_onto_new_generation(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        supersede: Uuid,
        family_id: Uuid,
        current_generation: i32,
        user_id: Uuid,
        company_id: CompanyId,
        device_id: Uuid,
        policy: &SessionPolicy,
        ip_hash: Option<&str>,
    ) -> StoreResult<Option<IssuedSession>> {
        // Membership is re-read on every rotation, so removing someone from a
        // company ends their access at the next refresh even if their session
        // had weeks left on it.
        let role_row: Option<String> = sqlx::query_scalar("SELECT membership_role($1, $2)")
            .bind(user_id)
            .bind(company_id.0)
            .fetch_one(&mut **tx)
            .await?;

        let Some(role) = role_row.as_deref().and_then(Role::parse) else {
            sqlx::query(
                "UPDATE sessions SET revoked_at = now(), revoked_reason = 'operator'
                 WHERE family_id = $1 AND revoked_at IS NULL",
            )
            .bind(family_id)
            .execute(&mut **tx)
            .await?;
            return Ok(None);
        };

        let now = Utc::now();
        let access = SecretToken::generate();
        let refresh = SecretToken::generate();

        sqlx::query("UPDATE sessions SET superseded_at = now() WHERE id = $1")
            .bind(supersede)
            .execute(&mut **tx)
            .await?;

        let inserted = sqlx::query(
            "INSERT INTO sessions (
                 user_id, device_id, company_id,
                 access_token_hash, access_expires_at,
                 refresh_token_hash, refresh_expires_at,
                 family_id, generation, client_kind, ip_hash)
             SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,client_kind,COALESCE($10, ip_hash)
             FROM sessions WHERE id = $11
             RETURNING id",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(company_id.0)
        .bind(access.hash().as_str())
        .bind(policy.access_expiry(now))
        .bind(refresh.hash().as_str())
        .bind(policy.refresh_expiry(now))
        .bind(family_id)
        .bind(current_generation + 1)
        .bind(ip_hash)
        .bind(supersede)
        .fetch_one(&mut **tx)
        .await?;

        Ok(Some(IssuedSession {
            session_id: inserted.get("id"),
            device_id,
            access_expires_at: policy.access_expiry(now),
            refresh_expires_at: policy.refresh_expiry(now),
            access_token: access,
            refresh_token: refresh,
            company_id,
            role,
        }))
    }

    /// Signs one session out.
    pub async fn revoke_session(&self, session_id: Uuid, reason: &str) -> StoreResult<()> {
        // By family, not by row. A session is a chain of generations, and
        // ending only the one the caller happens to hold would leave the
        // earlier rows able to detect reuse against a family nobody is using.
        sqlx::query(
            "UPDATE sessions SET revoked_at = now(), revoked_reason = $2
             WHERE family_id = (SELECT family_id FROM sessions WHERE id = $1)
               AND revoked_at IS NULL",
        )
        .bind(session_id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Signs every session for a user out — the "sign out everywhere" a
    /// customer reaches for when a device is lost.
    pub async fn revoke_all_sessions(&self, user_id: Uuid, reason: &str) -> StoreResult<u64> {
        let result = sqlx::query(
            "UPDATE sessions SET revoked_at = now(), revoked_reason = $2
             WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// The devices a customer is signed in on.
    pub async fn list_devices(&self, user_id: Uuid) -> StoreResult<Vec<DeviceSummary>> {
        let rows = sqlx::query(
            "SELECT d.id, d.platform, d.display_name, d.last_seen_at,
                    d.push_token IS NOT NULL AND d.push_failed_at IS NULL AS push_ready,
                    count(s.id) FILTER (
                        WHERE s.revoked_at IS NULL AND s.refresh_expires_at > now()
                    ) AS live_sessions
             FROM devices d
             LEFT JOIN sessions s ON s.device_id = d.id
             WHERE d.user_id = $1
             GROUP BY d.id
             ORDER BY d.last_seen_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| DeviceSummary {
                device_id: r.get("id"),
                platform: r.get("platform"),
                display_name: r.get("display_name"),
                last_seen_at: r.get("last_seen_at"),
                push_ready: r.get("push_ready"),
                live_sessions: r.get::<i64, _>("live_sessions"),
            })
            .collect())
    }

    /// Registers or clears a device's push token.
    pub async fn set_push_token(
        &self,
        user_id: Uuid,
        device_id: Uuid,
        token: Option<&str>,
        provider: Option<&str>,
    ) -> StoreResult<bool> {
        // Scoped by user as well as by device id, so knowing another
        // customer's device id is not enough to redirect their notifications.
        let result = sqlx::query(
            "UPDATE devices
             SET push_token = $3, push_provider = $4, push_failed_at = NULL, last_seen_at = now()
             WHERE id = $2 AND user_id = $1",
        )
        .bind(user_id)
        .bind(device_id)
        .bind(token)
        .bind(provider)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Creates a user with a password and makes them owner of a company.
    pub async fn create_user_with_password(
        &self,
        email: &str,
        password_hash: &str,
        company_id: CompanyId,
        role: Role,
    ) -> StoreResult<Uuid> {
        let mut tx = self.pool.begin().await?;

        // `company_members` is a tenant table under forced row-level security,
        // so the tenant has to be set before the membership insert or the
        // policy rejects it — which is the isolation working, not a bug to
        // route around. `users` and `user_credentials` sit outside RLS because
        // a person is not owned by a company: an accountant belongs to several.
        sqlx::query("SELECT set_config('skattjakt.company_id', $1, true)")
            .bind(company_id.0.to_string())
            .execute(&mut *tx)
            .await?;

        let normalised = email.trim().to_lowercase();

        let row = sqlx::query("INSERT INTO users (email) VALUES ($1) RETURNING id")
            .bind(&normalised)
            .fetch_one(&mut *tx)
            .await?;
        let user_id: Uuid = row.get("id");

        sqlx::query(
            "INSERT INTO user_credentials (user_id, method, password_hash)
             VALUES ($1, 'password', $2)",
        )
        .bind(user_id)
        .bind(password_hash)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO company_members (company_id, user_id, role, accepted_at)
             VALUES ($1, $2, $3, now())",
        )
        .bind(company_id.0)
        .bind(user_id)
        .bind(role.as_str())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(user_id)
    }

    /// Checks a user's current password.
    ///
    /// Used before a password change: holding a session must not be enough to
    /// replace the credential that creates sessions, or a borrowed unlocked
    /// laptop becomes a permanent account takeover.
    pub async fn verify_current_password(
        &self,
        user_id: Uuid,
        password: &str,
        verifier: &PasswordVerifier,
    ) -> StoreResult<bool> {
        let row = sqlx::query(
            "SELECT password_hash FROM user_credentials
             WHERE user_id = $1 AND method = 'password'",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(stored) = row.and_then(|r| r.get::<Option<String>, _>("password_hash")) else {
            verifier.spend_equivalent_work();
            return Ok(false);
        };
        Ok(verifier.verify(password, &stored).is_ok())
    }

    /// Changes a password and signs every other session out.
    ///
    /// Revoking the other sessions is the point of changing a password after a
    /// suspected compromise. A change that leaves the attacker's session alive
    /// is theatre.
    pub async fn change_password(
        &self,
        user_id: Uuid,
        new_hash: &str,
        keep_session: Option<Uuid>,
    ) -> StoreResult<u64> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE user_credentials
             SET password_hash = $2, must_change = FALSE,
                 failed_attempts = 0, locked_until = NULL, updated_at = now()
             WHERE user_id = $1 AND method = 'password'",
        )
        .bind(user_id)
        .bind(new_hash)
        .execute(&mut *tx)
        .await?;

        let result = sqlx::query(
            "UPDATE sessions SET revoked_at = now(), revoked_reason = 'password_changed'
             WHERE user_id = $1 AND revoked_at IS NULL AND id IS DISTINCT FROM $2",
        )
        .bind(user_id)
        .bind(keep_session)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(result.rows_affected())
    }

    /// Switches the active company on a session.
    ///
    /// For the accountant with several clients. Verified against membership
    /// rather than trusted from the request, and it rotates nothing: the
    /// session keeps its tokens and changes which tenant it acts in.
    pub async fn switch_company(
        &self,
        session_id: Uuid,
        user_id: Uuid,
        target: CompanyId,
    ) -> StoreResult<Option<Role>> {
        // `query_scalar::<Option<String>>`, not `get::<String>`: the function
        // returns NULL for a non-member, and unwrapping that as a String panics
        // the request — which is how "you are not a member" became a dropped
        // connection instead of a 404.
        let member: Option<String> = sqlx::query_scalar("SELECT membership_role($1, $2)")
            .bind(user_id)
            .bind(target.0)
            .fetch_one(&self.pool)
            .await?;

        let Some(role) = member.as_deref().and_then(Role::parse) else {
            return Ok(None);
        };

        sqlx::query("UPDATE sessions SET company_id = $2 WHERE id = $1 AND user_id = $3")
            .bind(session_id)
            .bind(target.0)
            .bind(user_id)
            .execute(&self.pool)
            .await?;

        Ok(Some(role))
    }

    /// Removes sessions whose refresh token expired long ago.
    ///
    /// Retention, not cleanliness: an expired session row is a record of when
    /// someone was signed in and from roughly where, and it should not outlive
    /// its usefulness.
    pub async fn purge_expired_sessions(&self, older_than_days: i64) -> StoreResult<u64> {
        let result = sqlx::query(
            "DELETE FROM sessions
             WHERE refresh_expires_at < now() - make_interval(days => $1::int)",
        )
        .bind(older_than_days as i32)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[derive(Debug, Clone)]
pub struct DeviceSummary {
    pub device_id: Uuid,
    pub platform: String,
    pub display_name: String,
    pub last_seen_at: DateTime<Utc>,
    pub push_ready: bool,
    pub live_sessions: i64,
}

impl From<CredentialError> for SignInError {
    fn from(_: CredentialError) -> Self {
        SignInError::InvalidCredentials
    }
}

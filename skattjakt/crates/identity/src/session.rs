//! Session lifetimes, refresh rotation, and detecting a stolen token.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::token::ClientKind;

/// How long an access token and a refresh token live, per client kind.
///
/// The two lifetimes answer different questions. The access token's lifetime is
/// how long a revoked session keeps working — so it is short. The refresh
/// token's lifetime is how often a customer has to sign in again — so it is as
/// long as the platform's credential storage justifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPolicy {
    pub access_lifetime: Duration,
    pub refresh_lifetime: Duration,
    /// How long a refresh token remains usable after being rotated away.
    ///
    /// Not zero, and the reason is a real failure that a zero-grace design
    /// produces constantly on mobile: the client sends a refresh, the server
    /// commits the rotation, and the response is lost to a tunnel or a
    /// backgrounded app. The client retries with the only token it has — the
    /// old one — and a strict implementation reads that as theft and signs a
    /// blameless customer out.
    ///
    /// Within the grace window a repeat of the *immediately* previous
    /// generation returns the current tokens again. Beyond it, or from an older
    /// generation, it is treated as reuse.
    pub refresh_grace: Duration,
}

impl SessionPolicy {
    /// The policy for a client kind.
    ///
    /// Web sessions are deliberately much shorter. A browser cannot hold a
    /// secret away from script running on its own page, so the exposure of a
    /// stolen web refresh token is bounded by making it expire in half a day.
    /// iOS and Android put the token in the Keychain or the Keystore, which is
    /// a materially different security property, and a customer who has to sign
    /// in to a phone app every twelve hours stops using the phone app.
    pub fn for_client(kind: ClientKind) -> Self {
        match kind {
            ClientKind::Web => Self {
                access_lifetime: Duration::minutes(15),
                refresh_lifetime: Duration::hours(12),
                refresh_grace: Duration::seconds(30),
            },
            ClientKind::Ios | ClientKind::Android => Self {
                access_lifetime: Duration::minutes(30),
                refresh_lifetime: Duration::days(30),
                // Wider on mobile, where a lost response is a routine event: a
                // tunnel, a handover between networks, an app suspended
                // mid-request.
                refresh_grace: Duration::seconds(60),
            },
        }
    }

    pub fn access_expiry(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + self.access_lifetime
    }

    pub fn refresh_expiry(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + self.refresh_lifetime
    }
}

/// What a stored session looks like to the policy code.
///
/// A projection rather than the row: this crate decides, and the store
/// persists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    pub generation: i32,
    pub refresh_expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// When the generation the caller presented was rotated away. `None` when
    /// they presented the current one.
    pub rotated_at: Option<DateTime<Utc>>,
}

/// What to do about a presented refresh token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Rotate: mint a new pair, increment the generation.
    Rotate,
    /// A retry inside the grace window. Return the current tokens again rather
    /// than rotating, so a lost response does not cost the customer a sign-in.
    ReplayWithinGrace,
    /// Two parties hold tokens from one family. Revoke the whole family.
    ///
    /// This signs out the customer as well as the thief. That is the intended
    /// behaviour and not a regrettable side effect: the alternative is issuing
    /// working tokens to both, which is how a stolen refresh token becomes
    /// permanent access.
    ReuseDetected,
    /// Past its expiry. Sign in again.
    Expired,
    /// Already revoked — signed out, or a family that was revoked earlier.
    Revoked,
}

impl SessionPolicy {
    /// Decides what a presented refresh token means.
    ///
    /// `presented_generation` is the generation of the token the caller sent;
    /// `state.generation` is the family's current one.
    pub fn evaluate_refresh(
        &self,
        state: &SessionState,
        presented_generation: i32,
        now: DateTime<Utc>,
    ) -> RefreshOutcome {
        // Revocation is checked before expiry: a session revoked for
        // `refresh_reuse` that then expires should still be reported as
        // revoked, because that is the fact an operator needs.
        if state.revoked_at.is_some() {
            return RefreshOutcome::Revoked;
        }
        if now >= state.refresh_expires_at {
            return RefreshOutcome::Expired;
        }

        match presented_generation.cmp(&state.generation) {
            std::cmp::Ordering::Equal => RefreshOutcome::Rotate,
            // A generation ahead of the family's current one cannot have been
            // issued by this server. Treated as reuse rather than ignored,
            // because the only ways to hold one are a forgery or a rollback of
            // the sessions table, and both deserve the family being torn down.
            std::cmp::Ordering::Greater => RefreshOutcome::ReuseDetected,
            std::cmp::Ordering::Less => {
                let immediately_previous = presented_generation == state.generation - 1;
                let inside_grace = state
                    .rotated_at
                    .is_some_and(|at| now < at + self.refresh_grace);
                if immediately_previous && inside_grace {
                    RefreshOutcome::ReplayWithinGrace
                } else {
                    RefreshOutcome::ReuseDetected
                }
            }
        }
    }
}

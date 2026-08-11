//! The notification outbox.
//!
//! A notification is written in the same transaction as the thing it describes,
//! and delivered later by a separate worker. That separation is the whole
//! design, and it is not incidental:
//!
//!   * Sending *inside* the transaction means a rollback leaves a customer told
//!     about a result that does not exist.
//!   * Sending *after* it means a crash in between loses the notification with
//!     no record that it was owed.
//!
//! An outbox row has neither problem. It commits atomically with the analysis,
//! and delivery retries against a row that is still there.

use chrono::Duration;
use serde::{Deserialize, Serialize};
use skattjakt_core::CompanyId;
use sqlx::Row;
use uuid::Uuid;

use crate::{StoreResult, Tenant};

/// What happened.
///
/// A closed set, because each kind is rendered into a customer-facing sentence
/// on the delivery side and a kind nobody has written a sentence for would be
/// delivered as a blank notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    AnalysisCompleted,
    AnalysisFailed,
    DocumentProcessed,
    MemberInvited,
    SecurityAlert,
}

impl NotificationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationKind::AnalysisCompleted => "analysis_completed",
            NotificationKind::AnalysisFailed => "analysis_failed",
            NotificationKind::DocumentProcessed => "document_processed",
            NotificationKind::MemberInvited => "member_invited",
            NotificationKind::SecurityAlert => "security_alert",
        }
    }

    /// Where this kind goes when the customer has expressed no preference.
    ///
    /// Defaults live here rather than as rows, so a user who has never opened
    /// their settings has no rows at all — which keeps "has not chosen" and
    /// "chose the defaults" distinguishable, and that distinction matters the
    /// day a default changes.
    pub fn default_channels(self) -> &'static [Channel] {
        match self {
            // The thing the customer is waiting for. Both channels: the push
            // reaches them now, the email is there when they come back.
            NotificationKind::AnalysisCompleted => &[Channel::Push, Channel::Email],
            // A failure they can act on — usually by re-uploading a readable
            // document — so it should not wait for them to check.
            NotificationKind::AnalysisFailed => &[Channel::Push, Channel::Email],
            // Routine progress. In-app only; a push for every processed
            // document is how a customer turns push off for everything.
            NotificationKind::DocumentProcessed => &[Channel::InApp],
            NotificationKind::MemberInvited => &[Channel::Email],
            // Never suppressible. See `is_mandatory`.
            NotificationKind::SecurityAlert => &[Channel::Email, Channel::Push],
        }
    }

    /// Whether a customer may turn this off.
    ///
    /// Security alerts may not be. "Someone signed in from a new device" is
    /// exactly the message an attacker who has taken over an account would
    /// disable first, so the preference is not offered rather than being
    /// offered and ignored.
    pub fn is_mandatory(self) -> bool {
        matches!(self, NotificationKind::SecurityAlert)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Push,
    Email,
    InApp,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Push => "push",
            Channel::Email => "email",
            Channel::InApp => "in_app",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "push" => Some(Channel::Push),
            "email" => Some(Channel::Email),
            "in_app" => Some(Channel::InApp),
            _ => None,
        }
    }
}

/// One notification to write.
#[derive(Debug, Clone)]
pub struct NewNotification {
    pub kind: NotificationKind,
    /// `None` means every member of the company.
    pub user_id: Option<Uuid>,
    pub subject_id: Option<Uuid>,
    pub subject_kind: Option<&'static str>,
    /// Scoped per tenant. Without it, a worker retrying an analysis after a
    /// lost lease notifies the customer twice about one result.
    pub dedupe_key: String,
    pub correlation_id: Uuid,
}

/// A notification ready to deliver.
#[derive(Debug, Clone)]
pub struct PendingNotification {
    pub id: Uuid,
    pub company_id: CompanyId,
    pub user_id: Option<Uuid>,
    pub kind: String,
    pub subject_id: Option<Uuid>,
    pub channels: Vec<String>,
    pub attempt: i32,
    pub correlation_id: Uuid,
}

impl Tenant<'_> {
    /// Writes a notification into the outbox.
    ///
    /// Call this inside the transaction that makes the notification true. It is
    /// idempotent on `(company, kind, dedupe_key)`, so a retried job produces
    /// one notification rather than one per attempt.
    pub async fn enqueue_notification(&mut self, notification: NewNotification) -> StoreResult<()> {
        let channels: Vec<String> = self
            .channels_for(notification.user_id, notification.kind)
            .await?
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();

        if channels.is_empty() {
            // The customer turned every channel off for this kind. Recorded as
            // `suppressed` rather than dropped, so "we never told them" is
            // answerable later — which is the question that gets asked after a
            // customer says they were not informed.
            sqlx::query(
                "INSERT INTO notifications (
                     company_id, user_id, kind, subject_id, subject_kind,
                     channels, state, dedupe_key, correlation_id)
                 VALUES ($1,$2,$3,$4,$5,'{}','suppressed',$6,$7)
                 ON CONFLICT (company_id, kind, dedupe_key) DO NOTHING",
            )
            .bind(self.company_id().0)
            .bind(notification.user_id)
            .bind(notification.kind.as_str())
            .bind(notification.subject_id)
            .bind(notification.subject_kind)
            .bind(&notification.dedupe_key)
            .bind(notification.correlation_id)
            .execute(&mut *self.tx)
            .await?;
            return Ok(());
        }

        sqlx::query(
            "INSERT INTO notifications (
                 company_id, user_id, kind, subject_id, subject_kind,
                 channels, dedupe_key, correlation_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
             ON CONFLICT (company_id, kind, dedupe_key) DO NOTHING",
        )
        .bind(self.company_id().0)
        .bind(notification.user_id)
        .bind(notification.kind.as_str())
        .bind(notification.subject_id)
        .bind(notification.subject_kind)
        .bind(&channels)
        .bind(&notification.dedupe_key)
        .bind(notification.correlation_id)
        .execute(&mut *self.tx)
        .await?;

        Ok(())
    }

    /// Which channels this notification should go to.
    async fn channels_for(
        &mut self,
        user_id: Option<Uuid>,
        kind: NotificationKind,
    ) -> StoreResult<Vec<Channel>> {
        if kind.is_mandatory() {
            return Ok(kind.default_channels().to_vec());
        }

        let Some(user_id) = user_id else {
            return Ok(kind.default_channels().to_vec());
        };

        let row = sqlx::query(
            "SELECT channels FROM notification_preferences WHERE user_id = $1 AND kind = $2",
        )
        .bind(user_id)
        .bind(kind.as_str())
        .fetch_optional(&mut *self.tx)
        .await?;

        match row {
            // A row with an empty array is a deliberate "none", and is honoured.
            Some(row) => Ok(row
                .get::<Vec<String>, _>("channels")
                .iter()
                .filter_map(|c| Channel::parse(c))
                .collect()),
            None => Ok(kind.default_channels().to_vec()),
        }
    }

    /// The unread in-app notifications for a caller.
    pub async fn recent_notifications(
        &mut self,
        user_id: Option<Uuid>,
        limit: i64,
    ) -> StoreResult<Vec<PendingNotification>> {
        let rows = sqlx::query(
            "SELECT id, company_id, user_id, kind, subject_id, channels, attempt, correlation_id
             FROM notifications
             WHERE (user_id = $1 OR user_id IS NULL)
               AND 'in_app' = ANY(channels)
               AND state <> 'suppressed'
             ORDER BY created_at DESC
             LIMIT $2",
        )
        .bind(user_id)
        .bind(limit.clamp(1, 200))
        .fetch_all(&mut *self.tx)
        .await?;

        Ok(rows.into_iter().map(row_to_pending).collect())
    }
}

fn row_to_pending(r: sqlx::postgres::PgRow) -> PendingNotification {
    PendingNotification {
        id: r.get("id"),
        company_id: CompanyId::from_uuid(r.get("company_id")),
        user_id: r.get("user_id"),
        kind: r.get("kind"),
        subject_id: r.get("subject_id"),
        channels: r.get("channels"),
        attempt: r.get("attempt"),
        correlation_id: r.get("correlation_id"),
    }
}

/// How long to wait before attempt `n`.
///
/// The same shape as the job queue's backoff and for the same reason: a
/// provider outage returns a hundred failures in one second, and without spread
/// they all retry in one second too.
pub fn delivery_backoff(attempt: i32, jitter_seed: u64) -> Duration {
    let base = 30i64.saturating_mul(2i64.saturating_pow(attempt.clamp(0, 6) as u32));
    // Deterministic per notification, so a retry schedule is reproducible when
    // someone is trying to work out what happened.
    let jitter = (jitter_seed % 30) as i64;
    Duration::seconds(base + jitter)
}

impl crate::Store {
    /// Claims notifications that are due.
    ///
    /// `SKIP LOCKED`, like the job queue, so several delivery workers can run
    /// without sending the same notification twice.
    pub async fn claim_notifications(&self, limit: i64) -> StoreResult<Vec<PendingNotification>> {
        let rows = sqlx::query(
            "UPDATE notifications SET state = 'delivering', attempt = attempt + 1,
                 updated_at = now()
             WHERE id IN (
                 SELECT id FROM notifications
                 WHERE state = 'pending' AND run_after <= now()
                 ORDER BY run_after
                 FOR UPDATE SKIP LOCKED
                 LIMIT $1)
             RETURNING id, company_id, user_id, kind, subject_id, channels, attempt,
                       correlation_id",
        )
        .bind(limit.clamp(1, 100))
        .fetch_all(self.pool())
        .await?;

        Ok(rows.into_iter().map(row_to_pending).collect())
    }

    /// Records the outcome of a delivery attempt.
    pub async fn record_delivery(
        &self,
        id: Uuid,
        delivered: &[String],
        error: Option<&str>,
    ) -> StoreResult<()> {
        // Partial delivery is a real outcome: a push may fail while the email
        // succeeds. Recording what actually went out means a retry can send
        // only what is missing rather than sending the email twice.
        sqlx::query(
            "UPDATE notifications
             SET delivered_channels = (
                     SELECT array_agg(DISTINCT c)
                     FROM unnest(delivered_channels || $2::text[]) AS c
                     WHERE c = ANY(channels)),
                 state = CASE
                     WHEN $3::text IS NULL THEN 'delivered'
                     WHEN attempt >= max_attempts THEN 'failed'
                     ELSE 'pending' END,
                 run_after = CASE
                     WHEN $3::text IS NULL THEN run_after
                     ELSE now() + make_interval(secs => 30 * power(2, least(attempt, 6))) END,
                 last_error = $3,
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(delivered)
        .bind(error)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_security_alert_cannot_be_turned_off() {
        // The message an attacker who has taken over an account would disable
        // first. The preference is not offered rather than offered and ignored.
        assert!(NotificationKind::SecurityAlert.is_mandatory());
        for kind in [
            NotificationKind::AnalysisCompleted,
            NotificationKind::AnalysisFailed,
            NotificationKind::DocumentProcessed,
            NotificationKind::MemberInvited,
        ] {
            assert!(!kind.is_mandatory());
        }
    }

    #[test]
    fn routine_progress_does_not_push() {
        // A push for every processed document is how a customer turns push off
        // for everything, including the one that mattered.
        assert_eq!(
            NotificationKind::DocumentProcessed.default_channels(),
            &[Channel::InApp]
        );
    }

    #[test]
    fn the_thing_the_customer_is_waiting_for_reaches_them() {
        let channels = NotificationKind::AnalysisCompleted.default_channels();
        assert!(channels.contains(&Channel::Push));
        assert!(channels.contains(&Channel::Email));
    }

    #[test]
    fn every_kind_has_somewhere_to_go() {
        for kind in [
            NotificationKind::AnalysisCompleted,
            NotificationKind::AnalysisFailed,
            NotificationKind::DocumentProcessed,
            NotificationKind::MemberInvited,
            NotificationKind::SecurityAlert,
        ] {
            assert!(
                !kind.default_channels().is_empty(),
                "{kind:?} would be written and never delivered"
            );
        }
    }

    #[test]
    fn backoff_grows_and_is_spread() {
        assert!(delivery_backoff(3, 0) > delivery_backoff(1, 0));
        // Two notifications failing in the same second do not retry in the
        // same second.
        assert_ne!(delivery_backoff(2, 7), delivery_backoff(2, 19));
    }

    #[test]
    fn backoff_is_capped() {
        // Uncapped doubling reaches years. A notification that would arrive
        // next February should have been given up on.
        assert_eq!(delivery_backoff(6, 0), delivery_backoff(50, 0));
    }

    #[test]
    fn channels_round_trip() {
        for channel in [Channel::Push, Channel::Email, Channel::InApp] {
            assert_eq!(Channel::parse(channel.as_str()), Some(channel));
        }
        assert_eq!(Channel::parse("carrier_pigeon"), None);
    }
}

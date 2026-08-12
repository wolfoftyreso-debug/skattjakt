//! Direct-to-storage uploads.
//!
//! A client asks for a ticket, writes the bytes straight to object storage, and
//! tells the API it is done. The API never handles the bytes.
//!
//! The ticket is what makes that safe. It names exactly one key the client may
//! write, it expires, it records who asked, and on completion the bytes are
//! checked against what the ticket declared — so a ticket for a small text file
//! cannot be redeemed for a large one, and a ticket cannot be used to write
//! anywhere but its own key.

use chrono::{DateTime, Duration, Utc};
use skattjakt_core::{DocumentId, DocumentVersionId};
use sqlx::Row;
use uuid::Uuid;

use crate::{StoreError, StoreResult, Tenant};

/// How long a ticket is good for.
///
/// Long enough to send a large scanned annual report over a poor mobile
/// connection, and no longer: the ticket is a bearer capability to write into
/// the customer's storage.
pub const TICKET_LIFETIME: Duration = Duration::minutes(30);

/// The largest document that may be declared.
///
/// Matches the limit the proxied upload path enforces. Two different limits on
/// two paths to the same storage is how one of them becomes the way in.
pub const MAX_DECLARED_BYTES: i64 = 32 * 1024 * 1024;

/// What a ticket authorises, read back at completion.
#[derive(Debug, Clone)]
pub struct TicketDetails {
    pub storage_key: String,
    pub declared_name: String,
    pub declared_type: String,
}

#[derive(Debug, Clone)]
pub struct UploadTicket {
    pub id: Uuid,
    pub storage_key: String,
    pub expires_at: DateTime<Utc>,
    pub declared_size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionOutcome {
    Accepted {
        document_version_id: DocumentVersionId,
    },
    /// The bytes did not match what the ticket declared.
    ///
    /// Carries both numbers. They are the sizes of the caller's own upload, so
    /// disclosing them tells them nothing they did not already have — and
    /// without them "the size does not match" is a message a client author has
    /// to guess at. The commonest cause is counting characters rather than
    /// bytes, which every Swedish document triggers.
    Rejected {
        reason: &'static str,
        declared: i64,
        observed: i64,
    },
}

impl Tenant<'_> {
    /// Issues a ticket for one upload.
    ///
    /// The storage key is derived here, from identifiers, and is the only key
    /// the ticket can be redeemed against. The client's filename is recorded as
    /// a label and never used to build a path — the same rule that makes path
    /// traversal structurally impossible on the proxied path.
    pub async fn issue_upload_ticket(
        &mut self,
        document_id: DocumentId,
        declared_name: &str,
        declared_type: &str,
        declared_size: i64,
        requested_by: Option<Uuid>,
    ) -> StoreResult<UploadTicket> {
        if declared_size <= 0 || declared_size > MAX_DECLARED_BYTES {
            return Err(StoreError::Invalid(format!(
                "a document must be between 1 byte and {} MB",
                MAX_DECLARED_BYTES / 1024 / 1024
            )));
        }

        let ticket_id = Uuid::new_v4();
        let storage_key = format!(
            "companies/{}/uploads/{}/{}",
            self.company_id().0,
            document_id.0,
            ticket_id
        );
        let expires_at = Utc::now() + TICKET_LIFETIME;
        let truncated: String = declared_name.chars().take(400).collect();

        sqlx::query(
            "INSERT INTO upload_tickets (
                 id, company_id, requested_by, storage_key,
                 declared_name, declared_type, declared_size, expires_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .bind(ticket_id)
        .bind(self.company_id().0)
        .bind(requested_by)
        .bind(&storage_key)
        .bind(&truncated)
        .bind(declared_type)
        .bind(declared_size)
        .bind(expires_at)
        .execute(&mut *self.tx)
        .await?;

        Ok(UploadTicket {
            id: ticket_id,
            storage_key,
            expires_at,
            declared_size,
        })
    }

    /// Redeems a ticket against what actually arrived.
    ///
    /// Called after the client reports the upload finished. The observed size
    /// and hash come from storage, not from the client — asking the client what
    /// it uploaded and believing the answer would make every check here
    /// decorative.
    pub async fn complete_upload_ticket(
        &mut self,
        ticket_id: Uuid,
        observed_size: i64,
        observed_sha256: &str,
        document_version_id: DocumentVersionId,
    ) -> StoreResult<CompletionOutcome> {
        let row = sqlx::query(
            "SELECT declared_size, state, expires_at FROM upload_tickets
             WHERE id = $1 FOR UPDATE",
        )
        .bind(ticket_id)
        .fetch_optional(&mut *self.tx)
        .await?;

        let Some(row) = row else {
            return Err(StoreError::NotFound);
        };

        let state: String = row.get("state");
        if state != "issued" {
            // A ticket is single-use. Redeeming one twice is either a client
            // retry or an attempt to overwrite a stored document with different
            // bytes, and the two are indistinguishable from here.
            return Err(StoreError::Invalid(
                "this upload ticket has already been used".into(),
            ));
        }

        let expires_at: DateTime<Utc> = row.get("expires_at");
        if Utc::now() > expires_at {
            self.reject_ticket(ticket_id, "not_found").await?;
            return Ok(CompletionOutcome::Rejected {
                reason: "the upload ticket expired",
                declared: row.get("declared_size"),
                observed: observed_size,
            });
        }

        let declared: i64 = row.get("declared_size");
        if observed_size != declared {
            // Not pedantry: the declared size is what the size limit was
            // checked against, so a ticket that can be redeemed for more bytes
            // than it declared is a ticket with no size limit.
            self.reject_ticket(ticket_id, "size_mismatch").await?;
            return Ok(CompletionOutcome::Rejected {
                reason: "the uploaded file is not the size the ticket declared",
                declared,
                observed: observed_size,
            });
        }

        sqlx::query(
            "UPDATE upload_tickets
             SET state = 'completed', observed_size = $2, observed_sha256 = $3,
                 document_version_id = $4, completed_at = now()
             WHERE id = $1",
        )
        .bind(ticket_id)
        .bind(observed_size)
        .bind(observed_sha256)
        .bind(document_version_id.0)
        .execute(&mut *self.tx)
        .await?;

        Ok(CompletionOutcome::Accepted {
            document_version_id,
        })
    }

    async fn reject_ticket(&mut self, ticket_id: Uuid, reason: &str) -> StoreResult<()> {
        sqlx::query(
            "UPDATE upload_tickets SET state = 'rejected', rejected_reason = $2 WHERE id = $1",
        )
        .bind(ticket_id)
        .bind(reason)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// What a still-valid ticket authorises: the key, and what the client said
    /// it would send.
    ///
    /// The declared name and type come back with the key because the completion
    /// needs them. Hardcoding a filename there would lose what the customer
    /// called their document, which is the label they will look for in their own
    /// list.
    pub async fn ticket_for_completion(
        &mut self,
        ticket_id: Uuid,
    ) -> StoreResult<Option<TicketDetails>> {
        let row = sqlx::query(
            "SELECT storage_key, declared_name, declared_type FROM upload_tickets
             WHERE id = $1 AND state = 'issued' AND expires_at > now()",
        )
        .bind(ticket_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        Ok(row.map(|r| TicketDetails {
            storage_key: r.get("storage_key"),
            declared_name: r.get("declared_name"),
            declared_type: r.get("declared_type"),
        }))
    }
}

impl crate::Store {
    /// Marks tickets that were never redeemed.
    ///
    /// An issued ticket that expired is not a failure worth alerting on — a
    /// customer changed their mind — but leaving it `issued` for ever would
    /// make the pending count meaningless as an operational signal.
    pub async fn expire_upload_tickets(&self) -> StoreResult<u64> {
        let result = sqlx::query(
            "UPDATE upload_tickets SET state = 'expired'
             WHERE state = 'issued' AND expires_at < now()",
        )
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected())
    }
}

/// Which content types may be uploaded.
///
/// An allowlist, not a denylist. A denylist is a list of the formats someone
/// thought of, and the interesting one is always the format nobody thought of.
pub fn is_supported_type(mime: &str) -> bool {
    matches!(
        mime,
        "application/pdf" | "text/plain" | "text/csv" | "image/jpeg" | "image/png"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_size_limit_matches_the_proxied_path() {
        // Two different limits on two paths into the same storage is how one of
        // them becomes the way in.
        assert_eq!(MAX_DECLARED_BYTES, 32 * 1024 * 1024);
    }

    #[test]
    fn a_ticket_is_short_lived() {
        assert!(TICKET_LIFETIME <= Duration::hours(1));
        // Long enough for a large scan on a poor connection.
        assert!(TICKET_LIFETIME >= Duration::minutes(15));
    }

    #[test]
    fn the_supported_types_are_an_allowlist() {
        assert!(is_supported_type("application/pdf"));
        assert!(is_supported_type("text/plain"));
        // The interesting format is always the one nobody thought of.
        assert!(!is_supported_type("application/x-sh"));
        assert!(!is_supported_type("text/html"));
        assert!(!is_supported_type("application/octet-stream"));
    }
}

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
/// # Two limits, and why they are different numbers
///
/// This one is about **storage**: a customer with a 5 GB export from their
/// bookkeeping system should be able to hand it over, and the ticket flow puts
/// the bytes straight into object storage without the API ever holding them.
///
/// It is deliberately not the same as `ExtractionBudget::DEFAULT_MAX_BYTES`,
/// which is about **reading**: nothing in this system can hold 5 GB in memory —
/// a WebAssembly module is capped at a 4 GiB address space by the target and a
/// serverless function well below that — so the extractor reads a bounded
/// prefix and records that it did.
///
/// Storing more than can be read is not a contradiction. The file is kept,
/// hashed and available; the analysis says how much of it it rested on.
pub const MAX_DECLARED_BYTES: i64 = 5 * 1024 * 1024 * 1024;

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
                "a document must be between 1 byte and {} GB",
                MAX_DECLARED_BYTES / 1024 / 1024 / 1024
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
/// # Why this now accepts everything
///
/// It was an allowlist of five, on the argument that a denylist is a list of
/// the formats someone thought of. That argument is right about *parsing* and
/// wrong about *receiving*, and the two were tangled together.
///
/// A customer with a folder of material does not know which parts we can read.
/// Refusing the folder teaches them nothing; taking it and saying which parts
/// were readable is exactly what they wanted to know. So every type is stored,
/// hashed and recorded, and `MimeType::sniff` decides from the bytes what it
/// actually is — the declared type is a claim either way.
///
/// The safety that the allowlist was providing has moved to where it belongs:
/// nothing is executed, archives are listed rather than inflated, inflation is
/// capped on its output, and a type with no extractor produces a document that
/// states what it was and why it was not read.
pub fn is_supported_type(_mime: &str) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_storage_limit_is_five_gigabytes_and_the_reading_limit_is_not() {
        // Two numbers, deliberately different, and the test says why so the
        // next person does not "fix" one to match the other.
        //
        // Storage: a customer with a large export should be able to hand it
        // over, and the ticket flow puts the bytes straight into object storage
        // without the API ever holding them.
        assert_eq!(MAX_DECLARED_BYTES, 5 * 1024 * 1024 * 1024);

        // Reading: nothing in this system can hold that. A WebAssembly module
        // is capped at a 4 GiB address space by the target; a serverless
        // function sits well below it. The extractor reads a bounded prefix and
        // records that it did.
        assert!(
            (skattjakt_extract::ExtractionBudget::DEFAULT_MAX_BYTES as i64) < MAX_DECLARED_BYTES,
            "the reading budget must stay below the storage limit; storing more \
             than can be read is the design, not a mistake"
        );
    }

    #[test]
    fn a_ticket_is_short_lived() {
        assert!(TICKET_LIFETIME <= Duration::hours(1));
        // Long enough for a large scan on a poor connection.
        assert!(TICKET_LIFETIME >= Duration::minutes(15));
    }

    #[test]
    fn every_type_may_be_uploaded_and_the_bytes_decide_what_it_is() {
        // This was an allowlist of five, on the argument that a denylist is a
        // list of the formats someone thought of. The argument is right about
        // *parsing* and wrong about *receiving*, and the two were tangled.
        //
        // A customer with a folder of material does not know which parts we can
        // read. Refusing the folder teaches them nothing.
        for declared in [
            "application/pdf",
            "text/plain",
            "application/x-sh",
            "text/html",
            "application/octet-stream",
            "",
        ] {
            assert!(is_supported_type(declared), "{declared} was refused");
        }

        // The safety moved to where it belongs: the declared type decides
        // nothing, and a type with no extractor produces a document that states
        // what it was rather than an analysis of nothing.
        use skattjakt_core::document::MimeType;
        let script = MimeType::sniff(b"#!/bin/sh\nrm -rf /", Some("bokslut.pdf"));
        assert!(!script.extracts_text() || script == MimeType::PlainText);
        let jpeg = MimeType::sniff(b"\xff\xd8\xff\xe0", Some("bokslut.pdf"));
        assert!(!jpeg.extracts_text());
        assert!(jpeg.why_unreadable().is_some());
    }
}

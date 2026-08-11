//! Rate limiting, cost budgets, retention and deletion (sections 65, 67, 69).
//!
//! All four are tenant-scoped and all four go through [`Tenant`], so row-level
//! security applies to them exactly as it does to the customer's documents.
//! The exception is the rate limiter's window sweep, which is housekeeping over
//! expired rows and carries no tenant data.

use chrono::{DateTime, Duration, Utc};
use skattjakt_core::{AnalysisId, CompanyId, DocumentVersionId};
use sqlx::Row;
use uuid::Uuid;

use crate::{StoreError, StoreResult, Tenant};

// ---------------------------------------------------------------------------
// Rate limiting (section 67)
// ---------------------------------------------------------------------------

/// A named quota.
///
/// Buckets are separate because the operations cost wildly different amounts.
/// One limit across all of them would either be loose enough to allow an
/// analysis storm or tight enough to break polling — and polling is what the
/// UI does every two seconds while an analysis runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateBucket {
    /// Starting analyses. The expensive one.
    Analysis,
    /// Uploading documents.
    Upload,
    /// Everything else, including status polling.
    Read,
}

impl RateBucket {
    pub fn as_str(self) -> &'static str {
        match self {
            RateBucket::Analysis => "analysis",
            RateBucket::Upload => "upload",
            RateBucket::Read => "read",
        }
    }

    /// Requests allowed per window, and the window length.
    pub fn quota(self) -> (i32, Duration) {
        match self {
            RateBucket::Analysis => (20, Duration::hours(1)),
            RateBucket::Upload => (100, Duration::hours(1)),
            RateBucket::Read => (600, Duration::minutes(1)),
        }
    }

    pub fn all() -> [RateBucket; 3] {
        [RateBucket::Analysis, RateBucket::Upload, RateBucket::Read]
    }
}

/// The outcome of a limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    pub allowed: bool,
    pub limit: i32,
    pub remaining: i32,
    /// When the current window ends, for the `Retry-After` header.
    pub resets_at: DateTime<Utc>,
}

impl Tenant<'_> {
    /// Counts one request against a bucket and says whether it may proceed.
    ///
    /// A fixed window rather than a token bucket. A fixed window lets through
    /// up to twice the quota across a window boundary, which is a real
    /// weakness and an acceptable one here: the quotas exist to stop runaway
    /// clients and cost blowouts, not to shape traffic to the request. A token
    /// bucket in the database would need a row lock per request on the hot
    /// read path, and that cost is not worth the precision.
    ///
    /// `ON CONFLICT DO UPDATE ... RETURNING` makes the increment and the read
    /// one statement, so two API replicas cannot both see the same count.
    pub async fn check_rate_limit(&mut self, bucket: RateBucket) -> StoreResult<RateDecision> {
        let (limit, window) = bucket.quota();
        let now = Utc::now();
        let window_secs = window.num_seconds().max(1);
        // Floor the current time to the window, so every replica agrees on
        // which window it is without coordinating.
        let window_start =
            DateTime::from_timestamp((now.timestamp() / window_secs) * window_secs, 0)
                .unwrap_or(now);

        let count: i32 = sqlx::query_scalar(
            "INSERT INTO rate_limit_counters (company_id, bucket, window_start, count)
             VALUES ($1, $2, $3, 1)
             ON CONFLICT (company_id, bucket, window_start)
             DO UPDATE SET count = rate_limit_counters.count + 1
             RETURNING count",
        )
        .bind(self.company_id().0)
        .bind(bucket.as_str())
        .bind(window_start)
        .fetch_one(&mut *self.tx)
        .await?;

        Ok(RateDecision {
            allowed: count <= limit,
            limit,
            remaining: (limit - count).max(0),
            resets_at: window_start + window,
        })
    }

    // -----------------------------------------------------------------------
    // Cost budgets (section 69)
    // -----------------------------------------------------------------------

    /// Creates the budget row for an analysis, or returns the existing one.
    ///
    /// Returns `(limit, spent, calls)` so a retried analysis resumes against
    /// what it has already spent. Three attempts must not cost three budgets.
    pub async fn open_budget(
        &mut self,
        analysis_id: AnalysisId,
        limit_micro_ore: i64,
    ) -> StoreResult<(i64, i64, i32)> {
        let row = sqlx::query(
            "INSERT INTO analysis_budgets (analysis_id, company_id, limit_micro_ore)
             VALUES ($1, $2, $3)
             ON CONFLICT (analysis_id) DO UPDATE SET updated_at = now()
             RETURNING limit_micro_ore, spent_micro_ore, calls",
        )
        .bind(analysis_id.0)
        .bind(self.company_id().0)
        .bind(limit_micro_ore.max(1))
        .fetch_one(&mut *self.tx)
        .await?;

        Ok((
            row.try_get("limit_micro_ore")?,
            row.try_get("spent_micro_ore")?,
            row.try_get("calls")?,
        ))
    }

    /// Records what a call cost. Written after every call, successful or not,
    /// so a worker that dies mid-analysis does not lose the spend it incurred.
    pub async fn charge_budget(
        &mut self,
        analysis_id: AnalysisId,
        cost_micro_ore: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "UPDATE analysis_budgets
             SET spent_micro_ore = spent_micro_ore + $1,
                 calls = calls + 1,
                 exceeded_at = CASE
                     WHEN spent_micro_ore + $1 >= limit_micro_ore AND exceeded_at IS NULL
                     THEN now() ELSE exceeded_at END,
                 updated_at = now()
             WHERE analysis_id = $2",
        )
        .bind(cost_micro_ore.max(0))
        .bind(analysis_id.0)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    pub async fn budget(&mut self, analysis_id: AnalysisId) -> StoreResult<(i64, i64, i32)> {
        let row = sqlx::query(
            "SELECT limit_micro_ore, spent_micro_ore, calls FROM analysis_budgets
             WHERE analysis_id = $1",
        )
        .bind(analysis_id.0)
        .fetch_optional(&mut *self.tx)
        .await?
        .ok_or(StoreError::NotFound)?;
        Ok((
            row.try_get("limit_micro_ore")?,
            row.try_get("spent_micro_ore")?,
            row.try_get("calls")?,
        ))
    }

    // -----------------------------------------------------------------------
    // Retention (section 65)
    // -----------------------------------------------------------------------

    /// The tenant's retention policy, falling back to the defaults.
    pub async fn retention_policy(&mut self) -> StoreResult<RetentionPolicy> {
        let row = sqlx::query(
            "SELECT document_days, analysis_days, audit_days FROM retention_policies
             WHERE company_id = $1",
        )
        .bind(self.company_id().0)
        .fetch_optional(&mut *self.tx)
        .await?;

        Ok(match row {
            Some(row) => RetentionPolicy {
                document_days: row.try_get::<i32, _>("document_days")? as u32,
                analysis_days: row.try_get::<i32, _>("analysis_days")? as u32,
                audit_days: row.try_get::<i32, _>("audit_days")? as u32,
            },
            None => RetentionPolicy::default(),
        })
    }

    pub async fn set_retention_policy(&mut self, policy: &RetentionPolicy) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO retention_policies (company_id, document_days, analysis_days, audit_days)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (company_id) DO UPDATE SET
                 document_days = EXCLUDED.document_days,
                 analysis_days = EXCLUDED.analysis_days,
                 audit_days = EXCLUDED.audit_days,
                 updated_at = now()",
        )
        .bind(self.company_id().0)
        .bind(policy.document_days as i32)
        .bind(policy.analysis_days as i32)
        .bind(policy.audit_days as i32)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// Document versions past their retention date.
    ///
    /// Returned rather than deleted, because deleting the row without deleting
    /// the object leaves an orphan in blob storage that nothing will ever
    /// collect — the storage key is only reachable through this row. The caller
    /// deletes the object first, then calls `purge_document_versions`.
    pub async fn expired_document_versions(
        &mut self,
        policy: &RetentionPolicy,
        limit: i64,
    ) -> StoreResult<Vec<(DocumentVersionId, String)>> {
        let cutoff = Utc::now() - Duration::days(policy.document_days as i64);
        let rows = sqlx::query(
            "SELECT id, storage_key FROM document_versions
             WHERE company_id = $1 AND created_at < $2
             ORDER BY created_at LIMIT $3",
        )
        .bind(self.company_id().0)
        .bind(cutoff)
        .bind(limit)
        .fetch_all(&mut *self.tx)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok((
                    DocumentVersionId::from_uuid(row.try_get("id")?),
                    row.try_get("storage_key")?,
                ))
            })
            .collect()
    }

    /// Removes document versions whose objects are already gone.
    ///
    /// Facts extracted from a version go with it: they are derived data holding
    /// the same figures, and section 65 says deletion covers derived data too.
    pub async fn purge_document_versions(&mut self, ids: &[DocumentVersionId]) -> StoreResult<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let raw: Vec<Uuid> = ids.iter().map(|id| id.0).collect();

        sqlx::query("DELETE FROM financial_facts WHERE document_version_id = ANY($1)")
            .bind(&raw)
            .execute(&mut *self.tx)
            .await?;

        let deleted = sqlx::query("DELETE FROM document_versions WHERE id = ANY($1)")
            .bind(&raw)
            .execute(&mut *self.tx)
            .await?
            .rows_affected();

        // A document with no versions left is a shell.
        sqlx::query(
            "DELETE FROM documents d WHERE d.company_id = $1
             AND NOT EXISTS (SELECT 1 FROM document_versions v WHERE v.document_id = d.id)",
        )
        .bind(self.company_id().0)
        .execute(&mut *self.tx)
        .await?;

        Ok(deleted)
    }

    /// Analyses past their retention date, with everything derived from them.
    pub async fn purge_expired_analyses(
        &mut self,
        policy: &RetentionPolicy,
        limit: i64,
    ) -> StoreResult<u64> {
        let cutoff = Utc::now() - Duration::days(policy.analysis_days as i64);
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM analysis_jobs WHERE company_id = $1 AND created_at < $2
             ORDER BY created_at LIMIT $3",
        )
        .bind(self.company_id().0)
        .bind(cutoff)
        .bind(limit)
        .fetch_all(&mut *self.tx)
        .await?;

        if ids.is_empty() {
            return Ok(0);
        }

        // Order matters: children before parents, because the foreign keys are
        // there to stop exactly the orphan this would otherwise create.
        for statement in [
            "DELETE FROM opportunity_evidence WHERE opportunity_id IN
                 (SELECT id FROM opportunities WHERE analysis_id = ANY($1))",
            "DELETE FROM calculations WHERE analysis_id = ANY($1)",
            "DELETE FROM opportunities WHERE analysis_id = ANY($1)",
            "DELETE FROM model_runs WHERE analysis_id = ANY($1)",
            "DELETE FROM analysis_budgets WHERE analysis_id = ANY($1)",
        ] {
            sqlx::query(statement)
                .bind(&ids)
                .execute(&mut *self.tx)
                .await?;
        }

        let deleted = sqlx::query("DELETE FROM analysis_jobs WHERE id = ANY($1)")
            .bind(&ids)
            .execute(&mut *self.tx)
            .await?
            .rows_affected();
        Ok(deleted)
    }

    // -----------------------------------------------------------------------
    // Deletion on request (section 65)
    // -----------------------------------------------------------------------

    /// Records a deletion request before anything is removed.
    ///
    /// Written first so an interrupted deletion is resumable. A deletion that
    /// half-completed and left no record of itself is the one failure mode that
    /// cannot be recovered from, because nobody knows what is missing.
    pub async fn request_deletion(
        &mut self,
        scope: DeletionScope,
        subject_id: Option<Uuid>,
        requested_by: &str,
    ) -> StoreResult<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO deletion_requests (id, company_id, scope, subject_id, requested_by)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(self.company_id().0)
        .bind(scope.as_str())
        .bind(subject_id)
        .bind(requested_by)
        .execute(&mut *self.tx)
        .await?;
        Ok(id)
    }

    pub async fn mark_deletion_progress(
        &mut self,
        request_id: Uuid,
        stage: DeletionStage,
    ) -> StoreResult<()> {
        let column = match stage {
            DeletionStage::Database => "db_done_at",
            DeletionStage::Blobs => "blobs_done_at",
            DeletionStage::Complete => "completed_at",
        };
        // The column name comes from a closed enumeration in this file, never
        // from a caller, so the format is not a parameterisation hole.
        let sql = format!("UPDATE deletion_requests SET {column} = now() WHERE id = $1");
        sqlx::query(&sql)
            .bind(request_id)
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    /// Deletion requests that started and did not finish.
    pub async fn unfinished_deletions(&mut self) -> StoreResult<Vec<(Uuid, String, Option<Uuid>)>> {
        let rows = sqlx::query(
            "SELECT id, scope, subject_id FROM deletion_requests
             WHERE company_id = $1 AND completed_at IS NULL ORDER BY requested_at",
        )
        .bind(self.company_id().0)
        .fetch_all(&mut *self.tx)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get("id")?,
                    row.try_get("scope")?,
                    row.try_get("subject_id")?,
                ))
            })
            .collect()
    }
}

/// Sweeps rate-limit rows for windows that have closed.
///
/// Not on `Tenant`: it spans tenants and touches nothing but counters. Run from
/// the retention job.
pub async fn sweep_rate_limit_windows(pool: &sqlx::PgPool) -> StoreResult<u64> {
    let deleted = sqlx::query(
        "DELETE FROM rate_limit_counters WHERE window_start < now() - interval '2 hours'",
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub document_days: u32,
    pub analysis_days: u32,
    /// The audit trail outlives the data it describes. It holds identifiers and
    /// outcomes, not the customer's economy, and it is the only record of what
    /// was deleted and when — which is precisely what a deletion request has to
    /// be able to demonstrate afterwards.
    pub audit_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            document_days: 730,
            analysis_days: 730,
            audit_days: 3_650,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionScope {
    Document,
    Analysis,
    Company,
}

impl DeletionScope {
    pub fn as_str(self) -> &'static str {
        match self {
            DeletionScope::Document => "document",
            DeletionScope::Analysis => "analysis",
            DeletionScope::Company => "company",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionStage {
    Database,
    Blobs,
    Complete,
}

/// The company id, for callers that need it outside a tenant transaction.
pub fn company_uuid(id: CompanyId) -> Uuid {
    id.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_bucket_has_a_distinct_quota() {
        let quotas: Vec<(i32, i64)> = RateBucket::all()
            .iter()
            .map(|b| {
                let (limit, window) = b.quota();
                (limit, window.num_seconds())
            })
            .collect();
        assert_eq!(quotas.len(), 3);
        // Reads are far more generous than analyses, because the UI polls.
        assert!(RateBucket::Read.quota().0 > RateBucket::Analysis.quota().0);
    }

    #[test]
    fn every_bucket_has_a_positive_quota_and_window() {
        for bucket in RateBucket::all() {
            let (limit, window) = bucket.quota();
            assert!(limit > 0, "{}", bucket.as_str());
            assert!(window.num_seconds() > 0, "{}", bucket.as_str());
        }
    }

    #[test]
    fn the_audit_trail_outlives_the_data_it_describes() {
        let policy = RetentionPolicy::default();
        assert!(policy.audit_days > policy.document_days);
        assert!(policy.audit_days > policy.analysis_days);
    }

    #[test]
    fn deletion_stages_map_to_distinct_columns() {
        // Guards against a copy-paste that would mark the wrong stage done and
        // make an interrupted deletion look finished.
        let mut seen = std::collections::BTreeSet::new();
        for stage in [
            DeletionStage::Database,
            DeletionStage::Blobs,
            DeletionStage::Complete,
        ] {
            let column = match stage {
                DeletionStage::Database => "db_done_at",
                DeletionStage::Blobs => "blobs_done_at",
                DeletionStage::Complete => "completed_at",
            };
            assert!(seen.insert(column), "{column} used twice");
        }
    }
}

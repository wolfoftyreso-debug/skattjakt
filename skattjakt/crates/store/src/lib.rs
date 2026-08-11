//! # skattjakt-store
//!
//! Persistence. Tenant isolation is Postgres's job (row-level security); this
//! crate's job is to make sure the tenant is always set, and to make it awkward
//! to run a query without one.
//!
//! Every tenant-scoped operation goes through [`Tenant`], which is a
//! transaction with `skattjakt.company_id` already applied. There is no way to
//! reach a tenant table from here without going through it.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod blob;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::Value;
use skattjakt_core::analysis::{AnalysisResult, AnalysisStage, AnalysisStatus};
use skattjakt_core::document::{AccountsState, DocumentKind, DocumentVersion, MimeType};
use skattjakt_core::{
    AnalysisId, CompanyId, CompanyProfile, DocumentId, DocumentVersionId, FiscalYear, FinancialFact,
    Opportunity, OrgNumber,
};
use skattjakt_model::ModelRunRecord;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Postgres, Row, Transaction};
use thiserror::Error;

pub use blob::{BlobError, BlobStore, FilesystemBlobStore};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("not found")]
    NotFound,

    #[error("stored data could not be read back: {0}")]
    Corrupt(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub async fn connect(url: &str) -> StoreResult<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(
                std::env::var("SKATTJAKT_DB_MAX_CONNECTIONS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10),
            )
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Applies the migrations. Intended to run as the owning role — the
    /// application role deliberately cannot create tables.
    pub async fn migrate(&self) -> StoreResult<()> {
        sqlx::migrate!("../../migrations").run(&self.pool).await?;
        Ok(())
    }

    pub async fn ping(&self) -> StoreResult<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Resolves a bearer token to the company it belongs to.
    ///
    /// This is the one query that runs before a tenant is set — it is what
    /// establishes the tenant. Lookup is by the token's SHA-256, so the stored
    /// value is not itself a credential.
    pub async fn authenticate(&self, token: &str) -> StoreResult<Option<CompanyId>> {
        let hash = skattjakt_core::document::sha256_hex(token.as_bytes());
        let row = sqlx::query(
            "UPDATE api_tokens SET last_used_at = now()
             WHERE token_hash = $1 AND revoked_at IS NULL
             RETURNING company_id",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| CompanyId::from_uuid(r.get("company_id"))))
    }

    /// Creates a company and issues its first token.
    ///
    /// The company's own id is set as the tenant *before* the insert, so the
    /// row-level security policy is satisfied without any bypass: the caller
    /// is, for the length of this transaction, the company being created.
    pub async fn create_company(
        &self,
        profile: &CompanyProfile,
        token: &str,
        token_label: &str,
    ) -> StoreResult<()> {
        let mut tx = self.pool.begin().await?;
        set_tenant(&mut tx, profile.id).await?;

        sqlx::query(
            "INSERT INTO companies (id, name, org_number, fiscal_year_start, fiscal_year_end, profile)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(profile.id.0)
        .bind(&profile.name)
        .bind(profile.org_number.as_digits())
        .bind(profile.fiscal_year.start)
        .bind(profile.fiscal_year.end)
        .bind(serde_json::to_value(profile).unwrap_or(Value::Null))
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO api_tokens (company_id, token_hash, label) VALUES ($1, $2, $3)",
        )
        .bind(profile.id.0)
        .bind(skattjakt_core::document::sha256_hex(token.as_bytes()))
        .bind(token_label)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO audit_events (company_id, actor, event_type, subject_id, detail)
             VALUES ($1, $2, 'company.created', $1, $3)",
        )
        .bind(profile.id.0)
        .bind("admin")
        .bind(serde_json::json!({"name": profile.name}))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Opens a transaction scoped to one company.
    pub async fn tenant(&self, company_id: CompanyId) -> StoreResult<Tenant<'_>> {
        let mut tx = self.pool.begin().await?;
        set_tenant(&mut tx, company_id).await?;
        Ok(Tenant { tx, company_id })
    }
}

/// Applies the tenant for the rest of the transaction. `set_config(..., true)`
/// is the parameterised equivalent of `SET LOCAL`, so the value never has to be
/// interpolated into SQL.
async fn set_tenant(tx: &mut Transaction<'_, Postgres>, company_id: CompanyId) -> StoreResult<()> {
    sqlx::query("SELECT set_config('skattjakt.company_id', $1, true)")
        .bind(company_id.0.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A transaction with the tenant applied.
#[derive(Debug)]
pub struct Tenant<'a> {
    tx: Transaction<'a, Postgres>,
    company_id: CompanyId,
}

impl Tenant<'_> {
    pub fn company_id(&self) -> CompanyId {
        self.company_id
    }

    pub async fn commit(self) -> StoreResult<()> {
        self.tx.commit().await?;
        Ok(())
    }

    // -- companies ----------------------------------------------------------

    pub async fn company(&mut self) -> StoreResult<CompanyProfile> {
        let row = sqlx::query("SELECT profile, name, org_number, fiscal_year_start, fiscal_year_end FROM companies WHERE id = $1")
            .bind(self.company_id.0)
            .fetch_optional(&mut *self.tx)
            .await?
            .ok_or(StoreError::NotFound)?;

        // The profile column is the canonical form; the columns beside it exist
        // for indexing and constraints and are used as a fallback.
        let profile: Value = row.get("profile");
        if let Ok(parsed) = serde_json::from_value::<CompanyProfile>(profile) {
            return Ok(parsed);
        }

        let org_number = OrgNumber::parse(row.get::<String, _>("org_number").as_str())
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
        let fiscal_year = FiscalYear::new(row.get("fiscal_year_start"), row.get("fiscal_year_end"))
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;

        Ok(CompanyProfile {
            id: self.company_id,
            name: row.get("name"),
            org_number,
            fiscal_year,
            industry: None,
            sni_code: None,
            employee_count: None,
            owner_count: None,
            in_group: None,
            operations_outside_sweden: None,
            does_development_work: None,
            owns_premises: None,
            has_vehicles: None,
            owners_active_in_company: None,
        })
    }

    pub async fn update_company_profile(&mut self, profile: &CompanyProfile) -> StoreResult<()> {
        sqlx::query(
            "UPDATE companies SET profile = $2, name = $3, fiscal_year_start = $4,
                    fiscal_year_end = $5, updated_at = now()
             WHERE id = $1",
        )
        .bind(self.company_id.0)
        .bind(serde_json::to_value(profile).unwrap_or(Value::Null))
        .bind(&profile.name)
        .bind(profile.fiscal_year.start)
        .bind(profile.fiscal_year.end)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    // -- documents ----------------------------------------------------------

    /// Records an uploaded document and its first version.
    ///
    /// The caller writes the bytes to blob storage under the returned
    /// `storage_key`. The key is derived from the tenant, the document and the
    /// content hash, never from the client's filename.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_document(
        &mut self,
        kind: DocumentKind,
        original_filename: &str,
        mime_type: MimeType,
        bytes: &[u8],
        page_count: Option<i32>,
        accounts_state: AccountsState,
    ) -> StoreResult<DocumentVersion> {
        let document_id = DocumentId::new();
        let version_id = DocumentVersionId::new();
        let sha256 = skattjakt_core::document::sha256_hex(bytes);
        let storage_key =
            DocumentVersion::build_storage_key(self.company_id, document_id, 1, &sha256);

        sqlx::query(
            "INSERT INTO documents (id, company_id, kind, original_filename)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(document_id.0)
        .bind(self.company_id.0)
        .bind(serde_json::to_value(kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "unknown".into()))
        .bind(original_filename)
        .execute(&mut *self.tx)
        .await?;

        sqlx::query(
            "INSERT INTO document_versions
               (id, document_id, company_id, version, mime_type, byte_size, sha256,
                storage_key, page_count, accounts_state)
             VALUES ($1, $2, $3, 1, $4, $5, $6, $7, $8, $9)",
        )
        .bind(version_id.0)
        .bind(document_id.0)
        .bind(self.company_id.0)
        .bind(mime_type.as_content_type())
        .bind(bytes.len() as i64)
        .bind(&sha256)
        .bind(&storage_key)
        .bind(page_count)
        .bind(match accounts_state {
            AccountsState::Preliminary => "preliminary",
            AccountsState::Final => "final",
            AccountsState::Unknown => "unknown",
        })
        .execute(&mut *self.tx)
        .await?;

        self.audit("document.uploaded", Some(version_id.0), serde_json::json!({
            // Metadata only. The filename is the user's own text and the hash
            // identifies the bytes; no document content goes into the audit log.
            "filename": original_filename,
            "sha256": sha256,
            "byte_size": bytes.len(),
        }))
        .await?;

        Ok(DocumentVersion {
            id: version_id,
            document_id,
            company_id: self.company_id,
            version: 1,
            mime_type,
            byte_size: bytes.len() as i64,
            sha256,
            storage_key,
            page_count,
            accounts_state,
            uploaded_at: Utc::now(),
        })
    }

    pub async fn document_version(
        &mut self,
        id: DocumentVersionId,
    ) -> StoreResult<DocumentVersion> {
        let row = sqlx::query(
            "SELECT id, document_id, version, mime_type, byte_size, sha256, storage_key,
                    page_count, accounts_state, uploaded_at
             FROM document_versions WHERE id = $1",
        )
        .bind(id.0)
        .fetch_optional(&mut *self.tx)
        .await?
        .ok_or(StoreError::NotFound)?;

        let mime_type = MimeType::from_content_type(row.get::<String, _>("mime_type").as_str())
            .ok_or_else(|| StoreError::Corrupt("unknown stored mime type".into()))?;

        Ok(DocumentVersion {
            id,
            document_id: DocumentId::from_uuid(row.get("document_id")),
            company_id: self.company_id,
            version: row.get("version"),
            mime_type,
            byte_size: row.get("byte_size"),
            sha256: row.get("sha256"),
            storage_key: row.get("storage_key"),
            page_count: row.get("page_count"),
            accounts_state: match row.get::<String, _>("accounts_state").as_str() {
                "final" => AccountsState::Final,
                "preliminary" => AccountsState::Preliminary,
                _ => AccountsState::Unknown,
            },
            uploaded_at: row.get("uploaded_at"),
        })
    }

    pub async fn list_document_versions(&mut self) -> StoreResult<Vec<(DocumentVersionId, String, String)>> {
        let rows = sqlx::query(
            "SELECT v.id, d.original_filename, v.sha256
             FROM document_versions v JOIN documents d ON d.id = v.document_id
             ORDER BY v.uploaded_at DESC",
        )
        .fetch_all(&mut *self.tx)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    DocumentVersionId::from_uuid(r.get("id")),
                    r.get("original_filename"),
                    r.get("sha256"),
                )
            })
            .collect())
    }

    // -- facts --------------------------------------------------------------

    pub async fn insert_facts(&mut self, facts: &[FinancialFact]) -> StoreResult<()> {
        for fact in facts {
            sqlx::query(
                "INSERT INTO financial_facts
                   (id, company_id, document_version_id, period_start, period_end, kind,
                    value_ore, currency, account, source_page, source_text, extraction_confidence)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(fact.id.0)
            .bind(self.company_id.0)
            .bind(fact.document_version_id.0)
            .bind(fact.period.start)
            .bind(fact.period.end)
            .bind(fact.kind.key())
            .bind(fact.value.ore())
            .bind(&fact.currency)
            .bind(fact.account.as_deref())
            .bind(fact.source_page.map(|p| p as i32))
            .bind(fact.source_text.as_deref())
            .bind(fact.extraction_confidence.get() as f32)
            .execute(&mut *self.tx)
            .await?;
        }
        Ok(())
    }

    // -- analyses -----------------------------------------------------------

    pub async fn create_analysis(
        &mut self,
        id: AnalysisId,
        document_version_ids: &[DocumentVersionId],
        rule_set_version: &str,
    ) -> StoreResult<()> {
        let ids: Vec<uuid::Uuid> = document_version_ids.iter().map(|d| d.0).collect();
        sqlx::query(
            "INSERT INTO analysis_jobs (id, company_id, document_version_ids, status, stage, rule_set_version)
             VALUES ($1, $2, $3, 'pending', 'queued', $4)",
        )
        .bind(id.0)
        .bind(self.company_id.0)
        .bind(&ids)
        .bind(rule_set_version)
        .execute(&mut *self.tx)
        .await?;

        self.audit("analysis.created", Some(id.0), serde_json::json!({
            "document_versions": ids.len(),
            "rule_set_version": rule_set_version,
        }))
        .await
    }

    pub async fn set_stage(&mut self, id: AnalysisId, stage: AnalysisStage) -> StoreResult<()> {
        sqlx::query(
            "UPDATE analysis_jobs
             SET stage = $2,
                 status = CASE WHEN status = 'pending' THEN 'running' ELSE status END,
                 started_at = COALESCE(started_at, now())
             WHERE id = $1",
        )
        .bind(id.0)
        .bind(stage_key(stage))
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// Records a completed analysis: the result document, every opportunity
    /// with its evidence and calculation, and the model runs.
    pub async fn complete_analysis(
        &mut self,
        result: &AnalysisResult,
        model_runs: &[ModelRunRecord],
    ) -> StoreResult<()> {
        sqlx::query(
            "UPDATE analysis_jobs
             SET status = 'succeeded', stage = 'done', result = $2, finished_at = now()
             WHERE id = $1",
        )
        .bind(result.analysis_id.0)
        .bind(serde_json::to_value(result).unwrap_or(Value::Null))
        .execute(&mut *self.tx)
        .await?;

        for opportunity in result.opportunities.iter().chain(result.rejected.iter()) {
            self.insert_opportunity(result.analysis_id, opportunity).await?;
        }

        for run in model_runs {
            self.insert_model_run(run).await?;
        }

        self.audit("analysis.completed", Some(result.analysis_id.0), serde_json::json!({
            "opportunities": result.opportunities.len(),
            "rejected": result.rejected.len(),
            "warnings": result.warnings.len(),
        }))
        .await
    }

    pub async fn fail_analysis(&mut self, id: AnalysisId, error: &str) -> StoreResult<()> {
        sqlx::query(
            "UPDATE analysis_jobs SET status = 'failed', error = $2, finished_at = now() WHERE id = $1",
        )
        .bind(id.0)
        .bind(error)
        .execute(&mut *self.tx)
        .await?;

        self.audit("analysis.failed", Some(id.0), serde_json::json!({"error": error}))
            .await
    }

    pub async fn analysis(&mut self, id: AnalysisId) -> StoreResult<StoredAnalysis> {
        let row = sqlx::query(
            "SELECT id, status, stage, rule_set_version, result, error,
                    created_at, started_at, finished_at
             FROM analysis_jobs WHERE id = $1",
        )
        .bind(id.0)
        .fetch_optional(&mut *self.tx)
        .await?
        .ok_or(StoreError::NotFound)?;

        let result: Option<Value> = row.get("result");
        Ok(StoredAnalysis {
            id,
            status: match row.get::<String, _>("status").as_str() {
                "running" => AnalysisStatus::Running,
                "succeeded" => AnalysisStatus::Succeeded,
                "failed" => AnalysisStatus::Failed,
                _ => AnalysisStatus::Pending,
            },
            stage: stage_from_key(row.get::<String, _>("stage").as_str()),
            rule_set_version: row.get("rule_set_version"),
            result: result
                .filter(|v| !v.is_null())
                .and_then(|v| serde_json::from_value(v).ok()),
            error: row.get("error"),
            created_at: row.get("created_at"),
            finished_at: row.get("finished_at"),
        })
    }

    pub async fn list_analyses(&mut self) -> StoreResult<Vec<(AnalysisId, String, DateTime<Utc>)>> {
        let rows = sqlx::query(
            "SELECT id, status, created_at FROM analysis_jobs ORDER BY created_at DESC LIMIT 100",
        )
        .fetch_all(&mut *self.tx)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    AnalysisId::from_uuid(r.get("id")),
                    r.get("status"),
                    r.get("created_at"),
                )
            })
            .collect())
    }

    async fn insert_opportunity(
        &mut self,
        analysis_id: AnalysisId,
        opportunity: &Opportunity,
    ) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO opportunities
               (id, analysis_id, company_id, category, status, title, rationale,
                impact_low_ore, impact_high_ore, confidence_score, confidence_band,
                risk, effort, urgency, priority_score, priority_band, rule_ids,
                missing_information, recommended_action, rejection_reason)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)",
        )
        .bind(opportunity.id.0)
        .bind(analysis_id.0)
        .bind(self.company_id.0)
        .bind(json_key(&opportunity.category))
        .bind(json_key(&opportunity.status))
        .bind(&opportunity.title)
        .bind(&opportunity.rationale)
        .bind(opportunity.impact.low.ore())
        .bind(opportunity.impact.high.ore())
        .bind(opportunity.confidence.score as i16)
        .bind(json_key(&opportunity.confidence.band))
        .bind(json_key(&opportunity.risk))
        .bind(json_key(&opportunity.effort))
        .bind(json_key(&opportunity.urgency))
        .bind(opportunity.priority.score as f32)
        .bind(json_key(&opportunity.priority.band))
        .bind(&opportunity.rule_ids)
        .bind(&opportunity.missing_information)
        .bind(&opportunity.recommended_action)
        .bind(opportunity.rejection_reason.as_deref())
        .execute(&mut *self.tx)
        .await?;

        for (position, item) in opportunity.evidence.items().iter().enumerate() {
            sqlx::query(
                "INSERT INTO opportunity_evidence (opportunity_id, company_id, position, item)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(opportunity.id.0)
            .bind(self.company_id.0)
            .bind(position as i32)
            .bind(serde_json::to_value(item).unwrap_or(Value::Null))
            .execute(&mut *self.tx)
            .await?;

            // The calculation is stored separately as well, with its expression,
            // so the arithmetic can be re-run without reconstructing it from the
            // evidence blob.
            if let skattjakt_core::EvidenceItem::Calculation {
                calculation_id,
                method,
                inputs,
                result_low,
                result_high,
            } = item
            {
                sqlx::query(
                    "INSERT INTO calculations
                       (id, opportunity_id, company_id, method, expression, inputs,
                        result_low_ore, result_high_ore)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(calculation_id.0)
                .bind(opportunity.id.0)
                .bind(self.company_id.0)
                .bind(method)
                .bind(Value::Null)
                .bind(serde_json::to_value(inputs).unwrap_or(Value::Null))
                .bind(result_low.ore())
                .bind(result_high.ore())
                .execute(&mut *self.tx)
                .await?;
            }
        }

        Ok(())
    }

    pub async fn opportunity(&mut self, id: skattjakt_core::OpportunityId) -> StoreResult<Value> {
        let row = sqlx::query(
            "SELECT o.*, coalesce(
                 (SELECT json_agg(e.item ORDER BY e.position)
                  FROM opportunity_evidence e WHERE e.opportunity_id = o.id), '[]'::json) AS evidence
             FROM opportunities o WHERE o.id = $1",
        )
        .bind(id.0)
        .fetch_optional(&mut *self.tx)
        .await?
        .ok_or(StoreError::NotFound)?;

        Ok(serde_json::json!({
            "id": row.get::<uuid::Uuid, _>("id"),
            "analysis_id": row.get::<uuid::Uuid, _>("analysis_id"),
            "category": row.get::<String, _>("category"),
            "status": row.get::<String, _>("status"),
            "title": row.get::<String, _>("title"),
            "rationale": row.get::<String, _>("rationale"),
            "impact": {
                "low": row.get::<i64, _>("impact_low_ore"),
                "high": row.get::<i64, _>("impact_high_ore"),
            },
            "confidence": {
                "score": row.get::<i16, _>("confidence_score"),
                "band": row.get::<String, _>("confidence_band"),
            },
            "priority": {
                "score": row.get::<f32, _>("priority_score"),
                "band": row.get::<String, _>("priority_band"),
            },
            "risk": row.get::<String, _>("risk"),
            "effort": row.get::<String, _>("effort"),
            "urgency": row.get::<String, _>("urgency"),
            "rule_ids": row.get::<Vec<String>, _>("rule_ids"),
            "missing_information": row.get::<Vec<String>, _>("missing_information"),
            "recommended_action": row.get::<String, _>("recommended_action"),
            "rejection_reason": row.get::<Option<String>, _>("rejection_reason"),
            "evidence": row.get::<Value, _>("evidence"),
            "disclaimer": skattjakt_core::DISCLAIMER_SV,
        }))
    }

    pub async fn list_opportunities(&mut self, analysis_id: AnalysisId) -> StoreResult<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT id FROM opportunities
             WHERE analysis_id = $1 AND status <> 'rejected'
             ORDER BY priority_score DESC",
        )
        .bind(analysis_id.0)
        .fetch_all(&mut *self.tx)
        .await?;

        let ids: Vec<skattjakt_core::OpportunityId> = rows
            .into_iter()
            .map(|r| skattjakt_core::OpportunityId::from_uuid(r.get("id")))
            .collect();

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.opportunity(id).await?);
        }
        Ok(out)
    }

    async fn insert_model_run(&mut self, run: &ModelRunRecord) -> StoreResult<()> {
        let document_versions: Vec<uuid::Uuid> =
            run.document_version_ids.iter().map(|d| d.0).collect();
        sqlx::query(
            "INSERT INTO model_runs
               (id, analysis_id, company_id, provider, model, task, prompt_version,
                document_version_ids, status, input_tokens, output_tokens, latency_ms,
                output, error, started_at, finished_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)",
        )
        .bind(run.id.0)
        .bind(run.analysis_id.0)
        .bind(self.company_id.0)
        .bind(&run.provider)
        .bind(&run.model)
        .bind(run.task.key())
        .bind(&run.prompt_version)
        .bind(&document_versions)
        .bind(json_key(&run.status))
        .bind(run.usage.input_tokens as i32)
        .bind(run.usage.output_tokens as i32)
        .bind(run.latency_ms as i64)
        // The structured conclusion only. Never a reasoning trace — the type
        // this comes from has nowhere to put one.
        .bind(&run.output)
        .bind(run.error.as_deref())
        .bind(run.started_at)
        .bind(run.finished_at)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    pub async fn model_runs(&mut self, analysis_id: AnalysisId) -> StoreResult<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT provider, model, task, prompt_version, status, input_tokens,
                    output_tokens, latency_ms, error, started_at
             FROM model_runs WHERE analysis_id = $1 ORDER BY started_at",
        )
        .bind(analysis_id.0)
        .fetch_all(&mut *self.tx)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "provider": r.get::<String, _>("provider"),
                    "model": r.get::<String, _>("model"),
                    "task": r.get::<String, _>("task"),
                    "prompt_version": r.get::<String, _>("prompt_version"),
                    "status": r.get::<String, _>("status"),
                    "input_tokens": r.get::<i32, _>("input_tokens"),
                    "output_tokens": r.get::<i32, _>("output_tokens"),
                    "latency_ms": r.get::<i64, _>("latency_ms"),
                    "error": r.get::<Option<String>, _>("error"),
                    "started_at": r.get::<DateTime<Utc>, _>("started_at"),
                })
            })
            .collect())
    }

    // -- audit --------------------------------------------------------------

    /// Appends an audit event. The application role has no UPDATE or DELETE on
    /// this table, so what is written here cannot later be rewritten.
    pub async fn audit(
        &mut self,
        event_type: &str,
        subject: Option<uuid::Uuid>,
        detail: Value,
    ) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO audit_events (company_id, actor, event_type, subject_id, detail)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(self.company_id.0)
        .bind("api")
        .bind(event_type)
        .bind(subject)
        .bind(detail)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    pub async fn audit_trail(&mut self, subject: uuid::Uuid) -> StoreResult<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT event_type, actor, detail, occurred_at FROM audit_events
             WHERE subject_id = $1 ORDER BY occurred_at",
        )
        .bind(subject)
        .fetch_all(&mut *self.tx)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "event_type": r.get::<String, _>("event_type"),
                    "actor": r.get::<String, _>("actor"),
                    "detail": r.get::<Value, _>("detail"),
                    "occurred_at": r.get::<DateTime<Utc>, _>("occurred_at"),
                })
            })
            .collect())
    }
}

/// An analysis as stored, which may not have finished yet.
#[derive(Debug, Clone)]
pub struct StoredAnalysis {
    pub id: AnalysisId,
    pub status: AnalysisStatus,
    pub stage: AnalysisStage,
    pub rule_set_version: String,
    pub result: Option<AnalysisResult>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Serialises an enum to its snake_case wire key.
fn json_key<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn stage_key(stage: AnalysisStage) -> String {
    json_key(&stage)
}

fn stage_from_key(key: &str) -> AnalysisStage {
    let mut by_key: BTreeMap<String, AnalysisStage> = BTreeMap::new();
    for stage in AnalysisStage::ordered() {
        by_key.insert(stage_key(stage), stage);
    }
    by_key.get(key).copied().unwrap_or(AnalysisStage::Queued)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_keys_round_trip() {
        for stage in AnalysisStage::ordered() {
            assert_eq!(stage_from_key(&stage_key(stage)), stage);
        }
    }

    #[test]
    fn an_unknown_stage_key_falls_back_to_queued() {
        assert_eq!(stage_from_key("nonsense"), AnalysisStage::Queued);
    }

    #[test]
    fn enum_keys_are_snake_case_wire_values() {
        use skattjakt_core::{OpportunityCategory, OpportunityStatus};
        assert_eq!(json_key(&OpportunityStatus::Verify), "verify");
        assert_eq!(
            json_key(&OpportunityCategory::ResearchAndDevelopment),
            "research_and_development"
        );
    }
}

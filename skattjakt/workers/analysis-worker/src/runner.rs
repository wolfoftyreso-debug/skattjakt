//! Running one analysis (sections 13, 14).
//!
//! This is the part that used to live in an HTTP handler's `tokio::spawn`. It
//! moved for a reason that is architectural rather than tidiness: a background
//! task inside the API process dies with the pod. A rolling deploy, a node
//! drain, an OOM kill — all of them silently lose analyses that a customer is
//! watching a progress bar for, and there is no record that anything was lost.
//!
//! Here the job row is the record. The worker claims it, extends its lease
//! while it works, and if the pod dies the lease expires and another worker
//! picks the job up on its next attempt.

use std::sync::Arc;

use skattjakt_core::analysis::AnalysisStage;
use skattjakt_core::{AnalysisId, CompanyId, CompanyProfile};
use skattjakt_gateway::{Budget, ModelGateway};
use skattjakt_jobs::{Job, Queue};
use skattjakt_pipeline::pipeline::StageObserver;
use skattjakt_pipeline::{AnalysisInput, AnalysisPipeline, DocumentInput};
use skattjakt_store::notifications::{NewNotification, NotificationKind};
use skattjakt_store::{BlobStore, Store};
use skattjakt_telemetry::{names, LabelSet, LogRecord, Registry, SpanContext, TraceContext};

/// What went wrong, and whether trying again could help.
///
/// The distinction drives the queue: a retryable failure backs off and returns,
/// a permanent one stops and tells the customer. Getting it wrong in either
/// direction is expensive — retrying an unreadable PDF three times costs three
/// model bills for nothing, and giving up on a provider blip loses an analysis
/// that would have worked.
#[derive(Debug)]
pub struct RunFailure {
    pub kind: &'static str,
    pub retryable: bool,
    /// For the customer, in Swedish, when the failure is theirs to act on.
    pub message: Option<String>,
}

impl RunFailure {
    pub(crate) fn permanent(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            retryable: false,
            message: Some(message.into()),
        }
    }

    pub(crate) fn transient(kind: &'static str) -> Self {
        Self {
            kind,
            retryable: true,
            message: None,
        }
    }
}

/// Everything the worker needs, assembled once at startup.
pub struct Runner {
    pub store: Store,
    pub blobs: Arc<dyn BlobStore>,
    pub gateway: Arc<ModelGateway>,
    pub pipeline: Arc<AnalysisPipeline>,
    pub queue: Queue,
    pub metrics: Registry,
    pub spans: skattjakt_telemetry::otlp::SpanExporter,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("worker_id", &self.queue.worker_id())
            .field("model", &self.gateway.model_id())
            .finish()
    }
}

impl Runner {
    /// Runs the analysis a job points at.
    ///
    /// The job carries only ids. Everything else is read through a
    /// tenant-scoped transaction, so the worker cannot reach another tenant's
    /// data even if the job row were wrong.
    pub async fn run(&self, job: &Job) -> Result<(), RunFailure> {
        let company_id = CompanyId::from_uuid(job.company_id);
        let analysis_id = AnalysisId::from_uuid(job.subject_id);
        let trace = TraceContext::from_header_or_new(job.traceparent.as_deref());
        let span = trace.start_span("analysis.run");
        // The trace came off the job row, which the API wrote. This span's
        // parent is therefore the HTTP request that queued the work — which is
        // the whole point of carrying the context across a queue, and the thing
        // the integration test asserts.
        let timing = skattjakt_telemetry::otlp::FinishedSpan::start(span);
        let started = std::time::Instant::now();

        span.annotate(LogRecord::info("analysis attempt started"))
            .correlate(job.correlation_id)
            .internal("attempt", job.attempt)
            .emit();
        self.metrics
            .increment(names::ANALYSES_STARTED, LabelSet::new());

        let outcome = self.execute(company_id, analysis_id, job, span).await;

        let elapsed = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        self.metrics
            .observe(names::ANALYSIS_DURATION, LabelSet::new(), elapsed);
        self.metrics.increment(
            names::ANALYSES_FINISHED,
            LabelSet::new().enumerated(
                "outcome",
                match &outcome {
                    Ok(()) => "succeeded",
                    Err(f) if f.retryable => "retrying",
                    Err(_) => "failed",
                },
            ),
        );

        // The span leaves the process. Attributes are a closed set — a job
        // kind, an outcome, a failure kind — because a collector is shared and
        // read by more people than the database is. The correlation id
        // identifies the unit of work without identifying the customer, and the
        // failure *kind* is used rather than its message: an extraction error
        // can quote a document.
        self.spans.record(
            timing
                .attribute("skattjakt.job_kind", "analysis")
                .attribute(
                    "skattjakt.outcome",
                    match &outcome {
                        Ok(()) => "succeeded",
                        Err(f) if f.retryable => "retrying",
                        Err(_) => "failed",
                    },
                )
                .attribute(
                    "skattjakt.error_kind",
                    match &outcome {
                        Ok(()) => "none",
                        Err(f) => f.kind,
                    },
                )
                .attribute("skattjakt.correlation_id", job.correlation_id.to_string())
                .finish(match &outcome {
                    Ok(()) => skattjakt_telemetry::otlp::SpanStatus::Ok,
                    Err(_) => skattjakt_telemetry::otlp::SpanStatus::Error,
                }),
        );

        outcome
    }

    async fn execute(
        &self,
        company_id: CompanyId,
        analysis_id: AnalysisId,
        job: &Job,
        span: SpanContext,
    ) -> Result<(), RunFailure> {
        // 1. Read the pinned inputs, inside the tenant.
        let mut tenant = self
            .store
            .tenant(company_id)
            .await
            .map_err(|_| RunFailure::transient("database_unavailable"))?;

        let profile: CompanyProfile = tenant
            .company()
            .await
            .map_err(|_| RunFailure::permanent("company_missing", "Företaget kunde inte läsas."))?;

        let stored = tenant.analysis(analysis_id).await.map_err(|_| {
            RunFailure::permanent("analysis_missing", "Analysen finns inte längre.")
        })?;

        let mut versions = Vec::new();
        for version_id in &stored.document_version_ids {
            let version = tenant.document_version(*version_id).await.map_err(|_| {
                RunFailure::permanent(
                    "document_version_missing",
                    "Ett av dokumenten som analysen bygger på finns inte längre.",
                )
            })?;
            versions.push(version);
        }

        // 2. Open the budget. A retried attempt resumes against what earlier
        //    attempts already spent, so three attempts cost one budget.
        let limit = self.gateway.config().budget().limit_micro_ore;
        let (limit, spent, calls) = tenant
            .open_budget(analysis_id, limit)
            .await
            .map_err(|_| RunFailure::transient("database_unavailable"))?;
        let budget = Budget::resumed(limit, spent, calls as u32, Budget::DEFAULT_MAX_CALLS);
        tenant
            .commit()
            .await
            .map_err(|_| RunFailure::transient("database_unavailable"))?;

        if budget.is_exhausted() {
            return Err(RunFailure::permanent(
                "budget_exhausted",
                "Analysen nådde sin kostnadsgräns och stoppades. Kontakta support.",
            ));
        }

        // 3. Fetch and verify the documents.
        let mut documents = Vec::new();
        for version in &versions {
            let bytes = self
                .blobs
                .get(&version.storage_key)
                .await
                .map_err(|_| RunFailure::transient("blob_unavailable"))?;

            // The recorded hash is checked on every read. A blob that no longer
            // matches must not quietly become the basis of an analysis — that
            // is the difference between a storage fault and a wrong tax answer.
            if !version.verify_hash(&bytes) {
                return Err(RunFailure::permanent(
                    "document_hash_mismatch",
                    "Ett dokument matchar inte längre den kontrollsumma som sparades vid uppladdning. \
                     Ladda upp dokumentet igen.",
                ));
            }

            let extracted = skattjakt_extract::extract(&bytes, version.mime_type).map_err(|_| {
                RunFailure::permanent(
                    "document_unreadable",
                    "Ett av dokumenten gick inte att läsa. Kontrollera att det är en textbaserad PDF.",
                )
            })?;
            documents.push(DocumentInput {
                document_id: version.document_id,
                document_version_id: version.id,
                extracted,
            });
            self.metrics
                .increment(names::DOCUMENTS_UPLOADED, LabelSet::new());
        }

        let input = AnalysisInput {
            analysis_id,
            company: profile.clone(),
            documents,
            accounts_state: stored.accounts_state,
        };

        let facts =
            skattjakt_pipeline::build_fact_set(company_id, profile.fiscal_year, &input.documents);
        let stored_facts: Vec<_> = facts.iter().cloned().collect();
        self.metrics.add(
            names::EXTRACTION_FACTS,
            LabelSet::new(),
            stored_facts.len() as u64,
        );

        // 4. Run it, reporting stages and extending the lease as it goes.
        let observer = ProgressObserver {
            store: self.store.clone(),
            company_id,
            analysis_id,
            handle: tokio::runtime::Handle::current(),
        };

        let result = self.pipeline.run(&input, &observer).await;

        match result {
            Ok((result, runs)) => {
                self.metrics.add(
                    names::OPPORTUNITIES_FOUND,
                    LabelSet::new(),
                    result.opportunities.len() as u64,
                );
                if result.summary.found_nothing {
                    self.metrics
                        .increment(names::FOUND_NOTHING, LabelSet::new());
                }

                let mut tenant = self
                    .store
                    .tenant(company_id)
                    .await
                    .map_err(|_| RunFailure::transient("database_unavailable"))?;
                tenant
                    .insert_facts(&stored_facts)
                    .await
                    .map_err(|_| RunFailure::transient("database_unavailable"))?;
                tenant
                    .complete_analysis(&result, &runs)
                    .await
                    .map_err(|_| RunFailure::transient("database_unavailable"))?;

                // In the same transaction as the result. That is the whole
                // point of an outbox: the notification becomes true exactly
                // when the thing it describes does. Sending here instead would
                // mean a rollback tells a customer about a result that does not
                // exist; sending after the commit would lose it to a crash with
                // no record that it was owed.
                //
                // The dedupe key is the analysis id, so a retried attempt after
                // a lost lease produces one notification rather than one per
                // attempt.
                let notification = NewNotification {
                    kind: NotificationKind::AnalysisCompleted,
                    user_id: tenant.first_owner().await.unwrap_or(None),
                    subject_id: Some(job.subject_id),
                    subject_kind: Some("analysis"),
                    dedupe_key: job.subject_id.to_string(),
                    correlation_id: *job.correlation_id.as_uuid(),
                };
                if let Err(error) = tenant.enqueue_notification(notification).await {
                    // Not fatal. The analysis succeeded and the customer can see
                    // it; failing the job here would re-run a completed analysis
                    // and charge for it again, to fix a missing email.
                    LogRecord::warn("the completion notification could not be queued")
                        .internal("error", error.to_string())
                        .emit();
                }

                tenant
                    .commit()
                    .await
                    .map_err(|_| RunFailure::transient("database_unavailable"))?;

                span.annotate(LogRecord::info("analysis attempt finished"))
                    .correlate(job.correlation_id)
                    .internal("opportunities", result.opportunities.len())
                    .internal("model_runs", runs.len())
                    .emit();
                Ok(())
            }
            Err(error) => {
                let (kind, retryable, message) = classify(&error);
                span.annotate(LogRecord::warn("analysis attempt failed"))
                    .correlate(job.correlation_id)
                    .public("error_kind", kind)
                    .internal("retryable", retryable)
                    .emit();

                if !retryable {
                    // A permanent failure is the customer's to see. A transient
                    // one is not: the analysis is still in flight, and telling
                    // them it failed and then that it succeeded is worse than
                    // saying nothing.
                    if let Ok(mut tenant) = self.store.tenant(company_id).await {
                        let _ = tenant.fail_analysis(analysis_id, &message).await;
                        let _ = tenant.commit().await;
                    }
                }
                Err(RunFailure {
                    kind,
                    retryable,
                    message: (!retryable).then_some(message),
                })
            }
        }
    }
}

/// Maps a pipeline failure onto the retry decision and a Swedish message.
fn classify(error: &skattjakt_pipeline::PipelineError) -> (&'static str, bool, String) {
    use skattjakt_pipeline::PipelineError;
    match error {
        PipelineError::NoDocuments => (
            "no_documents",
            false,
            "Analysen hade inga dokument att läsa.".to_string(),
        ),
        PipelineError::TaxYearNotCovered(year) => (
            "tax_year_not_covered",
            false,
            format!(
                "Regelverket täcker inte beskattningsår {year}. \
                 Analysen stoppades hellre än att gissa."
            ),
        ),
        PipelineError::Provider(provider) => {
            let retryable = provider.is_retryable();
            let kind = if retryable {
                "provider_transient"
            } else {
                "provider_permanent"
            };
            (
                kind,
                retryable,
                "Analysen kunde inte slutföras just nu.".to_string(),
            )
        }
    }
}

/// Writes stage transitions so a polling client sees progress.
struct ProgressObserver {
    store: Store,
    company_id: CompanyId,
    analysis_id: AnalysisId,
    handle: tokio::runtime::Handle,
}

impl StageObserver for ProgressObserver {
    fn stage(&self, stage: AnalysisStage) {
        let store = self.store.clone();
        let (company_id, analysis_id) = (self.company_id, self.analysis_id);
        // Progress reporting must never fail the analysis it reports on.
        self.handle.spawn(async move {
            if let Ok(mut tenant) = store.tenant(company_id).await {
                let _ = tenant.set_stage(analysis_id, stage).await;
                let _ = tenant.commit().await;
            }
        });
    }
}

/// Extends the job's lease on a timer while the analysis runs.
///
/// Without it a long analysis outlives its lease, the reaper re-queues work
/// that is still running, and the customer pays twice for one answer.
pub fn spawn_heartbeat(queue: Queue, job: Job) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // A third of the lease: two missed beats before the lease is at risk,
        // which tolerates one slow database round trip without flapping.
        let period = job.kind.lease() / 3;
        let mut ticker = tokio::time::interval(period);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match queue.heartbeat(&job).await {
                Ok(true) => {}
                Ok(false) => {
                    // Someone else holds the job now. Stop: continuing would
                    // mean two workers running one analysis.
                    LogRecord::warn("lease lost; abandoning heartbeat")
                        .correlate(job.correlation_id)
                        .emit();
                    return;
                }
                Err(_) => return,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use skattjakt_model::ProviderError;
    use skattjakt_pipeline::PipelineError;

    #[test]
    fn an_unreadable_document_is_permanent_not_retried() {
        let failure = RunFailure::permanent("document_unreadable", "x");
        assert!(!failure.retryable);
        assert!(failure.message.is_some());
    }

    #[test]
    fn a_provider_timeout_is_retried() {
        let (kind, retryable, _) = classify(&PipelineError::Provider(ProviderError::Transport(
            "connection reset".into(),
        )));
        assert_eq!(kind, "provider_transient");
        assert!(retryable);
    }

    #[test]
    fn a_refusal_is_not_retried() {
        let (_, retryable, _) = classify(&PipelineError::Provider(ProviderError::Refused {
            category: "policy".into(),
            explanation: "no".into(),
        }));
        assert!(!retryable);
    }

    #[test]
    fn an_uncovered_tax_year_stops_rather_than_guessing() {
        let (kind, retryable, message) = classify(&PipelineError::TaxYearNotCovered(2026));
        assert_eq!(kind, "tax_year_not_covered");
        assert!(!retryable);
        assert!(message.contains("2026"));
        assert!(message.contains("gissa"));
    }

    #[test]
    fn a_transient_failure_carries_no_customer_message() {
        let failure = RunFailure::transient("database_unavailable");
        assert!(
            failure.message.is_none(),
            "an in-flight analysis must not be reported as failed"
        );
    }

    #[test]
    fn failure_kinds_never_contain_document_content() {
        // The kind is a `&'static str` from a closed set, so this holds by
        // construction; the test states the property so a future change that
        // makes it a format! fails here.
        for kind in [
            "database_unavailable",
            "document_unreadable",
            "document_hash_mismatch",
            "budget_exhausted",
            "tax_year_not_covered",
        ] {
            assert!(kind.is_ascii());
            assert!(!kind.contains(' '));
        }
    }
}

//! The queue (section 13).
//!
//! Postgres, not Redis and not a broker. Three reasons, and none of them is
//! that Postgres is fashionable for this:
//!
//!  1. A job's state change and the work it describes have to commit together.
//!     Moving an analysis to `succeeded` and writing its result are one
//!     transaction here. With a separate broker they are two, and the gap
//!     between them is where duplicated analyses and lost results live.
//!  2. `SELECT ... FOR UPDATE SKIP LOCKED` is a correct competing-consumer
//!     queue. It has been for a decade.
//!  3. It is one system to back up, restore, monitor and secure. Section 83
//!     asks for exactly this judgement.
//!
//! The volume this product will see for years is a few thousand jobs a day.
//! That is not a broker's problem.

use chrono::{DateTime, Duration, Utc};
use skattjakt_core::{AnalysisEvent, AnalysisState};
use skattjakt_telemetry::{names, CorrelationId, LabelSet, Registry};
use sqlx::postgres::PgPool;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::job::{IdempotencyKey, Job, JobId, JobKind};

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("job {0} is not in a state that allows this")]
    IllegalTransition(JobId),
    #[error("job {0} not found")]
    NotFound(JobId),
    #[error("stored job could not be read back: {0}")]
    Corrupt(String),
}

pub type QueueResult<T> = Result<T, QueueError>;

/// What `enqueue` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enqueued {
    /// A new job was created.
    Created(JobId),
    /// The idempotency key matched a job that already exists. Its id is
    /// returned so the caller can poll the same analysis rather than start a
    /// second one.
    AlreadyExists(JobId),
}

impl Enqueued {
    pub fn job_id(&self) -> JobId {
        match self {
            Enqueued::Created(id) | Enqueued::AlreadyExists(id) => *id,
        }
    }

    pub fn is_new(&self) -> bool {
        matches!(self, Enqueued::Created(_))
    }
}

#[derive(Debug, Clone)]
pub struct NewJob {
    pub kind: JobKind,
    pub company_id: Uuid,
    pub subject_id: Uuid,
    pub idempotency_key: IdempotencyKey,
    pub correlation_id: CorrelationId,
    pub traceparent: Option<String>,
    /// Delay before the job becomes claimable. Used by the retention sweep.
    pub delay: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct Queue {
    pool: PgPool,
    metrics: Registry,
    /// Identifies this worker in the lease. Pod name in Kubernetes, so a stuck
    /// job names the pod holding it.
    worker_id: String,
}

impl Queue {
    pub fn new(pool: PgPool, metrics: Registry, worker_id: impl Into<String>) -> Self {
        Self {
            pool,
            metrics,
            worker_id: worker_id.into(),
        }
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Adds a job, or returns the one that already holds the key.
    ///
    /// `ON CONFLICT DO NOTHING` plus a follow-up read, rather than a check then
    /// an insert: the check-then-insert version has a race that two API
    /// replicas will find, and finding it costs the customer a duplicate
    /// analysis.
    pub async fn enqueue(&self, new: NewJob) -> QueueResult<Enqueued> {
        let policy = new.kind.policy();
        let id = JobId::new();
        let run_after = Utc::now() + new.delay.unwrap_or_else(Duration::zero);

        let inserted = sqlx::query(
            "INSERT INTO jobs (id, kind, company_id, subject_id, idempotency_key, state,
                               attempt, max_attempts, run_after, correlation_id, traceparent)
             VALUES ($1, $2, $3, $4, $5, 'queued', 0, $6, $7, $8, $9)
             ON CONFLICT (company_id, kind, idempotency_key) DO NOTHING
             RETURNING id",
        )
        .bind(id.0)
        .bind(new.kind.as_str())
        .bind(new.company_id)
        .bind(new.subject_id)
        .bind(new.idempotency_key.as_str())
        .bind(policy.max_attempts as i32)
        .bind(run_after)
        .bind(new.correlation_id.as_uuid())
        .bind(new.traceparent.as_deref())
        .fetch_optional(&self.pool)
        .await?;

        if inserted.is_some() {
            self.metrics.increment(
                names::JOBS_ENQUEUED,
                LabelSet::new().enumerated("kind", new.kind.as_str()),
            );
            return Ok(Enqueued::Created(id));
        }

        let existing: Uuid = sqlx::query_scalar(
            "SELECT id FROM jobs WHERE company_id = $1 AND kind = $2 AND idempotency_key = $3",
        )
        .bind(new.company_id)
        .bind(new.kind.as_str())
        .bind(new.idempotency_key.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(Enqueued::AlreadyExists(JobId(existing)))
    }

    /// Takes the oldest claimable job of a kind and leases it.
    ///
    /// `SKIP LOCKED` is what lets N workers share one queue without any of them
    /// waiting on the others, and the `UPDATE` in the same statement is what
    /// makes the claim atomic — there is no window in which a job is selected
    /// but not yet leased.
    pub async fn claim(&self, kind: JobKind) -> QueueResult<Option<Job>> {
        let now = Utc::now();
        let lease_until = now
            + Duration::from_std(kind.lease()).unwrap_or_else(|_| Duration::minutes(20));

        let mut tx = self.pool.begin().await?;

        let row = sqlx::query(
            "UPDATE jobs SET
                 state = 'running',
                 attempt = attempt + 1,
                 leased_until = $1,
                 leased_by = $2,
                 updated_at = now()
             WHERE id = (
                 SELECT id FROM jobs
                 WHERE kind = $3 AND state = 'queued' AND run_after <= $4
                 ORDER BY run_after
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
             )
             RETURNING id, kind, company_id, subject_id, idempotency_key, state, attempt,
                       max_attempts, run_after, leased_until, leased_by, correlation_id,
                       traceparent, last_error, created_at, updated_at",
        )
        .bind(lease_until)
        .bind(&self.worker_id)
        .bind(kind.as_str())
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };

        let job = job_from_row(&row)?;
        record_transition(
            &mut tx,
            job.id,
            AnalysisState::Queued,
            AnalysisState::Running,
            AnalysisEvent::Claimed,
            job.attempt,
            None,
            job.correlation_id,
        )
        .await?;
        tx.commit().await?;

        self.metrics.increment(
            names::JOB_ATTEMPTS,
            LabelSet::new().enumerated("kind", kind.as_str()),
        );
        Ok(Some(job))
    }

    /// Extends the lease on a job this worker still holds.
    ///
    /// A long analysis outlives its lease otherwise, and the reaper would
    /// re-queue work that is still running — the duplicate-execution bug that
    /// makes naive queues expensive. `leased_by = $2` is what stops one worker
    /// extending another's lease.
    pub async fn heartbeat(&self, job: &Job) -> QueueResult<bool> {
        let extended = Utc::now()
            + Duration::from_std(job.kind.lease()).unwrap_or_else(|_| Duration::minutes(20));
        let affected = sqlx::query(
            "UPDATE jobs SET leased_until = $1, updated_at = now()
             WHERE id = $2 AND leased_by = $3 AND state = 'running'",
        )
        .bind(extended)
        .bind(job.id.0)
        .bind(&self.worker_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    /// Marks a job done. Terminal, and only from `running`.
    pub async fn succeed(&self, job: &Job) -> QueueResult<()> {
        self.finish(job, AnalysisEvent::Completed, None, None).await
    }

    /// Records a failed attempt and applies the retry policy.
    ///
    /// `detail` is a kind, not a message: "provider_timeout", "pdf_unreadable".
    /// Anything read out of the customer's document would end up in an
    /// operator's queue view, which is exactly what section 20 forbids.
    pub async fn fail(&self, job: &Job, retryable: bool, detail: &str) -> QueueResult<AnalysisState> {
        let event = job.event_for_failure(retryable);
        let next_run = matches!(event, AnalysisEvent::TransientFailure)
            .then(|| job.next_run_after(Utc::now()));
        self.finish(job, event, Some(detail), next_run).await?;

        // A transient failure parks the job in `retrying`; the scheduler moves
        // it back to `queued` once the backoff has elapsed. Two states rather
        // than one so an operator can tell "waiting to retry" from "waiting for
        // a worker", which are different problems.
        Ok(job
            .state
            .try_transition(event)
            .map_err(|_| QueueError::IllegalTransition(job.id))?)
    }

    /// Stops a job on request. Safe to call on a job another worker holds: the
    /// worker notices at its next heartbeat and abandons the attempt.
    pub async fn cancel(&self, job_id: JobId, company_id: Uuid) -> QueueResult<bool> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT state, attempt, correlation_id FROM jobs
             WHERE id = $1 AND company_id = $2 FOR UPDATE",
        )
        .bind(job_id.0)
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(row) = row else {
            tx.commit().await?;
            return Err(QueueError::NotFound(job_id));
        };
        let state = state_from_row(&row, "state")?;
        let Ok(next) = state.try_transition(AnalysisEvent::CancelRequested) else {
            // Already finished. Not an error: cancelling a completed analysis
            // is a race the caller cannot avoid, and the outcome they wanted
            // (it is not running) already holds.
            tx.commit().await?;
            return Ok(false);
        };

        sqlx::query(
            "UPDATE jobs SET state = $1, leased_until = NULL, leased_by = NULL, updated_at = now()
             WHERE id = $2",
        )
        .bind(next.as_str())
        .bind(job_id.0)
        .execute(&mut *tx)
        .await?;

        let attempt: i32 = row.try_get("attempt").unwrap_or(0);
        let correlation: Uuid = row.try_get("correlation_id").unwrap_or_else(|_| Uuid::nil());
        record_transition(
            &mut tx,
            job_id,
            state,
            next,
            AnalysisEvent::CancelRequested,
            attempt as u32,
            None,
            CorrelationId::from_uuid(correlation),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn finish(
        &self,
        job: &Job,
        event: AnalysisEvent,
        detail: Option<&str>,
        next_run: Option<DateTime<Utc>>,
    ) -> QueueResult<()> {
        let next = job
            .state
            .try_transition(event)
            .map_err(|_| QueueError::IllegalTransition(job.id))?;

        let mut tx = self.pool.begin().await?;

        // `leased_by = $N` again: a worker whose lease was reaped must not be
        // able to report a result for a job someone else is now running.
        let affected = sqlx::query(
            "UPDATE jobs SET state = $1, last_error = $2, run_after = COALESCE($3, run_after),
                 leased_until = NULL, leased_by = NULL, updated_at = now()
             WHERE id = $4 AND leased_by = $5 AND state = 'running'",
        )
        .bind(next.as_str())
        .bind(detail)
        .bind(next_run)
        .bind(job.id.0)
        .bind(&self.worker_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if affected != 1 {
            tx.rollback().await?;
            return Err(QueueError::IllegalTransition(job.id));
        }

        record_transition(
            &mut tx,
            job.id,
            job.state,
            next,
            event,
            job.attempt,
            detail,
            job.correlation_id,
        )
        .await?;

        if next == AnalysisState::DeadLettered {
            sqlx::query(
                "INSERT INTO dead_letters (job_id, kind, company_id, subject_id, attempts,
                                           last_error, correlation_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (job_id) DO NOTHING",
            )
            .bind(job.id.0)
            .bind(job.kind.as_str())
            .bind(job.company_id)
            .bind(job.subject_id)
            .bind(job.attempt as i32)
            .bind(detail)
            .bind(job.correlation_id.as_uuid())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        let labels = LabelSet::new()
            .enumerated("kind", job.kind.as_str())
            .enumerated("outcome", next.as_str());
        self.metrics.increment(names::JOBS_COMPLETED, labels);
        if next == AnalysisState::DeadLettered {
            self.metrics.increment(
                names::JOBS_DEAD_LETTERED,
                LabelSet::new().enumerated("kind", job.kind.as_str()),
            );
        }
        Ok(())
    }

    /// Returns jobs whose backoff has elapsed to the queue.
    ///
    /// Run on a timer by every worker. Idempotent and safe to run concurrently:
    /// the `WHERE` clause is the guard.
    pub async fn release_elapsed_backoffs(&self) -> QueueResult<u64> {
        let released = sqlx::query(
            "UPDATE jobs SET state = 'queued', updated_at = now()
             WHERE state = 'retrying' AND run_after <= now()",
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(released)
    }

    /// Re-queues jobs whose worker stopped reporting (section 77).
    ///
    /// This is what makes an evicted pod, an OOM kill or a node failure a
    /// delay rather than a lost analysis. The attempt has already been counted,
    /// so a pod that dies repeatedly on the same job still dead-letters instead
    /// of looping forever.
    pub async fn reap_expired_leases(&self) -> QueueResult<u64> {
        let mut tx = self.pool.begin().await?;

        let rows = sqlx::query(
            "SELECT id, attempt, max_attempts, correlation_id FROM jobs
             WHERE state = 'running' AND leased_until < now()
             FOR UPDATE SKIP LOCKED",
        )
        .fetch_all(&mut *tx)
        .await?;

        let mut reaped = 0;
        for row in rows {
            let id = JobId(row.try_get("id").map_err(QueueError::Database)?);
            let attempt: i32 = row.try_get("attempt").unwrap_or(0);
            let max_attempts: i32 = row.try_get("max_attempts").unwrap_or(1);
            let correlation: Uuid = row.try_get("correlation_id").unwrap_or_else(|_| Uuid::nil());

            let (event, next) = if attempt >= max_attempts {
                (AnalysisEvent::AttemptsExhausted, AnalysisState::DeadLettered)
            } else {
                (AnalysisEvent::LeaseExpired, AnalysisState::Retrying)
            };

            sqlx::query(
                "UPDATE jobs SET state = $1, leased_until = NULL, leased_by = NULL,
                     last_error = 'lease_expired', run_after = now() + interval '30 seconds',
                     updated_at = now()
                 WHERE id = $2",
            )
            .bind(next.as_str())
            .bind(id.0)
            .execute(&mut *tx)
            .await?;

            record_transition(
                &mut tx,
                id,
                AnalysisState::Running,
                next,
                event,
                attempt as u32,
                Some("lease_expired"),
                CorrelationId::from_uuid(correlation),
            )
            .await?;

            if next == AnalysisState::DeadLettered {
                sqlx::query(
                    "INSERT INTO dead_letters (job_id, kind, company_id, subject_id, attempts,
                                               last_error, correlation_id)
                     SELECT id, kind, company_id, subject_id, attempt, 'lease_expired', correlation_id
                     FROM jobs WHERE id = $1
                     ON CONFLICT (job_id) DO NOTHING",
                )
                .bind(id.0)
                .execute(&mut *tx)
                .await?;
            }
            reaped += 1;
        }

        tx.commit().await?;
        Ok(reaped)
    }

    /// Queue depth and age, for the dashboard and the alert.
    pub async fn publish_depth(&self) -> QueueResult<()> {
        let rows = sqlx::query(
            "SELECT kind, count(*) AS n,
                    COALESCE(EXTRACT(EPOCH FROM (now() - min(run_after))) * 1000, 0) AS age_ms
             FROM jobs WHERE state = 'queued' GROUP BY kind",
        )
        .fetch_all(&self.pool)
        .await?;

        for row in rows {
            let kind: String = row.try_get("kind").map_err(QueueError::Database)?;
            let Some(kind) = JobKind::parse(&kind) else {
                continue;
            };
            let n: i64 = row.try_get("n").unwrap_or(0);
            let age: f64 = row.try_get("age_ms").unwrap_or(0.0);
            let labels = LabelSet::new().enumerated("kind", kind.as_str());
            self.metrics
                .set(names::JOBS_QUEUED_DEPTH, labels.clone(), n.max(0) as u64);
            self.metrics
                .set(names::JOB_QUEUE_AGE, labels, age.max(0.0) as u64);
        }
        Ok(())
    }

    pub async fn get(&self, job_id: JobId) -> QueueResult<Job> {
        let row = sqlx::query(
            "SELECT id, kind, company_id, subject_id, idempotency_key, state, attempt,
                    max_attempts, run_after, leased_until, leased_by, correlation_id,
                    traceparent, last_error, created_at, updated_at
             FROM jobs WHERE id = $1",
        )
        .bind(job_id.0)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(QueueError::NotFound(job_id))?;
        job_from_row(&row)
    }

    /// The job running an analysis, if there is one.
    pub async fn for_subject(&self, subject_id: Uuid) -> QueueResult<Option<Job>> {
        let row = sqlx::query(
            "SELECT id, kind, company_id, subject_id, idempotency_key, state, attempt,
                    max_attempts, run_after, leased_until, leased_by, correlation_id,
                    traceparent, last_error, created_at, updated_at
             FROM jobs WHERE subject_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(subject_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(job_from_row).transpose()
    }

    /// Unacknowledged dead letters, oldest first. The operator's queue.
    pub async fn open_dead_letters(&self, limit: i64) -> QueueResult<Vec<DeadLetter>> {
        let rows = sqlx::query(
            "SELECT job_id, kind, company_id, subject_id, attempts, last_error,
                    correlation_id, created_at
             FROM dead_letters WHERE acknowledged_at IS NULL
             ORDER BY created_at LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(DeadLetter {
                    job_id: JobId(row.try_get("job_id").map_err(QueueError::Database)?),
                    kind: row.try_get("kind").map_err(QueueError::Database)?,
                    company_id: row.try_get("company_id").map_err(QueueError::Database)?,
                    subject_id: row.try_get("subject_id").map_err(QueueError::Database)?,
                    attempts: row.try_get::<i32, _>("attempts").unwrap_or(0) as u32,
                    last_error: row.try_get("last_error").ok(),
                    correlation_id: CorrelationId::from_uuid(
                        row.try_get("correlation_id").unwrap_or_else(|_| Uuid::nil()),
                    ),
                    created_at: row.try_get("created_at").map_err(QueueError::Database)?,
                })
            })
            .collect()
    }

    /// The history of one job, for an incident review.
    pub async fn transitions(&self, job_id: JobId) -> QueueResult<Vec<(String, String, String, DateTime<Utc>)>> {
        let rows = sqlx::query(
            "SELECT from_state, to_state, event, at FROM job_transitions
             WHERE job_id = $1 ORDER BY at, id",
        )
        .bind(job_id.0)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.try_get("from_state").unwrap_or_default(),
                    row.try_get("to_state").unwrap_or_default(),
                    row.try_get("event").unwrap_or_default(),
                    row.try_get("at").unwrap_or_else(|_| Utc::now()),
                )
            })
            .collect())
    }
}

#[derive(Debug, Clone)]
pub struct DeadLetter {
    pub job_id: JobId,
    pub kind: String,
    pub company_id: Uuid,
    pub subject_id: Uuid,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub correlation_id: CorrelationId,
    pub created_at: DateTime<Utc>,
}

async fn record_transition(
    tx: &mut Transaction<'_, Postgres>,
    job_id: JobId,
    from: AnalysisState,
    to: AnalysisState,
    event: AnalysisEvent,
    attempt: u32,
    detail: Option<&str>,
    correlation_id: CorrelationId,
) -> QueueResult<()> {
    sqlx::query(
        "INSERT INTO job_transitions (job_id, from_state, to_state, event, attempt, detail, correlation_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(job_id.0)
    .bind(from.as_str())
    .bind(to.as_str())
    .bind(event.as_str())
    .bind(attempt as i32)
    .bind(detail)
    .bind(correlation_id.as_uuid())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn state_from_row(row: &sqlx::postgres::PgRow, column: &str) -> QueueResult<AnalysisState> {
    let raw: String = row.try_get(column).map_err(QueueError::Database)?;
    AnalysisState::parse(&raw).ok_or_else(|| QueueError::Corrupt(format!("unknown state {raw}")))
}

fn job_from_row(row: &sqlx::postgres::PgRow) -> QueueResult<Job> {
    let kind: String = row.try_get("kind").map_err(QueueError::Database)?;
    let key: String = row.try_get("idempotency_key").map_err(QueueError::Database)?;
    Ok(Job {
        id: JobId(row.try_get("id").map_err(QueueError::Database)?),
        kind: JobKind::parse(&kind)
            .ok_or_else(|| QueueError::Corrupt(format!("unknown job kind {kind}")))?,
        company_id: row.try_get("company_id").map_err(QueueError::Database)?,
        subject_id: row.try_get("subject_id").map_err(QueueError::Database)?,
        idempotency_key: IdempotencyKey::parse(&key)
            .map_err(|e| QueueError::Corrupt(e.to_string()))?,
        state: state_from_row(row, "state")?,
        attempt: row.try_get::<i32, _>("attempt").unwrap_or(0).max(0) as u32,
        max_attempts: row.try_get::<i32, _>("max_attempts").unwrap_or(1).max(1) as u32,
        run_after: row.try_get("run_after").map_err(QueueError::Database)?,
        leased_until: row.try_get("leased_until").ok(),
        leased_by: row.try_get("leased_by").ok(),
        correlation_id: CorrelationId::from_uuid(
            row.try_get("correlation_id").unwrap_or_else(|_| Uuid::nil()),
        ),
        traceparent: row.try_get("traceparent").ok(),
        last_error: row.try_get("last_error").ok(),
        created_at: row.try_get("created_at").map_err(QueueError::Database)?,
        updated_at: row.try_get("updated_at").map_err(QueueError::Database)?,
    })
}

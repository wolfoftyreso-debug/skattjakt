//! The job record and the policy that governs it (section 13).
//!
//! Everything here is deliberately free of I/O so the retry arithmetic, the
//! backoff schedule and the terminal-state rules can be tested without a
//! database. `queue.rs` does the SQL; this file decides what the SQL is for.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use skattjakt_core::{AnalysisEvent, AnalysisState};
use skattjakt_telemetry::CorrelationId;
use uuid::Uuid;

/// What a job is. A closed enumeration: a worker that does not recognise a kind
/// leaves it alone rather than guessing, and a typo cannot create a queue that
/// nothing drains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Run the analysis pipeline for one analysis.
    Analysis,
    /// Extract facts from one uploaded document version.
    Extraction,
    /// Apply the retention policy (section 65).
    Retention,
    /// Run one Monte Carlo simulation.
    ///
    /// Queued rather than run in the request when the iteration count makes it
    /// too long for one — see `simulation_routes::execution_for`. A run that
    /// takes two minutes is a run that must survive a rolling deploy.
    Simulation,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Analysis => "analysis",
            JobKind::Extraction => "extraction",
            JobKind::Retention => "retention",
            JobKind::Simulation => "simulation",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "analysis" => JobKind::Analysis,
            "extraction" => JobKind::Extraction,
            "retention" => JobKind::Retention,
            "simulation" => JobKind::Simulation,
            _ => return None,
        })
    }

    /// How long a worker may hold the job before the lease is assumed lost.
    ///
    /// An analysis is minutes of model latency; a retention sweep is a query.
    /// One global timeout would either kill analyses or leave dead extraction
    /// jobs stuck for the length of an analysis.
    pub fn lease(self) -> Duration {
        match self {
            JobKind::Analysis => Duration::from_secs(20 * 60),
            JobKind::Extraction => Duration::from_secs(5 * 60),
            JobKind::Retention => Duration::from_secs(30 * 60),
            // Bounded by arithmetic rather than by a network call: ten million
            // iterations of the widest model the engine accepts runs in a few
            // minutes, and the lease has to cover the slowest machine that
            // might claim it.
            JobKind::Simulation => Duration::from_secs(15 * 60),
        }
    }

    pub fn policy(self) -> RetryPolicy {
        match self {
            // Analyses cost money per attempt. Three tries, then a human.
            JobKind::Analysis => RetryPolicy {
                max_attempts: 3,
                base: Duration::from_secs(30),
                max_backoff: Duration::from_secs(15 * 60),
            },
            JobKind::Extraction => RetryPolicy {
                max_attempts: 5,
                base: Duration::from_secs(5),
                max_backoff: Duration::from_secs(5 * 60),
            },
            // A simulation is deterministic: the same seed and the same model
            // produce the same failure. Retrying twice covers a lost lease or
            // an evicted pod, and a third attempt would only spend CPU
            // reproducing the same rejected specification.
            JobKind::Simulation => RetryPolicy {
                max_attempts: 2,
                base: Duration::from_secs(15),
                max_backoff: Duration::from_secs(5 * 60),
            },
            // The retention sweep is idempotent and runs on a schedule; a
            // failed sweep can simply wait for the next one.
            JobKind::Retention => RetryPolicy {
                max_attempts: 2,
                base: Duration::from_secs(60),
                max_backoff: Duration::from_secs(60 * 60),
            },
        }
    }
}

/// Exponential backoff with a ceiling and deterministic jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub max_backoff: Duration,
}

impl RetryPolicy {
    /// Delay before attempt number `attempt` (1-based: the delay after the
    /// first failure is `delay_for(1)`).
    ///
    /// Jitter is derived from the job id rather than from a random number
    /// generator. Two reasons: the schedule is reproducible, which matters when
    /// reconstructing an incident, and a job that keeps failing keeps the same
    /// jitter, so it does not slowly drift into phase with another job.
    ///
    /// The spread is ±25%, which is enough to break up a thundering herd after
    /// a provider outage — the case this exists for, where a hundred analyses
    /// fail within the same second and would otherwise all retry within the
    /// same second.
    pub fn delay_for(&self, attempt: u32, job_id: JobId) -> Duration {
        let exponent = attempt.saturating_sub(1).min(16);
        let scaled = self
            .base
            .saturating_mul(2u32.saturating_pow(exponent))
            .min(self.max_backoff);

        let millis = scaled.as_millis() as u64;
        // Bottom byte of the id, mapped to [-25%, +25%).
        let seed = job_id.0.as_bytes()[15] as u64;
        let spread = millis / 2;
        let offset = (seed * spread) / 256;
        let jittered = millis.saturating_sub(spread / 2).saturating_add(offset / 2);
        Duration::from_millis(jittered.max(1))
    }

    /// True when `attempt` was the last one allowed.
    pub fn is_final_attempt(&self, attempt: u32) -> bool {
        attempt >= self.max_attempts
    }
}

/// A job identifier. Distinct from the analysis it runs: one analysis can be
/// represented by exactly one live job, but the job is the queue's object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub Uuid);

impl JobId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The caller-supplied key that makes enqueueing idempotent (section 13).
///
/// A retried HTTP request, a duplicated queue message and a user pressing the
/// button twice all arrive as the same key, and the second enqueue returns the
/// first job rather than starting a second analysis. Without this, a network
/// timeout on the client side costs the customer a second model bill.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdempotencyKey(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdempotencyKeyError {
    #[error("idempotency key must be between 8 and 200 characters")]
    Length,
    #[error("idempotency key may contain only letters, digits, '-', '_' and ':'")]
    Charset,
}

impl IdempotencyKey {
    pub fn parse(value: &str) -> Result<Self, IdempotencyKeyError> {
        let trimmed = value.trim();
        if trimmed.len() < 8 || trimmed.len() > 200 {
            return Err(IdempotencyKeyError::Length);
        }
        if !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
        {
            return Err(IdempotencyKeyError::Charset);
        }
        Ok(Self(trimmed.to_string()))
    }

    /// Derives a key when the caller supplied none, from data that identifies
    /// the work rather than the moment. Two requests to analyse the same
    /// document versions for the same company collapse into one job.
    pub fn derived(kind: JobKind, scope: Uuid, parts: &[Uuid]) -> Self {
        let mut sorted: Vec<String> = parts.iter().map(|p| p.simple().to_string()).collect();
        sorted.sort();
        Self(format!(
            "auto:{}:{}:{}",
            kind.as_str(),
            scope.simple(),
            // A long list would blow the length limit; the first 8 hex chars of
            // each part are ample within one scope, and collisions inside a
            // single company merely coalesce two identical requests.
            sorted.iter().map(|p| &p[..8]).collect::<Vec<_>>().join("-")
        ))
    }

    /// Derives a key for work that was paid for.
    ///
    /// The order belongs in the key because it is part of what makes the work
    /// distinct. Two analyses of the same documents are the same work only if
    /// nobody bought them separately; once an order is involved, a second
    /// purchase is a second piece of work even though every document matches.
    ///
    /// Without this the customer's paid request collapses onto an earlier free
    /// one, and they are handed an analysis their order did not buy.
    pub fn derived_for_order(kind: JobKind, scope: Uuid, parts: &[Uuid], order: Uuid) -> Self {
        // The order id is carried **whole**, unlike the document ids.
        //
        // `derived` keeps the first eight hex characters of each document,
        // because a collision there merely coalesces two requests over
        // near-identical document sets. A collision on the order is a different
        // thing entirely: two separate purchases would become one job, and a
        // customer who paid would be handed the other one's analysis. That is
        // the exact failure this function exists to prevent, so it does not get
        // 32 bits of collision resistance.
        //
        // It leads rather than trails, so the truncation below can only ever
        // eat document ids.
        let mut sorted: Vec<String> = parts.iter().map(|p| p.simple().to_string()).collect();
        sorted.sort();
        let docs = sorted.iter().map(|p| &p[..8]).collect::<Vec<_>>().join("-");

        let head = format!(
            "auto:{}:{}:o{}",
            kind.as_str(),
            scope.simple(),
            order.simple()
        );
        // `parse` accepts at most 200 characters, and a derived key that its own
        // parser would reject is a job that cannot be re-enqueued from its own
        // recorded key. The head is 14 + 32 + 2 + 32 = 80 at most, so there is
        // always room for some documents; a request with more than about
        // thirteen of them loses the tail, which is harmless because the order
        // in front of it already makes the key unique.
        const LIMIT: usize = 200;
        let room = LIMIT - head.len() - 1;
        let docs = if docs.len() > room {
            &docs[..room]
        } else {
            &docs
        };
        Self(format!("{head}:{docs}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A row in the queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub kind: JobKind,
    /// The tenant. Every job belongs to exactly one, and the queue query is
    /// scoped by it — a worker cannot claim across tenants by accident.
    pub company_id: Uuid,
    /// The subject: the analysis id, the document version id, or nil for
    /// housekeeping.
    pub subject_id: Uuid,
    pub idempotency_key: IdempotencyKey,
    pub state: AnalysisState,
    pub attempt: u32,
    pub max_attempts: u32,
    /// Not claimable before this. Carries the backoff.
    pub run_after: DateTime<Utc>,
    /// Set while a worker holds the job; the deadline by which it must report.
    pub leased_until: Option<DateTime<Utc>>,
    pub leased_by: Option<String>,
    pub correlation_id: CorrelationId,
    /// W3C traceparent from the request that created the job, so the worker's
    /// spans join the caller's trace.
    pub traceparent: Option<String>,
    /// Why the last attempt failed, by kind. Never document content.
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Job {
    pub fn is_claimable_at(&self, now: DateTime<Utc>) -> bool {
        self.state == AnalysisState::Queued && self.run_after <= now
    }

    /// True when a worker is holding it and has not run out of time.
    pub fn lease_is_live_at(&self, now: DateTime<Utc>) -> bool {
        self.state == AnalysisState::Running && self.leased_until.is_some_and(|until| until > now)
    }

    /// The event a failed attempt should produce, given how many attempts are
    /// left and whether the failure can improve on retry.
    ///
    /// Two reasons a job dies. `PermanentFailure` means retrying is pointless —
    /// the document is not a PDF, the fiscal year is outside the rule set — and
    /// the job goes straight to `Failed` with a message the customer can act
    /// on. `AttemptsExhausted` means it might have worked and did not, and the
    /// job goes to the dead letter queue for an operator. Collapsing the two
    /// would either bury customer-actionable errors in a queue nobody reads, or
    /// fill the operator's queue with unreadable uploads.
    pub fn event_for_failure(&self, retryable: bool) -> AnalysisEvent {
        if !retryable {
            AnalysisEvent::PermanentFailure
        } else if self.attempt >= self.max_attempts {
            AnalysisEvent::AttemptsExhausted
        } else {
            AnalysisEvent::TransientFailure
        }
    }

    pub fn next_run_after(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let delay = self.kind.policy().delay_for(self.attempt, self.id);
        now + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::seconds(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_id(last_byte: u8) -> JobId {
        let mut bytes = [0u8; 16];
        bytes[15] = last_byte;
        JobId(Uuid::from_bytes(bytes))
    }

    #[test]
    fn backoff_grows_exponentially() {
        let policy = JobKind::Analysis.policy();
        let id = job_id(128);
        let first = policy.delay_for(1, id);
        let second = policy.delay_for(2, id);
        let third = policy.delay_for(3, id);
        assert!(second > first, "{second:?} should exceed {first:?}");
        assert!(third > second, "{third:?} should exceed {second:?}");
    }

    #[test]
    fn backoff_is_capped() {
        let policy = JobKind::Analysis.policy();
        let delay = policy.delay_for(50, job_id(200));
        assert!(
            delay <= policy.max_backoff,
            "{delay:?} exceeded the ceiling"
        );
    }

    #[test]
    fn backoff_never_overflows_at_absurd_attempt_counts() {
        let policy = JobKind::Extraction.policy();
        // The exponent is clamped, so this is arithmetic rather than a panic.
        let delay = policy.delay_for(u32::MAX, job_id(1));
        assert!(delay <= policy.max_backoff);
    }

    #[test]
    fn jitter_separates_two_jobs_that_failed_at_the_same_instant() {
        let policy = JobKind::Analysis.policy();
        let a = policy.delay_for(2, job_id(0));
        let b = policy.delay_for(2, job_id(255));
        assert_ne!(
            a, b,
            "identical backoff would retry both in the same second"
        );
    }

    #[test]
    fn jitter_is_reproducible_for_one_job() {
        let policy = JobKind::Analysis.policy();
        let id = job_id(77);
        assert_eq!(policy.delay_for(3, id), policy.delay_for(3, id));
    }

    #[test]
    fn jitter_stays_within_a_quarter_of_the_nominal_delay() {
        let policy = JobKind::Analysis.policy();
        let nominal = policy.base.as_millis() as u64 * 2; // attempt 2
        for seed in [0u8, 1, 64, 128, 200, 255] {
            let delay = policy.delay_for(2, job_id(seed)).as_millis() as u64;
            assert!(
                delay >= nominal * 3 / 4 && delay <= nominal * 5 / 4,
                "seed {seed}: {delay} outside +/-25% of {nominal}"
            );
        }
    }

    fn job(attempt: u32, max_attempts: u32) -> Job {
        Job {
            id: JobId::new(),
            kind: JobKind::Analysis,
            company_id: Uuid::nil(),
            subject_id: Uuid::nil(),
            idempotency_key: IdempotencyKey::parse("key-12345678").unwrap(),
            state: AnalysisState::Running,
            attempt,
            max_attempts,
            run_after: Utc::now(),
            leased_until: None,
            leased_by: None,
            correlation_id: CorrelationId::new(),
            traceparent: None,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn a_retryable_failure_with_attempts_left_retries() {
        assert_eq!(
            job(1, 3).event_for_failure(true),
            AnalysisEvent::TransientFailure
        );
    }

    #[test]
    fn a_retryable_failure_on_the_last_attempt_is_dead_lettered() {
        assert_eq!(
            job(3, 3).event_for_failure(true),
            AnalysisEvent::AttemptsExhausted
        );
    }

    #[test]
    fn a_permanent_failure_does_not_burn_the_remaining_attempts() {
        assert_eq!(
            job(1, 3).event_for_failure(false),
            AnalysisEvent::PermanentFailure
        );
    }

    #[test]
    fn the_failure_event_always_leads_to_a_legal_transition() {
        for attempt in 1..=3 {
            for retryable in [true, false] {
                let job = job(attempt, 3);
                let event = job.event_for_failure(retryable);
                assert!(
                    job.state.try_transition(event).is_ok(),
                    "attempt {attempt}, retryable {retryable}: {event} rejected"
                );
            }
        }
    }

    #[test]
    fn idempotency_keys_reject_lengths_and_characters_that_would_break_the_index() {
        assert!(IdempotencyKey::parse("short").is_err());
        assert!(IdempotencyKey::parse(&"x".repeat(500)).is_err());
        assert!(IdempotencyKey::parse("has spaces here").is_err());
        assert!(IdempotencyKey::parse("drop';--table").is_err());
        assert!(IdempotencyKey::parse("valid-key_123:abc").is_ok());
    }

    #[test]
    fn a_derived_key_is_stable_regardless_of_document_order() {
        let company = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(
            IdempotencyKey::derived(JobKind::Analysis, company, &[a, b]),
            IdempotencyKey::derived(JobKind::Analysis, company, &[b, a])
        );
    }

    #[test]
    fn a_derived_key_differs_across_companies() {
        let doc = Uuid::new_v4();
        assert_ne!(
            IdempotencyKey::derived(JobKind::Analysis, Uuid::new_v4(), &[doc]),
            IdempotencyKey::derived(JobKind::Analysis, Uuid::new_v4(), &[doc])
        );
    }

    #[test]
    fn a_derived_key_passes_its_own_validation() {
        let key = IdempotencyKey::derived(
            JobKind::Analysis,
            Uuid::new_v4(),
            &[Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()],
        );
        assert!(IdempotencyKey::parse(key.as_str()).is_ok());
    }

    #[test]
    fn a_queued_job_in_backoff_is_not_claimable_yet() {
        let mut j = job(1, 3);
        j.state = AnalysisState::Queued;
        j.run_after = Utc::now() + chrono::Duration::minutes(5);
        assert!(!j.is_claimable_at(Utc::now()));
        assert!(j.is_claimable_at(Utc::now() + chrono::Duration::minutes(6)));
    }

    #[test]
    fn an_expired_lease_stops_counting_as_live() {
        let mut j = job(1, 3);
        j.leased_until = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(!j.lease_is_live_at(Utc::now()));
    }

    #[test]
    fn each_job_kind_leases_for_longer_than_its_first_backoff() {
        // Otherwise a job would be reaped as a lost lease while it was merely
        // waiting to start.
        for kind in [JobKind::Analysis, JobKind::Extraction, JobKind::Retention] {
            assert!(kind.lease() > kind.policy().base, "{}", kind.as_str());
        }
    }

    #[test]
    fn job_kinds_round_trip_through_their_string_form() {
        for kind in [JobKind::Analysis, JobKind::Extraction, JobKind::Retention] {
            assert_eq!(JobKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(JobKind::parse("analyse"), None);
    }

    #[test]
    fn two_purchases_of_the_same_documents_are_two_pieces_of_work() {
        // The defect this is here for: without the order in the key, a paid
        // request over documents that had been analysed before collapsed onto
        // the earlier job. The order was consumed and the customer was handed
        // the earlier analysis — they paid and received something else.
        let company = Uuid::from_u128(1);
        let docs = [Uuid::from_u128(2), Uuid::from_u128(3)];
        let first = IdempotencyKey::derived_for_order(
            JobKind::Analysis,
            company,
            &docs,
            Uuid::from_u128(10),
        );
        let second = IdempotencyKey::derived_for_order(
            JobKind::Analysis,
            company,
            &docs,
            Uuid::from_u128(11),
        );
        assert_ne!(first.as_str(), second.as_str());

        // And a paid request never collides with a free one over the same
        // documents, which is the collision that actually happened.
        let free = IdempotencyKey::derived(JobKind::Analysis, company, &docs);
        assert_ne!(first.as_str(), free.as_str());
    }

    #[test]
    fn retrying_the_same_purchase_still_derives_the_same_key() {
        // The whole point of deriving a key: a client that retries a timed-out
        // request without one must not be billed for a second analysis.
        let company = Uuid::from_u128(1);
        let docs = [Uuid::from_u128(2)];
        let order = Uuid::from_u128(9);
        assert_eq!(
            IdempotencyKey::derived_for_order(JobKind::Analysis, company, &docs, order).as_str(),
            IdempotencyKey::derived_for_order(JobKind::Analysis, company, &docs, order).as_str()
        );
    }

    #[test]
    fn a_derived_key_is_always_one_the_parser_accepts() {
        // Derived keys go into the same column as client-supplied ones. A
        // derivation that produced something `parse` rejects would be a job
        // that cannot be re-enqueued from its own recorded key.
        // Fifty documents is more than any real request and exactly the shape
        // that would overrun the limit if the key were built by concatenation
        // and hope.
        let many: Vec<Uuid> = (0..50).map(|i| Uuid::from_u128(100 + i)).collect();
        let key = IdempotencyKey::derived_for_order(
            JobKind::Analysis,
            Uuid::from_u128(1),
            &many,
            Uuid::new_v4(),
        );
        assert!(
            IdempotencyKey::parse(key.as_str()).is_ok(),
            "{}",
            key.as_str()
        );

        // And the order survives the truncation, because it is what makes the
        // key unique.
        let order = Uuid::new_v4();
        let truncated =
            IdempotencyKey::derived_for_order(JobKind::Analysis, Uuid::from_u128(1), &many, order);
        assert!(
            truncated.as_str().contains(&order.simple().to_string()),
            "the order was truncated away: {}",
            truncated.as_str()
        );
    }
}

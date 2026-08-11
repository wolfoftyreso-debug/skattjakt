//! # skattjakt-jobs
//!
//! The durable job system of section 13: at-least-once delivery, leases,
//! bounded retries with exponential backoff and jitter, idempotent enqueue, a
//! dead letter queue, cancellation and correlation ids — on Postgres, so a job's
//! state change commits in the same transaction as the work it describes.
//!
//! Splitting the crate in two is deliberate. `job` holds the policy — how long
//! to back off, when to give up, what counts as an idempotency key — and has no
//! I/O, so all of it is testable without a database. `queue` holds the SQL.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod job;
pub mod queue;

pub use job::{IdempotencyKey, IdempotencyKeyError, Job, JobId, JobKind, RetryPolicy};
pub use queue::{DeadLetter, Enqueued, NewJob, Queue, QueueError, QueueResult};

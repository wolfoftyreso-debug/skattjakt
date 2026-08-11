//! The analysis state machine (section 14).
//!
//! An analysis is a long-lived, retryable, cancellable, externally visible
//! process. Left as a `status` string it becomes a place where impossible
//! states accumulate: a run that is `failed` and later `succeeded`, a
//! `cancelled` run that keeps charging model tokens, a `succeeded` run with no
//! result row.
//!
//! So the transitions are enumerated. `AnalysisState::try_transition` is the
//! only way to move, it rejects anything not in the table, and the terminal
//! states are terminal — nothing leaves them, including a retry, which starts a
//! *new* analysis rather than resurrecting an old one.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Where an analysis is. Distinct from `AnalysisStage`, which is the
/// user-facing progress narration inside `Running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisState {
    /// Created and accepted. Documents pinned, nothing started.
    Queued,
    /// A worker holds the lease and is doing the work.
    Running,
    /// The attempt failed in a way that may succeed later. Waits out a backoff
    /// and returns to `Queued`.
    Retrying,
    /// Finished with a result.
    Succeeded,
    /// Finished without a result, and will not be tried again.
    Failed,
    /// Stopped on request. Terminal — a cancelled analysis never resumes.
    Cancelled,
    /// Attempts exhausted. The job sits in the dead letter queue for a human.
    /// Terminal for the analysis; the DLQ entry is a separate, live object.
    DeadLettered,
}

impl AnalysisState {
    pub fn as_str(self) -> &'static str {
        match self {
            AnalysisState::Queued => "queued",
            AnalysisState::Running => "running",
            AnalysisState::Retrying => "retrying",
            AnalysisState::Succeeded => "succeeded",
            AnalysisState::Failed => "failed",
            AnalysisState::Cancelled => "cancelled",
            AnalysisState::DeadLettered => "dead_lettered",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => AnalysisState::Queued,
            "running" => AnalysisState::Running,
            "retrying" => AnalysisState::Retrying,
            "succeeded" => AnalysisState::Succeeded,
            "failed" => AnalysisState::Failed,
            "cancelled" => AnalysisState::Cancelled,
            "dead_lettered" => AnalysisState::DeadLettered,
            _ => return None,
        })
    }

    /// No transition leaves a terminal state. This is what makes the audit
    /// trail trustworthy: a finished analysis stays finished.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            AnalysisState::Succeeded
                | AnalysisState::Failed
                | AnalysisState::Cancelled
                | AnalysisState::DeadLettered
        )
    }

    /// True when the analysis is consuming, or about to consume, resources —
    /// so budget and rate limiting look at exactly these.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            AnalysisState::Queued | AnalysisState::Running | AnalysisState::Retrying
        )
    }

    /// The states a caller may ask to cancel from.
    pub fn is_cancellable(self) -> bool {
        self.is_active()
    }

    pub fn all() -> [AnalysisState; 7] {
        [
            AnalysisState::Queued,
            AnalysisState::Running,
            AnalysisState::Retrying,
            AnalysisState::Succeeded,
            AnalysisState::Failed,
            AnalysisState::Cancelled,
            AnalysisState::DeadLettered,
        ]
    }
}

impl fmt::Display for AnalysisState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a state is being left. Named causes, not free text, so the table below
/// is exhaustive and the compiler checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisEvent {
    /// A worker claimed the job.
    Claimed,
    /// The pipeline produced a result.
    Completed,
    /// A retryable failure: timeout, provider 5xx, lost lease.
    TransientFailure,
    /// A failure that will not improve on retry: unreadable document, a rule
    /// set that no longer covers the fiscal year, a validation error.
    PermanentFailure,
    /// The backoff elapsed.
    BackoffElapsed,
    /// The caller asked to stop.
    CancelRequested,
    /// The last attempt was used up.
    AttemptsExhausted,
    /// The lease expired without the worker reporting. Treated as transient:
    /// the pod was probably evicted.
    LeaseExpired,
}

impl AnalysisEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            AnalysisEvent::Claimed => "claimed",
            AnalysisEvent::Completed => "completed",
            AnalysisEvent::TransientFailure => "transient_failure",
            AnalysisEvent::PermanentFailure => "permanent_failure",
            AnalysisEvent::BackoffElapsed => "backoff_elapsed",
            AnalysisEvent::CancelRequested => "cancel_requested",
            AnalysisEvent::AttemptsExhausted => "attempts_exhausted",
            AnalysisEvent::LeaseExpired => "lease_expired",
        }
    }

    pub fn all() -> [AnalysisEvent; 8] {
        [
            AnalysisEvent::Claimed,
            AnalysisEvent::Completed,
            AnalysisEvent::TransientFailure,
            AnalysisEvent::PermanentFailure,
            AnalysisEvent::BackoffElapsed,
            AnalysisEvent::CancelRequested,
            AnalysisEvent::AttemptsExhausted,
            AnalysisEvent::LeaseExpired,
        ]
    }
}

impl fmt::Display for AnalysisEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("analysis cannot move from {from} on {event}")]
pub struct InvalidTransition {
    pub from: AnalysisState,
    pub event: AnalysisEvent,
}

impl AnalysisState {
    /// The whole table. Anything absent is rejected, and that is the point:
    /// a new event or a new state will not compile until every combination has
    /// been considered.
    pub fn try_transition(self, event: AnalysisEvent) -> Result<AnalysisState, InvalidTransition> {
        use AnalysisEvent::*;
        use AnalysisState::*;

        let next = match (self, event) {
            (Queued, Claimed) => Running,
            (Queued, CancelRequested) => Cancelled,
            // A job can be marked permanently broken before anyone claims it,
            // e.g. when its pinned document version has been deleted.
            (Queued, PermanentFailure) => Failed,

            (Running, Completed) => Succeeded,
            (Running, TransientFailure) => Retrying,
            (Running, LeaseExpired) => Retrying,
            (Running, PermanentFailure) => Failed,
            (Running, CancelRequested) => Cancelled,
            (Running, AttemptsExhausted) => DeadLettered,

            (Retrying, BackoffElapsed) => Queued,
            (Retrying, AttemptsExhausted) => DeadLettered,
            (Retrying, CancelRequested) => Cancelled,
            (Retrying, PermanentFailure) => Failed,

            // Everything else, including every transition out of a terminal
            // state, is a bug in the caller.
            (Queued | Running | Retrying, _) => {
                return Err(InvalidTransition { from: self, event })
            }
            (Succeeded | Failed | Cancelled | DeadLettered, _) => {
                return Err(InvalidTransition { from: self, event })
            }
        };
        Ok(next)
    }
}

/// One recorded move, for the audit trail. Written in the same transaction as
/// the state change, so the history cannot disagree with the current value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateTransition {
    pub from: AnalysisState,
    pub to: AnalysisState,
    pub event: AnalysisEvent,
    pub attempt: u32,
    /// Free text for an operator. Never contains document content — see the
    /// classification rules; failures are described by kind, not by data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_happy_path_runs_queued_to_succeeded() {
        let s = AnalysisState::Queued
            .try_transition(AnalysisEvent::Claimed)
            .unwrap();
        assert_eq!(s, AnalysisState::Running);
        let s = s.try_transition(AnalysisEvent::Completed).unwrap();
        assert_eq!(s, AnalysisState::Succeeded);
    }

    #[test]
    fn a_transient_failure_goes_round_the_retry_loop() {
        let s = AnalysisState::Running
            .try_transition(AnalysisEvent::TransientFailure)
            .unwrap();
        assert_eq!(s, AnalysisState::Retrying);
        let s = s.try_transition(AnalysisEvent::BackoffElapsed).unwrap();
        assert_eq!(s, AnalysisState::Queued);
        assert_eq!(
            s.try_transition(AnalysisEvent::Claimed).unwrap(),
            AnalysisState::Running
        );
    }

    #[test]
    fn exhausted_attempts_land_in_the_dead_letter_queue() {
        let s = AnalysisState::Retrying
            .try_transition(AnalysisEvent::AttemptsExhausted)
            .unwrap();
        assert_eq!(s, AnalysisState::DeadLettered);
        assert!(s.is_terminal());
    }

    #[test]
    fn nothing_leaves_a_terminal_state() {
        for state in AnalysisState::all().into_iter().filter(|s| s.is_terminal()) {
            for event in AnalysisEvent::all() {
                assert!(
                    state.try_transition(event).is_err(),
                    "{state} should not accept {event}"
                );
            }
        }
    }

    #[test]
    fn a_succeeded_analysis_cannot_later_fail() {
        assert!(AnalysisState::Succeeded
            .try_transition(AnalysisEvent::PermanentFailure)
            .is_err());
        assert!(AnalysisState::Succeeded
            .try_transition(AnalysisEvent::TransientFailure)
            .is_err());
    }

    #[test]
    fn cancellation_is_accepted_from_every_active_state_and_no_other() {
        for state in AnalysisState::all() {
            let cancelled = state
                .try_transition(AnalysisEvent::CancelRequested)
                .is_ok_and(|s| s == AnalysisState::Cancelled);
            assert_eq!(
                cancelled,
                state.is_cancellable(),
                "{state} disagrees with is_cancellable"
            );
        }
    }

    #[test]
    fn a_lost_lease_is_treated_as_transient_not_as_data_loss() {
        assert_eq!(
            AnalysisState::Running
                .try_transition(AnalysisEvent::LeaseExpired)
                .unwrap(),
            AnalysisState::Retrying
        );
    }

    #[test]
    fn a_queued_job_cannot_complete_without_being_claimed() {
        assert!(AnalysisState::Queued
            .try_transition(AnalysisEvent::Completed)
            .is_err());
    }

    #[test]
    fn every_state_round_trips_through_its_string_form() {
        for state in AnalysisState::all() {
            assert_eq!(AnalysisState::parse(state.as_str()), Some(state));
        }
        assert_eq!(AnalysisState::parse("in_progress"), None);
    }

    #[test]
    fn active_and_terminal_partition_the_state_space() {
        for state in AnalysisState::all() {
            assert_ne!(
                state.is_active(),
                state.is_terminal(),
                "{state} is both or neither"
            );
        }
    }
}

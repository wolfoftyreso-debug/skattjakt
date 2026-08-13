//! Reading and writing how far each cited source has been checked.
//!
//! Not tenant data, so these methods take the pool directly rather than a
//! `Tenant` transaction: the law is the same for every company, and there is no
//! row-level security policy to satisfy. See `0008_source_retrievals.sql` for
//! why the state lives here instead of in the embedded rule set.

use chrono::{DateTime, Duration, Utc};
use sqlx::Row;

use skattjakt_rules::{Retrieval, SourceState};

use crate::{Store, StoreResult};

/// One source's current standing, as the database holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRetrieval {
    pub source_id: String,
    pub state: SourceState,
    pub retrieved_at: Option<DateTime<Utc>>,
    pub sha256: Option<String>,
    pub note: Option<String>,
    pub last_checked_at: DateTime<Utc>,
    pub failure_streak: i32,
}

impl SourceRetrieval {
    /// The shape the rule engine reads.
    pub fn as_retrieval(&self) -> Retrieval {
        Retrieval {
            state: self.state,
            at: self.retrieved_at.map(|t| t.to_rfc3339()),
            sha256: self.sha256.clone(),
            note: self.note.clone(),
        }
    }
}

fn state_of(row: &sqlx::postgres::PgRow) -> SourceState {
    match row.get::<String, _>("state").as_str() {
        "verified" => SourceState::Verified,
        "mismatch" => SourceState::Mismatch,
        "unreachable" => SourceState::Unreachable,
        _ => SourceState::Unretrieved,
    }
}

fn read(row: sqlx::postgres::PgRow) -> SourceRetrieval {
    SourceRetrieval {
        state: state_of(&row),
        source_id: row.get("source_id"),
        retrieved_at: row.get("retrieved_at"),
        sha256: row.get("sha256"),
        note: row.get("note"),
        last_checked_at: row.get("last_checked_at"),
        failure_streak: row.get("failure_streak"),
    }
}

impl Store {
    /// Every recorded retrieval, newest state per source.
    ///
    /// Read on the analysis path, so it is one query returning the whole table
    /// — 24 rows, and a join per finding would be worse in every way.
    pub async fn source_retrievals(&self) -> StoreResult<Vec<SourceRetrieval>> {
        let rows = sqlx::query(
            "SELECT source_id, state, retrieved_at, sha256, note, last_checked_at, failure_streak
             FROM source_retrievals ORDER BY source_id",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(read).collect())
    }

    /// Records a check that read the document, whatever it concluded.
    ///
    /// `retrieved_at` moves because the page was actually read. A verified
    /// result clears the failure streak; a mismatch does not touch it, because
    /// a contradiction is not a failure to reach the source.
    pub async fn record_source_read(
        &self,
        source_id: &str,
        state: SourceState,
        sha256: &str,
        note: Option<&str>,
    ) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO source_retrievals
                 (source_id, state, retrieved_at, sha256, note, last_checked_at, failure_streak)
             VALUES ($1, $2, now(), $3, $4, now(), 0)
             ON CONFLICT (source_id) DO UPDATE SET
                 state           = EXCLUDED.state,
                 retrieved_at    = EXCLUDED.retrieved_at,
                 sha256          = EXCLUDED.sha256,
                 note            = EXCLUDED.note,
                 last_checked_at = EXCLUDED.last_checked_at,
                 failure_streak  = 0",
        )
        .bind(source_id)
        .bind(state.as_str())
        .bind(sha256)
        .bind(note)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Records a check that could not reach the document.
    ///
    /// The load-bearing detail: an earlier successful retrieval is **kept**. A
    /// proxy outage, a DNS failure or a 503 is a fact about the network today,
    /// not about the law, and discarding last week's verified hash because a
    /// gateway said no would make the record less true rather than more. Only a
    /// source that has never been read is moved to `unreachable`, where it
    /// carries the reason so a broken URL is visible rather than silent.
    pub async fn record_source_unreachable(&self, source_id: &str, note: &str) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO source_retrievals
                 (source_id, state, retrieved_at, sha256, note, last_checked_at, failure_streak)
             VALUES ($1, 'unreachable', NULL, NULL, $2, now(), 1)
             ON CONFLICT (source_id) DO UPDATE SET
                 state = CASE
                     WHEN source_retrievals.state IN ('verified', 'mismatch')
                     THEN source_retrievals.state
                     ELSE 'unreachable'
                 END,
                 note            = $2,
                 last_checked_at = now(),
                 failure_streak  = source_retrievals.failure_streak + 1",
        )
        .bind(source_id)
        .bind(note)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Whether a sweep is due: no source has been checked within `interval`.
    ///
    /// Asks for the *oldest* check rather than the newest, so a registry that
    /// gained a source is swept immediately instead of waiting out the interval
    /// on the strength of its neighbours.
    pub async fn sources_due_for_check(&self, interval: Duration) -> StoreResult<bool> {
        let row = sqlx::query(
            "SELECT count(*) AS checked,
                    coalesce(min(last_checked_at), to_timestamp(0)) AS oldest
             FROM source_retrievals",
        )
        .fetch_one(self.pool())
        .await?;
        let checked: i64 = row.get("checked");
        if checked == 0 {
            return Ok(true);
        }
        let oldest: DateTime<Utc> = row.get("oldest");
        Ok(Utc::now() - oldest >= interval)
    }

    /// Takes the cluster-wide right to run a sweep, or returns `None` because
    /// another worker holds it.
    ///
    /// Two workers waking on the same schedule would otherwise both fetch all
    /// 24 documents — wasted requests against somebody else's servers as much
    /// as ours, and a good way to be rate-limited by the authority we depend
    /// on.
    ///
    /// A session-scoped advisory lock, and the guard **holds the connection**
    /// for the life of the sweep. That is not incidental: the lock belongs to
    /// the connection that took it, and running the unlock through the pool
    /// could land on a different connection, silently fail, and leave the lock
    /// held until that connection happened to be recycled. Holding it also
    /// means a worker that dies mid-sweep releases the lock when its socket
    /// closes, rather than blocking every later sweep.
    pub async fn claim_source_sweep(&self) -> StoreResult<Option<SourceSweepGuard>> {
        let mut connection = self.pool().acquire().await?;
        let row = sqlx::query("SELECT pg_try_advisory_lock($1) AS taken")
            .bind(SOURCE_SWEEP_LOCK)
            .fetch_one(&mut *connection)
            .await?;
        if row.get::<bool, _>("taken") {
            Ok(Some(SourceSweepGuard { connection }))
        } else {
            Ok(None)
        }
    }
}

/// An arbitrary but fixed key. Advisory locks share one namespace across the
/// database, so it is written down here rather than computed, where a
/// collision with some future lock is findable by grep.
const SOURCE_SWEEP_LOCK: i64 = 0x5052_CE05;

/// Holds the sweep lock, and the connection that owns it.
#[derive(Debug)]
pub struct SourceSweepGuard {
    connection: sqlx::pool::PoolConnection<sqlx::Postgres>,
}

impl SourceSweepGuard {
    /// Releases the lock and returns the connection to the pool.
    ///
    /// Explicit rather than in `Drop`, because releasing needs to await and a
    /// `Drop` that cannot await would either block a runtime thread or skip the
    /// unlock. Skipping it is survivable — the lock dies with the connection —
    /// but that leaves it held until the pool recycles, which is long enough to
    /// skip a sweep for no reason.
    pub async fn release(mut self) -> StoreResult<()> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(SOURCE_SWEEP_LOCK)
            .execute(&mut *self.connection)
            .await?;
        Ok(())
    }
}

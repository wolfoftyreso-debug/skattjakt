//! Storage for Monte Carlo simulations.
//!
//! Every method here runs inside a `Tenant` transaction, so row-level security
//! is doing the isolation and these queries never carry a `WHERE company_id`
//! for safety — only where it helps the planner. A missing filter is a missing
//! row, not another tenant's row.
//!
//! The division of labour with `skattjakt-simulate` is strict: that crate knows
//! statistics and nothing about storage, this module knows storage and nothing
//! about statistics. Nothing here recomputes anything.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use skattjakt_simulate::{RunOutcome, SimulationSpec};

use crate::page::{Cursor, Page};
use crate::{StoreError, StoreResult, Tenant};

/// Where a run is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Running => "running",
            RunState::Succeeded => "succeeded",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => RunState::Queued,
            "running" => RunState::Running,
            "succeeded" => RunState::Succeeded,
            "failed" => RunState::Failed,
            "cancelled" => RunState::Cancelled,
            _ => return None,
        })
    }

    /// Whether the run is over. A finished run never changes again, which is
    /// what makes a stored result safe to cache and safe to cite.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunState::Succeeded | RunState::Failed | RunState::Cancelled
        )
    }
}

/// Where the work happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Execution {
    /// Small enough to answer inside the request.
    Inline,
    /// Handed to the durable queue.
    Queued,
}

impl Execution {
    pub fn as_str(self) -> &'static str {
        match self {
            Execution::Inline => "inline",
            Execution::Queued => "queued",
        }
    }
}

/// A run row, without its results.
#[derive(Debug, Clone)]
pub struct StoredRun {
    pub id: Uuid,
    pub simulation_id: Uuid,
    pub version_id: Uuid,
    pub state: RunState,
    pub seed: u64,
    pub iterations: i32,
    pub completed_iterations: i32,
    pub engine_version: String,
    pub spec_hash: String,
    pub execution: String,
    pub reason: Option<String>,
    pub requested_by: Option<Uuid>,
    pub requested_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub iterations_per_second: Option<f64>,
    pub quality: Option<Value>,
    pub error: Option<String>,
    pub cancel_requested: bool,
}

impl StoredRun {
    fn from_row(row: &sqlx::postgres::PgRow) -> StoreResult<Self> {
        let seed: String = row.try_get("seed")?;
        Ok(Self {
            id: row.try_get("id")?,
            simulation_id: row.try_get("simulation_id")?,
            version_id: row.try_get("version_id")?,
            state: RunState::parse(row.try_get::<String, _>("state")?.as_str())
                .ok_or(StoreError::NotFound)?,
            // Written by this module as a decimal string and constrained by the
            // schema, so a parse failure means the row was written by something
            // other than this code.
            seed: seed.parse().map_err(|_| StoreError::NotFound)?,
            iterations: row.try_get("iterations")?,
            completed_iterations: row.try_get("completed_iterations")?,
            engine_version: row.try_get("engine_version")?,
            spec_hash: row.try_get("spec_hash")?,
            execution: row.try_get("execution")?,
            reason: row.try_get("reason")?,
            requested_by: row.try_get("requested_by")?,
            requested_at: row.try_get("requested_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            duration_ms: row.try_get("duration_ms")?,
            iterations_per_second: row.try_get("iterations_per_second")?,
            quality: row.try_get("quality")?,
            error: row.try_get("error")?,
            cancel_requested: row.try_get("cancel_requested")?,
        })
    }

    pub fn progress(&self) -> f64 {
        if self.iterations <= 0 {
            return 0.0;
        }
        f64::from(self.completed_iterations) / f64::from(self.iterations)
    }
}

/// A simulation and the version that is current.
#[derive(Debug, Clone)]
pub struct StoredSimulation {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub version: i32,
    pub version_id: Uuid,
    pub spec: Value,
    pub spec_hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const RUN_COLUMNS: &str = "id, simulation_id, version_id, state, seed, iterations, \
     completed_iterations, engine_version, spec_hash, execution, reason, requested_by, \
     requested_at, started_at, finished_at, duration_ms, iterations_per_second, quality, \
     error, cancel_requested";

impl Tenant<'_> {
    /// Creates a simulation and its first version.
    pub async fn create_simulation(
        &mut self,
        id: Uuid,
        spec: &SimulationSpec,
        created_by: Option<Uuid>,
        note: Option<&str>,
    ) -> StoreResult<Uuid> {
        let company = self.company_id().0;
        sqlx::query(
            "INSERT INTO simulations (id, company_id, name, description, current_version, created_by)
             VALUES ($1, $2, $3, $4, 1, $5)",
        )
        .bind(id)
        .bind(company)
        .bind(&spec.name)
        .bind(spec.description.as_deref())
        .bind(created_by)
        .execute(&mut *self.tx)
        .await?;

        self.insert_version(id, 1, spec, created_by, note).await
    }

    /// Appends a version. The previous one is never touched.
    pub async fn add_simulation_version(
        &mut self,
        simulation_id: Uuid,
        spec: &SimulationSpec,
        created_by: Option<Uuid>,
        note: Option<&str>,
    ) -> StoreResult<(Uuid, i32)> {
        let next: i32 = sqlx::query_scalar(
            "UPDATE simulations SET current_version = current_version + 1,
                    name = $2, description = $3, updated_at = now()
             WHERE id = $1 RETURNING current_version",
        )
        .bind(simulation_id)
        .bind(&spec.name)
        .bind(spec.description.as_deref())
        .fetch_optional(&mut *self.tx)
        .await?
        .ok_or(StoreError::NotFound)?;

        let version_id = self
            .insert_version(simulation_id, next, spec, created_by, note)
            .await?;
        Ok((version_id, next))
    }

    async fn insert_version(
        &mut self,
        simulation_id: Uuid,
        version: i32,
        spec: &SimulationSpec,
        created_by: Option<Uuid>,
        note: Option<&str>,
    ) -> StoreResult<Uuid> {
        let company = self.company_id().0;
        let version_id = Uuid::new_v4();
        let document =
            serde_json::to_value(spec).map_err(|error| StoreError::Corrupt(error.to_string()))?;

        sqlx::query(
            "INSERT INTO simulation_versions
                 (id, company_id, simulation_id, version, spec, spec_hash, note, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(version_id)
        .bind(company)
        .bind(simulation_id)
        .bind(version)
        .bind(&document)
        .bind(spec.hash())
        .bind(note)
        .bind(created_by)
        .execute(&mut *self.tx)
        .await?;

        // The normalised rows. Derived from the same spec in the same
        // transaction, so they cannot disagree with it.
        for (position, input) in spec.inputs.iter().enumerate() {
            let summary = input.summary();
            sqlx::query(
                "INSERT INTO simulation_inputs
                     (id, company_id, version_id, position, input_id, name, distribution_kind,
                      parameters, unit, source, confidence, description, constraints,
                      mean, std_dev)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
            )
            .bind(Uuid::new_v4())
            .bind(company)
            .bind(version_id)
            .bind(position as i32)
            .bind(&input.id)
            .bind(&input.name)
            .bind(input.distribution.kind())
            .bind(&summary.parameters)
            .bind(input.unit.as_deref())
            .bind(input.source.as_deref())
            .bind(
                input
                    .confidence
                    .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
                    .and_then(|v| v.as_str().map(str::to_string)),
            )
            .bind(input.description.as_deref())
            .bind(
                input
                    .constraints
                    .map(|c| serde_json::to_value(c).unwrap_or(Value::Null)),
            )
            // A distribution with an infinite mean has no finite summary, and
            // Postgres accepts NaN in a double column. Storing it would put a
            // NaN on a screen; there is no such distribution among the eleven,
            // so this is a guard rather than a path.
            .bind(if summary.mean.is_finite() {
                summary.mean
            } else {
                0.0
            })
            .bind(if summary.std_dev.is_finite() {
                summary.std_dev
            } else {
                0.0
            })
            .execute(&mut *self.tx)
            .await?;
        }

        for (position, output) in spec.outputs.iter().enumerate() {
            let direction = serde_json::to_value(output.target_direction)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "at_least".to_string());
            sqlx::query(
                "INSERT INTO simulation_outputs
                     (id, company_id, version_id, position, output_id, name, expression,
                      unit, description, target, target_direction, critical_threshold)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(Uuid::new_v4())
            .bind(company)
            .bind(version_id)
            .bind(position as i32)
            .bind(&output.id)
            .bind(&output.name)
            .bind(&output.expression)
            .bind(output.unit.as_deref())
            .bind(output.description.as_deref())
            .bind(output.target)
            .bind(direction)
            .bind(output.critical_threshold)
            .execute(&mut *self.tx)
            .await?;
        }

        Ok(version_id)
    }

    pub async fn simulation(&mut self, id: Uuid) -> StoreResult<StoredSimulation> {
        let row = sqlx::query(
            "SELECT s.id, s.name, s.description, s.created_at, s.updated_at,
                    v.id AS version_id, v.version, v.spec, v.spec_hash
             FROM simulations s
             JOIN simulation_versions v
               ON v.simulation_id = s.id AND v.version = s.current_version
             WHERE s.id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await?
        .ok_or(StoreError::NotFound)?;

        Ok(StoredSimulation {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            description: row.try_get("description")?,
            version: row.try_get("version")?,
            version_id: row.try_get("version_id")?,
            spec: row.try_get("spec")?,
            spec_hash: row.try_get("spec_hash")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }

    /// A specific version's specification, for reproducing an old run.
    pub async fn simulation_version_spec(&mut self, version_id: Uuid) -> StoreResult<Value> {
        sqlx::query_scalar("SELECT spec FROM simulation_versions WHERE id = $1")
            .bind(version_id)
            .fetch_optional(&mut *self.tx)
            .await?
            .ok_or(StoreError::NotFound)
    }

    pub async fn list_simulations(
        &mut self,
        after: Option<Cursor>,
        limit: i64,
    ) -> StoreResult<Page<Value>> {
        let rows = sqlx::query(
            "SELECT s.id, s.name, s.description, s.current_version, s.created_at, s.updated_at,
                    (SELECT count(*) FROM simulation_runs r WHERE r.simulation_id = s.id) AS runs
             FROM simulations s
             WHERE ($1::timestamptz IS NULL
                    OR (s.created_at, s.id) < ($1::timestamptz, $2::uuid))
             ORDER BY s.created_at DESC, s.id DESC
             LIMIT $3",
        )
        .bind(after.as_ref().map(|c| c.at))
        .bind(after.as_ref().map(|c| c.id))
        .bind(limit + 1)
        .fetch_all(&mut *self.tx)
        .await?;

        let mut items = Vec::new();
        let mut last = None;
        for row in rows.iter().take(limit as usize) {
            let id: Uuid = row.try_get("id")?;
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            last = Some(Cursor { at: created_at, id });
            items.push(json!({
                "id": id,
                "name": row.try_get::<String, _>("name")?,
                "description": row.try_get::<Option<String>, _>("description")?,
                "version": row.try_get::<i32, _>("current_version")?,
                "runs": row.try_get::<i64, _>("runs")?,
                "created_at": created_at,
                "updated_at": row.try_get::<DateTime<Utc>, _>("updated_at")?,
            }));
        }

        let has_more = rows.len() as i64 > limit;
        Ok(Page {
            items,
            next: if has_more { last } else { None },
        })
    }

    /// Records a run before any work starts.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_run(
        &mut self,
        id: Uuid,
        simulation_id: Uuid,
        version_id: Uuid,
        spec_hash: &str,
        seed: u64,
        iterations: i32,
        execution: Execution,
        reason: Option<&str>,
        requested_by: Option<Uuid>,
    ) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO simulation_runs
                 (id, company_id, simulation_id, version_id, state, seed, iterations,
                  engine_version, spec_hash, execution, reason, requested_by)
             VALUES ($1, $2, $3, $4, 'queued', $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(id)
        .bind(self.company_id().0)
        .bind(simulation_id)
        .bind(version_id)
        .bind(seed.to_string())
        .bind(iterations)
        .bind(skattjakt_simulate::ENGINE_VERSION)
        .bind(spec_hash)
        .bind(execution.as_str())
        .bind(reason)
        .bind(requested_by)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    pub async fn run(&mut self, id: Uuid) -> StoreResult<StoredRun> {
        let sql = format!("SELECT {RUN_COLUMNS} FROM simulation_runs WHERE id = $1");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(&mut *self.tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        StoredRun::from_row(&row)
    }

    /// The most recent run of a simulation, whatever state it is in.
    pub async fn latest_run(&mut self, simulation_id: Uuid) -> StoreResult<StoredRun> {
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM simulation_runs WHERE simulation_id = $1
             ORDER BY requested_at DESC, id DESC LIMIT 1"
        );
        let row = sqlx::query(&sql)
            .bind(simulation_id)
            .fetch_optional(&mut *self.tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        StoredRun::from_row(&row)
    }

    /// Runs of one simulation, newest first. The list a scenario comparison is
    /// built from.
    pub async fn list_runs(&mut self, simulation_id: Uuid, limit: i64) -> StoreResult<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT r.id, r.state, r.seed, r.iterations, r.completed_iterations,
                    r.requested_at, r.finished_at, r.duration_ms, r.error, r.reason,
                    r.engine_version, r.spec_hash, r.execution, v.version
             FROM simulation_runs r
             JOIN simulation_versions v ON v.id = r.version_id
             WHERE r.simulation_id = $1
             ORDER BY r.requested_at DESC, r.id DESC LIMIT $2",
        )
        .bind(simulation_id)
        .bind(limit)
        .fetch_all(&mut *self.tx)
        .await?;

        rows.iter()
            .map(|row| {
                Ok(json!({
                    "id": row.try_get::<Uuid, _>("id")?,
                    "state": row.try_get::<String, _>("state")?,
                    "seed": row.try_get::<String, _>("seed")?,
                    "iterations": row.try_get::<i32, _>("iterations")?,
                    "completed_iterations": row.try_get::<i32, _>("completed_iterations")?,
                    "model_version": row.try_get::<i32, _>("version")?,
                    "engine_version": row.try_get::<String, _>("engine_version")?,
                    "spec_hash": row.try_get::<String, _>("spec_hash")?,
                    "execution": row.try_get::<String, _>("execution")?,
                    "reason": row.try_get::<Option<String>, _>("reason")?,
                    "requested_at": row.try_get::<DateTime<Utc>, _>("requested_at")?,
                    "finished_at": row.try_get::<Option<DateTime<Utc>>, _>("finished_at")?,
                    "duration_ms": row.try_get::<Option<i64>, _>("duration_ms")?,
                    "error": row.try_get::<Option<String>, _>("error")?,
                }))
            })
            .collect()
    }

    pub async fn mark_run_running(&mut self, id: Uuid) -> StoreResult<()> {
        sqlx::query(
            "UPDATE simulation_runs SET state = 'running', started_at = now()
             WHERE id = $1 AND state = 'queued'",
        )
        .bind(id)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// Publishes progress, and reports back whether a cancellation is waiting.
    ///
    /// One statement for both directions, because the worker asks these two
    /// questions at the same moment and a second round trip per batch is a
    /// second round trip per batch.
    pub async fn report_run_progress(&mut self, id: Uuid, completed: i32) -> StoreResult<bool> {
        let cancel: Option<bool> = sqlx::query_scalar(
            "UPDATE simulation_runs SET completed_iterations = $2
             WHERE id = $1 RETURNING cancel_requested",
        )
        .bind(id)
        .bind(completed)
        .fetch_optional(&mut *self.tx)
        .await?;
        Ok(cancel.unwrap_or(false))
    }

    /// Asks a run to stop. Returns false if it had already finished.
    pub async fn request_run_cancellation(
        &mut self,
        id: Uuid,
        by: Option<Uuid>,
    ) -> StoreResult<bool> {
        let affected = sqlx::query(
            "UPDATE simulation_runs
             SET cancel_requested = TRUE, cancel_requested_by = $2
             WHERE id = $1 AND state IN ('queued', 'running')",
        )
        .bind(id)
        .bind(by)
        .execute(&mut *self.tx)
        .await?;
        Ok(affected.rows_affected() > 0)
    }

    pub async fn fail_run(&mut self, id: Uuid, error: &str) -> StoreResult<()> {
        sqlx::query(
            "UPDATE simulation_runs SET state = 'failed', finished_at = now(), error = $2
             WHERE id = $1 AND state NOT IN ('succeeded', 'cancelled')",
        )
        .bind(id)
        .bind(error)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    pub async fn cancel_run(&mut self, id: Uuid, completed: i32) -> StoreResult<()> {
        sqlx::query(
            "UPDATE simulation_runs
             SET state = 'cancelled', finished_at = now(), completed_iterations = $2
             WHERE id = $1 AND state NOT IN ('succeeded', 'failed')",
        )
        .bind(id)
        .bind(completed)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// Writes a finished run's results.
    ///
    /// Every table in one transaction: a run marked `succeeded` with no
    /// statistics beside it would be a result nobody could read and nobody
    /// could tell was missing.
    pub async fn complete_run(&mut self, id: Uuid, outcome: &RunOutcome) -> StoreResult<()> {
        let company = self.company_id().0;
        let quality = serde_json::to_value(&outcome.quality)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;

        sqlx::query(
            "UPDATE simulation_runs
             SET state = 'succeeded', finished_at = now(), duration_ms = $2,
                 iterations_per_second = $3, completed_iterations = $4, quality = $5,
                 engine_version = $6, spec_hash = $7
             WHERE id = $1",
        )
        .bind(id)
        .bind(outcome.duration_ms as i64)
        .bind(outcome.iterations_per_second)
        .bind(outcome.iterations as i32)
        .bind(&quality)
        .bind(outcome.engine_version)
        .bind(&outcome.spec_hash)
        .execute(&mut *self.tx)
        .await?;

        // The names and units live on the version rather than in the outcome,
        // so a stored statistic can be displayed without a second lookup.
        let labels: Vec<(String, String, Option<String>)> = sqlx::query(
            "SELECT o.output_id, o.name, o.unit FROM simulation_outputs o
             JOIN simulation_runs r ON r.version_id = o.version_id
             WHERE r.id = $1",
        )
        .bind(id)
        .fetch_all(&mut *self.tx)
        .await?
        .iter()
        .map(|row| {
            Ok((
                row.try_get("output_id")?,
                row.try_get("name")?,
                row.try_get("unit")?,
            ))
        })
        .collect::<StoreResult<Vec<_>>>()?;

        let label_for = |output_id: &str| -> (String, Option<String>) {
            labels
                .iter()
                .find(|(id, _, _)| id == output_id)
                .map(|(_, name, unit)| (name.clone(), unit.clone()))
                .unwrap_or_else(|| (output_id.to_string(), None))
        };

        for (output_id, statistics) in &outcome.statistics {
            let (name, unit) = label_for(output_id);
            let interval = statistics.mean_confidence_interval_95;
            sqlx::query(
                "INSERT INTO simulation_statistics
                     (run_id, company_id, output_id, name, unit, sample_count, mean, median,
                      minimum, maximum, std_dev, variance, p5, p10, p25, p50, p75, p90, p95,
                      p99, probability_of_target, probability_of_loss,
                      probability_below_threshold, probability_above_threshold,
                      mean_ci_low, mean_ci_high, relative_standard_error)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                         $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27)",
            )
            .bind(id)
            .bind(company)
            .bind(output_id)
            .bind(&name)
            .bind(unit.as_deref())
            .bind(statistics.count as i64)
            .bind(statistics.mean)
            .bind(statistics.median)
            .bind(statistics.min)
            .bind(statistics.max)
            .bind(statistics.std_dev)
            .bind(statistics.variance)
            .bind(statistics.p5)
            .bind(statistics.p10)
            .bind(statistics.p25)
            .bind(statistics.p50)
            .bind(statistics.p75)
            .bind(statistics.p90)
            .bind(statistics.p95)
            .bind(statistics.p99)
            .bind(statistics.probability_of_target)
            .bind(statistics.probability_of_loss)
            .bind(statistics.probability_below_threshold)
            .bind(statistics.probability_above_threshold)
            .bind(interval.map(|i| i[0]))
            .bind(interval.map(|i| i[1]))
            .bind(statistics.relative_standard_error)
            .execute(&mut *self.tx)
            .await?;
        }

        for report in &outcome.sensitivity {
            for entry in &report.inputs {
                sqlx::query(
                    "INSERT INTO simulation_sensitivity
                         (run_id, company_id, output_id, input_id, input_name, correlation,
                          rank_correlation, variance_contribution, influence_rank, referenced,
                          sample_size)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                )
                .bind(id)
                .bind(company)
                .bind(&report.output_id)
                .bind(&entry.input_id)
                .bind(&entry.input_name)
                .bind(entry.correlation)
                .bind(entry.rank_correlation)
                .bind(entry.variance_contribution)
                .bind(entry.rank as i32)
                .bind(entry.referenced)
                .bind(report.sample_size as i32)
                .execute(&mut *self.tx)
                .await?;
            }
        }

        for report in &outcome.convergence {
            for checkpoint in &report.checkpoints {
                sqlx::query(
                    "INSERT INTO simulation_convergence
                         (run_id, company_id, output_id, iterations, mean, median, p10, p90,
                          stable, largest_relative_change, warning)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                )
                .bind(id)
                .bind(company)
                .bind(&report.output_id)
                .bind(checkpoint.iterations as i32)
                .bind(checkpoint.mean)
                .bind(checkpoint.median)
                .bind(checkpoint.p10)
                .bind(checkpoint.p90)
                .bind(report.stable)
                .bind(if report.largest_relative_change.is_finite() {
                    Some(report.largest_relative_change)
                } else {
                    None
                })
                .bind(report.warning.as_deref())
                .execute(&mut *self.tx)
                .await?;
            }
        }

        for shape in &outcome.shapes {
            let payload = serde_json::to_value(shape)
                .map_err(|error| StoreError::Corrupt(error.to_string()))?;
            sqlx::query(
                "INSERT INTO simulation_shapes (run_id, company_id, output_id, payload)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(company)
            .bind(&shape.output_id)
            .bind(&payload)
            .execute(&mut *self.tx)
            .await?;
        }

        Ok(())
    }

    pub async fn run_statistics(&mut self, run_id: Uuid) -> StoreResult<Vec<Value>> {
        let rows =
            sqlx::query("SELECT * FROM simulation_statistics WHERE run_id = $1 ORDER BY output_id")
                .bind(run_id)
                .fetch_all(&mut *self.tx)
                .await?;

        rows.iter()
            .map(|row| {
                Ok(json!({
                    "output_id": row.try_get::<String, _>("output_id")?,
                    "name": row.try_get::<String, _>("name")?,
                    "unit": row.try_get::<Option<String>, _>("unit")?,
                    "count": row.try_get::<i64, _>("sample_count")?,
                    "mean": row.try_get::<f64, _>("mean")?,
                    "median": row.try_get::<f64, _>("median")?,
                    "min": row.try_get::<f64, _>("minimum")?,
                    "max": row.try_get::<f64, _>("maximum")?,
                    "std_dev": row.try_get::<f64, _>("std_dev")?,
                    "variance": row.try_get::<f64, _>("variance")?,
                    "p5": row.try_get::<f64, _>("p5")?,
                    "p10": row.try_get::<f64, _>("p10")?,
                    "p25": row.try_get::<f64, _>("p25")?,
                    "p50": row.try_get::<f64, _>("p50")?,
                    "p75": row.try_get::<f64, _>("p75")?,
                    "p90": row.try_get::<f64, _>("p90")?,
                    "p95": row.try_get::<f64, _>("p95")?,
                    "p99": row.try_get::<f64, _>("p99")?,
                    "probability_of_target": row.try_get::<Option<f64>, _>("probability_of_target")?,
                    "probability_of_loss": row.try_get::<f64, _>("probability_of_loss")?,
                    "probability_below_threshold":
                        row.try_get::<Option<f64>, _>("probability_below_threshold")?,
                    "probability_above_threshold":
                        row.try_get::<Option<f64>, _>("probability_above_threshold")?,
                    "mean_confidence_interval_95": match (
                        row.try_get::<Option<f64>, _>("mean_ci_low")?,
                        row.try_get::<Option<f64>, _>("mean_ci_high")?,
                    ) {
                        (Some(low), Some(high)) => json!([low, high]),
                        _ => Value::Null,
                    },
                    "relative_standard_error":
                        row.try_get::<Option<f64>, _>("relative_standard_error")?,
                }))
            })
            .collect()
    }

    pub async fn run_sensitivity(&mut self, run_id: Uuid) -> StoreResult<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT * FROM simulation_sensitivity WHERE run_id = $1
             ORDER BY output_id, influence_rank",
        )
        .bind(run_id)
        .fetch_all(&mut *self.tx)
        .await?;

        rows.iter()
            .map(|row| {
                Ok(json!({
                    "output_id": row.try_get::<String, _>("output_id")?,
                    "input_id": row.try_get::<String, _>("input_id")?,
                    "input_name": row.try_get::<String, _>("input_name")?,
                    "correlation": row.try_get::<Option<f64>, _>("correlation")?,
                    "rank_correlation": row.try_get::<Option<f64>, _>("rank_correlation")?,
                    "variance_contribution": row.try_get::<f64, _>("variance_contribution")?,
                    "rank": row.try_get::<i32, _>("influence_rank")?,
                    "referenced": row.try_get::<bool, _>("referenced")?,
                    "sample_size": row.try_get::<i32, _>("sample_size")?,
                }))
            })
            .collect()
    }

    pub async fn run_convergence(&mut self, run_id: Uuid) -> StoreResult<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT * FROM simulation_convergence WHERE run_id = $1
             ORDER BY output_id, iterations",
        )
        .bind(run_id)
        .fetch_all(&mut *self.tx)
        .await?;

        rows.iter()
            .map(|row| {
                Ok(json!({
                    "output_id": row.try_get::<String, _>("output_id")?,
                    "iterations": row.try_get::<i32, _>("iterations")?,
                    "mean": row.try_get::<f64, _>("mean")?,
                    "median": row.try_get::<f64, _>("median")?,
                    "p10": row.try_get::<f64, _>("p10")?,
                    "p90": row.try_get::<f64, _>("p90")?,
                    "stable": row.try_get::<bool, _>("stable")?,
                    "largest_relative_change":
                        row.try_get::<Option<f64>, _>("largest_relative_change")?,
                    "warning": row.try_get::<Option<String>, _>("warning")?,
                }))
            })
            .collect()
    }

    pub async fn run_shapes(&mut self, run_id: Uuid) -> StoreResult<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT payload FROM simulation_shapes WHERE run_id = $1 ORDER BY output_id",
        )
        .bind(run_id)
        .fetch_all(&mut *self.tx)
        .await?;
        rows.iter()
            .map(|row| Ok(row.try_get::<Value, _>("payload")?))
            .collect()
    }
}

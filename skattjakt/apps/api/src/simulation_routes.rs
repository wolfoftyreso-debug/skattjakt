//! The Monte Carlo HTTP surface.
//!
//! Nine endpoints, all tenant-scoped, all behind the same authorisation as the
//! rest of `/v1`. Three decisions here are worth reading before the code.
//!
//! **Seeds travel as strings.** A seed is a full 64-bit value and a JSON number
//! silently loses precision above 2^53 — a client would round-trip a seed,
//! send it back, and get a different simulation with no error anywhere. Section
//! 12 says a run must be reproducible, so the wire format is a decimal string.
//! Requests may send either for convenience; responses always return a string.
//!
//! **The engine decides where a run happens.** Section 3 asks the system to
//! choose between running locally and running server-side by size. Small runs
//! answer inside the request, because a round trip through a queue for eighty
//! milliseconds of arithmetic is latency with nothing to show for it. Large
//! ones go to the durable queue, because a two-minute computation inside an
//! HTTP request dies with the first rolling deploy. `execution_for` holds the
//! rule and the response says which way it went.
//!
//! **An inline run still does not block the runtime.** It goes through
//! `spawn_blocking`. The engine is a tight arithmetic loop with no `await` in
//! it; run directly on a Tokio worker thread it would stall every other request
//! that thread was multiplexing.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use skattjakt_identity::Permission;
use skattjakt_jobs::{IdempotencyKey, JobKind, NewJob};
use skattjakt_simulate::{RunConfig, RunControl, SimulationSpec};
use skattjakt_store::governance::RateBucket;
use skattjakt_store::page::{Cursor, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE};
use skattjakt_store::simulations::{Execution, RunState};
use skattjakt_telemetry::metrics::{names, LabelSet};

use crate::correlation_id;
use crate::routes::{company_scope, internal, store};
use crate::{authorise, AppState, Problem};

/// The largest run that answers inside the request.
///
/// Set from measurement rather than taste: the engine runs a realistic
/// eight-input, three-output model at roughly half a million iterations a
/// second in release, so fifty thousand is about a tenth of a second. Anything
/// larger is queued.
pub const INLINE_ITERATION_LIMIT: u32 = 50_000;

/// Whether a run of this size belongs in the request or on the queue.
pub fn execution_for(iterations: u32, outputs: usize) -> Execution {
    // Outputs multiply the work per iteration, so the threshold falls as the
    // model widens. A sixteen-output model at fifty thousand iterations is
    // eight hundred thousand expression evaluations, which is no longer a
    // request.
    let weighted = u64::from(iterations) * outputs.max(1) as u64;
    if weighted <= u64::from(INLINE_ITERATION_LIMIT) * 3 {
        Execution::Inline
    } else {
        Execution::Queued
    }
}

/// A seed on the wire: a string, or a number for a client that sends one.
#[derive(Debug, Clone, Copy)]
pub struct Seed(pub u64);

impl<'de> Deserialize<'de> for Seed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(text) => {
                text.trim().parse::<u64>().map(Seed).map_err(|_| {
                    D::Error::custom("a seed must be a whole number from 0 to 2^64 − 1")
                })
            }
            Value::Number(number) => number
                .as_u64()
                .map(Seed)
                .ok_or_else(|| D::Error::custom("a seed must be a non-negative whole number")),
            _ => Err(D::Error::custom("a seed must be a string or a number")),
        }
    }
}

impl Serialize for Seed {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateSimulationRequest {
    #[serde(flatten)]
    pub spec: SimulationSpec,
    /// Why this model exists. Recorded on the version.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RunRequest {
    #[serde(default)]
    pub iterations: Option<u32>,
    #[serde(default)]
    pub seed: Option<Seed>,
    /// Section 13's WHY. Stored on the run and in the audit trail.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct RunQuery {
    /// Which run to read. Defaults to the most recent, which is what a client
    /// that has just started one wants; naming an older one is how a scenario
    /// comparison reads both sides.
    #[serde(default)]
    pub run: Option<Uuid>,
}

fn spec_error(error: impl std::fmt::Display) -> Problem {
    Problem {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        title: "invalid simulation model".into(),
        detail: error.to_string(),
    }
}

fn engine_error(error: skattjakt_simulate::EngineError) -> Problem {
    use skattjakt_simulate::EngineError;
    match error {
        EngineError::Cancelled { .. } => Problem {
            status: StatusCode::CONFLICT,
            title: "simulation cancelled".into(),
            detail: error.to_string(),
        },
        // Everything else is a specification the engine could not run, and the
        // message names the input, output or iteration that caused it. It is a
        // 422 rather than a 500: nothing failed, the request asked for
        // something that has no answer.
        other => Problem {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            title: "simulation cannot run".into(),
            detail: other.to_string(),
        },
    }
}

fn not_found(error: skattjakt_store::StoreError) -> Problem {
    match error {
        skattjakt_store::StoreError::NotFound => Problem {
            status: StatusCode::NOT_FOUND,
            title: "not found".into(),
            // The same answer as for a simulation belonging to another tenant.
            // Whether it exists is not this caller's business.
            detail: "no simulation with that identifier".into(),
        },
        other => internal(other),
    }
}

/// The distributions the engine offers, so a client does not carry its own copy.
pub async fn catalogue(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    // Authenticated, but needing no permission inside a company: it is a
    // description of the software, not of anyone's data.
    authorise(&state, &headers).await?;
    Ok(Json(json!({
        "distributions": skattjakt_simulate::catalogue(),
        "limits": {
            "max_inputs": skattjakt_simulate::MAX_INPUTS,
            "max_outputs": skattjakt_simulate::MAX_OUTPUTS,
            "min_iterations": skattjakt_simulate::MIN_ITERATIONS,
            "max_iterations": skattjakt_simulate::MAX_ITERATIONS,
            "max_sample_cells": skattjakt_simulate::MAX_SAMPLE_CELLS,
            "inline_iteration_limit": INLINE_ITERATION_LIMIT,
            "sensitivity_sample": skattjakt_simulate::SENSITIVITY_SAMPLE,
        },
        "engine_version": skattjakt_simulate::ENGINE_VERSION,
        "disclaimer": skattjakt_simulate::DISCLAIMER,
    }))
    .into_response())
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateSimulationRequest>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::RunSimulation)?;
    let store = store(&state)?.clone();

    // Compiled before anything is written. A model that cannot run is not a
    // model worth storing, and finding out at run time would mean a stored
    // simulation that fails every time anyone presses the button.
    request.spec.compile().map_err(spec_error)?;

    let id = Uuid::new_v4();
    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    tenant
        .create_simulation(id, &request.spec, scope.user_id(), request.note.as_deref())
        .await
        .map_err(internal)?;
    tenant
        .audit_as(
            actor(&scope),
            "simulation.created",
            Some(id),
            json!({
                "name": request.spec.name,
                "inputs": request.spec.inputs.len(),
                "outputs": request.spec.outputs.len(),
                "spec_hash": request.spec.hash(),
                "note": request.note,
            }),
        )
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "version": 1,
            "spec_hash": request.spec.hash(),
            "engine_version": skattjakt_simulate::ENGINE_VERSION,
        })),
    )
        .into_response())
}

/// A new version of an existing model. The old one keeps working.
pub async fn add_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<CreateSimulationRequest>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::RunSimulation)?;
    let store = store(&state)?.clone();

    request.spec.compile().map_err(spec_error)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    // Reading it first turns "no such simulation" into a 404 rather than into
    // an update that affects no rows and reports success.
    tenant.simulation(id).await.map_err(not_found)?;

    let (_, version) = tenant
        .add_simulation_version(id, &request.spec, scope.user_id(), request.note.as_deref())
        .await
        .map_err(not_found)?;
    tenant
        .audit_as(
            actor(&scope),
            "simulation.version_added",
            Some(id),
            json!({
                "version": version,
                "spec_hash": request.spec.hash(),
                "note": request.note,
            }),
        )
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "version": version,
            "spec_hash": request.spec.hash(),
        })),
    )
        .into_response())
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::ReadSimulation)?;
    let store = store(&state)?.clone();

    let cursor = match query.cursor.as_deref() {
        Some(raw) => Some(Cursor::decode(raw).ok_or_else(|| Problem {
            status: StatusCode::BAD_REQUEST,
            title: "invalid cursor".into(),
            detail: "the cursor was not one this API issued".into(),
        })?),
        None => None,
    };
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let page = tenant
        .list_simulations(cursor, limit)
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok(Json(json!({
        "simulations": page.items,
        "next_cursor": page.next.map(|c| c.encode()),
    }))
    .into_response())
}

pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::ReadSimulation)?;
    let store = store(&state)?.clone();

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let simulation = tenant.simulation(id).await.map_err(not_found)?;
    let runs = tenant.list_runs(id, 20).await.map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    // The derived facts about each input — mean, spread, support — computed
    // from the distribution rather than stored beside it, so they cannot
    // disagree with what will actually be sampled.
    let spec: SimulationSpec = serde_json::from_value(simulation.spec.clone()).map_err(internal)?;
    let inputs: Vec<Value> = spec
        .inputs
        .iter()
        .map(|input| serde_json::to_value(input.summary()).unwrap_or(Value::Null))
        .collect();

    Ok(Json(json!({
        "id": simulation.id,
        "name": simulation.name,
        "description": simulation.description,
        "version": simulation.version,
        "spec_hash": simulation.spec_hash,
        "spec": simulation.spec,
        "inputs": inputs,
        "outputs": spec.outputs,
        "runs": runs,
        "created_at": simulation.created_at,
        "updated_at": simulation.updated_at,
        "engine_version": skattjakt_simulate::ENGINE_VERSION,
        "disclaimer": skattjakt_simulate::DISCLAIMER,
    }))
    .into_response())
}

pub async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    span: Option<axum::extract::Extension<skattjakt_telemetry::SpanContext>>,
    Json(request): Json<RunRequest>,
) -> Result<Response, Problem> {
    let span = span.map(|axum::extract::Extension(s)| s);
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::RunSimulation)?;
    let store = store(&state)?.clone();

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;

    let decision = tenant
        .check_rate_limit(RateBucket::Simulation)
        .await
        .map_err(internal)?;
    if !decision.allowed {
        tenant.commit().await.map_err(internal)?;
        state.metrics.increment(
            names::RATE_LIMITED,
            LabelSet::new().enumerated("bucket", "simulation"),
        );
        return Err(Problem::rate_limited(decision.limit, decision.resets_at));
    }

    let simulation = tenant.simulation(id).await.map_err(not_found)?;
    let spec: SimulationSpec = serde_json::from_value(simulation.spec.clone()).map_err(internal)?;
    let compiled = spec.compile().map_err(spec_error)?;

    let iterations = request.iterations.unwrap_or(10_000);
    // A seed the caller did not choose is drawn once, here, and stored — never
    // regenerated at run time. A run whose seed was invented by the worker and
    // not written down is a run nobody can reproduce.
    let seed = request.seed.map(|s| s.0).unwrap_or_else(random_seed);
    let config = RunConfig { iterations, seed };
    config.validate().map_err(spec_error)?;

    let execution = execution_for(iterations, spec.outputs.len());
    let run_id = Uuid::new_v4();

    tenant
        .create_run(
            run_id,
            id,
            simulation.version_id,
            &simulation.spec_hash,
            seed,
            iterations as i32,
            execution,
            request.reason.as_deref(),
            scope.user_id(),
        )
        .await
        .map_err(internal)?;
    tenant
        .audit_as(
            actor(&scope),
            "simulation.run_requested",
            Some(id),
            json!({
                "run_id": run_id,
                "seed": seed.to_string(),
                "iterations": iterations,
                "model_version": simulation.version,
                "spec_hash": simulation.spec_hash,
                "engine_version": skattjakt_simulate::ENGINE_VERSION,
                "execution": execution.as_str(),
                "reason": request.reason,
            }),
        )
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    state.metrics.increment(
        names::SIMULATIONS_STARTED,
        LabelSet::new().enumerated("execution", execution.as_str()),
    );

    match execution {
        Execution::Inline => {
            // `spawn_blocking`, because the engine is a few hundred million
            // floating-point operations with no yield point in them.
            let control = RunControl::new();
            let outcome = tokio::task::spawn_blocking(move || {
                skattjakt_simulate::run(&compiled, config, &control)
            })
            .await
            .map_err(internal)?;

            let mut tenant = store.tenant(company_id).await.map_err(internal)?;
            match outcome {
                Ok(outcome) => {
                    tenant.mark_run_running(run_id).await.map_err(internal)?;
                    tenant
                        .complete_run(run_id, &outcome)
                        .await
                        .map_err(internal)?;
                    let unstable = outcome
                        .convergence
                        .iter()
                        .filter(|report| !report.stable)
                        .count();
                    tenant
                        .audit_as(
                            actor(&scope),
                            "simulation.run_completed",
                            Some(id),
                            json!({
                                "run_id": run_id,
                                "duration_ms": outcome.duration_ms,
                                "iterations": outcome.iterations,
                                "unstable_outputs": unstable,
                            }),
                        )
                        .await
                        .map_err(internal)?;
                    tenant.commit().await.map_err(internal)?;

                    record_finish(&state, &outcome, unstable);

                    Ok((StatusCode::OK, Json(run_body(run_id, &outcome))).into_response())
                }
                Err(error) => {
                    tenant
                        .fail_run(run_id, &error.to_string())
                        .await
                        .map_err(internal)?;
                    tenant.commit().await.map_err(internal)?;
                    state.metrics.increment(
                        names::SIMULATIONS_FINISHED,
                        LabelSet::new().enumerated("outcome", "failed"),
                    );
                    Err(engine_error(error))
                }
            }
        }
        Execution::Queued => {
            let queue = crate::routes::require_queue(&state)?;
            let enqueued = queue
                .enqueue(NewJob {
                    kind: JobKind::Simulation,
                    company_id: company_id.0,
                    subject_id: run_id,
                    // Derived from the run rather than the model: two runs of
                    // the same model with different seeds are two pieces of
                    // work, and a retried request for the same run is one.
                    idempotency_key: IdempotencyKey::derived(
                        JobKind::Simulation,
                        company_id.0,
                        &[run_id],
                    ),
                    correlation_id: correlation_id(&headers),
                    traceparent: span.map(|s| s.traceparent()),
                    delay: None,
                })
                .await
                .map_err(crate::routes::map_queue_error)?;

            Ok((
                StatusCode::ACCEPTED,
                Json(json!({
                    "run_id": run_id,
                    "job_id": enqueued.job_id(),
                    "state": "queued",
                    "execution": "queued",
                    "seed": seed.to_string(),
                    "iterations": iterations,
                    "poll": format!("/v1/simulations/{id}/results?run={run_id}"),
                    "disclaimer": skattjakt_simulate::DISCLAIMER,
                })),
            )
                .into_response())
        }
    }
}

pub async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<RunQuery>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::RunSimulation)?;
    let store = store(&state)?.clone();

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    tenant.simulation(id).await.map_err(not_found)?;
    let run = match query.run {
        Some(run_id) => tenant.run(run_id).await.map_err(not_found)?,
        None => tenant.latest_run(id).await.map_err(not_found)?,
    };
    if run.simulation_id != id {
        return Err(not_found(skattjakt_store::StoreError::NotFound));
    }

    let requested = tenant
        .request_run_cancellation(run.id, scope.user_id())
        .await
        .map_err(internal)?;
    if requested {
        tenant
            .audit_as(
                actor(&scope),
                "simulation.run_cancelled",
                Some(id),
                json!({"run_id": run.id, "completed_iterations": run.completed_iterations}),
            )
            .await
            .map_err(internal)?;
    }
    tenant.commit().await.map_err(internal)?;

    if !requested {
        // Already finished. Not an error — the caller wanted it stopped and it
        // is stopped — but the response has to say that no cancellation
        // happened, or a client will report one that did not.
        return Ok((
            StatusCode::OK,
            Json(json!({
                "run_id": run.id,
                "state": run.state.as_str(),
                "cancelled": false,
                "detail": "körningen hade redan avslutats",
            })),
        )
            .into_response());
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "run_id": run.id,
            "state": "cancelling",
            "cancelled": true,
            "detail": "körningen stoppas efter innevarande batch",
        })),
    )
        .into_response())
}

/// One run: state, progress, statistics and the visualisation payload.
pub async fn results(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<RunQuery>,
) -> Result<Response, Problem> {
    let (run, mut tenant) = load_run(&state, &headers, id, query.run).await?;

    let body = if run.state == RunState::Succeeded {
        let statistics = tenant.run_statistics(run.id).await.map_err(internal)?;
        let shapes = tenant.run_shapes(run.id).await.map_err(internal)?;
        json!({
            "statistics": statistics,
            "shapes": shapes,
        })
    } else {
        // A run that has not finished returns its progress and no numbers.
        // Returning partial statistics would be returning a result from a
        // sample the caller has no way to know the size of.
        json!({ "statistics": Value::Null, "shapes": Value::Null })
    };
    tenant.commit().await.map_err(internal)?;

    let mut response = run_status(&run);
    if let Value::Object(map) = &mut response {
        if let Value::Object(extra) = body {
            map.extend(extra);
        }
    }
    Ok(Json(response).into_response())
}

pub async fn statistics(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<RunQuery>,
) -> Result<Response, Problem> {
    let (run, mut tenant) = load_run(&state, &headers, id, query.run).await?;
    let statistics = tenant.run_statistics(run.id).await.map_err(internal)?;
    tenant.commit().await.map_err(internal)?;
    Ok(Json(json!({
        "run_id": run.id,
        "state": run.state.as_str(),
        "statistics": statistics,
        "disclaimer": skattjakt_simulate::DISCLAIMER,
    }))
    .into_response())
}

pub async fn sensitivity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<RunQuery>,
) -> Result<Response, Problem> {
    let (run, mut tenant) = load_run(&state, &headers, id, query.run).await?;
    let sensitivity = tenant.run_sensitivity(run.id).await.map_err(internal)?;
    tenant.commit().await.map_err(internal)?;
    Ok(Json(json!({
        "run_id": run.id,
        "state": run.state.as_str(),
        "sensitivity": sensitivity,
        "method": "Rangkorrelation (Spearman) och dess andel av den förklarade variansen. \
                   Förutsätter att indata är oberoende av varandra.",
    }))
    .into_response())
}

pub async fn convergence(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<RunQuery>,
) -> Result<Response, Problem> {
    let (run, mut tenant) = load_run(&state, &headers, id, query.run).await?;
    let convergence = tenant.run_convergence(run.id).await.map_err(internal)?;
    tenant.commit().await.map_err(internal)?;
    Ok(Json(json!({
        "run_id": run.id,
        "state": run.state.as_str(),
        "convergence": convergence,
    }))
    .into_response())
}

/// The audit trail of section 13, for one simulation.
pub async fn audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, Problem> {
    let scope = authorise(&state, &headers).await?;
    let company_id = company_scope(&scope, Permission::ReadAuditTrail)?;
    let store = store(&state)?.clone();

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    tenant.simulation(id).await.map_err(not_found)?;
    let events = tenant.audit_trail(id).await.map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok(Json(json!({"simulation_id": id, "events": events})).into_response())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn load_run<'a>(
    state: &'a AppState,
    headers: &HeaderMap,
    simulation_id: Uuid,
    run_id: Option<Uuid>,
) -> Result<
    (
        skattjakt_store::simulations::StoredRun,
        skattjakt_store::Tenant<'a>,
    ),
    Problem,
> {
    let scope = authorise(state, headers).await?;
    let company_id = company_scope(&scope, Permission::ReadSimulation)?;
    let store = store(state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    tenant.simulation(simulation_id).await.map_err(not_found)?;
    let run = match run_id {
        Some(id) => tenant.run(id).await.map_err(not_found)?,
        None => tenant.latest_run(simulation_id).await.map_err(not_found)?,
    };
    // A run id from another simulation in the same tenant would otherwise read
    // as valid. Row-level security stops cross-tenant access; this stops
    // cross-model confusion.
    if run.simulation_id != simulation_id {
        return Err(not_found(skattjakt_store::StoreError::NotFound));
    }
    Ok((run, tenant))
}

fn run_status(run: &skattjakt_store::simulations::StoredRun) -> Value {
    json!({
        "run_id": run.id,
        "simulation_id": run.simulation_id,
        "state": run.state.as_str(),
        "seed": run.seed.to_string(),
        "iterations": run.iterations,
        "completed_iterations": run.completed_iterations,
        "progress": run.progress(),
        "engine_version": run.engine_version,
        "spec_hash": run.spec_hash,
        "execution": run.execution,
        "reason": run.reason,
        "requested_at": run.requested_at,
        "finished_at": run.finished_at,
        "duration_ms": run.duration_ms,
        "iterations_per_second": run.iterations_per_second,
        "quality": run.quality,
        "error": run.error,
        "cancel_requested": run.cancel_requested,
        "disclaimer": skattjakt_simulate::DISCLAIMER,
    })
}

fn run_body(run_id: Uuid, outcome: &skattjakt_simulate::RunOutcome) -> Value {
    json!({
        "run_id": run_id,
        "state": "succeeded",
        "execution": "inline",
        "seed": outcome.seed.to_string(),
        "iterations": outcome.iterations,
        "engine_version": outcome.engine_version,
        "spec_hash": outcome.spec_hash,
        "duration_ms": outcome.duration_ms,
        "iterations_per_second": outcome.iterations_per_second,
        "statistics": outcome.statistics.iter().map(|(id, statistics)| {
            let mut value = serde_json::to_value(statistics).unwrap_or(Value::Null);
            if let Value::Object(map) = &mut value {
                map.insert("output_id".into(), json!(id));
            }
            value
        }).collect::<Vec<_>>(),
        "shapes": outcome.shapes,
        // Flattened to exactly the shape the stored endpoints return.
        //
        // They used not to be: an inline run returned the engine's nested
        // structs and a queued run returned rows read back from the database,
        // so a client had to handle two shapes for the same data. The browser
        // suite found it the way a user would have — the sensitivity and
        // convergence panels were simply empty after a small run and full
        // after a large one. One resource, one shape.
        "sensitivity": outcome
            .sensitivity
            .iter()
            .flat_map(|report| {
                report.inputs.iter().map(move |entry| {
                    json!({
                        "output_id": report.output_id,
                        "input_id": entry.input_id,
                        "input_name": entry.input_name,
                        "correlation": entry.correlation,
                        "rank_correlation": entry.rank_correlation,
                        "variance_contribution": entry.variance_contribution,
                        "rank": entry.rank,
                        "referenced": entry.referenced,
                        "sample_size": report.sample_size,
                    })
                })
            })
            .collect::<Vec<_>>(),
        "convergence": outcome
            .convergence
            .iter()
            .flat_map(|report| {
                report.checkpoints.iter().map(move |checkpoint| {
                    json!({
                        "output_id": report.output_id,
                        "iterations": checkpoint.iterations,
                        "mean": checkpoint.mean,
                        "median": checkpoint.median,
                        "p10": checkpoint.p10,
                        "p90": checkpoint.p90,
                        "stable": report.stable,
                        "largest_relative_change":
                            if report.largest_relative_change.is_finite() {
                                Some(report.largest_relative_change)
                            } else {
                                None
                            },
                        "warning": report.warning,
                    })
                })
            })
            .collect::<Vec<_>>(),
        "quality": outcome.quality,
        "disclaimer": outcome.disclaimer,
    })
}

fn record_finish(state: &AppState, outcome: &skattjakt_simulate::RunOutcome, unstable: usize) {
    state.metrics.increment(
        names::SIMULATIONS_FINISHED,
        LabelSet::new().enumerated("outcome", "succeeded"),
    );
    state.metrics.observe(
        names::SIMULATION_DURATION,
        LabelSet::new(),
        outcome.duration_ms,
    );
    state.metrics.add(
        names::SIMULATION_ITERATIONS,
        LabelSet::new(),
        u64::from(outcome.iterations),
    );
    state.metrics.observe(
        names::SIMULATION_THROUGHPUT,
        LabelSet::new(),
        outcome.iterations_per_second.max(0.0) as u64,
    );
    if unstable > 0 {
        state.metrics.add(
            names::SIMULATION_CONVERGENCE_FAILURES,
            LabelSet::new(),
            unstable as u64,
        );
    }
}

/// Who acted, for the audit record.
fn actor(scope: &crate::Scope) -> String {
    match scope {
        crate::Scope::User(user) => format!("user:{}", user.user_id),
        crate::Scope::Company(_) => "company-token".to_string(),
        crate::Scope::Admin => "admin".to_string(),
        crate::Scope::Stateless => "static-token".to_string(),
    }
}

/// A seed nobody chose.
///
/// From the operating system rather than from a clock: two runs started in the
/// same millisecond by two API replicas would otherwise get the same "random"
/// seed and produce identical results that look like a coincidence.
fn random_seed() -> u64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // Never observed in practice; falling back to a clock is still better
        // than refusing to start a run, and the seed is recorded either way, so
        // reproducibility is unaffected.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5EED_5EED_5EED_5EED);
        return nanos;
    }
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_runs_answer_inline_and_large_ones_are_queued() {
        assert_eq!(execution_for(1_000, 1), Execution::Inline);
        assert_eq!(execution_for(50_000, 1), Execution::Inline);
        assert_eq!(execution_for(1_000_000, 1), Execution::Queued);
        // The threshold falls as the model widens, because the work per
        // iteration rises with it.
        assert_eq!(execution_for(50_000, 3), Execution::Inline);
        assert_eq!(execution_for(50_000, 16), Execution::Queued);
    }

    #[test]
    fn a_seed_survives_the_wire_at_full_width() {
        // The value that motivates the whole decision: above 2^53 a JSON number
        // would come back changed.
        let big = u64::MAX - 12345;
        let seed: Seed = serde_json::from_str(&format!("\"{big}\"")).unwrap();
        assert_eq!(seed.0, big);
        assert_eq!(serde_json::to_string(&seed).unwrap(), format!("\"{big}\""));
    }

    #[test]
    fn a_seed_may_also_be_sent_as_a_number() {
        let seed: Seed = serde_json::from_str("42").unwrap();
        assert_eq!(seed.0, 42);
    }

    #[test]
    fn a_seed_that_is_not_a_whole_number_is_rejected() {
        for text in ["-1", "1.5", "\"abc\"", "true", "null"] {
            assert!(
                serde_json::from_str::<Seed>(text).is_err(),
                "{text} was accepted as a seed"
            );
        }
    }

    #[test]
    fn a_drawn_seed_is_not_the_same_twice() {
        let seeds: std::collections::HashSet<u64> = (0..64).map(|_| random_seed()).collect();
        assert!(seeds.len() > 60, "the seed source is not varying");
    }
}

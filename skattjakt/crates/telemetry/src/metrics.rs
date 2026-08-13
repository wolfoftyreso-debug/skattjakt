//! Prometheus metrics (section 43).
//!
//! Hand-rolled rather than pulled from a client library, for one reason: the
//! label rule in section 45. Every general-purpose metrics crate lets a caller
//! write `counter!("x", "company" => id)`, and that is precisely the mistake
//! this product cannot make — a tenant identifier in a metric label puts
//! customer identity into a time-series database with its own retention and its
//! own access list, and every scrape after that keeps it alive.
//!
//! Here, `LabelSet::insert` takes a `Classification` and rejects anything above
//! `METRIC_LABEL_CEILING`. The check is in the constructor, not in review.
//!
//! The exposition format is the Prometheus text format, version 0.0.4.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use skattjakt_core::Classification;

/// A rejected label. Returned rather than panicked, so a metric mistake
/// degrades the metric instead of taking down the request that emitted it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    #[error("metric label {name} is classified {level} and metric labels accept PUBLIC only")]
    TooSensitive { name: String, level: Classification },
    #[error("metric label {name} has an unbounded value; use a bounded enumeration")]
    Unbounded { name: String },
}

/// Label names whose values are, by their nature, unbounded. Rejected outright:
/// high-cardinality labels are how a Prometheus server runs out of memory.
const UNBOUNDED_LABELS: &[&str] = &[
    "company",
    "company_id",
    "tenant",
    "tenant_id",
    "user",
    "user_id",
    "document",
    "document_id",
    "analysis",
    "analysis_id",
    "correlation_id",
    "org_number",
    "path",
    "url",
    "filename",
    "email",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct LabelSet(BTreeMap<String, String>);

impl LabelSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a label, if it is safe to publish.
    pub fn insert(
        mut self,
        name: &str,
        value: impl Into<String>,
        level: Classification,
    ) -> Result<Self, LabelError> {
        if !level.permitted_at(Classification::METRIC_LABEL_CEILING) {
            return Err(LabelError::TooSensitive {
                name: name.to_string(),
                level,
            });
        }
        if UNBOUNDED_LABELS.contains(&name) {
            return Err(LabelError::Unbounded {
                name: name.to_string(),
            });
        }
        self.0.insert(name.to_string(), value.into());
        Ok(self)
    }

    /// For labels drawn from a closed enumeration in the code — a stage name, a
    /// rule outcome, an HTTP method. Infallible because the value cannot come
    /// from a request.
    pub fn enumerated(self, name: &str, value: &'static str) -> Self {
        // `expect` is sound: a `'static` value from a code-defined enumeration
        // is public and bounded by construction, and the unbounded-name list
        // holds no name a static enumeration uses.
        self.insert(name, value, Classification::Public)
            .expect("enumerated labels are public and bounded")
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn render(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let inner = self
            .0
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{{inner}}}")
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    fn as_str(self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Histogram => "histogram",
        }
    }
}

/// Values are held as integers. Durations are milliseconds, money is öre,
/// costs are micro-öre — the same discipline as the money type, for the same
/// reason: a float counter drifts, and a drifting counter is worse than none.
#[derive(Debug, Default)]
struct Series {
    value: AtomicU64,
}

#[derive(Debug)]
struct Histogram {
    /// Upper bounds, ascending. The last bucket is +Inf implicitly.
    bounds: Vec<u64>,
    buckets: Vec<AtomicU64>,
    sum: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn new(bounds: Vec<u64>) -> Self {
        let buckets = (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect();
        Self {
            bounds,
            buckets,
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn observe(&self, value: u64) {
        let index = self
            .bounds
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(self.bounds.len());
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct Family {
    kind: MetricKind,
    help: &'static str,
    unit: Option<&'static str>,
    series: BTreeMap<LabelSet, Series>,
    histograms: BTreeMap<LabelSet, Histogram>,
    histogram_bounds: Vec<u64>,
}

/// The process-wide registry.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    families: Arc<RwLock<BTreeMap<&'static str, Family>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    fn family(
        &self,
        name: &'static str,
        kind: MetricKind,
        help: &'static str,
        unit: Option<&'static str>,
    ) {
        let mut families = self.families.write().expect("metrics registry poisoned");
        families.entry(name).or_insert_with(|| Family {
            kind,
            help,
            unit,
            series: BTreeMap::new(),
            histograms: BTreeMap::new(),
            histogram_bounds: Vec::new(),
        });
    }

    pub fn register_counter(&self, name: &'static str, help: &'static str) {
        self.family(name, MetricKind::Counter, help, None);
    }

    pub fn register_gauge(&self, name: &'static str, help: &'static str) {
        self.family(name, MetricKind::Gauge, help, None);
    }

    pub fn register_histogram(
        &self,
        name: &'static str,
        help: &'static str,
        unit: &'static str,
        bounds: Vec<u64>,
    ) {
        self.family(name, MetricKind::Histogram, help, Some(unit));
        let mut families = self.families.write().expect("metrics registry poisoned");
        if let Some(family) = families.get_mut(name) {
            family.histogram_bounds = bounds;
        }
    }

    pub fn increment(&self, name: &'static str, labels: LabelSet) {
        self.add(name, labels, 1);
    }

    pub fn add(&self, name: &'static str, labels: LabelSet, delta: u64) {
        let mut families = self.families.write().expect("metrics registry poisoned");
        let Some(family) = families.get_mut(name) else {
            return;
        };
        family
            .series
            .entry(labels)
            .or_default()
            .value
            .fetch_add(delta, Ordering::Relaxed);
    }

    pub fn set(&self, name: &'static str, labels: LabelSet, value: u64) {
        let mut families = self.families.write().expect("metrics registry poisoned");
        let Some(family) = families.get_mut(name) else {
            return;
        };
        family
            .series
            .entry(labels)
            .or_default()
            .value
            .store(value, Ordering::Relaxed);
    }

    pub fn observe(&self, name: &'static str, labels: LabelSet, value: u64) {
        let mut families = self.families.write().expect("metrics registry poisoned");
        let Some(family) = families.get_mut(name) else {
            return;
        };
        let bounds = family.histogram_bounds.clone();
        family
            .histograms
            .entry(labels)
            .or_insert_with(|| Histogram::new(bounds))
            .observe(value);
    }

    /// The scrape body.
    pub fn render(&self) -> String {
        let families = self.families.read().expect("metrics registry poisoned");
        let mut out = String::new();
        for (name, family) in families.iter() {
            let _ = writeln!(out, "# HELP {name} {}", family.help);
            let _ = writeln!(out, "# TYPE {name} {}", family.kind.as_str());
            if let Some(unit) = family.unit {
                let _ = writeln!(out, "# UNIT {name} {unit}");
            }
            match family.kind {
                MetricKind::Counter | MetricKind::Gauge => {
                    for (labels, series) in &family.series {
                        let _ = writeln!(
                            out,
                            "{name}{} {}",
                            labels.render(),
                            series.value.load(Ordering::Relaxed)
                        );
                    }
                }
                MetricKind::Histogram => {
                    for (labels, histogram) in &family.histograms {
                        let mut cumulative = 0u64;
                        for (index, bound) in histogram.bounds.iter().enumerate() {
                            cumulative += histogram.buckets[index].load(Ordering::Relaxed);
                            let with_le = labels
                                .clone()
                                .insert("le", bound.to_string(), Classification::Public)
                                .unwrap_or_else(|_| labels.clone());
                            let _ = writeln!(out, "{name}_bucket{} {cumulative}", with_le.render());
                        }
                        cumulative +=
                            histogram.buckets[histogram.bounds.len()].load(Ordering::Relaxed);
                        let with_inf = labels
                            .clone()
                            .insert("le", "+Inf", Classification::Public)
                            .unwrap_or_else(|_| labels.clone());
                        let _ = writeln!(out, "{name}_bucket{} {cumulative}", with_inf.render());
                        let _ = writeln!(
                            out,
                            "{name}_sum{} {}",
                            labels.render(),
                            histogram.sum.load(Ordering::Relaxed)
                        );
                        let _ = writeln!(
                            out,
                            "{name}_count{} {}",
                            labels.render(),
                            histogram.count.load(Ordering::Relaxed)
                        );
                    }
                }
            }
        }
        out
    }
}

/// Metric names, in one place.
///
/// Technical (section 43), business (section 44) and model quality (section 46)
/// live together because an operator debugging a bad night needs all three at
/// once: requests are fine, analyses are failing, and the failure is that the
/// provider started refusing.
pub mod names {
    // Technical
    pub const HTTP_REQUESTS: &str = "skattjakt_http_requests_total";
    /// Sign-in attempts by client and outcome. Outcome is a closed set —
    /// succeeded, rejected, locked — never a reason that could name a user.
    pub const SIGN_INS: &str = "skattjakt_sign_ins_total";
    /// Refresh exchanges. A rise in `rejected` is what a stolen-token sweep
    /// looks like from the outside.
    pub const SESSION_REFRESHES: &str = "skattjakt_session_refreshes_total";
    /// Notifications delivered, by channel and kind.
    pub const NOTIFICATIONS_DELIVERED: &str = "skattjakt_notifications_delivered_total";
    /// Notifications given up on. A rise in `permanent` is usually a relay
    /// rejecting a whole domain, which no retry will fix.
    pub const NOTIFICATIONS_FAILED: &str = "skattjakt_notifications_failed_total";
    pub const HTTP_DURATION: &str = "skattjakt_http_request_duration_ms";
    pub const HTTP_IN_FLIGHT: &str = "skattjakt_http_requests_in_flight";
    pub const RATE_LIMITED: &str = "skattjakt_rate_limited_total";
    pub const DB_QUERY_DURATION: &str = "skattjakt_db_query_duration_ms";

    // Jobs
    pub const JOBS_ENQUEUED: &str = "skattjakt_jobs_enqueued_total";
    pub const JOBS_COMPLETED: &str = "skattjakt_jobs_completed_total";
    pub const JOB_ATTEMPTS: &str = "skattjakt_job_attempts_total";
    pub const JOB_DURATION: &str = "skattjakt_job_duration_ms";
    pub const JOBS_QUEUED_DEPTH: &str = "skattjakt_jobs_queued";
    pub const JOBS_DEAD_LETTERED: &str = "skattjakt_jobs_dead_lettered_total";
    pub const JOB_QUEUE_AGE: &str = "skattjakt_job_oldest_queued_age_ms";

    // Business (section 44)
    pub const ANALYSES_STARTED: &str = "skattjakt_analyses_started_total";
    pub const ANALYSES_FINISHED: &str = "skattjakt_analyses_finished_total";
    pub const ANALYSIS_DURATION: &str = "skattjakt_analysis_duration_ms";
    pub const OPPORTUNITIES_FOUND: &str = "skattjakt_opportunities_total";
    pub const FOUND_NOTHING: &str = "skattjakt_analyses_found_nothing_total";
    pub const DOCUMENTS_UPLOADED: &str = "skattjakt_documents_uploaded_total";
    pub const EXTRACTION_FACTS: &str = "skattjakt_extracted_facts_total";

    /// How many cited legal sources stand in each retrieval state.
    ///
    /// The one metric in this file that describes whether the product's answers
    /// rest on anything. `state="mismatch"` above zero means a paragraph the
    /// rules depend on stopped saying what they assume — every finding on that
    /// rule has just been capped at "investigate", and somebody needs to read
    /// the law. `state="verified"` falling is the same event seen a moment
    /// earlier.
    pub const SOURCE_STATES: &str = "skattjakt_cited_sources";

    // Payments
    /// Payment requests sent to the provider, by outcome.
    pub const PAYMENTS_STARTED: &str = "skattjakt_payments_started_total";
    /// Payments that reached a terminal state, by outcome. A rise in `failed`
    /// with no rise in provider errors means we are refusing payments the
    /// provider called successful — an attack, or a bug in the amount handling.
    pub const PAYMENTS_SETTLED: &str = "skattjakt_payments_settled_total";
    /// Callbacks received. `unreadable` above zero means either Swish changed
    /// the shape or somebody is posting rubbish at the endpoint.
    pub const PAYMENT_CALLBACKS: &str = "skattjakt_payment_callbacks_total";
    /// Payments the sweep found still unresolved. Sustained above zero means
    /// callbacks are not arriving and customers are waiting.
    pub const PAYMENTS_UNSETTLED: &str = "skattjakt_payments_unsettled";

    // Monte Carlo simulation
    pub const SIMULATIONS_STARTED: &str = "skattjakt_simulations_started_total";
    pub const SIMULATIONS_FINISHED: &str = "skattjakt_simulations_finished_total";
    pub const SIMULATION_DURATION: &str = "skattjakt_simulation_duration_ms";
    pub const SIMULATION_ITERATIONS: &str = "skattjakt_simulation_iterations_total";
    pub const SIMULATION_THROUGHPUT: &str = "skattjakt_simulation_iterations_per_second";
    pub const SIMULATION_CONVERGENCE_FAILURES: &str =
        "skattjakt_simulation_convergence_failures_total";

    // Model quality and cost (sections 46, 68, 69)
    pub const MODEL_CALLS: &str = "skattjakt_model_calls_total";
    pub const MODEL_TOKENS: &str = "skattjakt_model_tokens_total";
    pub const MODEL_COST_MICRO_ORE: &str = "skattjakt_model_cost_micro_ore_total";
    pub const MODEL_LATENCY: &str = "skattjakt_model_latency_ms";
    pub const MODEL_FALLBACKS: &str = "skattjakt_model_fallbacks_total";
    pub const MODEL_REFUSALS: &str = "skattjakt_model_refusals_total";
    pub const MODEL_SCHEMA_FAILURES: &str = "skattjakt_model_schema_failures_total";
    pub const BUDGET_EXCEEDED: &str = "skattjakt_analysis_budget_exceeded_total";
    pub const SKEPTIC_REJECTIONS: &str = "skattjakt_skeptic_rejections_total";
    pub const PROMPT_INJECTION_SUSPECTED: &str = "skattjakt_prompt_injection_suspected_total";

    // Retention (section 65)
    pub const RETENTION_DELETED: &str = "skattjakt_retention_deleted_total";
}

/// Registers every metric this codebase emits.
///
/// Registration up front means `/metrics` shows a zero rather than an absent
/// series before the first event — the difference between "no errors" and "no
/// data", which is the difference between a working alert and a silent one.
pub fn register_all(registry: &Registry) {
    use names::*;

    registry.register_counter(
        HTTP_REQUESTS,
        "HTTP requests handled, by method, route and status class.",
    );
    registry.register_histogram(
        HTTP_DURATION,
        "HTTP request duration.",
        "milliseconds",
        vec![5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000],
    );
    registry.register_gauge(HTTP_IN_FLIGHT, "HTTP requests currently being handled.");
    registry.register_counter(RATE_LIMITED, "Requests rejected by the rate limiter.");
    registry.register_counter(
        SIMULATIONS_STARTED,
        "Simulation runs started, by execution mode.",
    );
    registry.register_counter(
        SIMULATIONS_FINISHED,
        "Simulation runs finished, by outcome.",
    );
    registry.register_histogram(
        SIMULATION_DURATION,
        "Wall-clock time of a simulation run.",
        "milliseconds",
        vec![10, 50, 100, 500, 1_000, 5_000, 15_000, 60_000, 300_000],
    );
    registry.register_counter(SIMULATION_ITERATIONS, "Monte Carlo iterations executed.");
    registry.register_histogram(
        SIMULATION_THROUGHPUT,
        "Iterations per second achieved by a run.",
        "iterations per second",
        vec![1_000, 10_000, 50_000, 100_000, 250_000, 500_000, 1_000_000],
    );
    registry.register_counter(
        SIMULATION_CONVERGENCE_FAILURES,
        "Runs whose results had not stabilised at the iteration count used.",
    );
    registry.register_counter(SIGN_INS, "Sign-in attempts, by client kind and outcome.");
    registry.register_counter(
        NOTIFICATIONS_DELIVERED,
        "Notifications delivered, by channel and kind.",
    );
    registry.register_counter(
        NOTIFICATIONS_FAILED,
        "Notifications given up on, by failure class.",
    );
    registry.register_counter(
        SESSION_REFRESHES,
        "Refresh-token exchanges, by outcome. A rise in rejections is what a \
         stolen-token sweep looks like from outside.",
    );
    registry.register_histogram(
        DB_QUERY_DURATION,
        "Database query duration.",
        "milliseconds",
        vec![1, 5, 10, 25, 50, 100, 250, 1_000],
    );

    registry.register_counter(JOBS_ENQUEUED, "Jobs enqueued, by kind.");
    registry.register_counter(
        JOBS_COMPLETED,
        "Jobs that reached a terminal state, by kind and outcome.",
    );
    registry.register_counter(JOB_ATTEMPTS, "Job attempts started, by kind.");
    registry.register_histogram(
        JOB_DURATION,
        "Job attempt duration.",
        "milliseconds",
        vec![
            1_000, 5_000, 15_000, 30_000, 60_000, 120_000, 300_000, 600_000,
        ],
    );
    registry.register_gauge(JOBS_QUEUED_DEPTH, "Jobs waiting to be claimed, by kind.");
    registry.register_counter(
        JOBS_DEAD_LETTERED,
        "Jobs moved to the dead letter queue, by kind.",
    );
    registry.register_gauge(JOB_QUEUE_AGE, "Age of the oldest queued job.");

    registry.register_counter(ANALYSES_STARTED, "Analyses started.");
    registry.register_counter(ANALYSES_FINISHED, "Analyses finished, by terminal state.");
    registry.register_histogram(
        ANALYSIS_DURATION,
        "End-to-end analysis duration.",
        "milliseconds",
        vec![10_000, 30_000, 60_000, 120_000, 300_000, 600_000, 1_200_000],
    );
    registry.register_counter(
        OPPORTUNITIES_FOUND,
        "Opportunities produced, by category and status.",
    );
    registry.register_counter(
        FOUND_NOTHING,
        "Analyses that completed with no presented finding.",
    );
    registry.register_gauge(
        SOURCE_STATES,
        "Cited legal sources by how far each has been checked.",
    );
    registry.register_counter(PAYMENTS_STARTED, "Payment requests sent, by outcome.");
    registry.register_counter(PAYMENTS_SETTLED, "Payments settled, by outcome.");
    registry.register_counter(PAYMENT_CALLBACKS, "Payment callbacks received, by outcome.");
    registry.register_gauge(PAYMENTS_UNSETTLED, "Payments still awaiting a resolution.");
    registry.register_counter(DOCUMENTS_UPLOADED, "Documents accepted, by mime type.");
    registry.register_counter(
        EXTRACTION_FACTS,
        "Financial facts extracted from documents.",
    );

    registry.register_counter(MODEL_CALLS, "Model calls, by task and outcome.");
    registry.register_counter(MODEL_TOKENS, "Model tokens, by direction.");
    registry.register_counter(MODEL_COST_MICRO_ORE, "Accumulated model cost in micro-ore.");
    registry.register_histogram(
        MODEL_LATENCY,
        "Model call latency.",
        "milliseconds",
        vec![500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000],
    );
    registry.register_counter(
        MODEL_FALLBACKS,
        "Model calls served by a fallback rather than the requested model.",
    );
    registry.register_counter(
        MODEL_REFUSALS,
        "Model calls that ended in a refusal stop reason.",
    );
    registry.register_counter(
        MODEL_SCHEMA_FAILURES,
        "Model responses that did not satisfy the requested schema.",
    );
    registry.register_counter(
        BUDGET_EXCEEDED,
        "Analyses stopped because they reached their cost ceiling.",
    );
    registry.register_counter(
        SKEPTIC_REJECTIONS,
        "Findings the falsification pass demoted or removed.",
    );
    registry.register_counter(
        PROMPT_INJECTION_SUSPECTED,
        "Document segments that matched an instruction-like pattern.",
    );

    registry.register_counter(
        RETENTION_DELETED,
        "Objects deleted by the retention job, by kind.",
    );
}

/// Publishes the standing of the cited legal sources after a verification
/// sweep.
///
/// All four states are written every time, including the zeroes. A gauge that
/// only appears when it is non-zero cannot be alerted on: `mismatch > 0` never
/// fires if the series does not exist until the bad day, and by then the alert
/// has no history to compare against.
pub fn set_source_states(
    registry: &Registry,
    verified: usize,
    mismatched: usize,
    unreachable: usize,
) {
    for (state, count) in [
        ("verified", verified),
        ("mismatch", mismatched),
        ("unreachable", unreachable),
    ] {
        registry.set(
            names::SOURCE_STATES,
            LabelSet::new().enumerated("state", state),
            count as u64,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_identifier_cannot_become_a_metric_label() {
        let err = LabelSet::new()
            .insert("company_id", "8a5e...", Classification::Confidential)
            .unwrap_err();
        assert!(matches!(err, LabelError::TooSensitive { .. }));
    }

    #[test]
    fn an_unbounded_label_name_is_rejected_even_when_marked_public() {
        let err = LabelSet::new()
            .insert("correlation_id", "abc", Classification::Public)
            .unwrap_err();
        assert!(matches!(err, LabelError::Unbounded { .. }));
    }

    #[test]
    fn a_financial_amount_cannot_become_a_metric_label() {
        let err = LabelSet::new()
            .insert("amount", "1250000", Classification::HighlyConfidential)
            .unwrap_err();
        assert!(matches!(err, LabelError::TooSensitive { .. }));
    }

    #[test]
    fn bounded_public_labels_are_accepted() {
        let labels = LabelSet::new()
            .insert("outcome", "succeeded", Classification::Public)
            .unwrap()
            .insert("kind", "analysis", Classification::Public)
            .unwrap();
        assert!(!labels.is_empty());
    }

    #[test]
    fn counters_render_in_the_prometheus_text_format() {
        let registry = Registry::new();
        registry.register_counter(names::ANALYSES_STARTED, "Analyses started.");
        registry.increment(names::ANALYSES_STARTED, LabelSet::new());
        registry.increment(names::ANALYSES_STARTED, LabelSet::new());
        let body = registry.render();
        assert!(body.contains("# TYPE skattjakt_analyses_started_total counter"));
        assert!(body.contains("skattjakt_analyses_started_total 2"));
    }

    #[test]
    fn histograms_render_cumulative_buckets_a_sum_and_a_count() {
        let registry = Registry::new();
        registry.register_histogram("t_ms", "test", "milliseconds", vec![10, 100]);
        registry.observe("t_ms", LabelSet::new(), 5);
        registry.observe("t_ms", LabelSet::new(), 50);
        registry.observe("t_ms", LabelSet::new(), 5_000);
        let body = registry.render();
        assert!(body.contains("t_ms_bucket{le=\"10\"} 1"));
        assert!(body.contains("t_ms_bucket{le=\"100\"} 2"));
        assert!(body.contains("t_ms_bucket{le=\"+Inf\"} 3"));
        assert!(body.contains("t_ms_sum 5055"));
        assert!(body.contains("t_ms_count 3"));
    }

    #[test]
    fn label_values_are_escaped() {
        let labels = LabelSet::new()
            .insert("outcome", "a\"b\\c", Classification::Public)
            .unwrap();
        assert_eq!(labels.render(), "{outcome=\"a\\\"b\\\\c\"}");
    }

    #[test]
    fn registered_metrics_appear_before_the_first_observation() {
        let registry = Registry::new();
        register_all(&registry);
        let body = registry.render();
        // No samples yet, but the families and their help text are there, so a
        // dashboard shows an empty panel rather than a missing one.
        assert!(body.contains("# TYPE skattjakt_model_refusals_total counter"));
        assert!(body.contains("# HELP skattjakt_analyses_started_total"));
    }

    #[test]
    fn every_registered_name_is_prefixed_and_typed_consistently() {
        let registry = Registry::new();
        register_all(&registry);
        for line in registry.render().lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let name = rest.split_whitespace().next().unwrap();
                assert!(name.starts_with("skattjakt_"), "{name} is unprefixed");
                if rest.ends_with("counter") {
                    assert!(
                        name.ends_with("_total"),
                        "{name} is a counter without _total"
                    );
                }
            }
        }
    }

    #[test]
    fn gauges_are_set_not_accumulated() {
        let registry = Registry::new();
        registry.register_gauge(names::JOBS_QUEUED_DEPTH, "depth");
        registry.set(names::JOBS_QUEUED_DEPTH, LabelSet::new(), 7);
        registry.set(names::JOBS_QUEUED_DEPTH, LabelSet::new(), 3);
        assert!(registry.render().contains("skattjakt_jobs_queued 3"));
    }

    #[test]
    fn writing_to_an_unregistered_metric_is_ignored_rather_than_fatal() {
        let registry = Registry::new();
        registry.increment("never_registered", LabelSet::new());
        assert!(registry.render().is_empty());
    }
}

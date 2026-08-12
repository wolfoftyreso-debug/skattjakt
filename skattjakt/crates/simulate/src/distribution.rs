//! Probability distributions.
//!
//! Eleven of them, each with the same four obligations: it can be named, it can
//! say whether its parameters are valid, it can draw a sample, and it can state
//! its own mean and variance analytically. The last one is not decoration — it
//! is how the statistical tests check the samplers. A sampler tested only
//! against itself is a sampler that can be confidently wrong.
//!
//! **Invalid parameters are never silently repaired.** A normal with a negative
//! standard deviation, a triangular whose mode sits outside its range, a
//! discrete distribution whose weights sum to zero: each of these is a rejected
//! specification with a message naming the parameter, not a quietly adjusted
//! one. A simulation that fixes its own inputs produces a number nobody asked
//! for and everybody believes.

use serde::{Deserialize, Serialize};

use crate::rng::Rng;

/// The most outcomes a tabular distribution may enumerate.
///
/// A bound rather than a preference, and it closes a denial of service that
/// every other limit in this crate missed. `Discrete` and `Custom` are the only
/// distributions whose *cost per draw* is chosen by the request rather than
/// fixed by the mathematics. Before this bound, a 12.6 MB request body — well
/// inside the API's 32 MB limit — carried a million outcomes, and a
/// 50 000-iteration run over it took 55 seconds. That run is small enough to be
/// answered inside the HTTP request, so it held a blocking thread for a minute
/// while passing the iteration bound, the memory bound, the rate limit and the
/// body limit.
///
/// A thousand is generous for what these distributions are for: a scenario
/// variable with more than a thousand named outcomes is not a scenario
/// variable, and a hand-drawn curve with more than a thousand points is a
/// continuous distribution being typed in one point at a time.
pub const MAX_CATEGORIES: usize = 1_000;

/// Why a distribution's parameters were rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DistributionError {
    #[error("{parameter} must be {requirement}, and is {value}")]
    Parameter {
        parameter: &'static str,
        requirement: &'static str,
        value: String,
    },
    #[error("{0}")]
    Shape(String),
}

impl DistributionError {
    fn parameter(parameter: &'static str, requirement: &'static str, value: f64) -> Self {
        Self::Parameter {
            parameter,
            requirement,
            value: format!("{value}"),
        }
    }
}

/// What a distribution says about itself before anything is drawn.
///
/// Analytic, not measured. The statistical tests draw a large sample and check
/// it against these, which is only meaningful because the two are computed by
/// different code from different formulas.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Moments {
    pub mean: f64,
    pub variance: f64,
    /// The lower end of the support, where it is finite.
    pub min: Option<f64>,
    /// The upper end of the support, where it is finite.
    pub max: Option<f64>,
}

impl Moments {
    pub fn std_dev(&self) -> f64 {
        self.variance.sqrt()
    }
}

/// The distributions the engine can draw from.
///
/// Serialised with an explicit `kind` tag, so a stored specification reads as
/// documentation and an unknown kind is a parse error rather than a default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Distribution {
    /// The default assumption for a quantity that is a sum of many small
    /// independent effects. Unbounded in both directions, which is why it is
    /// the wrong choice for anything that cannot be negative.
    Normal { mean: f64, std_dev: f64 },
    /// For quantities that cannot be negative and are multiplicative — prices,
    /// growth, durations. The parameters are those of the *underlying normal*,
    /// which is the usual source of confusion, so both are named for it.
    Lognormal { log_mean: f64, log_std_dev: f64 },
    /// Everything in the range equally likely. Honest when all that is known is
    /// a floor and a ceiling.
    Uniform { low: f64, high: f64 },
    /// Three-point estimation: worst, most likely, best. The distribution of
    /// choice when an expert can give those three numbers and nothing more.
    Triangular { low: f64, mode: f64, high: f64 },
    /// A bounded shape with real flexibility. Scaled onto `[low, high]` rather
    /// than left on `[0, 1]`, because a bare beta is almost never the quantity
    /// anyone actually wants to model.
    Beta {
        alpha: f64,
        beta: f64,
        #[serde(default)]
        low: f64,
        #[serde(default = "one")]
        high: f64,
    },
    /// Time until an event, given a constant rate.
    Exponential { rate: f64 },
    /// Counts of independent events in a fixed interval.
    Poisson { lambda: f64 },
    /// A single yes or no.
    Bernoulli { p: f64 },
    /// How many of `trials` succeed.
    Binomial { trials: u32, p: f64 },
    /// A named set of outcomes with weights. The right shape for a scenario
    /// variable — "the rate is 20.6%, or 21.4% if the proposal passes".
    Discrete { values: Vec<f64>, weights: Vec<f64> },
    /// An arbitrary distribution given as its cumulative points.
    ///
    /// `points` are `(value, cumulative_probability)` in increasing order,
    /// starting at 0 and ending at 1. Sampling inverts it piecewise-linearly,
    /// which is exactly what an analyst means when they sketch "there is a 30%
    /// chance of being under 100 and a 90% chance of being under 400".
    Custom { points: Vec<(f64, f64)> },
}

fn one() -> f64 {
    1.0
}

impl Distribution {
    /// The stable machine name. Used in the API, in storage and in the UI.
    pub fn kind(&self) -> &'static str {
        match self {
            Distribution::Normal { .. } => "normal",
            Distribution::Lognormal { .. } => "lognormal",
            Distribution::Uniform { .. } => "uniform",
            Distribution::Triangular { .. } => "triangular",
            Distribution::Beta { .. } => "beta",
            Distribution::Exponential { .. } => "exponential",
            Distribution::Poisson { .. } => "poisson",
            Distribution::Bernoulli { .. } => "bernoulli",
            Distribution::Binomial { .. } => "binomial",
            Distribution::Discrete { .. } => "discrete",
            Distribution::Custom { .. } => "custom",
        }
    }

    /// What to call it in the interface, in Swedish, and what it is for.
    ///
    /// Kept beside the mathematics on purpose. A picker that offers eleven
    /// distributions by name and no guidance is a picker whose users choose
    /// Normal every time, including for the quantities that cannot go below
    /// zero.
    pub fn label(&self) -> (&'static str, &'static str) {
        match self {
            Distribution::Normal { .. } => (
                "Normalfördelning",
                "För storheter som varierar symmetriskt kring ett väntevärde. \
                 Kan bli negativ — olämplig för volymer och priser.",
            ),
            Distribution::Lognormal { .. } => (
                "Lognormalfördelning",
                "För storheter som aldrig kan bli negativa och varierar \
                 multiplikativt. Parametrarna avser den underliggande normalfördelningen.",
            ),
            Distribution::Uniform { .. } => (
                "Likformig fördelning",
                "När endast ett golv och ett tak är känt och inget värde \
                 däremellan är mer troligt än något annat.",
            ),
            Distribution::Triangular { .. } => (
                "Triangulärfördelning",
                "Trepunktsskattning: lägsta, mest sannolika och högsta värdet.",
            ),
            Distribution::Beta { .. } => (
                "Betafördelning",
                "Flexibel form inom ett intervall. Används för andelar och \
                 för osäkerhet som inte är symmetrisk.",
            ),
            Distribution::Exponential { .. } => (
                "Exponentialfördelning",
                "Tid till en händelse vid konstant intensitet.",
            ),
            Distribution::Poisson { .. } => (
                "Poissonfördelning",
                "Antal oberoende händelser under en given period.",
            ),
            Distribution::Bernoulli { .. } => (
                "Bernoullifördelning",
                "Ett enskilt utfall: inträffar eller inte.",
            ),
            Distribution::Binomial { .. } => (
                "Binomialfördelning",
                "Antal lyckade utfall av ett bestämt antal oberoende försök.",
            ),
            Distribution::Discrete { .. } => (
                "Diskret fördelning",
                "Ett antal namngivna utfall med varsin sannolikhet. \
                 Lämplig för scenarier.",
            ),
            Distribution::Custom { .. } => (
                "Egen fördelning",
                "Anges som kumulativa punkter: värde och sannolikheten att \
                 hamna under det värdet.",
            ),
        }
    }

    /// The parameters, named, for the interface and for the audit record.
    pub fn parameters(&self) -> Vec<(&'static str, serde_json::Value)> {
        use serde_json::json;
        match self {
            Distribution::Normal { mean, std_dev } => {
                vec![("mean", json!(mean)), ("std_dev", json!(std_dev))]
            }
            Distribution::Lognormal {
                log_mean,
                log_std_dev,
            } => vec![
                ("log_mean", json!(log_mean)),
                ("log_std_dev", json!(log_std_dev)),
            ],
            Distribution::Uniform { low, high } => {
                vec![("low", json!(low)), ("high", json!(high))]
            }
            Distribution::Triangular { low, mode, high } => vec![
                ("low", json!(low)),
                ("mode", json!(mode)),
                ("high", json!(high)),
            ],
            Distribution::Beta {
                alpha,
                beta,
                low,
                high,
            } => vec![
                ("alpha", json!(alpha)),
                ("beta", json!(beta)),
                ("low", json!(low)),
                ("high", json!(high)),
            ],
            Distribution::Exponential { rate } => vec![("rate", json!(rate))],
            Distribution::Poisson { lambda } => vec![("lambda", json!(lambda))],
            Distribution::Bernoulli { p } => vec![("p", json!(p))],
            Distribution::Binomial { trials, p } => {
                vec![("trials", json!(trials)), ("p", json!(p))]
            }
            Distribution::Discrete { values, weights } => {
                vec![("values", json!(values)), ("weights", json!(weights))]
            }
            Distribution::Custom { points } => vec![("points", json!(points))],
        }
    }

    /// Rejects a specification that cannot produce meaningful samples.
    ///
    /// Every branch names the offending parameter. "Invalid distribution" tells
    /// a user nothing they can act on; "std_dev must be greater than zero, and
    /// is -1" tells them exactly what to change.
    pub fn validate(&self) -> Result<(), DistributionError> {
        let finite = |name: &'static str, value: f64| -> Result<(), DistributionError> {
            if value.is_finite() {
                Ok(())
            } else {
                Err(DistributionError::parameter(name, "a finite number", value))
            }
        };

        match self {
            Distribution::Normal { mean, std_dev } => {
                finite("mean", *mean)?;
                finite("std_dev", *std_dev)?;
                // Zero is allowed and means "this input is certain". Useful:
                // it lets a sensitivity run hold one variable fixed without
                // deleting it from the model.
                if *std_dev < 0.0 {
                    return Err(DistributionError::parameter(
                        "std_dev",
                        "zero or greater",
                        *std_dev,
                    ));
                }
            }
            Distribution::Lognormal {
                log_mean,
                log_std_dev,
            } => {
                finite("log_mean", *log_mean)?;
                finite("log_std_dev", *log_std_dev)?;
                if *log_std_dev < 0.0 {
                    return Err(DistributionError::parameter(
                        "log_std_dev",
                        "zero or greater",
                        *log_std_dev,
                    ));
                }
                // exp() overflows to infinity beyond about 709. A specification
                // that can only produce infinities is rejected here rather than
                // discovered as a NaN in the statistics.
                if log_mean + 8.0 * log_std_dev > 709.0 {
                    return Err(DistributionError::Shape(
                        "log_mean and log_std_dev together overflow: exp() of the \
                         upper tail exceeds what a 64-bit float can hold"
                            .into(),
                    ));
                }
            }
            Distribution::Uniform { low, high } => {
                finite("low", *low)?;
                finite("high", *high)?;
                if high < low {
                    return Err(DistributionError::Shape(format!(
                        "high ({high}) is below low ({low})"
                    )));
                }
            }
            Distribution::Triangular { low, mode, high } => {
                finite("low", *low)?;
                finite("mode", *mode)?;
                finite("high", *high)?;
                if high < low {
                    return Err(DistributionError::Shape(format!(
                        "high ({high}) is below low ({low})"
                    )));
                }
                if mode < low || mode > high {
                    return Err(DistributionError::Shape(format!(
                        "mode ({mode}) is outside [{low}, {high}] — a triangular \
                         distribution whose peak is outside its range is not a \
                         distribution"
                    )));
                }
            }
            Distribution::Beta {
                alpha,
                beta,
                low,
                high,
            } => {
                finite("alpha", *alpha)?;
                finite("beta", *beta)?;
                finite("low", *low)?;
                finite("high", *high)?;
                if *alpha <= 0.0 {
                    return Err(DistributionError::parameter(
                        "alpha",
                        "greater than zero",
                        *alpha,
                    ));
                }
                if *beta <= 0.0 {
                    return Err(DistributionError::parameter(
                        "beta",
                        "greater than zero",
                        *beta,
                    ));
                }
                if high < low {
                    return Err(DistributionError::Shape(format!(
                        "high ({high}) is below low ({low})"
                    )));
                }
            }
            Distribution::Exponential { rate } => {
                finite("rate", *rate)?;
                if *rate <= 0.0 {
                    return Err(DistributionError::parameter(
                        "rate",
                        "greater than zero",
                        *rate,
                    ));
                }
            }
            Distribution::Poisson { lambda } => {
                finite("lambda", *lambda)?;
                if *lambda < 0.0 {
                    return Err(DistributionError::parameter(
                        "lambda",
                        "zero or greater",
                        *lambda,
                    ));
                }
                if *lambda > 1e9 {
                    return Err(DistributionError::parameter(
                        "lambda",
                        "at most 1e9, above which counts are better modelled as continuous",
                        *lambda,
                    ));
                }
            }
            Distribution::Bernoulli { p } => {
                finite("p", *p)?;
                if !(0.0..=1.0).contains(p) {
                    return Err(DistributionError::parameter(
                        "p",
                        "a probability between 0 and 1",
                        *p,
                    ));
                }
            }
            Distribution::Binomial { trials, p } => {
                finite("p", *p)?;
                if !(0.0..=1.0).contains(p) {
                    return Err(DistributionError::parameter(
                        "p",
                        "a probability between 0 and 1",
                        *p,
                    ));
                }
                if *trials == 0 {
                    return Err(DistributionError::Shape(
                        "trials is zero, so the outcome is always zero; state it as a \
                         constant rather than a distribution"
                            .into(),
                    ));
                }
            }
            Distribution::Discrete { values, weights } => {
                if values.is_empty() {
                    return Err(DistributionError::Shape("values is empty".into()));
                }
                if values.len() > MAX_CATEGORIES {
                    return Err(DistributionError::Shape(format!(
                        "{} outcomes, and at most {MAX_CATEGORIES} are allowed; a variable \
                         with more outcomes than that is a continuous quantity and should \
                         be modelled as one",
                        values.len()
                    )));
                }
                if values.len() != weights.len() {
                    return Err(DistributionError::Shape(format!(
                        "{} values but {} weights",
                        values.len(),
                        weights.len()
                    )));
                }
                for value in values {
                    finite("values", *value)?;
                }
                let mut total = 0.0;
                for weight in weights {
                    finite("weights", *weight)?;
                    if *weight < 0.0 {
                        return Err(DistributionError::parameter(
                            "weights",
                            "zero or greater",
                            *weight,
                        ));
                    }
                    total += weight;
                }
                if total <= 0.0 {
                    return Err(DistributionError::Shape(
                        "the weights sum to zero, so no outcome can occur".into(),
                    ));
                }
            }
            Distribution::Custom { points } => {
                if points.len() < 2 {
                    return Err(DistributionError::Shape(
                        "a custom distribution needs at least two cumulative points".into(),
                    ));
                }
                if points.len() > MAX_CATEGORIES {
                    return Err(DistributionError::Shape(format!(
                        "{} points, and at most {MAX_CATEGORIES} are allowed",
                        points.len()
                    )));
                }
                let (first_value, first_p) = points[0];
                let (last_value, last_p) = points[points.len() - 1];
                finite("points", first_value)?;
                finite("points", last_value)?;
                if (first_p - 0.0).abs() > 1e-9 {
                    return Err(DistributionError::Shape(format!(
                        "the first cumulative probability is {first_p}, and must be 0"
                    )));
                }
                if (last_p - 1.0).abs() > 1e-9 {
                    return Err(DistributionError::Shape(format!(
                        "the last cumulative probability is {last_p}, and must be 1"
                    )));
                }
                for window in points.windows(2) {
                    let (v0, p0) = window[0];
                    let (v1, p1) = window[1];
                    finite("points", v1)?;
                    if v1 < v0 {
                        return Err(DistributionError::Shape(format!(
                            "values must not decrease: {v1} follows {v0}"
                        )));
                    }
                    if p1 < p0 {
                        return Err(DistributionError::Shape(format!(
                            "cumulative probabilities must not decrease: {p1} follows {p0}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// The mean and variance, from the closed form.
    pub fn moments(&self) -> Moments {
        match self {
            Distribution::Normal { mean, std_dev } => Moments {
                mean: *mean,
                variance: std_dev * std_dev,
                min: None,
                max: None,
            },
            Distribution::Lognormal {
                log_mean,
                log_std_dev,
            } => {
                let s2 = log_std_dev * log_std_dev;
                Moments {
                    mean: (log_mean + s2 / 2.0).exp(),
                    variance: (s2.exp() - 1.0) * (2.0 * log_mean + s2).exp(),
                    min: Some(0.0),
                    max: None,
                }
            }
            Distribution::Uniform { low, high } => Moments {
                mean: (low + high) / 2.0,
                variance: (high - low) * (high - low) / 12.0,
                min: Some(*low),
                max: Some(*high),
            },
            Distribution::Triangular { low, mode, high } => Moments {
                mean: (low + mode + high) / 3.0,
                variance: (low * low + mode * mode + high * high
                    - low * mode
                    - low * high
                    - mode * high)
                    / 18.0,
                min: Some(*low),
                max: Some(*high),
            },
            Distribution::Beta {
                alpha,
                beta,
                low,
                high,
            } => {
                let span = high - low;
                let total = alpha + beta;
                Moments {
                    mean: low + span * (alpha / total),
                    variance: span * span * (alpha * beta) / (total * total * (total + 1.0)),
                    min: Some(*low),
                    max: Some(*high),
                }
            }
            Distribution::Exponential { rate } => Moments {
                mean: 1.0 / rate,
                variance: 1.0 / (rate * rate),
                min: Some(0.0),
                max: None,
            },
            Distribution::Poisson { lambda } => Moments {
                mean: *lambda,
                variance: *lambda,
                min: Some(0.0),
                max: None,
            },
            Distribution::Bernoulli { p } => Moments {
                mean: *p,
                variance: p * (1.0 - p),
                min: Some(0.0),
                max: Some(1.0),
            },
            Distribution::Binomial { trials, p } => {
                let n = f64::from(*trials);
                Moments {
                    mean: n * p,
                    variance: n * p * (1.0 - p),
                    min: Some(0.0),
                    max: Some(n),
                }
            }
            Distribution::Discrete { values, weights } => {
                let total: f64 = weights.iter().sum();
                let mut mean = 0.0;
                let mut second = 0.0;
                for (value, weight) in values.iter().zip(weights) {
                    let p = weight / total;
                    mean += p * value;
                    second += p * value * value;
                }
                Moments {
                    mean,
                    variance: (second - mean * mean).max(0.0),
                    min: values.iter().copied().fold(f64::INFINITY, f64::min).into(),
                    max: values
                        .iter()
                        .copied()
                        .fold(f64::NEG_INFINITY, f64::max)
                        .into(),
                }
            }
            Distribution::Custom { points } => {
                // Piecewise-linear inverse CDF: within a segment the value is
                // uniform, so the segment contributes its own mean and second
                // moment weighted by the probability mass it carries.
                let mut mean = 0.0;
                let mut second = 0.0;
                for window in points.windows(2) {
                    let (v0, p0) = window[0];
                    let (v1, p1) = window[1];
                    let mass = p1 - p0;
                    if mass <= 0.0 {
                        continue;
                    }
                    mean += mass * (v0 + v1) / 2.0;
                    second += mass * (v0 * v0 + v0 * v1 + v1 * v1) / 3.0;
                }
                Moments {
                    mean,
                    variance: (second - mean * mean).max(0.0),
                    min: Some(points[0].0),
                    max: Some(points[points.len() - 1].0),
                }
            }
        }
    }

    /// Prepares this distribution for repeated sampling.
    ///
    /// The only way to draw from a distribution. There is deliberately no
    /// `Distribution::sample`: the tabular kinds need a cumulative table, and a
    /// method that looked like a cheap draw while rebuilding that table on
    /// every call is exactly the trap this crate was carrying.
    pub fn sampler(&self) -> Sampler {
        Sampler::new(self.clone())
    }

    /// Draws one sample, assuming any lookup table has already been built.
    ///
    /// Private: reached through `Sampler`, which owns the table.
    fn draw(&self, rng: &mut Rng, cumulative: &[f64]) -> f64 {
        match self {
            Distribution::Normal { mean, std_dev } => mean + std_dev * rng.standard_normal(),
            Distribution::Lognormal {
                log_mean,
                log_std_dev,
            } => (log_mean + log_std_dev * rng.standard_normal()).exp(),
            Distribution::Uniform { low, high } => rng.uniform(*low, *high),
            Distribution::Triangular { low, mode, high } => {
                // Inverse transform. The split point is where the CDF reaches
                // the mode; below it the inverse follows the rising edge, above
                // it the falling one.
                let span = high - low;
                if span <= 0.0 {
                    return *low;
                }
                let split = (mode - low) / span;
                let u = rng.uniform01();
                if u < split {
                    low + (u * span * (mode - low)).sqrt()
                } else {
                    high - ((1.0 - u) * span * (high - mode)).sqrt()
                }
            }
            Distribution::Beta {
                alpha,
                beta,
                low,
                high,
            } => {
                let x = rng.standard_gamma(*alpha);
                let y = rng.standard_gamma(*beta);
                let unit = if x + y > 0.0 { x / (x + y) } else { 0.5 };
                low + (high - low) * unit
            }
            Distribution::Exponential { rate } => -rng.open_uniform01().ln() / rate,
            Distribution::Poisson { lambda } => sample_poisson(rng, *lambda),
            Distribution::Bernoulli { p } => {
                if rng.uniform01() < *p {
                    1.0
                } else {
                    0.0
                }
            }
            Distribution::Binomial { trials, p } => sample_binomial(rng, *trials, *p),
            Distribution::Discrete { values, .. } => {
                // Binary search over the prepared table rather than a scan over
                // the weights. At the category bound that is ten comparisons
                // instead of a thousand additions, and the difference is the
                // difference between a bounded run and a held thread.
                values[select(cumulative, rng.uniform01()).min(values.len() - 1)]
            }
            Distribution::Custom { points } => {
                let u = rng.uniform01();
                // `cumulative` holds the points' own probabilities, so the
                // search finds the segment `u` falls in without walking to it.
                let upper = select(cumulative, u).clamp(1, points.len() - 1);
                let (v0, p0) = points[upper - 1];
                let (v1, p1) = points[upper];
                let mass = p1 - p0;
                if mass <= 0.0 {
                    v1
                } else {
                    v0 + (v1 - v0) * ((u - p0) / mass)
                }
            }
        }
    }
}

/// A distribution with whatever lookup table it needs already built.
///
/// The engine holds one of these per input for the length of a run, so the
/// table is built once rather than once per draw. For the nine distributions
/// with no table it is a thin wrapper and costs nothing.
#[derive(Debug, Clone)]
pub struct Sampler {
    distribution: Distribution,
    /// Cumulative probabilities, normalised to end at 1. Empty for the
    /// distributions that need no table.
    cumulative: Vec<f64>,
}

impl Sampler {
    pub fn new(distribution: Distribution) -> Self {
        let cumulative = match &distribution {
            Distribution::Discrete { weights, .. } => {
                let total: f64 = weights.iter().sum();
                let mut running = 0.0;
                weights
                    .iter()
                    .map(|weight| {
                        running += weight;
                        // Normalised here so the draw is a single uniform in
                        // [0, 1) with no multiplication in the inner loop.
                        if total > 0.0 {
                            running / total
                        } else {
                            1.0
                        }
                    })
                    .collect()
            }
            Distribution::Custom { points } => points.iter().map(|(_, p)| *p).collect(),
            _ => Vec::new(),
        };
        Self {
            distribution,
            cumulative,
        }
    }

    #[inline]
    pub fn sample(&self, rng: &mut Rng) -> f64 {
        self.distribution.draw(rng, &self.cumulative)
    }

    pub fn distribution(&self) -> &Distribution {
        &self.distribution
    }
}

/// The first index whose cumulative probability exceeds `u`.
///
/// `partition_point` is a binary search. Zero-weight outcomes repeat the
/// previous cumulative value, and because the predicate is `<=` they are
/// stepped over rather than selected — which is what a weight of zero means.
#[inline]
fn select(cumulative: &[f64], u: f64) -> usize {
    cumulative.partition_point(|edge| *edge <= u)
}

/// `ln(Γ(x))` by the Lanczos approximation, `g = 7`, nine coefficients.
///
/// Accurate to about fifteen significant figures over the range the samplers
/// use it in. Needed by the rejection samplers below, which compare a uniform
/// against a log-probability.
pub(crate) fn ln_gamma(x: f64) -> f64 {
    const COEFFICIENTS: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        // Reflection, so the series is only ever evaluated where it converges.
        std::f64::consts::PI.ln() - (std::f64::consts::PI * x).sin().ln() - ln_gamma(1.0 - x)
    } else {
        let x = x - 1.0;
        let mut series = COEFFICIENTS[0];
        for (index, coefficient) in COEFFICIENTS.iter().enumerate().skip(1) {
            series += coefficient / (x + index as f64);
        }
        let t = x + 7.5;
        0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + series.ln()
    }
}

/// Poisson variates.
///
/// Two algorithms, and the boundary is not arbitrary. Knuth's product method
/// consumes one uniform per unit of `lambda`, so at `lambda = 1000` it costs a
/// thousand draws for one sample — at a million iterations that is a billion
/// uniforms. Above ten, Hörmann's transformed rejection takes a bounded number
/// of draws regardless of `lambda`.
fn sample_poisson(rng: &mut Rng, lambda: f64) -> f64 {
    if lambda <= 0.0 {
        return 0.0;
    }
    if lambda < 10.0 {
        // Knuth. Exact, and at this size the loop is short.
        let limit = (-lambda).exp();
        let mut product = 1.0;
        let mut count = 0.0;
        loop {
            product *= rng.uniform01();
            if product <= limit {
                return count;
            }
            count += 1.0;
            if count > 1e6 {
                return count; // unreachable for lambda < 10; a guard, not a path
            }
        }
    }

    // Hörmann's PTRS.
    let b = 0.931 + 2.53 * lambda.sqrt();
    let a = -0.059 + 0.02483 * b;
    let inverse_alpha = 1.1239 + 1.1328 / (b - 3.4);
    let v_r = 0.9277 - 3.6224 / (b - 2.0);
    let ln_lambda = lambda.ln();

    loop {
        let u = rng.uniform01() - 0.5;
        let v = rng.open_uniform01();
        let us = 0.5 - u.abs();
        let k = ((2.0 * a / us + b) * u + lambda + 0.43).floor();

        if us >= 0.07 && v <= v_r {
            return k;
        }
        if k < 0.0 || (us < 0.013 && v > us) {
            continue;
        }
        let accept = (v * inverse_alpha / (a / (us * us) + b)).ln()
            <= -lambda + k * ln_lambda - ln_gamma(k + 1.0);
        if accept {
            return k;
        }
    }
}

/// Binomial variates.
///
/// Small `trials` is summed directly, which is exact and faster than any
/// rejection scheme at that size. Larger uses Hörmann's BTRS, with the standard
/// reflection so the algorithm only ever sees `p <= 0.5` — its constants are
/// derived for that half.
fn sample_binomial(rng: &mut Rng, trials: u32, p: f64) -> f64 {
    if p <= 0.0 {
        return 0.0;
    }
    if p >= 1.0 {
        return f64::from(trials);
    }
    if trials <= 128 {
        let mut successes = 0u32;
        for _ in 0..trials {
            if rng.uniform01() < p {
                successes += 1;
            }
        }
        return f64::from(successes);
    }

    let flipped = p > 0.5;
    let p = if flipped { 1.0 - p } else { p };
    let n = f64::from(trials);

    let spq = (n * p * (1.0 - p)).sqrt();
    let b = 1.15 + 2.53 * spq;
    let a = -0.0873 + 0.0248 * b + 0.01 * p;
    let c = n * p + 0.5;
    let v_r = 0.92 - 4.2 / b;
    let alpha = (2.83 + 5.1 / b) * spq;
    let log_odds = (p / (1.0 - p)).ln();
    let ln_gamma_n1 = ln_gamma(n + 1.0);

    let drawn = loop {
        let u = rng.uniform01() - 0.5;
        let v = rng.open_uniform01();
        let us = 0.5 - u.abs();
        let k = ((2.0 * a / us + b) * u + c).floor();

        if k < 0.0 || k > n {
            continue;
        }
        if us >= 0.07 && v <= v_r {
            break k;
        }
        let v = (v * alpha / (a / (us * us) + b)).ln();
        let bound = (n * p + 0.5).floor();
        let upper = ln_gamma_n1 - ln_gamma(bound + 1.0) - ln_gamma(n - bound + 1.0)
            + (bound - n * p) * log_odds;
        let here = ln_gamma_n1 - ln_gamma(k + 1.0) - ln_gamma(n - k + 1.0) + (k - n * p) * log_odds;
        if v <= here - upper {
            break k;
        }
    };

    if flipped {
        n - drawn
    } else {
        drawn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empirical(distribution: &Distribution, n: usize, seed: u64) -> (f64, f64, f64, f64) {
        let mut rng = Rng::new(seed);
        let sampler = distribution.sampler();
        let samples: Vec<f64> = (0..n).map(|_| sampler.sample(&mut rng)).collect();
        let mean = samples.iter().sum::<f64>() / n as f64;
        let variance = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let max = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        (mean, variance, min, max)
    }

    /// The statistical test of section 21, applied to every distribution.
    ///
    /// A sampler is checked against the closed-form moments, which are computed
    /// by different code. Tolerances are relative and generous enough not to
    /// flake, and tight enough that a wrong parameterisation — a variance used
    /// where a standard deviation belongs, say — fails.
    #[test]
    fn every_sampler_matches_its_analytic_moments() {
        let cases = vec![
            Distribution::Normal {
                mean: 120.0,
                std_dev: 15.0,
            },
            Distribution::Lognormal {
                log_mean: 3.0,
                log_std_dev: 0.5,
            },
            Distribution::Uniform {
                low: -20.0,
                high: 80.0,
            },
            Distribution::Triangular {
                low: 10.0,
                mode: 25.0,
                high: 90.0,
            },
            Distribution::Beta {
                alpha: 2.0,
                beta: 5.0,
                low: 0.0,
                high: 1.0,
            },
            Distribution::Beta {
                alpha: 0.7,
                beta: 0.9,
                low: 100.0,
                high: 400.0,
            },
            Distribution::Exponential { rate: 0.25 },
            Distribution::Poisson { lambda: 3.5 },
            Distribution::Poisson { lambda: 250.0 },
            Distribution::Bernoulli { p: 0.3 },
            Distribution::Binomial {
                trials: 40,
                p: 0.25,
            },
            Distribution::Binomial {
                trials: 5000,
                p: 0.6,
            },
            Distribution::Discrete {
                values: vec![10.0, 20.0, 35.0],
                weights: vec![1.0, 3.0, 1.0],
            },
            Distribution::Custom {
                points: vec![(0.0, 0.0), (100.0, 0.3), (400.0, 0.9), (500.0, 1.0)],
            },
        ];

        for distribution in cases {
            distribution.validate().expect("the case is valid");
            let expected = distribution.moments();
            let (mean, variance, min, max) = empirical(&distribution, 200_000, 20_250_812);

            let mean_tolerance = 0.02 * expected.mean.abs().max(expected.std_dev()).max(1e-9);
            assert!(
                (mean - expected.mean).abs() < mean_tolerance,
                "{}: sampled mean {mean} against analytic {}",
                distribution.kind(),
                expected.mean
            );

            if expected.variance > 0.0 {
                let ratio = variance / expected.variance;
                assert!(
                    (0.93..1.07).contains(&ratio),
                    "{}: sampled variance {variance} against analytic {} (ratio {ratio})",
                    distribution.kind(),
                    expected.variance
                );
            }

            if let Some(floor) = expected.min {
                assert!(
                    min >= floor - 1e-9,
                    "{}: sampled {min}, below the support floor {floor}",
                    distribution.kind()
                );
            }
            if let Some(ceiling) = expected.max {
                assert!(
                    max <= ceiling + 1e-9,
                    "{}: sampled {max}, above the support ceiling {ceiling}",
                    distribution.kind()
                );
            }
        }
    }

    #[test]
    fn samples_are_always_finite() {
        let cases = vec![
            Distribution::Normal {
                mean: 0.0,
                std_dev: 1e6,
            },
            Distribution::Lognormal {
                log_mean: 0.0,
                log_std_dev: 3.0,
            },
            Distribution::Exponential { rate: 1e-6 },
            Distribution::Beta {
                alpha: 0.01,
                beta: 0.01,
                low: 0.0,
                high: 1.0,
            },
            Distribution::Poisson { lambda: 1e6 },
        ];
        for distribution in cases {
            distribution.validate().expect("valid");
            let mut rng = Rng::new(1);
            let sampler = distribution.sampler();
            for _ in 0..50_000 {
                let x = sampler.sample(&mut rng);
                assert!(x.is_finite(), "{} produced {x}", distribution.kind());
            }
        }
    }

    #[test]
    fn a_zero_variance_input_is_allowed_and_is_constant() {
        let fixed = Distribution::Normal {
            mean: 42.0,
            std_dev: 0.0,
        };
        fixed.validate().expect("a certain input is a valid input");
        let mut rng = Rng::new(1);
        for _ in 0..1000 {
            assert_eq!(fixed.sampler().sample(&mut rng), 42.0);
        }
    }

    #[test]
    fn a_degenerate_uniform_is_allowed_and_is_constant() {
        let fixed = Distribution::Uniform {
            low: 5.0,
            high: 5.0,
        };
        fixed.validate().expect("valid");
        let mut rng = Rng::new(1);
        assert_eq!(fixed.sampler().sample(&mut rng), 5.0);
    }

    #[test]
    fn a_degenerate_triangular_is_allowed_and_is_constant() {
        let fixed = Distribution::Triangular {
            low: 3.0,
            mode: 3.0,
            high: 3.0,
        };
        fixed.validate().expect("valid");
        let mut rng = Rng::new(1);
        assert_eq!(fixed.sampler().sample(&mut rng), 3.0);
    }

    #[test]
    fn invalid_parameters_are_rejected_by_name() {
        let cases: Vec<(Distribution, &str)> = vec![
            (
                Distribution::Normal {
                    mean: 0.0,
                    std_dev: -1.0,
                },
                "std_dev",
            ),
            (
                Distribution::Uniform {
                    low: 10.0,
                    high: 1.0,
                },
                "high",
            ),
            (
                Distribution::Triangular {
                    low: 0.0,
                    mode: 50.0,
                    high: 10.0,
                },
                "mode",
            ),
            (
                Distribution::Beta {
                    alpha: 0.0,
                    beta: 1.0,
                    low: 0.0,
                    high: 1.0,
                },
                "alpha",
            ),
            (Distribution::Exponential { rate: 0.0 }, "rate"),
            (Distribution::Poisson { lambda: -1.0 }, "lambda"),
            (Distribution::Bernoulli { p: 1.5 }, "p"),
            (Distribution::Binomial { trials: 0, p: 0.5 }, "trials"),
            (
                Distribution::Discrete {
                    values: vec![1.0, 2.0],
                    weights: vec![0.0, 0.0],
                },
                "weights",
            ),
            (
                Distribution::Discrete {
                    values: vec![1.0, 2.0],
                    weights: vec![1.0],
                },
                "weights",
            ),
            (
                Distribution::Custom {
                    points: vec![(0.0, 0.0), (10.0, 0.8)],
                },
                "must be 1",
            ),
            (
                Distribution::Custom {
                    points: vec![(0.0, 0.0), (10.0, 0.6), (5.0, 1.0)],
                },
                "must not decrease",
            ),
        ];
        for (distribution, expected) in cases {
            let error = distribution
                .validate()
                .expect_err("this specification is not valid");
            assert!(
                error.to_string().contains(expected),
                "{} said {error}, which does not mention {expected}",
                distribution.kind()
            );
        }
    }

    #[test]
    fn a_nan_parameter_is_rejected() {
        let error = Distribution::Normal {
            mean: f64::NAN,
            std_dev: 1.0,
        }
        .validate()
        .expect_err("NaN is not a mean");
        assert!(error.to_string().contains("mean"));

        let error = Distribution::Uniform {
            low: 0.0,
            high: f64::INFINITY,
        }
        .validate()
        .expect_err("an infinite bound is not a bound");
        assert!(error.to_string().contains("high"));
    }

    #[test]
    fn a_lognormal_that_could_only_overflow_is_rejected() {
        let error = Distribution::Lognormal {
            log_mean: 700.0,
            log_std_dev: 20.0,
        }
        .validate()
        .expect_err("this can only produce infinities");
        assert!(error.to_string().contains("overflow"));
    }

    #[test]
    fn bernoulli_at_the_boundaries_is_certain() {
        let mut rng = Rng::new(1);
        let never = Distribution::Bernoulli { p: 0.0 };
        let always = Distribution::Bernoulli { p: 1.0 };
        for _ in 0..1000 {
            assert_eq!(never.sampler().sample(&mut rng), 0.0);
            assert_eq!(always.sampler().sample(&mut rng), 1.0);
        }
    }

    #[test]
    fn a_discrete_distribution_only_produces_its_own_values() {
        let distribution = Distribution::Discrete {
            values: vec![1.0, 7.0, 9.0],
            weights: vec![1.0, 1.0, 1.0],
        };
        let mut rng = Rng::new(2);
        let sampler = distribution.sampler();
        for _ in 0..10_000 {
            let x = sampler.sample(&mut rng);
            assert!(x == 1.0 || x == 7.0 || x == 9.0, "produced {x}");
        }
    }

    #[test]
    fn a_discrete_outcome_with_zero_weight_never_occurs() {
        let distribution = Distribution::Discrete {
            values: vec![1.0, 2.0, 3.0],
            weights: vec![1.0, 0.0, 1.0],
        };
        let mut rng = Rng::new(3);
        let sampler = distribution.sampler();
        for _ in 0..20_000 {
            assert_ne!(sampler.sample(&mut rng), 2.0);
        }
    }

    #[test]
    fn a_custom_distribution_respects_its_quantiles() {
        // 30% of the mass below 100 by construction. The empirical share should
        // land on it.
        let distribution = Distribution::Custom {
            points: vec![(0.0, 0.0), (100.0, 0.3), (400.0, 0.9), (500.0, 1.0)],
        };
        let mut rng = Rng::new(4);
        let sampler = distribution.sampler();
        let n = 100_000;
        let below = (0..n).filter(|_| sampler.sample(&mut rng) < 100.0).count() as f64 / n as f64;
        assert!((below - 0.3).abs() < 0.01, "share below 100 was {below}");
    }

    #[test]
    fn ln_gamma_matches_known_values() {
        // Γ(5) = 24, Γ(0.5) = √π, Γ(1) = Γ(2) = 1.
        assert!((ln_gamma(5.0) - 24.0_f64.ln()).abs() < 1e-10);
        assert!((ln_gamma(0.5) - std::f64::consts::PI.sqrt().ln()).abs() < 1e-10);
        assert!(ln_gamma(1.0).abs() < 1e-10);
        assert!(ln_gamma(2.0).abs() < 1e-10);
        assert!((ln_gamma(100.0) - 359.134_205_369_575_4).abs() < 1e-8);
    }

    /// The bound that closes the denial of service.
    #[test]
    fn a_tabular_distribution_larger_than_the_bound_is_refused() {
        let huge = Distribution::Discrete {
            values: (0..MAX_CATEGORIES + 1).map(|i| i as f64).collect(),
            weights: vec![1.0; MAX_CATEGORIES + 1],
        };
        let error = huge.validate().expect_err("this is a denial of service");
        assert!(error.to_string().contains("at most"), "{error}");

        let at_the_bound = Distribution::Discrete {
            values: (0..MAX_CATEGORIES).map(|i| i as f64).collect(),
            weights: vec![1.0; MAX_CATEGORIES],
        };
        at_the_bound
            .validate()
            .expect("the bound itself is allowed");

        let many_points = Distribution::Custom {
            points: (0..MAX_CATEGORIES + 1)
                .map(|i| (i as f64, i as f64 / MAX_CATEGORIES as f64))
                .collect(),
        };
        assert!(many_points.validate().is_err());
    }

    /// The other half of the fix: the cost of a draw must not follow the size
    /// of the table.
    ///
    /// Timing in a test is usually a bad idea, and this one is written to be
    /// robust to a slow machine: it compares the *ratio* between a table at the
    /// bound and a table of three, and a linear scan would make that ratio
    /// enormous rather than marginal. A regression to a scan fails this by two
    /// orders of magnitude, not by a few per cent.
    #[test]
    fn the_cost_of_a_draw_does_not_follow_the_size_of_the_table() {
        let draws = 200_000;
        let time = |values: usize| {
            let distribution = Distribution::Discrete {
                values: (0..values).map(|i| i as f64).collect(),
                weights: vec![1.0; values],
            };
            let sampler = distribution.sampler();
            let mut rng = Rng::new(1);
            let started = std::time::Instant::now();
            let mut total = 0.0;
            for _ in 0..draws {
                total += sampler.sample(&mut rng);
            }
            assert!(total > 0.0);
            started.elapsed().as_nanos().max(1)
        };

        let small = time(3);
        let large = time(MAX_CATEGORIES);
        assert!(
            large < small * 20,
            "a draw from {MAX_CATEGORIES} outcomes cost {large} ns against {small} ns \
             from three; the table is being scanned rather than searched"
        );
    }

    /// The table has to select exactly what a scan would have.
    #[test]
    fn the_table_selects_what_a_scan_would_have() {
        let values = vec![10.0, 20.0, 30.0, 40.0, 50.0];
        let weights = vec![3.0, 0.0, 1.0, 6.0, 2.0];
        let distribution = Distribution::Discrete {
            values: values.clone(),
            weights: weights.clone(),
        };
        let sampler = distribution.sampler();

        // The scan this replaced, kept here as the oracle.
        let scan = |u: f64| -> f64 {
            let total: f64 = weights.iter().sum();
            let mut target = u * total;
            for (value, weight) in values.iter().zip(&weights) {
                target -= weight;
                if target <= 0.0 {
                    return *value;
                }
            }
            values[values.len() - 1]
        };

        // A discrete draw consumes exactly one uniform, so two generators on
        // the same seed feed the sampler and the oracle identical inputs.
        let mut drawn = Rng::new(9);
        let mut mirrored = Rng::new(9);
        for _ in 0..50_000 {
            let from_table = sampler.sample(&mut drawn);
            let u = mirrored.uniform01();
            assert_eq!(
                from_table,
                scan(u),
                "the table and the scan disagreed at u = {u}"
            );
        }

        // The boundaries, where the two deliberately differ.
        //
        // Weights [3, 0, 1, 6, 2] put the first outcome's mass on [0, 0.25).
        // The scan tested `target <= 0`, which made that interval closed on the
        // right and gave `u == 0.25` to the first outcome. The table uses the
        // half-open convention, so `u == 0.25` belongs to the next outcome with
        // any weight. Half-open is the correct one, and this is the only input
        // on which the two disagree — a single value out of 2^53, so no
        // statistic moves. It is still a change to what a seed produces, which
        // is why ENGINE_VERSION was bumped for it.
        let edges: Vec<f64> = {
            let total: f64 = weights.iter().sum();
            let mut running = 0.0;
            weights
                .iter()
                .map(|w| {
                    running += w;
                    running / total
                })
                .collect()
        };
        let selected = |u: f64| {
            values[edges
                .partition_point(|edge| *edge <= u)
                .min(values.len() - 1)]
        };

        assert_eq!(selected(0.0), 10.0, "the bottom of the range");
        assert_eq!(selected(0.249_999), 10.0, "just inside the first outcome");
        assert_eq!(selected(0.25), 30.0, "the edge belongs to the next outcome");
        assert_eq!(selected(0.250_001), 30.0);
        assert_eq!(selected(0.999_999), 50.0, "the top of the range");
        // The zero-weight outcome is never reachable from any input.
        for step in 0..10_000 {
            assert_ne!(selected(f64::from(step) / 10_000.0), 20.0);
        }
    }

    #[test]
    fn a_distribution_survives_a_round_trip_through_json() {
        let distribution = Distribution::Triangular {
            low: 1.0,
            mode: 2.0,
            high: 3.0,
        };
        let text = serde_json::to_string(&distribution).unwrap();
        assert!(text.contains("\"kind\":\"triangular\""));
        let back: Distribution = serde_json::from_str(&text).unwrap();
        assert_eq!(back, distribution);
    }

    #[test]
    fn an_unknown_kind_is_a_parse_error_rather_than_a_default() {
        let result: Result<Distribution, _> =
            serde_json::from_str(r#"{"kind":"cauchy","scale":1}"#);
        assert!(result.is_err());
    }
}

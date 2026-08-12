//! What a simulation is, before it has been run.
//!
//! The model of section 4: inputs with distributions, outputs with expressions
//! over them, and the configuration of one run. Everything here is data — it
//! serialises to JSON, is stored as written, and is hashed so that a result can
//! name the exact specification that produced it.
//!
//! One thing deliberately absent: an input does not carry its own `mean`,
//! `median`, `min`, `max` and `std_dev` fields. The specification asks for
//! them, and they are all available — but as *derived* values from the
//! distribution rather than as stored ones. A stored mean beside a stored
//! distribution is two sources of truth for the same fact, and the day someone
//! edits the standard deviation without editing the mean is the day the
//! interface starts lying. `Input::summary` computes them.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::distribution::{Distribution, DistributionError, Moments};
use crate::expr::{Environment, ExprError, Expression};

/// Why a specification was rejected.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SpecError {
    #[error("the simulation has no inputs")]
    NoInputs,
    #[error("the simulation has no outputs")]
    NoOutputs,
    #[error("more than {limit} inputs")]
    TooManyInputs { limit: usize },
    #[error("more than {limit} outputs")]
    TooManyOutputs { limit: usize },
    #[error("the identifier {id:?} is used more than once")]
    DuplicateIdentifier { id: String },
    #[error("{id:?} is not a usable identifier: {reason}")]
    BadIdentifier { id: String, reason: &'static str },
    #[error("input {id:?}: {source}")]
    Distribution {
        id: String,
        #[source]
        source: DistributionError,
    },
    #[error("output {id:?}: {source}")]
    Expression {
        id: String,
        #[source]
        source: ExprError,
    },
    #[error("input {id:?}: the constraint [{low}, {high}] is empty")]
    EmptyConstraint { id: String, low: f64, high: f64 },
    #[error(
        "input {id:?}: the constraint [{low:?}, {high:?}] excludes the whole support of the \
         distribution, so no sample could ever satisfy it"
    )]
    ImpossibleConstraint {
        id: String,
        low: Option<f64>,
        high: Option<f64>,
    },
    #[error("iterations must be between {min} and {max}, and is {given}")]
    Iterations { min: u32, max: u32, given: u32 },
}

/// The most variables one simulation may carry.
///
/// A ceiling rather than a guess: every input costs a retained sample array
/// during sensitivity analysis, and a model with two hundred variables is not
/// a model anyone can interpret. It is also the bound that stops a request from
/// choosing the engine's memory use.
pub const MAX_INPUTS: usize = 64;
/// The most outputs one simulation may carry. Each one retains its full sample
/// vector for exact percentiles, so this bounds memory alongside `iterations`.
pub const MAX_OUTPUTS: usize = 16;
/// The fewest iterations that produce a percentile worth reading. At 100 draws
/// a P99 is the second-largest sample and means nothing.
pub const MIN_ITERATIONS: u32 = 100;
/// The most iterations one run may ask for.
pub const MAX_ITERATIONS: u32 = 10_000_000;

/// How much the person who supplied a number believes it.
///
/// Recorded rather than used in the arithmetic. It belongs in the audit record
/// and on the screen beside the input, and it must never quietly widen a
/// distribution — that would be the system inventing uncertainty the analyst
/// did not state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// What to do with a sample that falls outside an input's permitted range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintMode {
    /// Draw again. The result is the distribution *truncated* to the range, and
    /// it is what an analyst usually means by "it cannot be negative".
    #[default]
    Resample,
    /// Move the sample to the nearest bound. The result is the distribution
    /// *censored*, which piles probability mass onto the two bounds — sometimes
    /// right, and never right by accident.
    Clamp,
}

/// A permitted range for an input.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Constraints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default)]
    pub mode: ConstraintMode,
}

impl Constraints {
    fn is_empty_range(&self) -> bool {
        matches!((self.min, self.max), (Some(low), Some(high)) if low > high)
    }

    #[inline]
    pub fn permits(&self, value: f64) -> bool {
        if let Some(low) = self.min {
            if value < low {
                return false;
            }
        }
        if let Some(high) = self.max {
            if value > high {
                return false;
            }
        }
        true
    }

    #[inline]
    pub fn clamp(&self, value: f64) -> f64 {
        let mut value = value;
        if let Some(low) = self.min {
            value = value.max(low);
        }
        if let Some(high) = self.max {
            value = value.min(high);
        }
        value
    }
}

/// One uncertain quantity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    /// The name used in expressions. Stable; renaming it is a model change.
    pub id: String,
    /// What to call it on screen.
    pub name: String,
    pub distribution: Distribution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Where the number came from. A simulation whose inputs have no stated
    /// source is a simulation nobody can defend afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<Constraints>,
}

/// The derived facts about an input, for the interface and the audit record.
#[derive(Debug, Clone, Serialize)]
pub struct InputSummary {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub label: &'static str,
    pub guidance: &'static str,
    pub parameters: serde_json::Value,
    pub mean: f64,
    pub std_dev: f64,
    pub variance: f64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub unit: Option<String>,
    pub source: Option<String>,
    pub confidence: Option<Confidence>,
    pub constraints: Option<Constraints>,
}

impl Input {
    pub fn moments(&self) -> Moments {
        self.distribution.moments()
    }

    /// Everything derived, computed rather than stored.
    pub fn summary(&self) -> InputSummary {
        let moments = self.moments();
        let (label, guidance) = self.distribution.label();
        let parameters = serde_json::Value::Object(
            self.distribution
                .parameters()
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
        );
        InputSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.distribution.kind(),
            label,
            guidance,
            parameters,
            mean: moments.mean,
            std_dev: moments.std_dev(),
            variance: moments.variance,
            min: moments.min,
            max: moments.max,
            unit: self.unit.clone(),
            source: self.source.clone(),
            confidence: self.confidence,
            constraints: self.constraints,
        }
    }
}

/// Which side of a target counts as reaching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetDirection {
    /// The target is a floor: revenue, savings, return.
    #[default]
    AtLeast,
    /// The target is a ceiling: cost, time, exposure.
    AtMost,
}

/// One calculated result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Output {
    pub id: String,
    pub name: String,
    /// An expression over the input identifiers and over any output declared
    /// before this one.
    pub expression: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The value the decision is about. Reported as a probability of reaching
    /// it, never as a verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<f64>,
    #[serde(default)]
    pub target_direction: TargetDirection,
    /// The value below which the outcome is a problem. Reported as the
    /// probability of falling below it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical_threshold: Option<f64>,
}

/// A whole model: what is uncertain, and what is computed from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulationSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
}

/// A specification that has been checked and compiled.
///
/// Holding this is proof the model is runnable: every distribution validated,
/// every expression parsed, every name resolved. The engine takes one of these
/// rather than a raw spec, so there is no path that runs an unvalidated model.
#[derive(Debug, Clone)]
pub struct CompiledSpec {
    pub spec: SimulationSpec,
    pub(crate) expressions: Vec<Expression>,
    /// Total slots: one per input, then one per output.
    pub(crate) slots: usize,
}

impl CompiledSpec {
    pub fn input_slot(&self, index: usize) -> usize {
        index
    }

    pub fn output_slot(&self, index: usize) -> usize {
        self.spec.inputs.len() + index
    }

    pub fn expressions(&self) -> &[Expression] {
        &self.expressions
    }

    pub fn slots(&self) -> usize {
        self.slots
    }
}

fn check_identifier(id: &str) -> Result<(), SpecError> {
    if id.is_empty() {
        return Err(SpecError::BadIdentifier {
            id: id.to_string(),
            reason: "it is empty",
        });
    }
    if id.len() > 64 {
        return Err(SpecError::BadIdentifier {
            id: id.to_string(),
            reason: "it is longer than 64 characters",
        });
    }
    let first = id.chars().next().expect("not empty");
    if !(first.is_alphabetic() || first == '_') {
        return Err(SpecError::BadIdentifier {
            id: id.to_string(),
            reason: "it must start with a letter or an underscore",
        });
    }
    if !id.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(SpecError::BadIdentifier {
            id: id.to_string(),
            reason: "it may only contain letters, digits and underscores",
        });
    }
    // Reserved so a model cannot shadow the language's own words and then
    // behave differently from how it reads.
    if matches!(
        id,
        "if" | "and" | "or" | "not" | "pi" | "e" | "true" | "false"
    ) {
        return Err(SpecError::BadIdentifier {
            id: id.to_string(),
            reason: "it is a reserved word in the expression language",
        });
    }
    Ok(())
}

impl SimulationSpec {
    /// Validates everything and compiles the expressions.
    ///
    /// The order matters: distributions first, then constraints against those
    /// distributions, then expressions against the resulting name table. An
    /// error names the input or output it came from, because "invalid model" is
    /// not something a user can act on.
    pub fn compile(&self) -> Result<CompiledSpec, SpecError> {
        if self.inputs.is_empty() {
            return Err(SpecError::NoInputs);
        }
        if self.outputs.is_empty() {
            return Err(SpecError::NoOutputs);
        }
        if self.inputs.len() > MAX_INPUTS {
            return Err(SpecError::TooManyInputs { limit: MAX_INPUTS });
        }
        if self.outputs.len() > MAX_OUTPUTS {
            return Err(SpecError::TooManyOutputs { limit: MAX_OUTPUTS });
        }

        let mut environment = Environment::new();

        for (index, input) in self.inputs.iter().enumerate() {
            check_identifier(&input.id)?;
            if environment.contains_key(&input.id) {
                return Err(SpecError::DuplicateIdentifier {
                    id: input.id.clone(),
                });
            }
            input
                .distribution
                .validate()
                .map_err(|source| SpecError::Distribution {
                    id: input.id.clone(),
                    source,
                })?;

            if let Some(constraints) = input.constraints {
                if constraints.is_empty_range() {
                    return Err(SpecError::EmptyConstraint {
                        id: input.id.clone(),
                        low: constraints.min.unwrap_or(f64::NEG_INFINITY),
                        high: constraints.max.unwrap_or(f64::INFINITY),
                    });
                }
                // A constraint that excludes the distribution's whole support
                // cannot be satisfied by resampling, and would otherwise be
                // discovered as a run that exhausts its attempts a minute in.
                let moments = input.distribution.moments();
                let support_low = moments.min.unwrap_or(f64::NEG_INFINITY);
                let support_high = moments.max.unwrap_or(f64::INFINITY);
                let excluded = constraints.min.is_some_and(|low| low > support_high)
                    || constraints.max.is_some_and(|high| high < support_low);
                if excluded {
                    return Err(SpecError::ImpossibleConstraint {
                        id: input.id.clone(),
                        low: constraints.min,
                        high: constraints.max,
                    });
                }
            }

            environment.insert(input.id.clone(), index);
        }

        let mut expressions = Vec::with_capacity(self.outputs.len());
        for (index, output) in self.outputs.iter().enumerate() {
            check_identifier(&output.id)?;
            if environment.contains_key(&output.id) {
                return Err(SpecError::DuplicateIdentifier {
                    id: output.id.clone(),
                });
            }
            // Compiled against the names declared *so far*, which is what makes
            // `profit = revenue - costs` work while making a circular reference
            // an unknown-name error rather than an infinite loop.
            let compiled =
                Expression::compile(&output.expression, &environment).map_err(|source| {
                    SpecError::Expression {
                        id: output.id.clone(),
                        source,
                    }
                })?;
            expressions.push(compiled);
            environment.insert(output.id.clone(), self.inputs.len() + index);
        }

        Ok(CompiledSpec {
            spec: self.clone(),
            expressions,
            slots: self.inputs.len() + self.outputs.len(),
        })
    }

    /// A stable fingerprint of the model.
    ///
    /// Part of the reproducibility record of section 12: a run stores this, so
    /// "same seed, same inputs" can be checked rather than assumed. Serialised
    /// through `serde_json::Value`, whose maps are sorted, so the hash does not
    /// depend on field order in the request.
    pub fn hash(&self) -> String {
        let canonical = serde_json::to_value(self).expect("a specification is always serialisable");
        let text = serde_json::to_string(&canonical).expect("a Value is always serialisable");
        let digest = Sha256::digest(text.as_bytes());
        hex::encode(digest)
    }
}

/// What one run does with a model.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
    pub iterations: u32,
    /// Fixed by the caller, or drawn once and recorded. Either way it is stored,
    /// because a run whose seed was not recorded cannot be reproduced.
    pub seed: u64,
}

impl RunConfig {
    pub fn validate(&self) -> Result<(), SpecError> {
        if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&self.iterations) {
            return Err(SpecError::Iterations {
                min: MIN_ITERATIONS,
                max: MAX_ITERATIONS,
                given: self.iterations,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(id: &str, distribution: Distribution) -> Input {
        Input {
            id: id.to_string(),
            name: id.to_string(),
            distribution,
            unit: None,
            source: None,
            confidence: None,
            description: None,
            constraints: None,
        }
    }

    fn output(id: &str, expression: &str) -> Output {
        Output {
            id: id.to_string(),
            name: id.to_string(),
            expression: expression.to_string(),
            unit: None,
            description: None,
            target: None,
            target_direction: TargetDirection::AtLeast,
            critical_threshold: None,
        }
    }

    fn spec() -> SimulationSpec {
        SimulationSpec {
            name: "Resultat".into(),
            description: None,
            inputs: vec![
                input(
                    "customers",
                    Distribution::Normal {
                        mean: 1000.0,
                        std_dev: 100.0,
                    },
                ),
                input(
                    "average_revenue",
                    Distribution::Triangular {
                        low: 700.0,
                        mode: 850.0,
                        high: 1100.0,
                    },
                ),
                input(
                    "costs",
                    Distribution::Uniform {
                        low: 400_000.0,
                        high: 600_000.0,
                    },
                ),
            ],
            outputs: vec![
                output("revenue", "customers * average_revenue"),
                output("profit", "revenue - costs"),
            ],
        }
    }

    #[test]
    fn a_well_formed_model_compiles() {
        let compiled = spec().compile().expect("compiles");
        assert_eq!(compiled.slots(), 5);
        assert_eq!(compiled.expressions().len(), 2);
    }

    #[test]
    fn an_output_may_use_an_earlier_output() {
        spec().compile().expect("profit reads revenue");
    }

    #[test]
    fn an_output_may_not_use_a_later_one() {
        let mut broken = spec();
        broken.outputs[0].expression = "profit / 2".into();
        let error = broken.compile().unwrap_err();
        assert!(matches!(error, SpecError::Expression { .. }));
        assert!(error.to_string().contains("profit"));
    }

    #[test]
    fn an_output_may_not_refer_to_itself() {
        let mut broken = spec();
        broken.outputs[0].expression = "revenue + 1".into();
        assert!(broken.compile().is_err());
    }

    #[test]
    fn duplicate_identifiers_are_rejected() {
        let mut broken = spec();
        broken.inputs[1].id = "customers".into();
        assert_eq!(
            broken.compile().unwrap_err(),
            SpecError::DuplicateIdentifier {
                id: "customers".into()
            }
        );

        let mut broken = spec();
        broken.outputs[1].id = "customers".into();
        assert!(matches!(
            broken.compile().unwrap_err(),
            SpecError::DuplicateIdentifier { .. }
        ));
    }

    #[test]
    fn identifiers_are_checked() {
        for bad in ["", "2customers", "cust-omers", "if", "and", "pi"] {
            let mut broken = spec();
            broken.inputs[0].id = bad.into();
            assert!(
                matches!(broken.compile(), Err(SpecError::BadIdentifier { .. })),
                "{bad:?} was accepted as an identifier"
            );
        }
    }

    #[test]
    fn an_empty_model_is_rejected_from_both_ends() {
        let mut broken = spec();
        broken.inputs.clear();
        assert_eq!(broken.compile().unwrap_err(), SpecError::NoInputs);

        let mut broken = spec();
        broken.outputs.clear();
        assert_eq!(broken.compile().unwrap_err(), SpecError::NoOutputs);
    }

    #[test]
    fn an_invalid_distribution_names_its_input() {
        let mut broken = spec();
        broken.inputs[0].distribution = Distribution::Normal {
            mean: 0.0,
            std_dev: -5.0,
        };
        let error = broken.compile().unwrap_err();
        assert!(error.to_string().contains("customers"));
        assert!(error.to_string().contains("std_dev"));
    }

    #[test]
    fn conflicting_constraints_are_rejected() {
        let mut broken = spec();
        broken.inputs[0].constraints = Some(Constraints {
            min: Some(100.0),
            max: Some(10.0),
            mode: ConstraintMode::Resample,
        });
        assert!(matches!(
            broken.compile().unwrap_err(),
            SpecError::EmptyConstraint { .. }
        ));
    }

    #[test]
    fn a_constraint_outside_the_support_is_rejected_before_the_run() {
        // A uniform on [400_000, 600_000] required to be at least a million can
        // never be satisfied. Better a rejected model than a run that exhausts
        // its resampling attempts after a minute of work.
        let mut broken = spec();
        broken.inputs[2].constraints = Some(Constraints {
            min: Some(1_000_000.0),
            max: None,
            mode: ConstraintMode::Resample,
        });
        assert!(matches!(
            broken.compile().unwrap_err(),
            SpecError::ImpossibleConstraint { .. }
        ));
    }

    #[test]
    fn the_hash_follows_the_model_and_not_the_field_order() {
        let a = spec();
        let mut b = spec();
        assert_eq!(a.hash(), b.hash());
        b.inputs[0].distribution = Distribution::Normal {
            mean: 1001.0,
            std_dev: 100.0,
        };
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn iteration_counts_are_bounded_at_both_ends() {
        assert!(RunConfig {
            iterations: 10,
            seed: 1
        }
        .validate()
        .is_err());
        assert!(RunConfig {
            iterations: 50_000_000,
            seed: 1
        }
        .validate()
        .is_err());
        assert!(RunConfig {
            iterations: 10_000,
            seed: 1
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn an_input_summary_derives_rather_than_stores() {
        let input = input(
            "x",
            Distribution::Uniform {
                low: 0.0,
                high: 10.0,
            },
        );
        let summary = input.summary();
        assert_eq!(summary.mean, 5.0);
        assert_eq!(summary.min, Some(0.0));
        assert_eq!(summary.max, Some(10.0));
        assert_eq!(summary.kind, "uniform");
    }
}

//! Deterministic monetary expressions.
//!
//! Every number Skattjakt shows comes from evaluating one of these against the
//! extracted facts. The expression is data, stored with the rule and versioned
//! with it, so a calculation can be re-run years later and produce the same
//! figure — and so no arithmetic is ever delegated to a language model
//! (section 9).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use skattjakt_core::{FactKind, FactSet, Money};
use thiserror::Error;

/// Per-tax-year constants: amounts such as prisbasbelopp, and rates in basis
/// points such as the corporate income tax rate.
///
/// These change annually. Holding them as versioned data rather than literals
/// is what lets a rule survive a rate change without being rewritten.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaxYearConstants {
    pub tax_year: i32,
    /// Every figure the rules use for this year, each with its own citation.
    ///
    /// This used to be two untyped maps plus one free-text sentence describing
    /// where *all* of them came from. Twelve numbers with a dozen different
    /// origins — a tax rate from the Income Tax Act, a base amount fixed
    /// annually by the government, a VAT treatment from a Skatteverket
    /// position — shared one sentence, so "where does 20.6 % come from" had no
    /// answer distinct from "where does prisbasbeloppet come from". A number
    /// whose provenance cannot be stated on its own is a number nobody can
    /// check, and these numbers are what the arithmetic actually multiplies by.
    #[serde(default)]
    pub parameters: BTreeMap<String, Parameter>,
}

/// One figure, and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parameter {
    pub kind: ParameterKind,
    /// Öre for an amount, basis points for a rate. Integers throughout: a rate
    /// of 20.6 % is 2060 bp, so no percentage is ever a float.
    pub value: i64,
    /// A key into the rule set's source registry.
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterKind {
    AmountOre,
    RateBp,
}

impl TaxYearConstants {
    pub fn amount(&self, name: &str) -> Option<Money> {
        self.parameters
            .get(name)
            .filter(|p| p.kind == ParameterKind::AmountOre)
            .map(|p| Money::from_ore(p.value))
    }

    pub fn rate_bp(&self, name: &str) -> Option<i64> {
        self.parameters
            .get(name)
            .filter(|p| p.kind == ParameterKind::RateBp)
            .map(|p| p.value)
    }

    /// Which source a figure comes from. The question that could not be asked
    /// before.
    pub fn source_of(&self, name: &str) -> Option<&str> {
        self.parameters.get(name).map(|p| p.source.as_str())
    }

    /// Records a rate and the source it comes from.
    ///
    /// The source is a required argument rather than an optional one, in
    /// fixtures as much as in the rule set. A constructor that let a figure in
    /// without saying where it came from is how the untraceable numbers got
    /// there in the first place.
    pub fn with_rate(mut self, name: &str, basis_points: i64, source: &str) -> Self {
        self.parameters.insert(
            name.to_string(),
            Parameter {
                kind: ParameterKind::RateBp,
                value: basis_points,
                source: source.to_string(),
            },
        );
        self
    }

    pub fn with_amount(mut self, name: &str, ore: i64, source: &str) -> Self {
        self.parameters.insert(
            name.to_string(),
            Parameter {
                kind: ParameterKind::AmountOre,
                value: ore,
                source: source.to_string(),
            },
        );
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EvalError {
    /// The fact the expression needs was not extracted. This is the common
    /// case and is never an error in the user-facing sense: it downgrades a
    /// finding to "needs more information".
    #[error("missing financial fact: {0}")]
    MissingFact(String),

    #[error("unknown constant `{0}` for this tax year")]
    UnknownConstant(String),

    #[error("unknown rate `{0}` for this tax year")]
    UnknownRate(String),

    #[error("arithmetic overflowed while evaluating `{0}`")]
    Overflow(String),
}

impl EvalError {
    /// Whether the failure means "not enough information" as opposed to "this
    /// rule set is broken". The two lead to very different user-facing states.
    pub fn is_missing_information(&self) -> bool {
        matches!(self, EvalError::MissingFact(_))
    }
}

/// A monetary expression tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Expr {
    /// A required fact. Absent means the expression cannot be evaluated.
    Fact {
        fact: FactKind,
    },
    /// A fact that defaults to zero when absent. Only correct where absence
    /// genuinely means "none booked" — e.g. a reserve that was never made.
    FactOrZero {
        fact: FactKind,
    },
    /// A literal amount in whole kronor.
    Amount {
        sek: i64,
    },
    /// A named per-year amount, e.g. `prisbasbelopp`.
    Constant {
        name: String,
    },

    Add {
        a: Box<Expr>,
        b: Box<Expr>,
    },
    Sub {
        a: Box<Expr>,
        b: Box<Expr>,
    },
    /// Multiply by a literal rate in basis points.
    MulBp {
        of: Box<Expr>,
        bp: i64,
    },
    /// Multiply by a named per-year rate, e.g. `corporate_tax`.
    MulRate {
        of: Box<Expr>,
        rate: String,
    },
    /// Clamp negatives to zero. Used constantly: an "unused headroom" that
    /// comes out negative means there is no headroom, not a negative one.
    Max0 {
        of: Box<Expr>,
    },
    Min {
        a: Box<Expr>,
        b: Box<Expr>,
    },
    Max {
        a: Box<Expr>,
        b: Box<Expr>,
    },
    Abs {
        of: Box<Expr>,
    },
}

impl Expr {
    pub fn eval(&self, facts: &FactSet, constants: &TaxYearConstants) -> Result<Money, EvalError> {
        match self {
            Expr::Fact { fact } => facts
                .value(fact)
                .ok_or_else(|| EvalError::MissingFact(fact.key())),
            Expr::FactOrZero { fact } => Ok(facts.value(fact).unwrap_or(Money::ZERO)),
            Expr::Amount { sek } => {
                Money::from_sek(*sek).map_err(|_| EvalError::Overflow("amount".into()))
            }
            Expr::Constant { name } => constants
                .amount(name)
                .ok_or_else(|| EvalError::UnknownConstant(name.clone())),
            Expr::Add { a, b } => {
                let (x, y) = (a.eval(facts, constants)?, b.eval(facts, constants)?);
                x.checked_add(y)
                    .map_err(|_| EvalError::Overflow("add".into()))
            }
            Expr::Sub { a, b } => {
                let (x, y) = (a.eval(facts, constants)?, b.eval(facts, constants)?);
                x.checked_sub(y)
                    .map_err(|_| EvalError::Overflow("sub".into()))
            }
            Expr::MulBp { of, bp } => of
                .eval(facts, constants)?
                .mul_basis_points(*bp)
                .map_err(|_| EvalError::Overflow("mul_bp".into())),
            Expr::MulRate { of, rate } => {
                let bp = constants
                    .rate_bp(rate)
                    .ok_or_else(|| EvalError::UnknownRate(rate.clone()))?;
                of.eval(facts, constants)?
                    .mul_basis_points(bp)
                    .map_err(|_| EvalError::Overflow("mul_rate".into()))
            }
            Expr::Max0 { of } => Ok(of.eval(facts, constants)?.max(Money::ZERO)),
            Expr::Min { a, b } => Ok(a.eval(facts, constants)?.min(b.eval(facts, constants)?)),
            Expr::Max { a, b } => Ok(a.eval(facts, constants)?.max(b.eval(facts, constants)?)),
            Expr::Abs { of } => of
                .eval(facts, constants)?
                .abs()
                .map_err(|_| EvalError::Overflow("abs".into())),
        }
    }

    /// Every fact the expression reads. Drives the "what is missing" reporting
    /// without having to evaluate and catch failures one at a time.
    pub fn referenced_facts(&self) -> Vec<FactKind> {
        let mut out = Vec::new();
        self.collect_facts(&mut out);
        out.dedup_by_key(|f| f.key());
        out
    }

    fn collect_facts(&self, out: &mut Vec<FactKind>) {
        match self {
            Expr::Fact { fact } | Expr::FactOrZero { fact } => out.push(fact.clone()),
            Expr::Amount { .. } | Expr::Constant { .. } => {}
            Expr::Add { a, b } | Expr::Sub { a, b } | Expr::Min { a, b } | Expr::Max { a, b } => {
                a.collect_facts(out);
                b.collect_facts(out);
            }
            Expr::MulBp { of, .. }
            | Expr::MulRate { of, .. }
            | Expr::Max0 { of }
            | Expr::Abs { of } => of.collect_facts(out),
        }
    }

    /// Facts that are strictly required — `FactOrZero` does not count, because
    /// its absence is a defined value rather than a gap.
    pub fn required_facts(&self) -> Vec<FactKind> {
        let mut out = Vec::new();
        self.collect_required(&mut out);
        out.dedup_by_key(|f| f.key());
        out
    }

    fn collect_required(&self, out: &mut Vec<FactKind>) {
        match self {
            Expr::Fact { fact } => out.push(fact.clone()),
            Expr::FactOrZero { .. } | Expr::Amount { .. } | Expr::Constant { .. } => {}
            Expr::Add { a, b } | Expr::Sub { a, b } | Expr::Min { a, b } | Expr::Max { a, b } => {
                a.collect_required(out);
                b.collect_required(out);
            }
            Expr::MulBp { of, .. }
            | Expr::MulRate { of, .. }
            | Expr::Max0 { of }
            | Expr::Abs { of } => of.collect_required(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skattjakt_core::{
        CompanyId, DocumentVersionId, FinancialFact, FinancialFactId, FiscalYear, UnitInterval,
    };

    fn constants() -> TaxYearConstants {
        TaxYearConstants {
            tax_year: 2025,
            ..Default::default()
        }
        .with_amount("prisbasbelopp", 5_880_000, "sfb-2-6") // 58 800 kr
        .with_rate("corporate_tax", 2060, "il-65-10") // 20.6 %
    }

    fn facts(pairs: &[(FactKind, i64)]) -> FactSet {
        FactSet::from_facts(pairs.iter().map(|(kind, sek)| FinancialFact {
            id: FinancialFactId::new(),
            company_id: CompanyId::new(),
            document_version_id: DocumentVersionId::new(),
            period: FiscalYear::calendar(2025).unwrap(),
            kind: kind.clone(),
            value: Money::from_sek(*sek).unwrap(),
            currency: "SEK".into(),
            account: None,
            source_page: Some(1),
            source_text: Some("x".into()),
            extraction_confidence: UnitInterval::ONE,
        }))
    }

    fn fact(kind: FactKind) -> Box<Expr> {
        Box::new(Expr::Fact { fact: kind })
    }

    #[test]
    fn evaluates_a_headroom_calculation() {
        // 25 % of the taxable result, less what is already reserved.
        let expr = Expr::Max0 {
            of: Box::new(Expr::Sub {
                a: Box::new(Expr::MulBp {
                    of: fact(FactKind::TaxableResult),
                    bp: 2500,
                }),
                b: Box::new(Expr::FactOrZero {
                    fact: FactKind::TaxAllocationReserveThisYear,
                }),
            }),
        };
        let f = facts(&[(FactKind::TaxableResult, 1_000_000)]);
        assert_eq!(
            expr.eval(&f, &constants()).unwrap(),
            Money::from_sek(250_000).unwrap()
        );
    }

    #[test]
    fn max0_clamps_a_negative_headroom_to_zero() {
        let expr = Expr::Max0 {
            of: Box::new(Expr::Sub {
                a: Box::new(Expr::MulBp {
                    of: fact(FactKind::TaxableResult),
                    bp: 2500,
                }),
                b: Box::new(Expr::FactOrZero {
                    fact: FactKind::TaxAllocationReserveThisYear,
                }),
            }),
        };
        let f = facts(&[
            (FactKind::TaxableResult, 100_000),
            (FactKind::TaxAllocationReserveThisYear, 90_000),
        ]);
        assert_eq!(expr.eval(&f, &constants()).unwrap(), Money::ZERO);
    }

    #[test]
    fn a_missing_required_fact_is_reported_as_missing_information() {
        let expr = Expr::Fact {
            fact: FactKind::TaxableResult,
        };
        let err = expr.eval(&facts(&[]), &constants()).unwrap_err();
        assert!(err.is_missing_information());
        assert_eq!(err, EvalError::MissingFact("taxable_result".into()));
    }

    #[test]
    fn fact_or_zero_defaults_instead_of_failing() {
        let expr = Expr::FactOrZero {
            fact: FactKind::TaxAllocationReserveThisYear,
        };
        assert_eq!(expr.eval(&facts(&[]), &constants()).unwrap(), Money::ZERO);
    }

    #[test]
    fn named_rates_and_amounts_resolve_per_year() {
        let tax = Expr::MulRate {
            of: fact(FactKind::TaxableResult),
            rate: "corporate_tax".into(),
        };
        let f = facts(&[(FactKind::TaxableResult, 100_000)]);
        assert_eq!(
            tax.eval(&f, &constants()).unwrap(),
            Money::from_sek(20_600).unwrap()
        );

        let pbb = Expr::Constant {
            name: "prisbasbelopp".into(),
        };
        assert_eq!(
            pbb.eval(&f, &constants()).unwrap(),
            Money::from_sek(58_800).unwrap()
        );
    }

    #[test]
    fn an_unknown_rate_is_an_engine_fault_not_missing_information() {
        let expr = Expr::MulRate {
            of: fact(FactKind::TaxableResult),
            rate: "nonexistent".into(),
        };
        let err = expr
            .eval(&facts(&[(FactKind::TaxableResult, 1)]), &constants())
            .unwrap_err();
        assert!(!err.is_missing_information());
        assert_eq!(err, EvalError::UnknownRate("nonexistent".into()));
    }

    #[test]
    fn required_facts_exclude_optional_ones() {
        let expr = Expr::Sub {
            a: fact(FactKind::TaxableResult),
            b: Box::new(Expr::FactOrZero {
                fact: FactKind::TaxAllocationReserveThisYear,
            }),
        };
        assert_eq!(expr.required_facts(), vec![FactKind::TaxableResult]);
        assert_eq!(expr.referenced_facts().len(), 2);
    }

    #[test]
    fn min_and_max_pick_the_right_bound() {
        let f = facts(&[(FactKind::TaxableResult, 100), (FactKind::Cash, 400)]);
        let min = Expr::Min {
            a: fact(FactKind::TaxableResult),
            b: fact(FactKind::Cash),
        };
        let max = Expr::Max {
            a: fact(FactKind::TaxableResult),
            b: fact(FactKind::Cash),
        };
        assert_eq!(
            min.eval(&f, &constants()).unwrap(),
            Money::from_sek(100).unwrap()
        );
        assert_eq!(
            max.eval(&f, &constants()).unwrap(),
            Money::from_sek(400).unwrap()
        );
    }

    #[test]
    fn expressions_round_trip_through_json() {
        let expr = Expr::Max0 {
            of: Box::new(Expr::MulRate {
                of: fact(FactKind::TaxableResult),
                rate: "corporate_tax".into(),
            }),
        };
        let json = serde_json::to_string(&expr).unwrap();
        let back: Expr = serde_json::from_str(&json).unwrap();
        assert_eq!(expr, back);
    }
}

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
    /// Amounts in öre, keyed by name.
    #[serde(default)]
    pub amounts: BTreeMap<String, i64>,
    /// Rates in basis points (1 bp = 0.01 %), keyed by name.
    #[serde(default)]
    pub rates_bp: BTreeMap<String, i64>,
    /// Free-text note on where the figures come from and what still needs
    /// checking. Surfaced rather than hidden.
    #[serde(default)]
    pub source: String,
}

impl TaxYearConstants {
    pub fn amount(&self, name: &str) -> Option<Money> {
        self.amounts.get(name).copied().map(Money::from_ore)
    }

    pub fn rate_bp(&self, name: &str) -> Option<i64> {
        self.rates_bp.get(name).copied()
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
    Fact { fact: FactKind },
    /// A fact that defaults to zero when absent. Only correct where absence
    /// genuinely means "none booked" — e.g. a reserve that was never made.
    FactOrZero { fact: FactKind },
    /// A literal amount in whole kronor.
    Amount { sek: i64 },
    /// A named per-year amount, e.g. `prisbasbelopp`.
    Constant { name: String },

    Add { a: Box<Expr>, b: Box<Expr> },
    Sub { a: Box<Expr>, b: Box<Expr> },
    /// Multiply by a literal rate in basis points.
    MulBp { of: Box<Expr>, bp: i64 },
    /// Multiply by a named per-year rate, e.g. `corporate_tax`.
    MulRate { of: Box<Expr>, rate: String },
    /// Clamp negatives to zero. Used constantly: an "unused headroom" that
    /// comes out negative means there is no headroom, not a negative one.
    Max0 { of: Box<Expr> },
    Min { a: Box<Expr>, b: Box<Expr> },
    Max { a: Box<Expr>, b: Box<Expr> },
    Abs { of: Box<Expr> },
}

impl Expr {
    pub fn eval(&self, facts: &FactSet, constants: &TaxYearConstants) -> Result<Money, EvalError> {
        match self {
            Expr::Fact { fact } => facts
                .value(fact)
                .ok_or_else(|| EvalError::MissingFact(fact.key())),
            Expr::FactOrZero { fact } => Ok(facts.value(fact).unwrap_or(Money::ZERO)),
            Expr::Amount { sek } => Money::from_sek(*sek).map_err(|_| EvalError::Overflow("amount".into())),
            Expr::Constant { name } => constants
                .amount(name)
                .ok_or_else(|| EvalError::UnknownConstant(name.clone())),
            Expr::Add { a, b } => {
                let (x, y) = (a.eval(facts, constants)?, b.eval(facts, constants)?);
                x.checked_add(y).map_err(|_| EvalError::Overflow("add".into()))
            }
            Expr::Sub { a, b } => {
                let (x, y) = (a.eval(facts, constants)?, b.eval(facts, constants)?);
                x.checked_sub(y).map_err(|_| EvalError::Overflow("sub".into()))
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
            Expr::MulBp { of, .. } | Expr::MulRate { of, .. } | Expr::Max0 { of } | Expr::Abs { of } => {
                of.collect_facts(out)
            }
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
            Expr::MulBp { of, .. } | Expr::MulRate { of, .. } | Expr::Max0 { of } | Expr::Abs { of } => {
                of.collect_required(out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skattjakt_core::{CompanyId, DocumentVersionId, FinancialFact, FinancialFactId, FiscalYear, UnitInterval};

    fn constants() -> TaxYearConstants {
        let mut c = TaxYearConstants { tax_year: 2025, ..Default::default() };
        c.amounts.insert("prisbasbelopp".into(), 5_880_000); // 58 800 kr
        c.rates_bp.insert("corporate_tax".into(), 2060); // 20.6 %
        c
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
                a: Box::new(Expr::MulBp { of: fact(FactKind::TaxableResult), bp: 2500 }),
                b: Box::new(Expr::FactOrZero { fact: FactKind::TaxAllocationReserveThisYear }),
            }),
        };
        let f = facts(&[(FactKind::TaxableResult, 1_000_000)]);
        assert_eq!(expr.eval(&f, &constants()).unwrap(), Money::from_sek(250_000).unwrap());
    }

    #[test]
    fn max0_clamps_a_negative_headroom_to_zero() {
        let expr = Expr::Max0 {
            of: Box::new(Expr::Sub {
                a: Box::new(Expr::MulBp { of: fact(FactKind::TaxableResult), bp: 2500 }),
                b: Box::new(Expr::FactOrZero { fact: FactKind::TaxAllocationReserveThisYear }),
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
        let expr = Expr::Fact { fact: FactKind::TaxableResult };
        let err = expr.eval(&facts(&[]), &constants()).unwrap_err();
        assert!(err.is_missing_information());
        assert_eq!(err, EvalError::MissingFact("taxable_result".into()));
    }

    #[test]
    fn fact_or_zero_defaults_instead_of_failing() {
        let expr = Expr::FactOrZero { fact: FactKind::TaxAllocationReserveThisYear };
        assert_eq!(expr.eval(&facts(&[]), &constants()).unwrap(), Money::ZERO);
    }

    #[test]
    fn named_rates_and_amounts_resolve_per_year() {
        let tax = Expr::MulRate { of: fact(FactKind::TaxableResult), rate: "corporate_tax".into() };
        let f = facts(&[(FactKind::TaxableResult, 100_000)]);
        assert_eq!(tax.eval(&f, &constants()).unwrap(), Money::from_sek(20_600).unwrap());

        let pbb = Expr::Constant { name: "prisbasbelopp".into() };
        assert_eq!(pbb.eval(&f, &constants()).unwrap(), Money::from_sek(58_800).unwrap());
    }

    #[test]
    fn an_unknown_rate_is_an_engine_fault_not_missing_information() {
        let expr = Expr::MulRate { of: fact(FactKind::TaxableResult), rate: "nonexistent".into() };
        let err = expr.eval(&facts(&[(FactKind::TaxableResult, 1)]), &constants()).unwrap_err();
        assert!(!err.is_missing_information());
        assert_eq!(err, EvalError::UnknownRate("nonexistent".into()));
    }

    #[test]
    fn required_facts_exclude_optional_ones() {
        let expr = Expr::Sub {
            a: fact(FactKind::TaxableResult),
            b: Box::new(Expr::FactOrZero { fact: FactKind::TaxAllocationReserveThisYear }),
        };
        assert_eq!(expr.required_facts(), vec![FactKind::TaxableResult]);
        assert_eq!(expr.referenced_facts().len(), 2);
    }

    #[test]
    fn min_and_max_pick_the_right_bound() {
        let f = facts(&[(FactKind::TaxableResult, 100), (FactKind::Cash, 400)]);
        let min = Expr::Min { a: fact(FactKind::TaxableResult), b: fact(FactKind::Cash) };
        let max = Expr::Max { a: fact(FactKind::TaxableResult), b: fact(FactKind::Cash) };
        assert_eq!(min.eval(&f, &constants()).unwrap(), Money::from_sek(100).unwrap());
        assert_eq!(max.eval(&f, &constants()).unwrap(), Money::from_sek(400).unwrap());
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

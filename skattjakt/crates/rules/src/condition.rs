//! Rule conditions, evaluated in three-valued logic.
//!
//! Two-valued logic is the wrong tool here. "The company is not in a group" and
//! "nobody has said whether the company is in a group" must lead to different
//! outcomes, and collapsing the second into the first is exactly how a
//! confident, wrong finding gets produced. So a condition evaluates to true,
//! false, or unknown, and unknown propagates (Kleene semantics) until the
//! engine turns it into "needs more information".

use serde::{Deserialize, Serialize};
use skattjakt_core::document::AccountsState;
use skattjakt_core::{CompanyProfile, FactSet};

use crate::expr::{EvalError, Expr, TaxYearConstants};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Truth {
    True,
    False,
    /// Not enough information to decide.
    Unknown,
}

impl Truth {
    pub fn from_bool(value: bool) -> Self {
        if value {
            Truth::True
        } else {
            Truth::False
        }
    }

    pub fn and(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::False, _) | (_, Truth::False) => Truth::False,
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            _ => Truth::True,
        }
    }

    pub fn or(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::True, _) | (_, Truth::True) => Truth::True,
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            _ => Truth::False,
        }
    }

    /// Named `negate` rather than `not` so it cannot be confused with
    /// `std::ops::Not`, which has different semantics for a three-valued type.
    pub fn negate(self) -> Truth {
        match self {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        }
    }

    pub fn is_true(self) -> bool {
        self == Truth::True
    }

    pub fn is_unknown(self) -> bool {
        self == Truth::Unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Gt,
    Gte,
    Lt,
    Lte,
    Eq,
    Neq,
}

impl CmpOp {
    fn apply(self, a: i64, b: i64) -> bool {
        match self {
            CmpOp::Gt => a > b,
            CmpOp::Gte => a >= b,
            CmpOp::Lt => a < b,
            CmpOp::Lte => a <= b,
            CmpOp::Eq => a == b,
            CmpOp::Neq => a != b,
        }
    }
}

/// Yes/no questions from onboarding. `None` on the profile means unanswered,
/// which yields `Unknown` rather than `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileFlag {
    InGroup,
    OwnershipChanged,
    OperationsOutsideSweden,
    DoesDevelopmentWork,
    OwnsPremises,
    HasVehicles,
    OwnersActiveInCompany,
}

impl ProfileFlag {
    fn read(self, profile: &CompanyProfile) -> Option<bool> {
        match self {
            ProfileFlag::InGroup => profile.in_group,
            ProfileFlag::OwnershipChanged => profile.ownership_changed,
            ProfileFlag::OperationsOutsideSweden => profile.operations_outside_sweden,
            ProfileFlag::DoesDevelopmentWork => profile.does_development_work,
            ProfileFlag::OwnsPremises => profile.owns_premises,
            ProfileFlag::HasVehicles => profile.has_vehicles,
            ProfileFlag::OwnersActiveInCompany => profile.owners_active_in_company,
        }
    }

    /// The `CompanyProfile` field this flag reads, as the profile names it.
    pub fn field_name(self) -> &'static str {
        match self {
            ProfileFlag::InGroup => "in_group",
            ProfileFlag::OwnershipChanged => "ownership_changed",
            ProfileFlag::OperationsOutsideSweden => "operations_outside_sweden",
            ProfileFlag::DoesDevelopmentWork => "does_development_work",
            ProfileFlag::OwnsPremises => "owns_premises",
            ProfileFlag::HasVehicles => "has_vehicles",
            ProfileFlag::OwnersActiveInCompany => "owners_active_in_company",
        }
    }

    /// The onboarding question to ask when this is what is blocking a finding.
    pub fn question_sv(self) -> &'static str {
        match self {
            ProfileFlag::InGroup => "Ingår bolaget i en koncern?",
            ProfileFlag::OwnershipChanged => {
                "Har det bestämmande inflytandet över bolaget bytt ägare de senaste fem åren?"
            }
            ProfileFlag::OperationsOutsideSweden => "Har bolaget verksamhet utanför Sverige?",
            ProfileFlag::DoesDevelopmentWork => "Bedriver bolaget utvecklingsarbete?",
            ProfileFlag::OwnsPremises => "Äger bolaget lokaler eller fastighet?",
            ProfileFlag::HasVehicles => "Har bolaget fordon?",
            ProfileFlag::OwnersActiveInCompany => {
                "Är ägarna verksamma i betydande omfattning i bolaget?"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileNumber {
    EmployeeCount,
    OwnerCount,
}

impl ProfileNumber {
    /// The `CompanyProfile` field this reads, as the profile names it.
    pub fn field_name(self) -> &'static str {
        match self {
            ProfileNumber::EmployeeCount => "employee_count",
            ProfileNumber::OwnerCount => "owner_count",
        }
    }

    fn read(self, profile: &CompanyProfile) -> Option<i64> {
        match self {
            ProfileNumber::EmployeeCount => profile.employee_count.map(i64::from),
            ProfileNumber::OwnerCount => profile.owner_count.map(i64::from),
        }
    }
}

/// Everything a condition is evaluated against.
#[derive(Debug, Clone, Copy)]
pub struct EvalContext<'a> {
    pub facts: &'a FactSet,
    pub profile: &'a CompanyProfile,
    pub constants: &'a TaxYearConstants,
    pub tax_year: i32,
    pub accounts_state: AccountsState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Condition {
    /// The quantity was extracted from a document at all.
    FactPresent {
        fact: skattjakt_core::FactKind,
    },
    /// The quantity was not found. Genuinely knowable: absence of a value in an
    /// otherwise well-parsed document is information.
    FactAbsent {
        fact: skattjakt_core::FactKind,
    },
    /// Compare two monetary expressions.
    Compare {
        left: Expr,
        op: CmpOp,
        right: Expr,
    },
    /// An onboarding yes/no answer.
    Profile {
        flag: ProfileFlag,
        is: bool,
    },
    /// A numeric profile answer.
    ProfileNumber {
        field: ProfileNumber,
        op: CmpOp,
        value: i64,
    },
    AccountsStateIs {
        state: AccountsState,
    },
    TaxYearAtLeast {
        year: i32,
    },
    TaxYearAtMost {
        year: i32,
    },

    All {
        of: Vec<Condition>,
    },
    Any {
        of: Vec<Condition>,
    },
    Not {
        of: Box<Condition>,
    },
    /// Always true. Used by rules that apply unconditionally within their year.
    Always,
}

impl Condition {
    pub fn eval(&self, ctx: &EvalContext<'_>) -> Truth {
        match self {
            Condition::FactPresent { fact } => Truth::from_bool(ctx.facts.get(fact).is_some()),
            Condition::FactAbsent { fact } => Truth::from_bool(ctx.facts.get(fact).is_none()),

            Condition::Compare { left, op, right } => {
                let a = left.eval(ctx.facts, ctx.constants);
                let b = right.eval(ctx.facts, ctx.constants);
                match (a, b) {
                    (Ok(x), Ok(y)) => Truth::from_bool(op.apply(x.ore(), y.ore())),
                    // A missing fact makes the comparison undecidable. An
                    // engine fault (bad rate name) does too — and is surfaced
                    // separately by `validate`, so it cannot pass silently.
                    (Err(_), _) | (_, Err(_)) => Truth::Unknown,
                }
            }

            Condition::Profile { flag, is } => match flag.read(ctx.profile) {
                Some(value) => Truth::from_bool(value == *is),
                None => Truth::Unknown,
            },

            Condition::ProfileNumber { field, op, value } => match field.read(ctx.profile) {
                Some(actual) => Truth::from_bool(op.apply(actual, *value)),
                None => Truth::Unknown,
            },

            Condition::AccountsStateIs { state } => match ctx.accounts_state {
                AccountsState::Unknown => Truth::Unknown,
                actual => Truth::from_bool(actual == *state),
            },

            Condition::TaxYearAtLeast { year } => Truth::from_bool(ctx.tax_year >= *year),
            Condition::TaxYearAtMost { year } => Truth::from_bool(ctx.tax_year <= *year),

            Condition::All { of } => of.iter().fold(Truth::True, |acc, c| acc.and(c.eval(ctx))),
            Condition::Any { of } => of.iter().fold(Truth::False, |acc, c| acc.or(c.eval(ctx))),
            Condition::Not { of } => of.eval(ctx).negate(),
            Condition::Always => Truth::True,
        }
    }

    /// Profile questions this condition depends on that have not been answered.
    /// Turned into follow-up onboarding questions and "missing information".
    pub fn unanswered_profile_questions(&self, ctx: &EvalContext<'_>) -> Vec<ProfileFlag> {
        let mut out = Vec::new();
        self.collect_unanswered(ctx, &mut out);
        out.dedup();
        out
    }

    fn collect_unanswered(&self, ctx: &EvalContext<'_>, out: &mut Vec<ProfileFlag>) {
        match self {
            Condition::Profile { flag, .. } if flag.read(ctx.profile).is_none() => out.push(*flag),
            Condition::All { of } | Condition::Any { of } => {
                for c in of {
                    c.collect_unanswered(ctx, out);
                }
            }
            Condition::Not { of } => of.collect_unanswered(ctx, out),
            _ => {}
        }
    }

    /// Facts referenced anywhere in the condition, required or not.
    pub fn referenced_facts(&self) -> Vec<skattjakt_core::FactKind> {
        let mut out = Vec::new();
        self.collect_facts(&mut out);
        out.dedup_by_key(|f| f.key());
        out
    }

    fn collect_facts(&self, out: &mut Vec<skattjakt_core::FactKind>) {
        match self {
            Condition::FactPresent { fact } | Condition::FactAbsent { fact } => {
                out.push(fact.clone())
            }
            Condition::Compare { left, right, .. } => {
                out.extend(left.referenced_facts());
                out.extend(right.referenced_facts());
            }
            Condition::All { of } | Condition::Any { of } => {
                for c in of {
                    c.collect_facts(out);
                }
            }
            Condition::Not { of } => of.collect_facts(out),
            _ => {}
        }
    }

    /// Structural check that every named constant and rate exists for the year.
    /// Run at load time so a typo in a rule file is caught before an analysis
    /// silently treats the comparison as undecidable.
    pub fn validate(&self, constants: &TaxYearConstants) -> Result<(), EvalError> {
        match self {
            Condition::Compare { left, right, .. } => {
                validate_expr(left, constants)?;
                validate_expr(right, constants)
            }
            Condition::All { of } | Condition::Any { of } => {
                for c in of {
                    c.validate(constants)?;
                }
                Ok(())
            }
            Condition::Not { of } => of.validate(constants),
            _ => Ok(()),
        }
    }
}

/// Walks an expression and checks that named constants and rates resolve.
pub fn validate_expr(expr: &Expr, constants: &TaxYearConstants) -> Result<(), EvalError> {
    match expr {
        Expr::Constant { name } => constants
            .amount(name)
            .map(|_| ())
            .ok_or_else(|| EvalError::UnknownConstant(name.clone())),
        Expr::MulRate { of, rate } => {
            constants
                .rate_bp(rate)
                .ok_or_else(|| EvalError::UnknownRate(rate.clone()))?;
            validate_expr(of, constants)
        }
        Expr::Add { a, b } | Expr::Sub { a, b } | Expr::Min { a, b } | Expr::Max { a, b } => {
            validate_expr(a, constants)?;
            validate_expr(b, constants)
        }
        Expr::MulBp { of, .. } | Expr::Max0 { of } | Expr::Abs { of } => {
            validate_expr(of, constants)
        }
        Expr::Fact { .. } | Expr::FactOrZero { .. } | Expr::Amount { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skattjakt_core::{
        CompanyId, DocumentVersionId, FactKind, FinancialFact, FinancialFactId, FiscalYear, Money,
        OrgNumber, UnitInterval,
    };

    fn profile() -> CompanyProfile {
        CompanyProfile {
            id: CompanyId::new(),
            name: "Testbolaget AB".into(),
            org_number: OrgNumber::parse("556016-0680").unwrap(),
            fiscal_year: FiscalYear::calendar(2025).unwrap(),
            industry: None,
            sni_code: None,
            employee_count: None,
            owner_count: None,
            in_group: None,
            ownership_changed: None,
            operations_outside_sweden: None,
            does_development_work: None,
            owns_premises: None,
            has_vehicles: None,
            owners_active_in_company: None,
        }
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

    fn constants() -> TaxYearConstants {
        TaxYearConstants {
            tax_year: 2025,
            ..Default::default()
        }
        .with_rate("corporate_tax", 2060, "il-65-10")
    }

    #[test]
    fn kleene_and_or_not_behave_as_specified() {
        use Truth::*;
        assert_eq!(True.and(Unknown), Unknown);
        assert_eq!(False.and(Unknown), False, "false dominates conjunction");
        assert_eq!(True.or(Unknown), True, "true dominates disjunction");
        assert_eq!(False.or(Unknown), Unknown);
        assert_eq!(Unknown.negate(), Unknown);
        assert_eq!(True.negate(), False);
    }

    #[test]
    fn an_unanswered_profile_question_is_unknown_not_false() {
        let (f, p, c) = (facts(&[]), profile(), constants());
        let ctx = EvalContext {
            facts: &f,
            profile: &p,
            constants: &c,
            tax_year: 2025,
            accounts_state: AccountsState::Preliminary,
        };
        let cond = Condition::Profile {
            flag: ProfileFlag::InGroup,
            is: false,
        };
        assert_eq!(cond.eval(&ctx), Truth::Unknown);

        let answered = CompanyProfile {
            in_group: Some(false),
            ownership_changed: None,
            ..p.clone()
        };
        let ctx2 = EvalContext {
            profile: &answered,
            ..ctx
        };
        assert_eq!(cond.eval(&ctx2), Truth::True);
    }

    #[test]
    fn a_comparison_over_a_missing_fact_is_unknown() {
        let (f, p, c) = (facts(&[]), profile(), constants());
        let ctx = EvalContext {
            facts: &f,
            profile: &p,
            constants: &c,
            tax_year: 2025,
            accounts_state: AccountsState::Preliminary,
        };
        let cond = Condition::Compare {
            left: Expr::Fact {
                fact: FactKind::TaxableResult,
            },
            op: CmpOp::Gt,
            right: Expr::Amount { sek: 0 },
        };
        assert_eq!(cond.eval(&ctx), Truth::Unknown);
    }

    #[test]
    fn a_comparison_over_present_facts_decides() {
        let f = facts(&[(FactKind::TaxableResult, 500_000)]);
        let (p, c) = (profile(), constants());
        let ctx = EvalContext {
            facts: &f,
            profile: &p,
            constants: &c,
            tax_year: 2025,
            accounts_state: AccountsState::Preliminary,
        };
        let cond = Condition::Compare {
            left: Expr::Fact {
                fact: FactKind::TaxableResult,
            },
            op: CmpOp::Gt,
            right: Expr::Amount { sek: 0 },
        };
        assert_eq!(cond.eval(&ctx), Truth::True);
    }

    #[test]
    fn one_unknown_leaf_makes_a_conjunction_unknown() {
        let f = facts(&[(FactKind::TaxableResult, 500_000)]);
        let (p, c) = (profile(), constants());
        let ctx = EvalContext {
            facts: &f,
            profile: &p,
            constants: &c,
            tax_year: 2025,
            accounts_state: AccountsState::Preliminary,
        };
        let cond = Condition::All {
            of: vec![
                Condition::Compare {
                    left: Expr::Fact {
                        fact: FactKind::TaxableResult,
                    },
                    op: CmpOp::Gt,
                    right: Expr::Amount { sek: 0 },
                },
                Condition::Profile {
                    flag: ProfileFlag::InGroup,
                    is: false,
                },
            ],
        };
        assert_eq!(cond.eval(&ctx), Truth::Unknown);
    }

    #[test]
    fn a_false_leaf_still_makes_a_conjunction_false_despite_unknowns() {
        let f = facts(&[(FactKind::TaxableResult, -1)]);
        let (p, c) = (profile(), constants());
        let ctx = EvalContext {
            facts: &f,
            profile: &p,
            constants: &c,
            tax_year: 2025,
            accounts_state: AccountsState::Preliminary,
        };
        let cond = Condition::All {
            of: vec![
                Condition::Compare {
                    left: Expr::Fact {
                        fact: FactKind::TaxableResult,
                    },
                    op: CmpOp::Gt,
                    right: Expr::Amount { sek: 0 },
                },
                Condition::Profile {
                    flag: ProfileFlag::InGroup,
                    is: false,
                },
            ],
        };
        assert_eq!(cond.eval(&ctx), Truth::False);
    }

    #[test]
    fn unknown_accounts_state_does_not_decide_the_condition() {
        let (f, p, c) = (facts(&[]), profile(), constants());
        let ctx = EvalContext {
            facts: &f,
            profile: &p,
            constants: &c,
            tax_year: 2025,
            accounts_state: AccountsState::Unknown,
        };
        let cond = Condition::AccountsStateIs {
            state: AccountsState::Final,
        };
        assert_eq!(cond.eval(&ctx), Truth::Unknown);
    }

    #[test]
    fn unanswered_questions_are_collected_for_follow_up() {
        let (f, p, c) = (facts(&[]), profile(), constants());
        let ctx = EvalContext {
            facts: &f,
            profile: &p,
            constants: &c,
            tax_year: 2025,
            accounts_state: AccountsState::Preliminary,
        };
        let cond = Condition::All {
            of: vec![
                Condition::Profile {
                    flag: ProfileFlag::InGroup,
                    is: false,
                },
                Condition::Not {
                    of: Box::new(Condition::Profile {
                        flag: ProfileFlag::DoesDevelopmentWork,
                        is: false,
                    }),
                },
            ],
        };
        let questions = cond.unanswered_profile_questions(&ctx);
        assert_eq!(questions.len(), 2);
        assert!(questions.contains(&ProfileFlag::InGroup));
        assert!(!ProfileFlag::InGroup.question_sv().is_empty());
    }

    #[test]
    fn validation_catches_a_misspelled_rate_at_load_time() {
        let cond = Condition::Compare {
            left: Expr::MulRate {
                of: Box::new(Expr::Fact {
                    fact: FactKind::TaxableResult,
                }),
                rate: "corporate_taxx".into(),
            },
            op: CmpOp::Gt,
            right: Expr::Amount { sek: 0 },
        };
        assert!(cond.validate(&constants()).is_err());
    }

    #[test]
    fn conditions_round_trip_through_json() {
        let cond = Condition::Any {
            of: vec![
                Condition::Always,
                Condition::TaxYearAtLeast { year: 2021 },
                Condition::FactAbsent {
                    fact: FactKind::CarriedForwardLoss,
                },
            ],
        };
        let json = serde_json::to_string(&cond).unwrap();
        assert_eq!(serde_json::from_str::<Condition>(&json).unwrap(), cond);
    }
}

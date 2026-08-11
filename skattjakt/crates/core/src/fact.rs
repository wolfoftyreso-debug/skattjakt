//! The canonical financial fact model.
//!
//! Everything downstream — rules, calculations, model prompts — reads facts,
//! never raw document text. A fact always knows which document version and page
//! it came from, which is what makes the evidence chain in section 7 possible.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::company::FiscalYear;
use crate::confidence::UnitInterval;
use crate::ids::{CompanyId, DocumentVersionId, FinancialFactId};
use crate::money::Money;

/// The canonical vocabulary of financial quantities.
///
/// Closed variants are the ones rules may depend on. `Other` keeps the model
/// free to surface something the taxonomy does not name yet without inventing a
/// variant that a rule might accidentally match.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    // Income statement
    Revenue,
    OtherOperatingIncome,
    ExternalCosts,
    PersonnelCosts,
    Depreciation,
    OperatingProfit,
    InterestIncome,
    InterestExpense,
    ProfitBeforeTax,
    TaxExpense,
    NetProfit,

    // Balance sheet
    IntangibleAssets,
    FixedAssets,
    Inventory,
    Receivables,
    Cash,
    Equity,
    UntaxedReserves,
    Provisions,
    LongTermLiabilities,
    CurrentLiabilities,
    TotalAssets,
    TotalEquityAndLiabilities,

    // Tax-specific positions
    /// Balance of existing tax allocation reserves (periodiseringsfonder).
    TaxAllocationReserve,
    /// Reserve amount set aside in the year under analysis.
    TaxAllocationReserveThisYear,
    /// Accumulated excess depreciation (ackumulerade överavskrivningar).
    ExcessDepreciation,
    /// Carried-forward loss from earlier years (outnyttjat underskott).
    CarriedForwardLoss,
    /// Taxable result before appropriations, as reported.
    TaxableResult,
    /// Costs that are not deductible (ej avdragsgilla kostnader).
    NonDeductibleCosts,
    /// Income that is not taxable (ej skattepliktiga intäkter).
    NonTaxableIncome,
    /// Group contributions given or received (koncernbidrag).
    GroupContribution,

    // VAT and payroll
    InputVat,
    OutputVat,
    VatPayable,
    PayrollTax,
    PensionCosts,

    /// Anything outside the taxonomy. Carries a free-text label.
    Other(String),
}

impl FactKind {
    /// Whether the quantity belongs to the balance sheet, which determines
    /// whether a period-end or a period-flow reading is correct.
    pub fn is_balance_sheet(&self) -> bool {
        use FactKind::*;
        matches!(
            self,
            IntangibleAssets
                | FixedAssets
                | Inventory
                | Receivables
                | Cash
                | Equity
                | UntaxedReserves
                | Provisions
                | LongTermLiabilities
                | CurrentLiabilities
                | TotalAssets
                | TotalEquityAndLiabilities
                | TaxAllocationReserve
                | ExcessDepreciation
                | CarriedForwardLoss
        )
    }

    /// Whether the quantity is a cost or an expense.
    ///
    /// Swedish income statements present costs as negative numbers. The
    /// canonical model stores them as positive magnitudes instead, so a rule
    /// can write `personnel_costs > 0` and mean it. Without this, every rule
    /// touching a cost would need its own `abs`, and the one that forgot would
    /// fail silently rather than loudly.
    pub fn is_cost(&self) -> bool {
        use FactKind::*;
        matches!(
            self,
            ExternalCosts
                | PersonnelCosts
                | Depreciation
                | InterestExpense
                | TaxExpense
                | PensionCosts
                | PayrollTax
                | NonDeductibleCosts
        )
    }

    /// A stable key for storage and for grouping.
    pub fn key(&self) -> String {
        match self {
            FactKind::Other(label) => format!("other:{label}"),
            other => serde_json::to_value(other)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string()),
        }
    }
}

/// One extracted quantity, tied to where it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinancialFact {
    pub id: FinancialFactId,
    pub company_id: CompanyId,
    pub document_version_id: DocumentVersionId,
    pub period: FiscalYear,
    pub kind: FactKind,
    pub value: Money,
    #[serde(default = "default_currency")]
    pub currency: String,

    /// BAS account number, when the source was granular enough to have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// 1-based page in the source document version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_page: Option<u32>,
    /// The literal text the value was read from. This is what a reviewer checks
    /// the number against, so it is stored verbatim rather than normalised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_text: Option<String>,
    /// How reliable the extraction itself was (OCR quality, label ambiguity).
    pub extraction_confidence: UnitInterval,
}

fn default_currency() -> String {
    crate::money::SEK.to_string()
}

impl FinancialFact {
    /// Whether this fact is anchored well enough to support an actionable
    /// finding: it must point at a page and carry the text it was read from.
    pub fn is_traceable(&self) -> bool {
        self.source_page.is_some() && self.source_text.is_some()
    }
}

/// A period's facts, indexed for lookup.
///
/// When several facts describe the same quantity — two documents, or a value
/// restated on a later page — the one with the highest extraction confidence
/// wins, and the rest stay reachable for the evidence trail.
#[derive(Debug, Clone, Default)]
pub struct FactSet {
    facts: Vec<FinancialFact>,
    index: BTreeMap<String, usize>,
}

impl FactSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_facts(facts: impl IntoIterator<Item = FinancialFact>) -> Self {
        let mut set = Self::new();
        for fact in facts {
            set.insert(fact);
        }
        set
    }

    pub fn insert(&mut self, fact: FinancialFact) {
        let key = fact.kind.key();
        match self.index.get(&key) {
            Some(&existing) if self.facts[existing].extraction_confidence >= fact.extraction_confidence => {
                // Keep the better-supported reading as canonical, but retain
                // this one so contradictions remain visible.
                self.facts.push(fact);
            }
            _ => {
                self.facts.push(fact);
                self.index.insert(key, self.facts.len() - 1);
            }
        }
    }

    /// The canonical fact for a quantity, if extracted.
    pub fn get(&self, kind: &FactKind) -> Option<&FinancialFact> {
        self.index.get(&kind.key()).map(|&i| &self.facts[i])
    }

    pub fn value(&self, kind: &FactKind) -> Option<Money> {
        self.get(kind).map(|f| f.value)
    }

    /// Every reading of a quantity, canonical first.
    pub fn all_readings(&self, kind: &FactKind) -> Vec<&FinancialFact> {
        let key = kind.key();
        let mut readings: Vec<&FinancialFact> =
            self.facts.iter().filter(|f| f.kind.key() == key).collect();
        readings.sort_by(|a, b| b.extraction_confidence.cmp(&a.extraction_confidence));
        readings
    }

    pub fn iter(&self) -> impl Iterator<Item = &FinancialFact> {
        self.facts.iter()
    }

    pub fn len(&self) -> usize {
        self.facts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    /// Which of the given quantities are absent. Feeds both the missing-
    /// information factor of the confidence engine and the report section that
    /// tells the user what would make the analysis better.
    pub fn missing_among(&self, kinds: &[FactKind]) -> Vec<FactKind> {
        kinds.iter().filter(|k| self.get(k).is_none()).cloned().collect()
    }

    /// Two readings of the same quantity that disagree. A genuine signal in its
    /// own right — section 4 lists inconsistency between statements as
    /// something Skattjakt should surface.
    pub fn contradictions(&self) -> Vec<Contradiction> {
        let mut out = Vec::new();
        for key in self.index.keys() {
            let readings: Vec<&FinancialFact> =
                self.facts.iter().filter(|f| &f.kind.key() == key).collect();
            if readings.len() < 2 {
                continue;
            }
            let canonical = readings[0];
            for other in &readings[1..] {
                if other.value != canonical.value {
                    out.push(Contradiction {
                        kind: canonical.kind.clone(),
                        a: canonical.value,
                        b: other.value,
                        a_page: canonical.source_page,
                        b_page: other.source_page,
                    });
                }
            }
        }
        out
    }

    /// Whether the balance sheet balances, when both totals were extracted.
    /// `None` means the question could not be answered, which is different from
    /// answering "no".
    pub fn balance_sheet_balances(&self) -> Option<bool> {
        let assets = self.value(&FactKind::TotalAssets)?;
        let equity_and_liabilities = self.value(&FactKind::TotalEquityAndLiabilities)?;
        Some(assets == equity_and_liabilities)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contradiction {
    pub kind: FactKind,
    pub a: Money,
    pub b: Money,
    pub a_page: Option<u32>,
    pub b_page: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{CompanyId, DocumentVersionId};

    fn fact(kind: FactKind, sek: i64, conf: f64) -> FinancialFact {
        FinancialFact {
            id: FinancialFactId::new(),
            company_id: CompanyId::from_uuid(uuid::Uuid::nil()),
            document_version_id: DocumentVersionId::from_uuid(uuid::Uuid::nil()),
            period: FiscalYear::calendar(2025).unwrap(),
            kind,
            value: Money::from_sek(sek).unwrap(),
            currency: crate::money::SEK.to_string(),
            account: None,
            source_page: Some(1),
            source_text: Some("test".to_string()),
            extraction_confidence: UnitInterval::saturating(conf),
        }
    }

    #[test]
    fn canonical_reading_is_the_most_confident_one() {
        let mut set = FactSet::new();
        set.insert(fact(FactKind::Revenue, 1_000, 0.6));
        set.insert(fact(FactKind::Revenue, 2_000, 0.9));
        assert_eq!(set.value(&FactKind::Revenue), Some(Money::from_sek(2_000).unwrap()));
        assert_eq!(set.all_readings(&FactKind::Revenue).len(), 2);
    }

    #[test]
    fn a_weaker_later_reading_does_not_displace_a_stronger_one() {
        let mut set = FactSet::new();
        set.insert(fact(FactKind::Revenue, 2_000, 0.9));
        set.insert(fact(FactKind::Revenue, 1_000, 0.4));
        assert_eq!(set.value(&FactKind::Revenue), Some(Money::from_sek(2_000).unwrap()));
    }

    #[test]
    fn disagreeing_readings_surface_as_contradictions() {
        let mut set = FactSet::new();
        set.insert(fact(FactKind::Revenue, 1_000, 0.9));
        set.insert(fact(FactKind::Revenue, 1_500, 0.5));
        let c = set.contradictions();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].kind, FactKind::Revenue);
    }

    #[test]
    fn agreeing_readings_are_not_contradictions() {
        let mut set = FactSet::new();
        set.insert(fact(FactKind::Revenue, 1_000, 0.9));
        set.insert(fact(FactKind::Revenue, 1_000, 0.5));
        assert!(set.contradictions().is_empty());
    }

    #[test]
    fn balance_check_distinguishes_unknown_from_unbalanced() {
        let mut set = FactSet::new();
        assert_eq!(set.balance_sheet_balances(), None);

        set.insert(fact(FactKind::TotalAssets, 500, 1.0));
        assert_eq!(set.balance_sheet_balances(), None, "one side alone answers nothing");

        set.insert(fact(FactKind::TotalEquityAndLiabilities, 500, 1.0));
        assert_eq!(set.balance_sheet_balances(), Some(true));
    }

    #[test]
    fn detects_an_unbalanced_balance_sheet() {
        let mut set = FactSet::new();
        set.insert(fact(FactKind::TotalAssets, 500, 1.0));
        set.insert(fact(FactKind::TotalEquityAndLiabilities, 450, 1.0));
        assert_eq!(set.balance_sheet_balances(), Some(false));
    }

    #[test]
    fn missing_among_reports_only_absent_kinds() {
        let mut set = FactSet::new();
        set.insert(fact(FactKind::Revenue, 100, 1.0));
        let missing = set.missing_among(&[FactKind::Revenue, FactKind::Cash]);
        assert_eq!(missing, vec![FactKind::Cash]);
    }

    #[test]
    fn other_kinds_do_not_collide_with_each_other() {
        let mut set = FactSet::new();
        set.insert(fact(FactKind::Other("konstverk".into()), 10, 1.0));
        set.insert(fact(FactKind::Other("bilförmån".into()), 20, 1.0));
        assert_eq!(set.value(&FactKind::Other("konstverk".into())), Some(Money::from_sek(10).unwrap()));
        assert_eq!(set.value(&FactKind::Other("bilförmån".into())), Some(Money::from_sek(20).unwrap()));
    }

    #[test]
    fn cost_kinds_are_identified_for_sign_normalisation() {
        assert!(FactKind::PersonnelCosts.is_cost());
        assert!(FactKind::Depreciation.is_cost());
        assert!(FactKind::InterestExpense.is_cost());
        assert!(!FactKind::Revenue.is_cost());
        assert!(!FactKind::OperatingProfit.is_cost(), "a result may legitimately be negative");
        assert!(!FactKind::TaxableResult.is_cost());
    }

    #[test]
    fn fact_kind_keys_are_stable_snake_case() {
        assert_eq!(FactKind::ProfitBeforeTax.key(), "profit_before_tax");
        assert_eq!(FactKind::TaxAllocationReserve.key(), "tax_allocation_reserve");
    }

    #[test]
    fn traceability_requires_page_and_source_text() {
        let mut f = fact(FactKind::Revenue, 1, 1.0);
        assert!(f.is_traceable());
        f.source_page = None;
        assert!(!f.is_traceable());
    }
}

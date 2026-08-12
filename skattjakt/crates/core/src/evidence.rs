//! Evidence chains.
//!
//! Section 7 states the rule this module enforces: no actionable opportunity
//! without evidence. An `EvidenceChain` is the answer to "why does Skattjakt
//! say this?", and it is constructed as the analysis runs rather than
//! reconstructed afterwards.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::fact::FactKind;
use crate::ids::{CalculationId, DocumentId, DocumentVersionId, FinancialFactId, ModelRunId};
use crate::money::Money;

/// One authority a rule rests on, as it appears in a report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    /// `Inkomstskattelag (1999:1229) 30 kap. 5 §`.
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// What the rule assumes this authority says.
    pub claim: String,
    /// `unretrieved`, `unreachable`, `mismatch` or `verified`.
    ///
    /// Carried into the report on purpose. A reader told a finding rests on
    /// `30 kap. 5 §` learns nothing about whether anyone has read that
    /// paragraph; a reader told it rests on one that has never been retrieved
    /// learns exactly what the finding is worth.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieved_at: Option<String>,
}

/// One link in the chain from a stored document to a stated conclusion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvidenceItem {
    /// A value read off a page of a specific document version.
    DocumentValue {
        document_id: DocumentId,
        document_version_id: DocumentVersionId,
        fact_id: FinancialFactId,
        kind: FactKind,
        value: Money,
        page: Option<u32>,
        /// Verbatim text the value was read from.
        excerpt: Option<String>,
    },
    /// A versioned rule whose conditions were evaluated against the facts.
    Rule {
        rule_id: String,
        rule_version: String,
        title: String,
        /// Where the rule comes from, e.g. a Skatteverket page or a statute.
        source: String,
        /// Each authority the rule rests on, and how far it has been checked.
        ///
        /// A list rather than the single sentence this used to be, and carrying
        /// the state rather than only the citation. A reader who is told a
        /// finding rests on `30 kap. 5 §` learns nothing about whether anybody
        /// has read that paragraph; a reader told it rests on a paragraph that
        /// has never been retrieved learns exactly what the finding is worth.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        citations: Vec<Citation>,
    },
    /// A deterministic computation, with its inputs, so it can be re-run.
    Calculation {
        calculation_id: CalculationId,
        method: String,
        inputs: Vec<CalculationInput>,
        result_low: Money,
        result_high: Money,
    },
    /// A model pass that contributed a judgement. Never the sole support for an
    /// actionable finding.
    ModelJudgement {
        model_run_id: ModelRunId,
        task: String,
        /// A short rationale. Chain-of-thought is deliberately not stored
        /// (section 21); this is the conclusion in the model's own words.
        rationale_summary: String,
    },
    /// A stated assumption. Makes explicit what the analysis had to take for
    /// granted, so a reviewer can disagree with the premise rather than the
    /// arithmetic.
    Assumption { statement: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalculationInput {
    pub name: String,
    pub value: Money,
    /// The fact this input came from, when it came from a document at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_id: Option<FinancialFactId>,
}

impl EvidenceItem {
    pub fn is_document_anchored(&self) -> bool {
        matches!(self, EvidenceItem::DocumentValue { .. })
    }

    pub fn is_rule(&self) -> bool {
        matches!(self, EvidenceItem::Rule { .. })
    }

    pub fn is_calculation(&self) -> bool {
        matches!(self, EvidenceItem::Calculation { .. })
    }

    pub fn is_model_only(&self) -> bool {
        matches!(self, EvidenceItem::ModelJudgement { .. })
    }
}

/// The ordered set of links supporting one finding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvidenceChain {
    items: Vec<EvidenceItem>,
}

impl EvidenceChain {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, item: EvidenceItem) -> Self {
        self.items.push(item);
        self
    }

    pub fn push(&mut self, item: EvidenceItem) {
        self.items.push(item);
    }

    pub fn items(&self) -> &[EvidenceItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn has_document_anchor(&self) -> bool {
        self.items.iter().any(EvidenceItem::is_document_anchored)
    }

    pub fn has_rule(&self) -> bool {
        self.items.iter().any(EvidenceItem::is_rule)
    }

    pub fn has_calculation(&self) -> bool {
        self.items.iter().any(EvidenceItem::is_calculation)
    }

    /// True when every link is a model judgement — the shape a hallucination
    /// takes. Such a chain can never support an actionable finding.
    pub fn is_model_only(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(EvidenceItem::is_model_only)
    }

    pub fn document_versions(&self) -> Vec<DocumentVersionId> {
        let mut ids: Vec<DocumentVersionId> = self
            .items
            .iter()
            .filter_map(|i| match i {
                EvidenceItem::DocumentValue {
                    document_version_id,
                    ..
                } => Some(*document_version_id),
                _ => None,
            })
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    pub fn assumptions(&self) -> Vec<&str> {
        self.items
            .iter()
            .filter_map(|i| match i {
                EvidenceItem::Assumption { statement } => Some(statement.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The gate an opportunity must pass before it may be presented as
    /// actionable: at least one value read from a document, and at least one
    /// versioned rule that was actually evaluated.
    pub fn validate_actionable(&self) -> CoreResult<()> {
        if self.is_empty() || !self.has_document_anchor() || !self.has_rule() {
            return Err(CoreError::EvidenceRequired);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{DocumentId, DocumentVersionId, FinancialFactId, ModelRunId};

    fn doc_value() -> EvidenceItem {
        EvidenceItem::DocumentValue {
            document_id: DocumentId::new(),
            document_version_id: DocumentVersionId::new(),
            fact_id: FinancialFactId::new(),
            kind: FactKind::ProfitBeforeTax,
            value: Money::from_sek(500_000).unwrap(),
            page: Some(3),
            excerpt: Some("Resultat före skatt 500 000".to_string()),
        }
    }

    fn rule() -> EvidenceItem {
        EvidenceItem::Rule {
            citations: Vec::new(),
            rule_id: "se.periodiseringsfond.avsattning".to_string(),
            rule_version: "2025.1".to_string(),
            title: "Periodiseringsfond".to_string(),
            source: "inkomstskattelagen 30 kap.".to_string(),
        }
    }

    fn model() -> EvidenceItem {
        EvidenceItem::ModelJudgement {
            model_run_id: ModelRunId::new(),
            task: "opportunity_discovery".to_string(),
            rationale_summary: "Taxable surplus with no reserve booked.".to_string(),
        }
    }

    #[test]
    fn an_empty_chain_is_not_actionable() {
        assert_eq!(
            EvidenceChain::new().validate_actionable(),
            Err(CoreError::EvidenceRequired)
        );
    }

    #[test]
    fn a_model_only_chain_is_not_actionable() {
        let chain = EvidenceChain::new().with(model());
        assert!(chain.is_model_only());
        assert!(chain.validate_actionable().is_err());
    }

    #[test]
    fn a_document_value_without_a_rule_is_not_actionable() {
        let chain = EvidenceChain::new().with(doc_value());
        assert!(chain.validate_actionable().is_err());
    }

    #[test]
    fn a_rule_without_a_document_value_is_not_actionable() {
        let chain = EvidenceChain::new().with(rule());
        assert!(chain.validate_actionable().is_err());
    }

    #[test]
    fn document_value_plus_rule_is_actionable() {
        let chain = EvidenceChain::new()
            .with(doc_value())
            .with(rule())
            .with(model());
        assert!(chain.validate_actionable().is_ok());
        assert!(!chain.is_model_only());
        assert!(chain.has_document_anchor());
        assert!(chain.has_rule());
    }

    #[test]
    fn assumptions_are_extractable_for_display() {
        let chain =
            EvidenceChain::new()
                .with(doc_value())
                .with(rule())
                .with(EvidenceItem::Assumption {
                    statement: "Bolaget är inte i koncern.".to_string(),
                });
        assert_eq!(chain.assumptions(), vec!["Bolaget är inte i koncern."]);
    }

    #[test]
    fn document_versions_are_deduplicated() {
        let item = doc_value();
        let chain = EvidenceChain::new()
            .with(item.clone())
            .with(item)
            .with(rule());
        assert_eq!(chain.document_versions().len(), 1);
    }

    #[test]
    fn evidence_stored_before_citations_existed_still_loads() {
        // Analyses are stored as JSON. `citations` was added to a variant that
        // customers already have rows of, so every one of those rows lacks the
        // field. Without `serde(default)` this deserialises to an error and the
        // customer's history becomes unreadable — the failure would appear as
        // an old analysis that simply will not open, months after the deploy
        // that caused it.
        let stored = r#"{
            "type": "rule",
            "rule_id": "se.tax.periodiseringsfond.outnyttjat_utrymme",
            "rule_version": "2025.1",
            "title": "Periodiseringsfond",
            "source": "Inkomstskattelagen (1999:1229) 30 kap. 5–6 §§"
        }"#;
        let item: EvidenceItem = serde_json::from_str(stored).expect("old evidence must load");
        match &item {
            EvidenceItem::Rule { citations, .. } => assert!(citations.is_empty()),
            other => panic!("wrong variant: {other:?}"),
        }

        // And re-serialising must not invent an empty list where the reader
        // would otherwise correctly see "nothing was recorded".
        let round_tripped = serde_json::to_string(&item).unwrap();
        assert!(!round_tripped.contains("citations"), "{round_tripped}");
    }
}

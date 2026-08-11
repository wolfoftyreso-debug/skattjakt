//! Turning extracted text into the canonical fact model.

use skattjakt_core::{
    CompanyId, DocumentId, DocumentVersionId, FactSet, FinancialFact, FinancialFactId, FiscalYear,
    UnitInterval,
};
use skattjakt_extract::ExtractedDocument;

/// One document version taking part in an analysis.
#[derive(Debug, Clone)]
pub struct DocumentInput {
    pub document_id: DocumentId,
    pub document_version_id: DocumentVersionId,
    pub extracted: ExtractedDocument,
}

/// How much to trust a value the deterministic parser read.
///
/// A parsed line is strong evidence — it has a page and the text it came from —
/// but it is still only as good as the text layer underneath it, so the score
/// is scaled by how much of the document was readable at all.
fn extraction_confidence(document: &ExtractedDocument) -> UnitInterval {
    const PARSED_LINE_BASE: f64 = 0.95;
    UnitInterval::saturating(PARSED_LINE_BASE * document.readable_fraction())
}

/// Builds the fact set for a period from every document in the analysis.
///
/// Facts from different documents that describe the same quantity are all
/// retained; `FactSet` picks the best-supported reading as canonical and keeps
/// the disagreement visible as a contradiction.
pub fn build_fact_set(
    company_id: CompanyId,
    period: FiscalYear,
    documents: &[DocumentInput],
) -> FactSet {
    let mut set = FactSet::new();

    for document in documents {
        let confidence = extraction_confidence(&document.extracted);
        for extracted in document.extracted.facts() {
            // Costs are presented as negative in a Swedish income statement and
            // stored as positive magnitudes in the canonical model, so a rule
            // can compare a cost against a ceiling without every rule carrying
            // its own `abs`. The source text keeps the sign as printed, so a
            // reviewer still sees what the document said.
            let amount = if extracted.kind.is_cost() {
                extracted.amount_sek.abs()
            } else {
                extracted.amount_sek
            };

            let value = match skattjakt_core::Money::from_sek(amount) {
                Ok(value) => value,
                // An amount that cannot be represented is a parse failure, not
                // a fact; dropping it is safer than storing a wrapped number.
                Err(_) => continue,
            };

            set.insert(FinancialFact {
                id: FinancialFactId::new(),
                company_id,
                document_version_id: document.document_version_id,
                period,
                kind: extracted.kind,
                value,
                currency: skattjakt_core::SEK.to_string(),
                account: None,
                source_page: Some(extracted.page),
                source_text: Some(extracted.source_text),
                extraction_confidence: confidence,
            });
        }
    }

    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use skattjakt_core::{FactKind, Money};
    use skattjakt_extract::{Page, Scale};

    fn document(text: &str) -> DocumentInput {
        DocumentInput {
            document_id: DocumentId::new(),
            document_version_id: DocumentVersionId::new(),
            extracted: ExtractedDocument {
                pages: vec![Page {
                    number: 1,
                    text: text.to_string(),
                }],
                unreadable_pages: vec![],
                scale: Scale::Kronor,
            },
        }
    }

    #[test]
    fn parsed_lines_become_traceable_facts() {
        let docs = vec![document("Nettoomsättning    12 500 000")];
        let set = build_fact_set(CompanyId::new(), FiscalYear::calendar(2025).unwrap(), &docs);

        let fact = set
            .get(&FactKind::Revenue)
            .expect("revenue should be extracted");
        assert_eq!(fact.value, Money::from_sek(12_500_000).unwrap());
        assert!(
            fact.is_traceable(),
            "a fact must carry its page and source text"
        );
        assert_eq!(fact.source_page, Some(1));
    }

    #[test]
    fn a_document_with_unreadable_pages_yields_lower_confidence() {
        let mut doc = document("Nettoomsättning    1 000");
        doc.extracted.pages.push(Page {
            number: 2,
            text: String::new(),
        });
        doc.extracted.unreadable_pages.push(2);

        let set = build_fact_set(
            CompanyId::new(),
            FiscalYear::calendar(2025).unwrap(),
            &[doc],
        );
        let fact = set.get(&FactKind::Revenue).unwrap();
        assert!(
            fact.extraction_confidence.get() < 0.6,
            "half the pages were unreadable"
        );
    }

    #[test]
    fn disagreeing_documents_produce_a_visible_contradiction() {
        let docs = vec![
            document("Nettoomsättning    12 500 000"),
            document("Nettoomsättning    9 000 000"),
        ];
        let set = build_fact_set(CompanyId::new(), FiscalYear::calendar(2025).unwrap(), &docs);
        assert_eq!(set.contradictions().len(), 1);
    }

    #[test]
    fn an_empty_document_set_yields_an_empty_fact_set() {
        let set = build_fact_set(CompanyId::new(), FiscalYear::calendar(2025).unwrap(), &[]);
        assert!(set.is_empty());
    }
}

#[cfg(test)]
mod sign_tests {
    use super::*;
    use skattjakt_core::{FactKind, Money};
    use skattjakt_extract::{Page, Scale};

    fn doc(text: &str) -> DocumentInput {
        DocumentInput {
            document_id: DocumentId::new(),
            document_version_id: DocumentVersionId::new(),
            extracted: ExtractedDocument {
                pages: vec![Page {
                    number: 1,
                    text: text.to_string(),
                }],
                unreadable_pages: vec![],
                scale: Scale::Kronor,
            },
        }
    }

    #[test]
    fn costs_are_stored_as_positive_magnitudes() {
        // As printed: "Personalkostnader   -5 800 000".
        let set = build_fact_set(
            CompanyId::new(),
            FiscalYear::calendar(2025).unwrap(),
            &[doc("Personalkostnader   -5 800 000")],
        );
        let fact = set.get(&FactKind::PersonnelCosts).unwrap();
        assert_eq!(fact.value, Money::from_sek(5_800_000).unwrap());
        assert!(
            fact.source_text.as_ref().unwrap().contains("-5 800 000"),
            "the source text must still show the sign as printed"
        );
    }

    #[test]
    fn a_negative_result_keeps_its_sign() {
        // A loss is genuinely negative and must not be flipped.
        let set = build_fact_set(
            CompanyId::new(),
            FiscalYear::calendar(2025).unwrap(),
            &[doc("Rörelseresultat   -1 200 000")],
        );
        assert_eq!(
            set.value(&FactKind::OperatingProfit),
            Some(Money::from_sek(-1_200_000).unwrap())
        );
    }
}

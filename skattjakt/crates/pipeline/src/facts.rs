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

/// A line parsed out of a text layer.
const PARSED_LINE_BASE: f64 = 0.95;

/// A line read off the pixels.
///
/// Deliberately far below the text-layer figure, and the reason is measured
/// rather than assumed: on a two-column statement the recogniser dropped
/// three of four minus signs, so a cost came back looking like income. Under
/// the `not_actionable` threshold a finding resting on such a reading is
/// never presented as something to act on — which is the correct answer
/// until a person has looked at the scan.
const OCR_LINE_BASE: f64 = 0.45;

/// How much to trust a value the deterministic parser read.
///
/// A parsed line is strong evidence — it has a page and the text it came from
/// — but it is only as good as what lay underneath it, so the score is scaled
/// by how much of the document was readable at all and by how that page's
/// text was obtained.
///
/// Per page rather than per document, because a scanned PDF routinely mixes
/// the two: a covering letter with real text in front of photographed
/// statements. Marking the whole document by its worst page would understate
/// the pages that were read properly.
fn extraction_confidence_for_page(document: &ExtractedDocument, page: u32) -> UnitInterval {
    let base = if document.page_was_read_by_ocr(page) {
        OCR_LINE_BASE
    } else {
        PARSED_LINE_BASE
    };
    UnitInterval::saturating(base * document.readable_fraction())
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
                extraction_confidence: extraction_confidence_for_page(
                    &document.extracted,
                    extracted.page,
                ),
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
                ..Default::default()
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

    /// A figure read off a scan is not a figure read out of a text layer,
    /// and the confidence has to say so. Three of four minus signs were lost
    /// on a measured statement; a cost that comes back looking like income
    /// must not be presented as established.
    #[test]
    fn a_fact_read_by_ocr_is_trusted_less_than_a_parsed_one() {
        let parsed = document("Nettoomsättning    1 000 000");
        let mut scanned = document("Nettoomsättning    1 000 000");
        scanned.extracted.ocr_pages.push(1);

        let year = FiscalYear::calendar(2025).unwrap();
        let from_text = build_fact_set(CompanyId::new(), year, &[parsed]);
        let from_scan = build_fact_set(CompanyId::new(), year, &[scanned]);

        let text_confidence = from_text
            .get(&FactKind::Revenue)
            .unwrap()
            .extraction_confidence
            .get();
        let scan_confidence = from_scan
            .get(&FactKind::Revenue)
            .unwrap()
            .extraction_confidence
            .get();

        assert!(
            scan_confidence < text_confidence,
            "a scanned reading ({scan_confidence}) must not be trusted like a parsed one ({text_confidence})"
        );
        assert!(
            scan_confidence < 0.5,
            "a scanned reading must fall below the actionable threshold, was {scan_confidence}"
        );
    }

    /// A scanned PDF routinely mixes the two — a covering letter with real
    /// text in front of photographed statements — so the penalty has to
    /// follow the page, not the document.
    #[test]
    fn only_the_scanned_page_is_penalised() {
        let mut doc = document("Nettoomsättning    1 000 000");
        doc.extracted.pages.push(Page {
            number: 2,
            text: "Rörelseresultat    250 000".to_string(),
        });
        doc.extracted.ocr_pages.push(2);

        let set = build_fact_set(
            CompanyId::new(),
            FiscalYear::calendar(2025).unwrap(),
            &[doc],
        );
        let from_text_layer = set.get(&FactKind::Revenue).unwrap();
        let from_scan = set.get(&FactKind::OperatingProfit).unwrap();

        assert_eq!(from_text_layer.source_page, Some(1));
        assert_eq!(from_scan.source_page, Some(2));
        assert!(
            from_scan.extraction_confidence < from_text_layer.extraction_confidence,
            "the page read off pixels should be the only one penalised"
        );
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
                ..Default::default()
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

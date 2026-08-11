//! # skattjakt-extract
//!
//! Turns uploaded bytes into pages of text, and pages of text into candidate
//! financial facts.
//!
//! The pipeline stage this crate implements is deliberately conservative: it
//! reads what it can prove and reports what it could not. A page that produced
//! no text is surfaced as a warning rather than silently contributing nothing
//! (section 31).

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod swedish;

use skattjakt_core::document::MimeType;
use thiserror::Error;

pub use swedish::{detect_scale, extract_from_page, find_amounts, ExtractedFact, Scale};

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("the file is not a readable {0:?} document")]
    Unreadable(MimeType),

    #[error("{0} is not supported for extraction yet")]
    Unsupported(String),

    #[error("the document contained no extractable text; it may be a scan")]
    NoText,
}

/// One page of extracted text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// 1-based.
    pub number: u32,
    pub text: String,
}

impl Page {
    /// A page with no usable text — typically a scan without OCR.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// The text layer of a document, plus what could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedDocument {
    pub pages: Vec<Page>,
    /// Pages that yielded no text. Reported to the user, never ignored.
    pub unreadable_pages: Vec<u32>,
    pub scale: Scale,
}

impl ExtractedDocument {
    pub fn page_count(&self) -> u32 {
        self.pages.len() as u32
    }

    pub fn full_text(&self) -> String {
        self.pages
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Fraction of pages that produced text. Feeds the extraction-quality
    /// component of the confidence engine.
    pub fn readable_fraction(&self) -> f64 {
        if self.pages.is_empty() {
            return 0.0;
        }
        let readable = self.pages.iter().filter(|p| !p.is_empty()).count();
        readable as f64 / self.pages.len() as f64
    }

    /// Runs the deterministic Swedish parser over every page.
    pub fn facts(&self) -> Vec<ExtractedFact> {
        self.pages
            .iter()
            .flat_map(|page| extract_from_page(page.number, &page.text, self.scale))
            .collect()
    }
}

/// Extracts text from raw bytes according to the declared type.
pub fn extract(bytes: &[u8], mime: MimeType) -> Result<ExtractedDocument, ExtractError> {
    let pages = match mime {
        MimeType::Pdf => extract_pdf(bytes)?,
        MimeType::PlainText | MimeType::Csv | MimeType::Sie => extract_text(bytes),
        MimeType::Xlsx => {
            return Err(ExtractError::Unsupported(
                "XLSX (the pipeline accepts the format but has no reader yet)".to_string(),
            ))
        }
    };

    let unreadable_pages = pages
        .iter()
        .filter(|p| p.is_empty())
        .map(|p| p.number)
        .collect();
    let scale = detect_scale(
        &pages
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let document = ExtractedDocument {
        pages,
        unreadable_pages,
        scale,
    };

    if document.pages.is_empty() || document.readable_fraction() == 0.0 {
        return Err(ExtractError::NoText);
    }

    Ok(document)
}

fn extract_text(bytes: &[u8]) -> Vec<Page> {
    let text = String::from_utf8_lossy(bytes).to_string();
    vec![Page { number: 1, text }]
}

fn extract_pdf(bytes: &[u8]) -> Result<Vec<Page>, ExtractError> {
    // `pdf-extract` returns the whole document as one string with form feeds
    // between pages, which is enough to keep page attribution honest.
    let text = pdf_extract::extract_text_from_mem(bytes)
        .map_err(|_| ExtractError::Unreadable(MimeType::Pdf))?;

    let pages: Vec<Page> = text
        .split('\u{c}')
        .enumerate()
        .map(|(i, page)| Page {
            number: i as u32 + 1,
            text: page.to_string(),
        })
        .collect();

    if pages.is_empty() {
        return Err(ExtractError::NoText);
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_extraction_produces_one_page() {
        let doc = extract(b"Nettoomsattning 100\n", MimeType::PlainText).unwrap();
        assert_eq!(doc.page_count(), 1);
        assert_eq!(doc.readable_fraction(), 1.0);
    }

    #[test]
    fn an_empty_document_is_an_error_not_an_empty_result() {
        assert!(matches!(
            extract(b"   \n", MimeType::PlainText),
            Err(ExtractError::NoText)
        ));
    }

    #[test]
    fn a_document_that_is_not_a_pdf_fails_cleanly() {
        assert!(matches!(
            extract(b"not a pdf at all", MimeType::Pdf),
            Err(ExtractError::Unreadable(MimeType::Pdf))
        ));
    }

    #[test]
    fn unsupported_formats_say_so_rather_than_returning_nothing() {
        assert!(matches!(
            extract(b"PK\x03\x04", MimeType::Xlsx),
            Err(ExtractError::Unsupported(_))
        ));
    }

    #[test]
    fn readable_fraction_reflects_blank_pages() {
        let doc = ExtractedDocument {
            pages: vec![
                Page {
                    number: 1,
                    text: "text".into(),
                },
                Page {
                    number: 2,
                    text: "  ".into(),
                },
                Page {
                    number: 3,
                    text: "more".into(),
                },
                Page {
                    number: 4,
                    text: String::new(),
                },
            ],
            unreadable_pages: vec![2, 4],
            scale: Scale::Kronor,
        };
        assert_eq!(doc.readable_fraction(), 0.5);
    }

    #[test]
    fn facts_are_collected_across_pages_with_page_attribution() {
        let doc = ExtractedDocument {
            pages: vec![
                Page {
                    number: 1,
                    text: "Nettoomsättning 1 000 000".into(),
                },
                Page {
                    number: 2,
                    text: "Summa tillgångar 5 000 000".into(),
                },
            ],
            unreadable_pages: vec![],
            scale: Scale::Kronor,
        };
        let facts = doc.facts();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].page, 1);
        assert_eq!(facts[1].page, 2);
    }

    #[test]
    fn the_thousands_scale_is_detected_from_the_document_as_a_whole() {
        let doc = extract(
            b"Belopp i tkr\nNettoomsattning 12 500\n",
            MimeType::PlainText,
        )
        .unwrap();
        assert_eq!(doc.scale, Scale::Thousands);
    }
}

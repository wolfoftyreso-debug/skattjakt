//! Reading what extraction could not.
//!
//! `skattjakt-extract` crosses to wasm32 and cannot carry an OCR engine, so it
//! reports an image as read-but-not-understood and stops. This is the other
//! half, and it runs only server-side: given the same bytes and a loaded
//! reader, it fills in the text and records which pages it had to read off
//! pixels to get.

use skattjakt_extract::{ExtractedDocument, Page};

use crate::engine::Reader;
use crate::layout::{Row, Sign};

/// What happened, so the caller can log it and the report can say it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filled {
    /// Nothing needed reading: the document had a text layer.
    NotNeeded,
    /// No reader was configured. Said rather than silently skipped — a
    /// deployment without models reads no scans at all, and that must not
    /// look the same as a scan that held nothing.
    NoReader,
    /// The bytes are not a single image this reader can open. A scanned PDF
    /// lands here: its pages are images inside a container, and pulling them
    /// out is not done yet.
    Unsupported,
    /// Read, with the number of rows that carried a figure.
    Read {
        rows_with_figures: usize,
    },
    Failed(String),
}

/// True when extraction produced nothing usable and the bytes may be a picture.
fn worth_reading(document: &ExtractedDocument) -> bool {
    document.pages.iter().all(Page::is_empty)
}

/// Render the rows back into the line-per-row text the Swedish parser expects.
///
/// A row whose sign was never read is written without one. That is not the
/// same as writing a positive number and hoping: the parser sees the same
/// text a human would see on the scan, and the figure carries the reduced
/// confidence that says how it was obtained.
fn as_text(rows: &[Row]) -> String {
    let mut out = String::new();
    for row in rows {
        out.push_str(&row.label);
        if let Some(amount) = &row.amount {
            out.push_str("    ");
            if amount.sign == Sign::Negative {
                out.push('-');
            }
            out.push_str(&amount.digits);
        }
        out.push('\n');
    }
    out
}

/// Read an uploaded image and put its text into the document.
///
/// Returns what happened rather than a bool, because "there was no reader"
/// and "the picture held nothing" need different words in front of a customer
/// who uploaded a photograph on purpose.
pub fn read_images(
    document: &mut ExtractedDocument,
    bytes: &[u8],
    reader: Option<&Reader>,
) -> Filled {
    if !worth_reading(document) {
        return Filled::NotNeeded;
    }
    if !document.detected_type.starts_with("image/") {
        return Filled::Unsupported;
    }
    let Some(reader) = reader else {
        return Filled::NoReader;
    };

    let rows = match reader.read_page(bytes) {
        Ok(rows) => rows,
        Err(e) => return Filled::Failed(e.to_string()),
    };
    let with_figures = rows.iter().filter(|r| r.amount.is_some()).count();
    let text = as_text(&rows);
    if text.trim().is_empty() {
        return Filled::Read {
            rows_with_figures: 0,
        };
    }

    document.pages = vec![Page { number: 1, text }];
    document.unreadable_pages.clear();
    document.ocr_pages = vec![1];
    document.not_read_because = None;
    Filled::Read {
        rows_with_figures: with_figures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Amount, Row};

    fn row(label: &str, digits: &str, sign: Sign) -> Row {
        Row {
            label: label.to_string(),
            amount: Some(Amount {
                digits: digits.to_string(),
                sign,
            }),
            top: 0.0,
        }
    }

    #[test]
    fn a_read_sign_survives_into_the_text() {
        let text = as_text(&[row("Avskrivningar", "1120000", Sign::Negative)]);
        assert!(text.contains("-1120000"), "{text}");
    }

    /// The figure whose sign was lost is written without one, not with a
    /// plus. Inventing the sign is how a cost becomes income.
    #[test]
    fn an_unread_sign_is_not_invented() {
        let text = as_text(&[row("Rorelsens kostnader", "9830000", Sign::Unsigned)]);
        assert!(text.contains("9830000"), "{text}");
        assert!(!text.contains("-9830000"), "a sign was invented: {text}");
        assert!(!text.contains('+'), "a sign was invented: {text}");
    }

    #[test]
    fn a_document_with_text_is_left_alone() {
        let mut doc = ExtractedDocument {
            pages: vec![Page {
                number: 1,
                text: "Nettoomsättning 1 000".into(),
            }],
            ..Default::default()
        };
        assert_eq!(read_images(&mut doc, &[], None), Filled::NotNeeded);
        assert!(doc.ocr_pages.is_empty());
    }

    /// Without models the answer is "no reader", never silence: a deployment
    /// that reads no scans must not look like a scan that held nothing.
    #[test]
    fn a_missing_reader_is_reported_not_hidden() {
        let mut doc = ExtractedDocument {
            pages: vec![Page {
                number: 1,
                text: String::new(),
            }],
            detected_type: "image/jpeg".into(),
            ..Default::default()
        };
        assert_eq!(read_images(&mut doc, &[], None), Filled::NoReader);
    }

    /// A scanned PDF is not a single image and is not read yet. It must say
    /// so rather than report the scan as empty.
    #[test]
    fn a_pdf_is_reported_as_unsupported_not_empty() {
        let mut doc = ExtractedDocument {
            pages: vec![Page {
                number: 1,
                text: String::new(),
            }],
            detected_type: "application/pdf".into(),
            ..Default::default()
        };
        assert_eq!(read_images(&mut doc, &[], None), Filled::Unsupported);
    }
}

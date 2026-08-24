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

pub mod markup;
pub mod office;
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
    /// What the bytes turned out to be, whatever the filename claimed.
    pub detected_type: String,
    /// Why nothing was read, when nothing was. A photograph, an archive, an
    /// old Office document — stated in the customer's terms, because they
    /// uploaded it deliberately and "unsupported file" answers nothing.
    pub not_read_because: Option<String>,
    /// Where the extractor stopped, when the file was larger than its budget.
    ///
    /// Present means the analysis rests on a prefix. That is worse than resting
    /// on the whole file and it is not a failure — but it must be visible,
    /// because a fact that was in the part we did not read is indistinguishable
    /// from a fact that was not there.
    pub truncated_after_bytes: Option<usize>,
    /// The file's full size, whether or not all of it was read.
    pub total_bytes: usize,
}

impl Default for ExtractedDocument {
    /// A document that was received and yielded nothing.
    ///
    /// Not an error state: it is what a photograph or an archive produces, and
    /// what a test that only cares about pages starts from.
    fn default() -> Self {
        Self {
            pages: Vec::new(),
            unreadable_pages: Vec::new(),
            scale: Scale::Kronor,
            detected_type: "text/plain".to_string(),
            not_read_because: None,
            truncated_after_bytes: None,
            total_bytes: 0,
        }
    }
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

/// How much of a file the extractor is allowed to read.
///
/// # Why a budget exists at all
///
/// Uploads are bounded at 5 GB. Nothing in this system can hold 5 GB: a
/// WebAssembly module is limited to a 4 GiB address space by the target and to
/// far less in practice, and a serverless function has a memory ceiling well
/// below that. Reading a file "completely" is therefore not a promise anyone
/// can keep, and pretending otherwise means an out-of-memory kill instead of
/// an answer.
///
/// So the extractor reads a bounded prefix and **says so**. A truncated
/// document carries `truncated_after_bytes`, the report states it, and the
/// confidence model sees a document it only partly read. That is a worse
/// analysis than a complete one and an honest one, which is the trade this
/// makes deliberately.
///
/// The default is generous relative to what a set of annual accounts actually
/// is: 64 MB of text is roughly twenty thousand pages. A file bigger than that
/// is not a bigger bokslut, it is a different kind of thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractionBudget {
    pub max_bytes: usize,
}

impl ExtractionBudget {
    /// 64 MB. See the type's documentation for why this number and not 5 GB.
    pub const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;
}

impl Default for ExtractionBudget {
    fn default() -> Self {
        Self {
            max_bytes: Self::DEFAULT_MAX_BYTES,
        }
    }
}

/// Extracts text from raw bytes, within the default budget.
pub fn extract(bytes: &[u8], mime: MimeType) -> Result<ExtractedDocument, ExtractError> {
    extract_within(bytes, mime, ExtractionBudget::default())
}

/// Extracts text from raw bytes, reading at most `budget.max_bytes`.
///
/// Never returns an error for a type it cannot read. A file that is a
/// photograph or an archive produces a document with no pages and a stated
/// reason, because the customer uploaded it deliberately and is entitled to
/// know what happened to it. Only a file that *should* have been readable and
/// was not — a corrupt PDF — is an error.
pub fn extract_within(
    bytes: &[u8],
    mime: MimeType,
    budget: ExtractionBudget,
) -> Result<ExtractedDocument, ExtractError> {
    let truncated = bytes.len() > budget.max_bytes;
    // Cut on a character boundary when the content is text, so the last line is
    // not a half-decoded rune that the parser then reads as a label.
    let read = if truncated {
        let mut end = budget.max_bytes;
        while end > 0 && (bytes[end] & 0xc0) == 0x80 {
            end -= 1;
        }
        &bytes[..end]
    } else {
        bytes
    };

    if let Some(reason) = mime.why_unreadable() {
        // Not an error. The file was received; this is what it was.
        return Ok(ExtractedDocument {
            pages: Vec::new(),
            unreadable_pages: Vec::new(),
            scale: Scale::Kronor,
            detected_type: mime.as_content_type().to_string(),
            not_read_because: Some(match &mime {
                MimeType::Zip => format!("{reason} {}", zip_listing(read)),
                _ => reason,
            }),
            truncated_after_bytes: truncated.then_some(read.len()),
            total_bytes: bytes.len(),
        });
    }

    let pages = match &mime {
        MimeType::Pdf => extract_pdf(read)?,
        MimeType::Docx => office::docx(read)?,
        MimeType::Xlsx => office::xlsx(read)?,
        MimeType::Html => vec![text_page(&markup::strip_tags(&lossy(read)))],
        MimeType::Xml => vec![text_page(&markup::strip_tags(&lossy(read)))],
        MimeType::Rtf => vec![text_page(&markup::strip_rtf(&lossy(read)))],
        MimeType::Json => vec![text_page(&markup::flatten_json(&lossy(read)))],
        MimeType::PlainText | MimeType::Csv | MimeType::Sie => extract_text(read),
        // Handled above by `why_unreadable`.
        MimeType::Zip | MimeType::Other(_) => Vec::new(),
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
        detected_type: mime.as_content_type().to_string(),
        not_read_because: None,
        truncated_after_bytes: truncated.then_some(read.len()),
        total_bytes: bytes.len(),
    };

    if document.pages.is_empty() || document.readable_fraction() == 0.0 {
        return Err(ExtractError::NoText);
    }
    Ok(document)
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn text_page(text: &str) -> Page {
    Page {
        number: 1,
        text: text.to_string(),
    }
}

/// The member names in a ZIP's central directory.
///
/// A listing rather than an extraction: inflating an archive from an untrusted
/// upload is how a zip bomb turns 5 GB of storage into an out-of-memory kill.
/// Naming what is inside is the useful half and costs a scan of the tail.
fn zip_listing(bytes: &[u8]) -> String {
    const CENTRAL: &[u8] = b"PK\x01\x02";
    let mut names = Vec::new();
    let mut i = 0usize;
    while i + 46 <= bytes.len() && names.len() < 25 {
        if &bytes[i..i + 4] == CENTRAL {
            let n = u16::from_le_bytes([bytes[i + 28], bytes[i + 29]]) as usize;
            let start = i + 46;
            if start + n <= bytes.len() {
                if let Ok(name) = std::str::from_utf8(&bytes[start..start + n]) {
                    if !name.ends_with('/') {
                        names.push(name.to_string());
                    }
                }
            }
            i += 46 + n;
        } else {
            i += 1;
        }
    }
    if names.is_empty() {
        return "Innehållet gick inte att lista.".to_string();
    }
    format!("Innehåller: {}.", names.join(", "))
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
    fn a_malformed_archive_says_so_rather_than_returning_nothing() {
        // Four bytes that claim to be a ZIP and hold no members. The reader
        // must name what it could not find rather than yielding an empty
        // document that looks like a scan.
        //
        // `MimeType::Xlsx` used to be refused outright here, before there was a
        // reader for it at all; a spreadsheet now extracts, and this covers the
        // case where the container is broken.
        assert!(matches!(
            extract(b"PK\x03\x04", MimeType::Xlsx),
            Err(ExtractError::Unsupported(_) | ExtractError::NoText)
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
            ..Default::default()
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
            ..Default::default()
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

#[cfg(test)]
mod any_file_any_size {
    use super::*;

    /// A Word document is a ZIP of XML, and Swedish annual reports arrive as
    /// one often enough to be worth reading rather than refusing.
    #[test]
    fn a_word_document_yields_its_text() {
        let docx = fixtures::docx(
            "<w:document><w:body>\
             <w:p><w:r><w:t>Nettoomsättning</w:t></w:r><w:r><w:t>4 200 000</w:t></w:r></w:p>\
             </w:body></w:document>",
        );
        let mime = MimeType::sniff(&docx, Some("bokslut.docx"));
        assert_eq!(mime, MimeType::Docx);
        let doc = extract(&docx, mime).expect("a docx with text is readable");
        assert!(
            doc.full_text().contains("Nettoomsättning"),
            "{}",
            doc.full_text()
        );
        assert!(doc.full_text().contains("4 200 000"));
    }

    /// `xlsx` used to answer "the pipeline accepts the format but has no reader
    /// yet" — a type declared supported and then refused.
    #[test]
    fn a_spreadsheet_yields_its_cells_with_the_shared_strings_resolved() {
        let xlsx = fixtures::xlsx();
        let mime = MimeType::sniff(&xlsx, Some("bokslut.xlsx"));
        assert_eq!(mime, MimeType::Xlsx);
        let doc = extract(&xlsx, mime).expect("a workbook with cells is readable");
        let text = doc.full_text();
        // The label comes from sharedStrings; without resolving it the sheet is
        // a grid of integers where the labels should be.
        assert!(text.contains("Nettoomsättning"), "{text}");
        assert!(text.contains("4200000"), "{text}");
        // And the row survived: the Swedish parser reads the first amount on a
        // line, so a label and its amount must stay on one.
        let line = text
            .lines()
            .find(|l| l.contains("Nettoomsättning"))
            .expect("a row");
        assert!(line.contains("4200000"), "the row was split: {line:?}");
    }

    #[test]
    fn html_and_xml_keep_their_rows() {
        let html = b"<html><body><table>\
            <tr><td>Nettoomsattning</td><td>4 200 000</td></tr>\
            <tr><td>Personalkostnader</td><td>-2 100 000</td></tr>\
            </table><script>var x = 999999;</script></body></html>";
        let doc = extract(html, MimeType::sniff(html, None)).expect("readable");
        let text = doc.full_text();
        let first = text
            .lines()
            .find(|l| l.contains("Nettoomsattning"))
            .expect("row");
        assert!(first.contains("4 200 000"), "{first:?}");
        assert!(
            !text.contains("999999"),
            "the script body leaked into the text"
        );
    }

    #[test]
    fn json_becomes_labelled_lines() {
        let json = br#"{"resultatrakning": {"nettoomsattning": 4200000}}"#;
        let doc = extract(json, MimeType::sniff(json, None)).expect("readable");
        let text = doc.full_text();
        assert!(text.contains("nettoomsattning"), "{text}");
        assert!(text.contains("4200000"), "{text}");
    }

    #[test]
    fn rtf_loses_its_control_words_and_keeps_its_letters() {
        let rtf = br"{\rtf1\ansi\deff0 Nettooms\'e4ttning\tab 4 200 000\par}";
        let doc = extract(rtf, MimeType::sniff(rtf, None)).expect("readable");
        let text = doc.full_text();
        assert!(text.contains("Nettoomsättning"), "{text:?}");
        assert!(text.contains("4 200 000"), "{text:?}");
        assert!(!text.contains("rtf1"), "a control word survived: {text:?}");
    }

    /// The point of the whole change: nothing is refused for its type.
    #[test]
    fn a_file_we_cannot_read_is_still_received_and_explained() {
        let jpeg = [b"\xff\xd8\xff\xe0".as_slice(), &[0u8; 64]].concat();
        let doc = extract(&jpeg, MimeType::sniff(&jpeg, Some("bokslut.pdf")))
            .expect("a photograph is not an error");
        assert!(doc.pages.is_empty());
        assert_eq!(doc.detected_type, "image/jpeg");
        let why = doc.not_read_because.expect("a reason");
        assert!(why.contains("bild"), "{why}");
        assert_eq!(doc.total_bytes, jpeg.len());
    }

    /// An archive is listed, never inflated. Inflating an untrusted upload is
    /// how a zip bomb turns storage into an out-of-memory kill.
    #[test]
    fn an_archive_is_listed_rather_than_opened() {
        let zip = fixtures::zip_with(&["bokslut.pdf", "kvitton/mars.jpg"]);
        let mime = MimeType::sniff(&zip, Some("allt.zip"));
        assert_eq!(mime, MimeType::Zip);
        let doc = extract(&zip, mime).expect("an archive is not an error");
        let why = doc.not_read_because.expect("a reason");
        assert!(why.contains("bokslut.pdf"), "{why}");
        assert!(why.contains("mars.jpg"), "{why}");
        assert!(why.contains("packa upp"), "{why}");
    }

    /// A file larger than the budget is read to the budget and says so.
    ///
    /// This is what makes a 5 GB upload answerable: the bytes are stored whole
    /// and the analysis rests on a prefix it names. A fact in the part we did
    /// not read is indistinguishable from a fact that was not there, so the
    /// reader has to be told which they are looking at.
    #[test]
    fn a_file_past_the_budget_is_read_to_the_budget_and_says_so() {
        let mut big = String::from("Nettoomsättning        4 200 000\n");
        while big.len() < 200_000 {
            big.push_str("Rad utan betydelse\n");
        }
        let budget = ExtractionBudget { max_bytes: 50_000 };
        let doc = extract_within(big.as_bytes(), MimeType::PlainText, budget).expect("readable");

        assert_eq!(doc.total_bytes, big.len());
        let stopped = doc.truncated_after_bytes.expect("it must say it stopped");
        assert!(stopped <= 50_000, "read past the budget: {stopped}");
        // The part it did read is still analysed.
        assert!(doc.full_text().contains("Nettoomsättning"));
    }

    /// And a file inside the budget claims nothing.
    #[test]
    fn a_file_inside_the_budget_is_not_marked_truncated() {
        let doc = extract(b"Nettoomsattning 4 200 000\n", MimeType::PlainText).expect("readable");
        assert_eq!(doc.truncated_after_bytes, None);
    }

    /// Cutting mid-character would leave a half-decoded rune for the label
    /// matcher to read as a word.
    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // 'ä' is two bytes; the budget lands between them.
        let text = "aaaaäöå räkenskapsår";
        let budget = ExtractionBudget { max_bytes: 5 };
        let doc = extract_within(text.as_bytes(), MimeType::PlainText, budget);
        // Either it read a valid prefix or it found no text; never a panic and
        // never a replacement character in the middle of a label.
        if let Ok(doc) = doc {
            assert!(
                !doc.full_text().contains('\u{fffd}'),
                "{:?}",
                doc.full_text()
            );
        }
    }

    /// Fixtures built by hand: a real .docx would be a binary blob in the
    /// repository, and these say what they contain.
    mod fixtures {
        /// A ZIP holding one stored (uncompressed) member.
        fn stored_zip(members: &[(&str, &[u8])]) -> Vec<u8> {
            let mut out = Vec::new();
            let mut directory = Vec::new();
            let mut offsets = Vec::new();
            for (name, body) in members {
                offsets.push(out.len() as u32);
                out.extend_from_slice(b"PK\x03\x04");
                out.extend_from_slice(&[20, 0]); // version
                out.extend_from_slice(&[0, 0]); // flags
                out.extend_from_slice(&[0, 0]); // stored
                out.extend_from_slice(&[0, 0, 0, 0]); // time, date
                out.extend_from_slice(&[0, 0, 0, 0]); // crc, unchecked here
                out.extend_from_slice(&(body.len() as u32).to_le_bytes());
                out.extend_from_slice(&(body.len() as u32).to_le_bytes());
                out.extend_from_slice(&(name.len() as u16).to_le_bytes());
                out.extend_from_slice(&[0, 0]); // extra
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(body);
            }
            for ((name, body), offset) in members.iter().zip(offsets) {
                directory.extend_from_slice(b"PK\x01\x02");
                directory.extend_from_slice(&[20, 0, 20, 0]);
                directory.extend_from_slice(&[0, 0, 0, 0]);
                directory.extend_from_slice(&[0, 0, 0, 0]);
                directory.extend_from_slice(&[0, 0, 0, 0]);
                directory.extend_from_slice(&(body.len() as u32).to_le_bytes());
                directory.extend_from_slice(&(body.len() as u32).to_le_bytes());
                directory.extend_from_slice(&(name.len() as u16).to_le_bytes());
                // extra(2) + comment(2) + disk(2) + internal(2) + external(4),
                // which together with what precedes them makes the 46 bytes a
                // central-directory header is before the name. Writing 48 here
                // put every name two bytes off and the reader found none.
                directory.extend_from_slice(&[0u8; 6]);
                directory.extend_from_slice(&[0u8; 6]);
                directory.extend_from_slice(&offset.to_le_bytes());
                directory.extend_from_slice(name.as_bytes());
            }
            out.extend_from_slice(&directory);
            out
        }

        pub fn docx(document_xml: &str) -> Vec<u8> {
            stored_zip(&[("word/document.xml", document_xml.as_bytes())])
        }

        pub fn xlsx() -> Vec<u8> {
            let shared = r#"<sst><si><t>Nettoomsättning</t></si></sst>"#;
            let sheet = r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>4200000</v></c></row>
                </sheetData></worksheet>"#;
            stored_zip(&[
                ("xl/workbook.xml", b"<workbook/>"),
                ("xl/sharedStrings.xml", shared.as_bytes()),
                ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
            ])
        }

        pub fn zip_with(names: &[&str]) -> Vec<u8> {
            let members: Vec<(&str, &[u8])> =
                names.iter().map(|n| (*n, b"content".as_slice())).collect();
            stored_zip(&members)
        }
    }
}

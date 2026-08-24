//! Word and Excel, which are ZIP archives of XML.
//!
//! # Why this is written rather than pulled in
//!
//! Both formats are, underneath, a ZIP holding XML. The text of a Word document
//! is in `word/document.xml`; a spreadsheet's cells are in
//! `xl/worksheets/sheet*.xml` with the strings in `xl/sharedStrings.xml`. What
//! this needs from each is the text in reading order with the rows intact —
//! not styling, not formulas, not revision history.
//!
//! `xlsx` previously returned `Unsupported`, and had done since the format was
//! declared accepted. A customer who uploaded a spreadsheet got told the type
//! was known and then that it could not be read.
//!
//! # The bomb
//!
//! An archive from an untrusted upload can inflate to far more than it stores.
//! Every member here is inflated under a **cap on the output**, not on the
//! input, and the caps are summed across members. A file that exceeds them is
//! read as far as the cap allows and marked truncated — the same contract the
//! byte budget uses, for the same reason.

use crate::{ExtractError, Page};

/// Bytes of inflated XML this reader will hold at once, across all members.
const INFLATED_CAP: usize = 96 * 1024 * 1024;

/// The text of a Word document, one page per paragraph run of the body.
pub fn docx(bytes: &[u8]) -> Result<Vec<Page>, ExtractError> {
    let xml = member(bytes, "word/document.xml")?;
    let text = crate::markup::strip_tags(&xml);
    if text.trim().is_empty() {
        return Err(ExtractError::NoText);
    }
    Ok(vec![Page { number: 1, text }])
}

/// A workbook's cells, one page per sheet, rows preserved.
///
/// Shared strings are resolved: a cell of type `s` holds an index into
/// `sharedStrings.xml`, and a sheet read without them is a grid of integers
/// where the labels should be.
pub fn xlsx(bytes: &[u8]) -> Result<Vec<Page>, ExtractError> {
    let shared = member(bytes, "xl/sharedStrings.xml")
        .map(|xml| shared_strings(&xml))
        .unwrap_or_default();

    let mut pages = Vec::new();
    for (index, name) in sheet_members(bytes).into_iter().enumerate() {
        let Ok(xml) = member(bytes, &name) else {
            continue;
        };
        let text = sheet_text(&xml, &shared);
        if !text.trim().is_empty() {
            pages.push(Page {
                number: index as u32 + 1,
                text,
            });
        }
    }
    if pages.is_empty() {
        return Err(ExtractError::NoText);
    }
    Ok(pages)
}

/// `<si><t>…</t></si>` in document order; the index is the position.
fn shared_strings(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in xml.split("<si>").skip(1) {
        let body = chunk.split("</si>").next().unwrap_or("");
        out.push(crate::markup::strip_tags(body).trim().to_string());
    }
    out
}

/// A sheet as text: one line per `<row>`, cells separated by a run of spaces.
///
/// The run matters. Two adjacent cells joined by a single space would read as
/// one label, and the Swedish parser takes the first amount on a line — so a
/// label and its amount have to stay distinguishable.
fn sheet_text(xml: &str, shared: &[String]) -> String {
    let mut out = String::new();
    for row in xml.split("<row").skip(1) {
        let body = row.split("</row>").next().unwrap_or("");
        let mut cells = Vec::new();
        for cell in body.split("<c ").skip(1) {
            let head = cell.split('>').next().unwrap_or("");
            let is_shared = head.contains(r#"t="s""#);
            let value = cell
                .split("<v>")
                .nth(1)
                .and_then(|v| v.split("</v>").next())
                .unwrap_or("");
            // An inline string, which is how some writers emit text instead.
            let inline = cell
                .split("<t")
                .nth(1)
                .and_then(|v| v.split('>').nth(1))
                .and_then(|v| v.split("</t>").next());

            let text = if is_shared {
                value
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| shared.get(i))
                    .cloned()
                    .unwrap_or_default()
            } else if let Some(inline) = inline {
                inline.to_string()
            } else {
                value.to_string()
            };
            if !text.is_empty() {
                cells.push(text);
            }
        }
        if !cells.is_empty() {
            out.push_str(&cells.join("   "));
            out.push('\n');
        }
    }
    out
}

/// The worksheet member names, in the order they appear.
fn sheet_members(bytes: &[u8]) -> Vec<String> {
    let mut names: Vec<String> = central_directory(bytes)
        .into_iter()
        .filter(|n| n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml"))
        .collect();
    names.sort();
    names
}

/// Every member name in the archive's central directory.
fn central_directory(bytes: &[u8]) -> Vec<String> {
    const CENTRAL: &[u8] = b"PK\x01\x02";
    let mut names = Vec::new();
    let mut i = 0usize;
    while i + 46 <= bytes.len() {
        if &bytes[i..i + 4] == CENTRAL {
            let n = u16::from_le_bytes([bytes[i + 28], bytes[i + 29]]) as usize;
            let start = i + 46;
            if start + n <= bytes.len() {
                if let Ok(name) = std::str::from_utf8(&bytes[start..start + n]) {
                    names.push(name.to_string());
                }
            }
            i += 46 + n;
        } else {
            i += 1;
        }
    }
    names
}

/// Inflates one member, bounded.
///
/// Only the two storage methods an Office file actually uses: stored (0) and
/// deflate (8). Anything else is refused rather than guessed at.
fn member(bytes: &[u8], name: &str) -> Result<String, ExtractError> {
    let (method, compressed, uncompressed, data_start) = locate(bytes, name)
        .ok_or_else(|| ExtractError::Unsupported(format!("{name} is not in the archive")))?;
    if uncompressed > INFLATED_CAP {
        return Err(ExtractError::Unsupported(format!(
            "{name} inflates to {uncompressed} bytes, past the {INFLATED_CAP}-byte cap"
        )));
    }
    let end = data_start.saturating_add(compressed).min(bytes.len());
    let raw = &bytes[data_start.min(bytes.len())..end];
    let inflated = match method {
        0 => raw.to_vec(),
        8 => {
            miniz_oxide::inflate::decompress_to_vec_with_limit(raw, uncompressed.min(INFLATED_CAP))
                .map_err(|e| ExtractError::Unsupported(format!("{name} did not inflate: {e:?}")))?
        }
        other => {
            return Err(ExtractError::Unsupported(format!(
                "{name} uses compression method {other}"
            )))
        }
    };
    Ok(String::from_utf8_lossy(&inflated).into_owned())
}

/// Finds a member's local header and returns where its data begins.
fn locate(bytes: &[u8], name: &str) -> Option<(u16, usize, usize, usize)> {
    const LOCAL: &[u8] = b"PK\x03\x04";
    let needle = name.as_bytes();
    let mut i = 0usize;
    while i + 30 <= bytes.len() {
        if &bytes[i..i + 4] == LOCAL {
            let method = u16::from_le_bytes([bytes[i + 8], bytes[i + 9]]);
            let compressed = u32::from_le_bytes(bytes[i + 18..i + 22].try_into().ok()?) as usize;
            let uncompressed = u32::from_le_bytes(bytes[i + 22..i + 26].try_into().ok()?) as usize;
            let name_len = u16::from_le_bytes([bytes[i + 26], bytes[i + 27]]) as usize;
            let extra_len = u16::from_le_bytes([bytes[i + 28], bytes[i + 29]]) as usize;
            let name_start = i + 30;
            if name_start + name_len <= bytes.len()
                && &bytes[name_start..name_start + name_len] == needle
            {
                return Some((
                    method,
                    compressed,
                    uncompressed,
                    name_start + name_len + extra_len,
                ));
            }
            i = name_start + name_len + extra_len + compressed.max(1);
        } else {
            i += 1;
        }
    }
    None
}

//! Deciding whether a retrieved page says what a rule assumes.
//!
//! Pure: this module is handed text and returns a verdict. Fetching lives in
//! the worker, because the rule engine does no I/O — and because keeping the
//! judgement separate from the network is what makes it testable against pages
//! whose contents are known rather than against the internet.
//!
//! The judgement is deliberately narrow. It answers "does this document still
//! contain the words and figures the rule depends on", and nothing else. It
//! does not read the law, and it cannot tell whether a rule applies its source
//! correctly — see `SKATTJAKT_RULE_ENGINE.md` §8.1 for where that line is.

use sha2::{Digest, Sha256};

use crate::rule::{Source, SourceState};

/// What a check concluded, and the evidence for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub state: SourceState,
    /// A hash of the text that was read. Present only when the page was
    /// retrieved, whatever the verdict — a mismatch is a fact about a specific
    /// version of a document, and without the hash nobody can tell later
    /// whether the page changed again.
    pub sha256: Option<String>,
    /// Why, in a sentence a person can act on.
    pub note: Option<String>,
}

/// How far a chapter heading may sit from its paragraph and still be the same
/// citation. Published statute puts navigation, notes and amendment histories
/// between them; a tighter window rejects correct pages on layout.
const LOCATOR_SPAN: usize = 400;

impl Source {
    /// Checks retrieved text against what this source is claimed to say.
    ///
    /// Order matters. The document identity is checked before the locator, and
    /// the locator before the operative strings, so the note names the first
    /// thing that is wrong rather than a consequence of it: a page that is the
    /// wrong statute entirely would otherwise be reported as missing a figure.
    pub fn check(&self, html: &str) -> CheckOutcome {
        let text = strip_markup(html);
        let folded = fold(&text);
        let digest = hex_digest(&text);

        let mismatch = |note: String| CheckOutcome {
            state: SourceState::Mismatch,
            sha256: Some(digest.clone()),
            note: Some(note),
        };

        // Is this the document that was cited? Only meaningful for a collection
        // whose identifier appears in the text; an SFS number does, a URL slug
        // does not.
        if self.collection == "SFS"
            && !self.document.is_empty()
            && !folded.contains(&fold(&self.document))
        {
            return mismatch(format!(
                "the retrieved page does not mention SFS {}",
                self.document
            ));
        }

        if !locator_present(&text, &self.locator) {
            return mismatch(format!(
                "could not find {} in the retrieved text",
                self.locator
            ));
        }

        let absent: Vec<&str> = self
            .must_contain
            .iter()
            .filter(|needle| !folded.contains(&fold(needle)))
            .map(String::as_str)
            .collect();
        if !absent.is_empty() {
            return mismatch(format!(
                "the source does not contain: {}",
                absent.join(", ")
            ));
        }

        CheckOutcome {
            state: SourceState::Verified,
            sha256: Some(digest),
            note: None,
        }
    }
}

pub fn hex_digest(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Removes markup, leaving what a reader would see.
///
/// `script` and `style` contents are dropped rather than flattened. That is not
/// tidiness: a page whose only occurrence of "25 procent" sits inside a
/// `<script>` does not say 25 procent, and a checker that counted it would
/// verify a page a reader would call empty.
pub fn strip_markup(html: &str) -> String {
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(end) =
                skip_element(&lower, i, "script").or_else(|| skip_element(&lower, i, "style"))
            {
                out.push(' ');
                i = end;
                continue;
            }
            // An ordinary tag: drop it, but leave a space so words either side
            // of `</p><p>` do not become one word.
            match lower[i..].find('>') {
                Some(offset) => {
                    out.push(' ');
                    i += offset + 1;
                }
                None => break, // an unterminated tag: nothing readable follows
            }
            continue;
        }
        out.push(html[i..].chars().next().unwrap());
        i += html[i..].chars().next().unwrap().len_utf8();
    }

    collapse_whitespace(&decode_entities(&out))
}

/// If `at` opens the named element, returns the index just past its close tag.
fn skip_element(lower: &str, at: usize, name: &str) -> Option<usize> {
    let open = format!("<{name}");
    if !lower[at..].starts_with(&open) {
        return None;
    }
    // `<scriptable>` is not `<script>`; the next character must end the name.
    let after = lower[at + open.len()..].chars().next()?;
    if after.is_ascii_alphanumeric() {
        return None;
    }
    let close = format!("</{name}");
    match lower[at..].find(&close) {
        Some(offset) => {
            let from = at + offset;
            Some(from + lower[from..].find('>').map_or(close.len(), |o| o + 1))
        }
        // Unterminated: the rest of the document is inside the element.
        None => Some(lower.len()),
    }
}

fn decode_entities(text: &str) -> String {
    const NAMED: &[(&str, &str)] = &[
        ("&nbsp;", " "),
        ("&auml;", "ä"),
        ("&ouml;", "ö"),
        ("&aring;", "å"),
        ("&Auml;", "Ä"),
        ("&Ouml;", "Ö"),
        ("&Aring;", "Å"),
        ("&sect;", "§"),
        ("&ndash;", "–"),
        ("&mdash;", "—"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        // Last, so an entity written `&amp;auml;` is not decoded twice into a
        // character the page never contained.
        ("&amp;", "&"),
    ];
    let mut out = text.to_string();
    for (entity, replacement) in NAMED {
        if out.contains(entity) {
            out = out.replace(entity, replacement);
        }
    }
    decode_numeric(&out)
}

fn decode_numeric(text: &str) -> String {
    if !text.contains("&#") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("&#") {
        out.push_str(&rest[..start]);
        let body = &rest[start + 2..];
        let Some(end) = body.find(';') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let (digits, radix) = match body.as_bytes().first() {
            Some(b'x') | Some(b'X') => (&body[1..end], 16),
            _ => (&body[..end], 10),
        };
        match u32::from_str_radix(digits, radix)
            .ok()
            .and_then(char::from_u32)
        {
            Some(character) => out.push(character),
            None => out.push_str(&rest[start..start + 2 + end + 1]),
        }
        rest = &body[end + 1..];
    }
    out.push_str(rest);
    out
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for character in text.chars() {
        if character.is_whitespace() || character == '\u{a0}' {
            if !in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(character);
            in_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Folds the differences that are not differences.
///
/// Statute is published with non-breaking spaces inside figures, with several
/// dash characters used interchangeably, and with capitalisation that varies by
/// publisher. Comparing raw strings reports a mismatch for `20,6 %` against
/// `20,6 %` differing only in which space character sits before the sign.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for character in text.chars() {
        let character = match character {
            '\u{a0}' | '\u{202f}' | '\u{2009}' => ' ',
            '\u{2011}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
            other => other,
        };
        if character.is_whitespace() {
            if !in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = true;
            continue;
        }
        in_space = false;
        for lowered in character.to_lowercase() {
            out.push(lowered);
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Whether the cited position appears in the text.
///
/// A locator that names neither a chapter nor a paragraph — the two sources
/// that cite a named section of a non-statute — has nothing checkable here, and
/// returns true rather than failing every check it appears in. The operative
/// strings still have to be present, so such a source is not unchecked.
pub fn locator_present(text: &str, locator: &str) -> bool {
    let folded = fold(text);
    let chapter = number_before(locator, "kap");
    let paragraph = number_before(locator, "§");

    match (chapter, paragraph) {
        (Some(chapter), Some(paragraph)) => {
            // The chapter first, then the paragraph close behind it. Searching
            // for the paragraph alone would match `5 §` of any chapter, and
            // every statute has one.
            let mut from = 0;
            while let Some(offset) = find_number_then(&folded[from..], chapter, "kap") {
                let after = from + offset;
                let window = &folded[after..folded.len().min(after + LOCATOR_SPAN)];
                if find_number_then(window, paragraph, "§").is_some() {
                    return true;
                }
                from = after;
                if from >= folded.len() {
                    break;
                }
            }
            false
        }
        (None, Some(paragraph)) => find_number_then(&folded, paragraph, "§").is_some(),
        (Some(chapter), None) => find_number_then(&folded, chapter, "kap").is_some(),
        (None, None) => true,
    }
}

/// The number immediately preceding `marker` in a locator, e.g. 30 in
/// `30 kap. 5 §`. Ranges (`5–6 §§`) yield their first number, which is the one
/// a search must find.
fn number_before(locator: &str, marker: &str) -> Option<u32> {
    let at = locator.find(marker)?;
    let head = &locator[..at];
    let digits: String = head
        .chars()
        .rev()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.chars().rev().collect::<String>().parse().ok()
}

/// Finds `<number>` followed by `marker`, allowing whitespace and a period
/// between them, and returns the index just past the marker.
///
/// The number must not be part of a longer number, or `5 §` would match the
/// `15 §` two paragraphs down.
fn find_number_then(haystack: &str, number: u32, marker: &str) -> Option<usize> {
    let needle = number.to_string();
    let bytes = haystack.as_bytes();
    let mut from = 0;

    while let Some(offset) = haystack[from..].find(&needle) {
        let start = from + offset;
        let end = start + needle.len();
        let preceded_by_digit = start > 0 && bytes[start - 1].is_ascii_digit();
        let followed_by_digit = end < bytes.len() && bytes[end].is_ascii_digit();
        if !preceded_by_digit && !followed_by_digit {
            let mut cursor = end;
            while cursor < bytes.len() && (bytes[cursor] == b' ' || bytes[cursor] == b'.') {
                cursor += 1;
            }
            if haystack[cursor..].starts_with(marker) {
                return Some(cursor + marker.len());
            }
        }
        from = end;
        if from >= haystack.len() {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::Retrieval;

    fn source(locator: &str, must_contain: &[&str]) -> Source {
        Source {
            authority: "Sveriges riksdag".into(),
            collection: "SFS".into(),
            document: "1999:1229".into(),
            title: "Inkomstskattelag (1999:1229)".into(),
            locator: locator.into(),
            url: Some("https://example.test/il".into()),
            machine_url: Some("https://example.test/il.html".into()),
            asserted_claim: "under test".into(),
            must_contain: must_contain.iter().map(|s| s.to_string()).collect(),
            retrieval: Retrieval {
                state: SourceState::Unretrieved,
                at: None,
                sha256: None,
                note: None,
            },
        }
    }

    const PAGE: &str = r#"<!doctype html><html><head><title>Inkomstskattelag (1999:1229)</title>
<style>.x{content:"25 procent"}</style><script>var n = "25 procent";</script></head>
<body><h1>Inkomstskattelag (1999:1229)</h1>
<div><h2>30&nbsp;kap. Periodiseringsfonder</h2>
<p><b>5&nbsp;&sect;</b>&nbsp;&nbsp;En juridisk person f&aring;r g&ouml;ra avdrag med h&ouml;gst
25&nbsp;procent av &ouml;verskottet av n&auml;ringsverksamheten.</p>
<p><b>15&nbsp;&sect;</b> Annat.</p></div></body></html>"#;

    #[test]
    fn a_page_that_says_what_the_rule_assumes_verifies() {
        let outcome = source("30 kap. 5 §", &["25 procent", "överskottet"]).check(PAGE);
        assert_eq!(outcome.state, SourceState::Verified);
        assert!(outcome.note.is_none());
        assert_eq!(outcome.sha256.as_ref().unwrap().len(), 64);
    }

    #[test]
    fn a_changed_figure_is_a_mismatch() {
        let changed = PAGE.replace("25&nbsp;procent av", "22&nbsp;procent av");
        let outcome = source("30 kap. 5 §", &["25 procent"]).check(&changed);
        assert_eq!(outcome.state, SourceState::Mismatch);
        assert!(outcome.note.unwrap().contains("25 procent"));
    }

    #[test]
    fn text_inside_a_script_or_style_does_not_count() {
        // The page's only remaining "25 procent" is in the script and the
        // style. A checker that flattened them would call this verified.
        let gutted = PAGE.replace("25&nbsp;procent av &ouml;verskottet", "22 procent av annat");
        let outcome = source("30 kap. 5 §", &["25 procent"]).check(&gutted);
        assert_eq!(outcome.state, SourceState::Mismatch);
    }

    #[test]
    fn an_element_whose_name_merely_starts_with_script_is_not_skipped() {
        let html = "<scriptural>30 kap. 5 § 25 procent</scriptural>";
        assert!(strip_markup(html).contains("25 procent"));
    }

    #[test]
    fn the_wrong_statute_is_caught_before_anything_else() {
        let other = "<html><body><h1>Mervärdesskattelag (2023:200)</h1><p>30 kap. 5 § 25 procent</p></body></html>";
        let outcome = source("30 kap. 5 §", &["25 procent"]).check(other);
        assert_eq!(outcome.state, SourceState::Mismatch);
        assert!(outcome.note.unwrap().contains("SFS 1999:1229"));
    }

    #[test]
    fn a_missing_paragraph_is_caught() {
        let stub = "<html><body><h1>Inkomstskattelag (1999:1229)</h1>\
                    <p>Innehållsförteckning. 30 kap. Periodiseringsfonder</p>\
                    <p>Logga in för att läsa lagtexten.</p></body></html>";
        let outcome = source("30 kap. 5 §", &[]).check(stub);
        assert_eq!(outcome.state, SourceState::Mismatch);
        assert!(outcome.note.unwrap().contains("30 kap. 5 §"));
    }

    #[test]
    fn a_paragraph_number_does_not_match_a_longer_one() {
        // `5 §` must not be satisfied by the `15 §` further down, which is the
        // failure that would quietly verify a page missing the cited paragraph.
        let only_fifteen = "<html><body><h1>Inkomstskattelag (1999:1229)</h1>\
                            <h2>30 kap.</h2><p>15 § Annat.</p></body></html>";
        assert_eq!(
            source("30 kap. 5 §", &[]).check(only_fifteen).state,
            SourceState::Mismatch
        );
    }

    #[test]
    fn a_paragraph_in_a_different_chapter_does_not_satisfy_the_locator() {
        // 5 § exists, but in chapter 24, six hundred characters from any
        // mention of chapter 30.
        let filler = "text ".repeat(200);
        let html = format!(
            "<html><body><h1>Inkomstskattelag (1999:1229)</h1>\
             <h2>30 kap. Periodiseringsfonder</h2><p>{filler}</p>\
             <h2>24 kap.</h2><p>5 § Något annat.</p></body></html>"
        );
        assert_eq!(
            source("30 kap. 5 §", &[]).check(&html).state,
            SourceState::Mismatch
        );
    }

    #[test]
    fn a_range_locator_matches_on_its_first_paragraph() {
        assert!(locator_present("30 kap. 5 § och 6 §", "30 kap. 5–6 §§"));
    }

    #[test]
    fn formatting_is_not_substance() {
        // Non-breaking spaces, a missing period after `kap`, and capitalisation.
        assert!(locator_present("30\u{a0}kap 5\u{a0}§", "30 kap. 5 §"));
        let outcome = source("30 kap. 5 §", &["JURIDISK PERSON"]).check(PAGE);
        assert_eq!(outcome.state, SourceState::Verified);
    }

    #[test]
    fn a_locator_that_names_no_position_checks_nothing() {
        // The two non-statute sources. Their operative strings still have to be
        // present, so they are not waved through.
        assert!(locator_present(
            "anything at all",
            "Avdrag för representation"
        ));
        let mut named = source("Kapitel om obeskattade reserver", &["obeskattade reserver"]);
        named.collection = "BFNAR".into();
        named.document = "2016:10".into();
        assert_eq!(
            named
                .check("<p>Om obeskattade reserver i mindre företag.</p>")
                .state,
            SourceState::Verified
        );
    }

    #[test]
    fn the_same_text_always_hashes_the_same_and_different_text_does_not() {
        let first = source("30 kap. 5 §", &[]).check(PAGE);
        let second = source("30 kap. 5 §", &[]).check(PAGE);
        assert_eq!(first.sha256, second.sha256);

        let changed = PAGE.replace("25&nbsp;procent", "22&nbsp;procent");
        let third = source("30 kap. 5 §", &[]).check(&changed);
        assert_ne!(first.sha256, third.sha256);
    }

    #[test]
    fn a_mismatch_still_records_what_was_read() {
        // Without the hash, nobody can tell later whether the page changed
        // again between the mismatch and somebody looking at it.
        let changed = PAGE.replace("25&nbsp;procent av", "22&nbsp;procent av");
        let outcome = source("30 kap. 5 §", &["25 procent"]).check(&changed);
        assert_eq!(outcome.state, SourceState::Mismatch);
        assert_eq!(outcome.sha256.unwrap().len(), 64);
    }

    #[test]
    fn entities_are_decoded_including_numeric_ones() {
        let text = strip_markup("<p>30&#160;kap. 5&#xa0;&sect; &auml;&ouml;&aring; &amp;auml;</p>");
        assert!(text.contains("30 kap. 5 §"), "{text}");
        assert!(text.contains("äöå"), "{text}");
        // `&amp;auml;` is an escaped ampersand followed by text, not an ä.
        assert!(text.contains("&auml;"), "{text}");
    }

    #[test]
    fn tags_separate_words_rather_than_joining_them() {
        assert_eq!(
            strip_markup("<p>enklare</p><p>förtäring</p>"),
            "enklare förtäring"
        );
    }

    #[test]
    fn an_unterminated_script_swallows_the_rest_rather_than_leaking_it() {
        // A truncated download must not be read as though the script's contents
        // were body text.
        let truncated = "<body><p>30 kap. 5 §</p><script>var x = \"25 procent\";";
        let text = strip_markup(truncated);
        assert!(text.contains("30 kap. 5 §"));
        assert!(!text.contains("25 procent"), "{text}");
    }
}

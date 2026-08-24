//! Text out of markup, without a parser for each dialect.
//!
//! HTML, XML and RTF all wrap the numbers we want in syntax we do not. A full
//! parser for each is three dependencies and a lot of surface for a job whose
//! whole requirement is "give me the text, in reading order, with the line
//! breaks where the rows were". These are deliberately simple and deliberately
//! documented as such.
//!
//! What they must get right, because the Swedish parser downstream depends on
//! it: a row of a table has to stay on one line, and one cell has to stay
//! separate from the next. Collapse a table into a single line and every label
//! runs into its neighbour's amount.

/// Strips tags from HTML or XML, keeping the text and the row structure.
///
/// Elements that end a row or a block emit a newline; everything else emits a
/// space. `<script>` and `<style>` bodies are dropped — they are code, and a
/// stylesheet full of numbers is exactly the sort of thing a label matcher
/// would find something in.
pub fn strip_tags(input: &str) -> String {
    const BLOCK: &[&str] = &[
        "/tr", "/p", "/div", "/li", "/h1", "/h2", "/h3", "/h4", "/table", "br", "/row", "/section",
        "/article",
    ];
    let mut out = String::with_capacity(input.len() / 2);
    let mut chars = input.chars().peekable();
    let mut skipping: Option<&'static str> = None;

    while let Some(ch) = chars.next() {
        if ch != '<' {
            if skipping.is_none() {
                out.push(ch);
            }
            continue;
        }
        let mut tag = String::new();
        for c in chars.by_ref() {
            if c == '>' {
                break;
            }
            tag.push(c);
        }
        let name = tag
            .trim()
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        let lower = name.to_ascii_lowercase();
        let closing = tag.trim_start().starts_with('/');

        if let Some(open) = skipping {
            if closing && lower == open {
                skipping = None;
            }
            continue;
        }
        if !closing && (lower == "script" || lower == "style") {
            skipping = Some(if lower == "script" { "script" } else { "style" });
            continue;
        }
        let marker = if closing {
            format!("/{lower}")
        } else {
            lower.clone()
        };
        if BLOCK.contains(&marker.as_str()) {
            out.push('\n');
        } else if closing && (lower == "td" || lower == "th" || lower == "cell") {
            // A cell boundary is a run of spaces, not nothing: two adjacent
            // cells must not become one word.
            out.push_str("   ");
        } else {
            out.push(' ');
        }
    }
    decode_entities(&squeeze(&out))
}

/// RTF is a control-word language; the text is what is left after the controls.
///
/// Enough for a document saved out of Word: `\par` ends a paragraph, `\'xx` is
/// a hex-escaped byte, braces group, and everything else beginning with a
/// backslash is a control word to drop.
pub fn strip_rtf(input: &str) -> String {
    let mut out = String::with_capacity(input.len() / 2);
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' | '}' => {}
            '\\' => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphabetic() {
                        word.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // A numeric parameter, and the space that terminates the word.
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_digit() || c == '-' {
                        chars.next();
                    } else {
                        break;
                    }
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
                match word.as_str() {
                    "par" | "line" | "row" | "sect" | "page" => out.push('\n'),
                    "tab" | "cell" => out.push_str("   "),
                    "" => {
                        // `\'xx` — a byte in hex, which is how Word writes å ä ö
                        // in a document that is not Unicode-escaped.
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            let hex: String = chars.by_ref().take(2).collect();
                            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                                // Windows-1252, which is what such a file is.
                                out.push(cp1252(byte));
                            }
                        } else if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                    }
                    _ => {}
                }
            }
            _ => out.push(ch),
        }
    }
    squeeze(&out)
}

/// JSON, flattened to `path  value` lines.
///
/// An export from a bookkeeping system is a tree of numbers with names on them,
/// which is the same shape as a statement once the nesting is written out. The
/// leaf's path becomes the label, so `{"resultat":{"nettoomsättning":4200000}}`
/// reads as a line the Swedish parser can match.
pub fn flatten_json(input: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        // Not valid JSON after all. The raw text is still better than nothing —
        // the label matcher may find something in it.
        return input.to_string();
    };
    let mut out = String::new();
    walk(&value, &mut String::new(), &mut out);
    out
}

fn walk(value: &serde_json::Value, path: &mut String, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map {
                let mark = path.len();
                if !path.is_empty() {
                    path.push(' ');
                }
                path.push_str(key);
                walk(v, path, out);
                path.truncate(mark);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                walk(item, path, out);
            }
        }
        serde_json::Value::Number(n) => {
            out.push_str(path);
            out.push_str("   ");
            out.push_str(&n.to_string());
            out.push('\n');
        }
        serde_json::Value::String(s) => {
            out.push_str(path);
            out.push_str("   ");
            out.push_str(s);
            out.push('\n');
        }
        _ => {}
    }
}

/// The five entities that appear in a document full of amounts.
fn decode_entities(input: &str) -> String {
    input
        .replace("&nbsp;", "\u{00a0}")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&auml;", "ä")
        .replace("&ouml;", "ö")
        .replace("&aring;", "å")
        .replace("&Auml;", "Ä")
        .replace("&Ouml;", "Ö")
        .replace("&Aring;", "Å")
}

/// Collapses runs of blank lines and trailing spaces without touching the
/// spacing inside a line — the label matcher normalises that itself, and the
/// column padding is a hint the amount parser uses.
fn squeeze(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut blank_run = 0;
    for line in input.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
            out.push('\n');
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out
}

/// Windows-1252, for the bytes RTF escapes as `\'xx`.
fn cp1252(byte: u8) -> char {
    match byte {
        0x80 => '€',
        0x82 => '‚',
        0x83 => 'ƒ',
        0x84 => '„',
        0x85 => '…',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '“',
        0x94 => '”',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        other => other as char,
    }
}

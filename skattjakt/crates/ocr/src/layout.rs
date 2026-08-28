//! Turning recognised words back into the rows of a financial statement.
//!
//! # Why this exists
//!
//! An OCR engine reads prose. Given a resultaträkning it does something
//! reasonable for prose and useless here: it groups the page into *columns*,
//! so every label arrives in one block and every amount in another, and the
//! pairing between them — the only thing that carries meaning — is gone.
//!
//! Measured on a two-column statement, ocrs returned fifteen "lines": eight
//! containing only labels and seven containing only amounts. Nothing in that
//! output says which amount belongs to which label.
//!
//! So the rows are rebuilt here, from the geometry, before anything tries to
//! read a figure out of them. This module holds no OCR dependency at all: it
//! takes words with boxes and returns rows, which means the part that is easy
//! to get quietly wrong is also the part that is tested without a model.

/// One recognised word and where it sat on the page.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

impl Word {
    pub fn new(text: &str, left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            text: text.to_string(),
            left,
            top,
            right,
            bottom,
        }
    }
    fn vertical_middle(&self) -> f32 {
        (self.top + self.bottom) / 2.0
    }
    fn height(&self) -> f32 {
        (self.bottom - self.top).abs()
    }
}

/// What the reading says about an amount's sign.
///
/// There is no `Positive`. A statement figure printed without a minus may be
/// genuinely positive, or it may be a cost whose minus the recogniser dropped
/// — and on the page those two are the same pixels once the glyph is lost.
/// Claiming the first would be inventing evidence, so the reading reports
/// only what it saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sign {
    /// A minus, or brackets, were read.
    Negative,
    /// No sign was read. Not the same as positive.
    Unsigned,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Amount {
    /// Digits as read, separators removed, without the sign.
    pub digits: String,
    pub sign: Sign,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    pub amount: Option<Amount>,
    /// Where the row sat, so a fact can name the place it was read from.
    pub top: f32,
}

/// True for the pieces an amount is written in: digits, and the separators
/// Swedish statements use between and inside them.
fn is_amount_fragment(s: &str) -> bool {
    let t = s
        .trim_matches(|c| c == '(' || c == ')')
        .trim_start_matches(['-', '\u{2212}', '\u{2013}', '\u{2014}']);
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_digit() || c == ' ' || c == '\u{a0}' || c == ',' || c == '.')
        && t.chars().any(|c| c.is_ascii_digit())
}

/// A lone sign glyph, sitting apart from the digits it belongs to.
///
/// This is the shape the dropped minus takes: on a right-aligned amount
/// column the sign is a small mark separated from the first digit by a gap,
/// and the recogniser either attaches it, drops it, or returns it alone.
fn is_lone_minus(s: &str) -> bool {
    matches!(s.trim(), "-" | "\u{2212}" | "\u{2013}" | "\u{2014}")
}

/// Group words into the rows they were printed on.
///
/// Two words share a row when their vertical middles are closer than a
/// fraction of the taller one's height. Comparing middles rather than tops
/// keeps a row together when a figure is set in a smaller face than its
/// label, which is common.
pub fn rows_from_words(mut words: Vec<Word>, tolerance: f32) -> Vec<Row> {
    words.retain(|w| !w.text.trim().is_empty());
    words.sort_by(|a, b| {
        a.vertical_middle()
            .partial_cmp(&b.vertical_middle())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut grouped: Vec<Vec<Word>> = Vec::new();
    for w in words {
        match grouped.last_mut() {
            Some(row) => {
                let anchor = &row[0];
                let reach = anchor.height().max(w.height()) * tolerance;
                if (w.vertical_middle() - anchor.vertical_middle()).abs() <= reach {
                    row.push(w);
                } else {
                    grouped.push(vec![w]);
                }
            }
            None => grouped.push(vec![w]),
        }
    }

    grouped.into_iter().map(assemble_row).collect()
}

/// Split one row into its label and its amount.
///
/// The amount is taken from the right, not from a column position: a
/// hard-coded x would be a guess about one document's layout, while "the
/// run of numeric words at the end of the row" holds for any statement that
/// puts figures after names. The walk stops at the first word that is not
/// part of a number, which is where the label ends.
fn assemble_row(mut words: Vec<Word>) -> Row {
    words.sort_by(|a, b| {
        a.left
            .partial_cmp(&b.left)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let top = words.iter().map(|w| w.top).fold(f32::INFINITY, f32::min);

    let mut first_amount = words.len();
    while first_amount > 0 && is_amount_fragment(&words[first_amount - 1].text) {
        first_amount -= 1;
    }

    // Nothing numeric at the end: the whole row is a heading or a label.
    if first_amount == words.len() {
        let label = words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        return Row {
            label: label.trim().to_string(),
            amount: None,
            top,
        };
    }

    // A sign left adrift by the recogniser belongs to the amount, not the label.
    let mut sign = Sign::Unsigned;
    if first_amount > 0 && is_lone_minus(&words[first_amount - 1].text) {
        sign = Sign::Negative;
        first_amount -= 1;
    }

    let amount_words = &words[first_amount..];
    let joined: String = amount_words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join("");

    if joined.contains('(')
        || joined.contains(')')
        || joined.starts_with(['-', '\u{2212}', '\u{2013}', '\u{2014}'])
    {
        sign = Sign::Negative;
    }
    let digits: String = joined.chars().filter(|c| c.is_ascii_digit()).collect();

    let label = words[..first_amount]
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");

    Row {
        label: label.trim().to_string(),
        amount: (!digits.is_empty()).then_some(Amount { digits, sign }),
        top,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boxes ocrs actually returned for a two-column resultaträkning.
    /// Coordinates are the measured ones: the label column begins at x≈60,
    /// the amount column at x≈820, and rows are 56 pixels apart.
    fn statement() -> Vec<Word> {
        let mut w = Vec::new();
        let rows: [(&str, &[&str]); 4] = [
            ("Nettoomsattning", &["12", "450", "000"]),
            ("Rorelsens kostnader", &["-9", "830", "000"]),
            ("Avskrivningar", &["-1", "120", "000"]),
            ("Rorelseresultat", &["1", "500", "000"]),
        ];
        for (i, (label, amount)) in rows.iter().enumerate() {
            let top = 100.0 + i as f32 * 56.0;
            let mut x = 60.0;
            for part in label.split(' ') {
                w.push(Word::new(part, x, top, x + 160.0, top + 30.0));
                x += 180.0;
            }
            let mut ax = 820.0;
            for part in *amount {
                w.push(Word::new(part, ax, top, ax + 44.0, top + 30.0));
                ax += 52.0;
            }
        }
        w
    }

    #[test]
    fn columns_become_rows_again() {
        let rows = rows_from_words(statement(), 0.6);
        assert_eq!(rows.len(), 4, "{rows:#?}");
        assert_eq!(rows[0].label, "Nettoomsattning");
        assert_eq!(rows[0].amount.as_ref().unwrap().digits, "12450000");
        assert_eq!(rows[1].label, "Rorelsens kostnader");
        assert_eq!(rows[1].amount.as_ref().unwrap().digits, "9830000");
    }

    #[test]
    fn a_label_never_swallows_its_figure() {
        let rows = rows_from_words(statement(), 0.6);
        for r in &rows {
            assert!(
                !r.label.chars().any(|c| c.is_ascii_digit()),
                "the figure leaked into the label: {:?}",
                r.label
            );
        }
    }

    #[test]
    fn a_read_minus_is_kept() {
        let rows = rows_from_words(statement(), 0.6);
        assert_eq!(rows[1].amount.as_ref().unwrap().sign, Sign::Negative);
        assert_eq!(rows[2].amount.as_ref().unwrap().sign, Sign::Negative);
    }

    /// The failure this module exists to make visible. When the recogniser
    /// drops the glyph, the reading must say "no sign was read" — never
    /// "positive", because on the page those are the same pixels.
    #[test]
    fn a_dropped_minus_reads_as_unsigned_not_positive() {
        let mut words = statement();
        for w in &mut words {
            if w.text == "-9" {
                w.text = "9".to_string();
            }
        }
        let rows = rows_from_words(words, 0.6);
        assert_eq!(rows[1].amount.as_ref().unwrap().digits, "9830000");
        assert_eq!(
            rows[1].amount.as_ref().unwrap().sign,
            Sign::Unsigned,
            "a lost minus must not be reported as a positive figure"
        );
    }

    /// A sign the recogniser returned as its own box, adrift to the left of
    /// the digits. Attaching it to the label would lose it silently.
    #[test]
    fn a_sign_left_adrift_rejoins_its_amount() {
        let words = vec![
            Word::new("Avskrivningar", 60.0, 100.0, 300.0, 130.0),
            Word::new("-", 800.0, 100.0, 812.0, 130.0),
            Word::new("1", 820.0, 100.0, 864.0, 130.0),
            Word::new("120", 872.0, 100.0, 940.0, 130.0),
            Word::new("000", 948.0, 100.0, 1016.0, 130.0),
        ];
        let rows = rows_from_words(words, 0.6);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Avskrivningar");
        let a = rows[0].amount.as_ref().unwrap();
        assert_eq!(a.digits, "1120000");
        assert_eq!(a.sign, Sign::Negative);
    }

    #[test]
    fn brackets_are_a_minus() {
        let words = vec![
            Word::new("Rantekostnader", 60.0, 100.0, 300.0, 130.0),
            Word::new("(85", 820.0, 100.0, 880.0, 130.0),
            Word::new("000)", 888.0, 100.0, 960.0, 130.0),
        ];
        let rows = rows_from_words(words, 0.6);
        let a = rows[0].amount.as_ref().unwrap();
        assert_eq!(a.digits, "85000");
        assert_eq!(a.sign, Sign::Negative);
    }

    #[test]
    fn a_heading_has_no_amount() {
        let words = vec![Word::new("RESULTATRAKNING", 60.0, 40.0, 420.0, 76.0)];
        let rows = rows_from_words(words, 0.6);
        assert_eq!(rows[0].label, "RESULTATRAKNING");
        assert!(rows[0].amount.is_none());
    }

    /// A figure set smaller than its label still belongs to the same row.
    #[test]
    fn a_shorter_figure_stays_on_its_row() {
        let words = vec![
            Word::new("Arets resultat", 60.0, 100.0, 300.0, 140.0),
            Word::new("1", 820.0, 110.0, 850.0, 132.0),
            Word::new("415", 858.0, 110.0, 910.0, 132.0),
        ];
        let rows = rows_from_words(words, 0.6);
        assert_eq!(rows.len(), 1, "the figure was split onto its own row");
        assert_eq!(rows[0].amount.as_ref().unwrap().digits, "1415");
    }
}

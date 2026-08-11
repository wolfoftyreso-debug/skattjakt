//! Deterministic extraction from Swedish financial statements.
//!
//! This runs *before* the model. Values a parser can read off a labelled line
//! are read that way, because a fact that came from a rule-based parse carries
//! its own page and verbatim text and can be re-derived exactly. The model is
//! then asked about what the parser could not resolve — not about the numbers
//! it already found.

use skattjakt_core::FactKind;

/// A quantity read from a document, with the text it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedFact {
    pub kind: FactKind,
    /// Whole kronor.
    pub amount_sek: i64,
    /// 1-based page.
    pub page: u32,
    /// The line the value was read from, verbatim.
    pub source_text: String,
}

/// Label to fact mapping for K2/K3 annual accounts.
///
/// Ordered longest-first at match time so that "Resultat efter finansiella
/// poster" is never shadowed by "Resultat".
const LABELS: &[(&str, FactKind)] = &[
    // Income statement
    ("nettoomsättning", FactKind::Revenue),
    ("rörelsens intäkter", FactKind::Revenue),
    ("övriga rörelseintäkter", FactKind::OtherOperatingIncome),
    ("råvaror och förnödenheter", FactKind::ExternalCosts),
    ("övriga externa kostnader", FactKind::ExternalCosts),
    ("handelsvaror", FactKind::ExternalCosts),
    ("personalkostnader", FactKind::PersonnelCosts),
    ("av- och nedskrivningar", FactKind::Depreciation),
    (
        "avskrivningar av materiella anläggningstillgångar",
        FactKind::Depreciation,
    ),
    ("rörelseresultat", FactKind::OperatingProfit),
    ("ränteintäkter", FactKind::InterestIncome),
    ("räntekostnader", FactKind::InterestExpense),
    (
        "resultat efter finansiella poster",
        FactKind::ProfitBeforeTax,
    ),
    ("resultat före skatt", FactKind::ProfitBeforeTax),
    ("skatt på årets resultat", FactKind::TaxExpense),
    ("årets resultat", FactKind::NetProfit),
    // Balance sheet
    (
        "immateriella anläggningstillgångar",
        FactKind::IntangibleAssets,
    ),
    ("materiella anläggningstillgångar", FactKind::FixedAssets),
    (
        "inventarier, verktyg och installationer",
        FactKind::FixedAssets,
    ),
    ("varulager", FactKind::Inventory),
    ("kundfordringar", FactKind::Receivables),
    ("kassa och bank", FactKind::Cash),
    ("summa tillgångar", FactKind::TotalAssets),
    (
        "summa eget kapital och skulder",
        FactKind::TotalEquityAndLiabilities,
    ),
    ("summa eget kapital", FactKind::Equity),
    ("obeskattade reserver", FactKind::UntaxedReserves),
    ("avsättningar", FactKind::Provisions),
    ("långfristiga skulder", FactKind::LongTermLiabilities),
    ("kortfristiga skulder", FactKind::CurrentLiabilities),
    // Tax positions
    ("periodiseringsfond", FactKind::TaxAllocationReserve),
    (
        "årets avsättning till periodiseringsfond",
        FactKind::TaxAllocationReserveThisYear,
    ),
    (
        "ackumulerade överavskrivningar",
        FactKind::ExcessDepreciation,
    ),
    ("outnyttjat underskott", FactKind::CarriedForwardLoss),
    ("skattemässigt resultat", FactKind::TaxableResult),
    ("ej avdragsgilla kostnader", FactKind::NonDeductibleCosts),
    ("ej skattepliktiga intäkter", FactKind::NonTaxableIncome),
    ("koncernbidrag", FactKind::GroupContribution),
    // Payroll and VAT
    ("pensionskostnader", FactKind::PensionCosts),
    ("sociala avgifter", FactKind::PayrollTax),
    ("ingående moms", FactKind::InputVat),
    ("utgående moms", FactKind::OutputVat),
];

/// Whether the statement reports in thousands of kronor.
///
/// Getting this wrong is a thousand-fold error in the headline figure, so it is
/// detected explicitly and treated as unknown-until-seen rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    Kronor,
    Thousands,
}

impl Scale {
    fn multiplier(self) -> i64 {
        match self {
            Scale::Kronor => 1,
            Scale::Thousands => 1_000,
        }
    }
}

/// Looks for the "belopp i tkr" style note that changes what every number on
/// the page means.
pub fn detect_scale(text: &str) -> Scale {
    let lower = text.to_lowercase();
    const THOUSAND_MARKERS: [&str; 5] = [
        "belopp i tkr",
        "belopp i tusental",
        "tkr)",
        "(tkr",
        "belopp anges i tusental kronor",
    ];
    if THOUSAND_MARKERS.iter().any(|m| lower.contains(m)) {
        Scale::Thousands
    } else {
        Scale::Kronor
    }
}

/// Reads every amount on a line, in order.
///
/// Swedish statements write negatives three ways — a leading minus, a trailing
/// minus, and parentheses — and group thousands with spaces (often
/// non-breaking) or dots. All are accepted; a comma introduces the decimal part.
pub fn find_amounts(line: &str) -> Vec<i64> {
    let chars: Vec<char> = line.chars().collect();
    let mut amounts = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }

        // Walk back over an explicit sign or opening parenthesis.
        let mut negative = false;
        let mut start = i;
        let mut back = i;
        while back > 0 {
            let prev = chars[back - 1];
            // A hyphen, a Unicode minus, or an opening parenthesis all mark a
            // negative in Swedish statements.
            if matches!(prev, '-' | '\u{2212}' | '(') {
                negative = true;
                start = back - 1;
                break;
            } else if prev == ' ' || prev == '\u{a0}' {
                // A space may separate the sign from the digits, but only one.
                back -= 1;
                if back == 0 || (chars[back - 1] != '-' && chars[back - 1] != '(') {
                    break;
                }
            } else {
                break;
            }
        }

        // The integer part: a leading run of digits, then any number of
        // separated groups of *exactly three* digits.
        //
        // The three-digit rule is what keeps two adjacent year columns from
        // being read as one number: in "12 500 000 11 200 000" the run after
        // the last separator is "11", not a valid group, so the number ends
        // there. Column gaps of two or more spaces end it for the same reason.
        let mut integer = String::new();
        let mut j = i;
        while j < chars.len() && chars[j].is_ascii_digit() {
            integer.push(chars[j]);
            j += 1;
        }

        loop {
            let is_separator = matches!(chars.get(j), Some(' ' | '\u{a0}' | '\u{202f}' | '.'));
            if !is_separator {
                break;
            }
            let group: Vec<char> = chars.iter().skip(j + 1).take(3).copied().collect();
            let followed_by_digit = chars.get(j + 4).is_some_and(char::is_ascii_digit);
            if group.len() == 3 && group.iter().all(char::is_ascii_digit) && !followed_by_digit {
                integer.extend(group);
                j += 4;
            } else {
                break;
            }
        }

        // Öre, when present, follow a comma.
        let mut decimals = String::new();
        if chars.get(j) == Some(&',') && chars.get(j + 1).is_some_and(char::is_ascii_digit) {
            j += 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                decimals.push(chars[j]);
                j += 1;
            }
        }

        // A trailing minus, or a closing parenthesis, also marks a negative.
        if let Some(next) = chars.get(j) {
            if *next == '-' || *next == '\u{2212}' || (*next == ')' && chars[start] == '(') {
                negative = true;
            }
        }

        if let Ok(mut value) = integer.parse::<i64>() {
            // Round to whole kronor; öre never appear in a headline figure.
            if decimals.chars().next().is_some_and(|d| d >= '5') {
                value += 1;
            }
            amounts.push(if negative { -value } else { value });
        }

        i = j.max(i + 1);
    }

    amounts
}

/// Finds the label a line starts with, preferring the longest match.
fn match_label(line: &str) -> Option<FactKind> {
    let normalised = line.trim().to_lowercase();
    LABELS
        .iter()
        .filter(|(label, _)| normalised.starts_with(*label) || normalised.contains(*label))
        .max_by_key(|(label, _)| label.len())
        .map(|(_, kind)| kind.clone())
}

/// Extracts facts from a page's text.
///
/// Amounts are only read from lines that matched a known label, which keeps
/// page numbers, dates and organisationsnummer from being mistaken for money.
/// The first amount on the line is taken: Swedish statements put the year under
/// analysis in the first column and the comparison year in the second.
pub fn extract_from_page(page: u32, text: &str, scale: Scale) -> Vec<ExtractedFact> {
    let lines: Vec<&str> = text.lines().collect();
    let mut facts = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        let Some(kind) = match_label(line) else {
            continue;
        };

        // PDF text extraction often breaks a row so the label and the amount
        // land on consecutive lines; look ahead one line before giving up.
        let (amounts, source) = match find_amounts(line) {
            found if !found.is_empty() => (found, line.to_string()),
            _ => match lines.get(index + 1) {
                Some(next) if match_label(next).is_none() => (
                    find_amounts(next),
                    format!("{} {}", line.trim(), next.trim()),
                ),
                _ => (Vec::new(), String::new()),
            },
        };

        let Some(&first) = amounts.first() else {
            continue;
        };

        let Some(amount) = first.checked_mul(scale.multiplier()) else {
            continue;
        };

        facts.push(ExtractedFact {
            kind,
            amount_sek: amount,
            page,
            source_text: source.trim().to_string(),
        });
    }

    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_space_grouped_thousands() {
        assert_eq!(find_amounts("Nettoomsättning 12 500 000"), vec![12_500_000]);
        assert_eq!(
            find_amounts("Nettoomsättning 12\u{a0}500\u{a0}000"),
            vec![12_500_000]
        );
    }

    #[test]
    fn parses_dot_grouped_thousands() {
        assert_eq!(find_amounts("Summa 1.234.567"), vec![1_234_567]);
    }

    #[test]
    fn reads_all_three_negative_conventions() {
        assert_eq!(find_amounts("Räntekostnader -45 000"), vec![-45_000]);
        assert_eq!(find_amounts("Räntekostnader 45 000-"), vec![-45_000]);
        assert_eq!(find_amounts("Räntekostnader (45 000)"), vec![-45_000]);
    }

    #[test]
    fn rounds_ore_to_whole_kronor() {
        assert_eq!(find_amounts("Belopp 1 234,49"), vec![1_234]);
        assert_eq!(find_amounts("Belopp 1 234,50"), vec![1_235]);
    }

    #[test]
    fn reads_both_year_columns_in_order() {
        let amounts = find_amounts("Nettoomsättning    12 500 000    11 200 000");
        assert_eq!(amounts, vec![12_500_000, 11_200_000]);
    }

    #[test]
    fn adjacent_columns_do_not_merge_into_one_number() {
        // The hard case: single-spaced columns, where the grouping separator
        // and the column separator look identical. The three-digit group rule
        // is what ends the first number.
        assert_eq!(
            find_amounts("Nettoomsättning 12 500 000 11 200 000"),
            vec![12_500_000, 11_200_000]
        );
    }

    #[test]
    fn takes_the_current_year_column() {
        let facts = extract_from_page(
            2,
            "Nettoomsättning    12 500 000    11 200 000",
            Scale::Kronor,
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].amount_sek, 12_500_000);
        assert_eq!(facts[0].kind, FactKind::Revenue);
        assert_eq!(facts[0].page, 2);
    }

    #[test]
    fn longest_label_wins_so_subtotals_are_not_confused() {
        let facts = extract_from_page(
            1,
            "Resultat efter finansiella poster 850 000",
            Scale::Kronor,
        );
        assert_eq!(facts[0].kind, FactKind::ProfitBeforeTax);

        let facts = extract_from_page(1, "Rörelseresultat 900 000", Scale::Kronor);
        assert_eq!(facts[0].kind, FactKind::OperatingProfit);

        let facts = extract_from_page(1, "Årets resultat 620 000", Scale::Kronor);
        assert_eq!(facts[0].kind, FactKind::NetProfit);
    }

    #[test]
    fn unlabelled_numbers_are_ignored() {
        // Page numbers, dates and organisationsnummer must never become facts.
        let text = "Sida 3\n2025-12-31\nOrg.nr 556016-0680\n";
        assert!(extract_from_page(1, text, Scale::Kronor).is_empty());
    }

    #[test]
    fn a_label_with_no_amount_yields_nothing() {
        assert!(extract_from_page(1, "Nettoomsättning", Scale::Kronor).is_empty());
    }

    #[test]
    fn an_amount_on_the_following_line_is_still_found() {
        let text = "Nettoomsättning\n12 500 000\n";
        let facts = extract_from_page(1, text, Scale::Kronor);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].amount_sek, 12_500_000);
        assert!(facts[0].source_text.contains("Nettoomsättning"));
    }

    #[test]
    fn a_following_label_line_is_not_consumed_as_an_amount() {
        let text = "Nettoomsättning\nRörelseresultat 900 000\n";
        let facts = extract_from_page(1, text, Scale::Kronor);
        assert_eq!(facts.len(), 1, "the revenue line has no amount of its own");
        assert_eq!(facts[0].kind, FactKind::OperatingProfit);
    }

    #[test]
    fn thousands_scale_is_detected_and_applied() {
        assert_eq!(detect_scale("Belopp i tkr"), Scale::Thousands);
        assert_eq!(detect_scale("Alla belopp i kronor"), Scale::Kronor);

        let facts = extract_from_page(1, "Nettoomsättning 12 500", Scale::Thousands);
        assert_eq!(facts[0].amount_sek, 12_500_000);
    }

    #[test]
    fn source_text_is_preserved_verbatim_for_review() {
        let facts = extract_from_page(1, "  Nettoomsättning 12 500 000  ", Scale::Kronor);
        assert_eq!(facts[0].source_text, "Nettoomsättning 12 500 000");
    }

    #[test]
    fn extracts_a_realistic_income_statement() {
        let page = "\
RESULTATRÄKNING
Belopp i kr                          2025            2024
Nettoomsättning                12 500 000      11 200 000
Övriga rörelseintäkter             150 000         120 000
Råvaror och förnödenheter      -3 200 000      -2 900 000
Personalkostnader              -5 800 000      -5 400 000
Av- och nedskrivningar           -450 000        -430 000
Rörelseresultat                  3 200 000       2 590 000
Räntekostnader                     -85 000         -92 000
Resultat efter finansiella poster 3 115 000      2 498 000
Skatt på årets resultat           -641 690        -514 588
Årets resultat                   2 473 310       1 983 412
";
        let facts = extract_from_page(1, page, Scale::Kronor);
        let get = |kind: FactKind| facts.iter().find(|f| f.kind == kind).map(|f| f.amount_sek);

        assert_eq!(get(FactKind::Revenue), Some(12_500_000));
        assert_eq!(get(FactKind::PersonnelCosts), Some(-5_800_000));
        assert_eq!(get(FactKind::OperatingProfit), Some(3_200_000));
        assert_eq!(get(FactKind::InterestExpense), Some(-85_000));
        assert_eq!(get(FactKind::ProfitBeforeTax), Some(3_115_000));
        assert_eq!(get(FactKind::NetProfit), Some(2_473_310));
        // The column header row must not have produced a fact.
        assert!(!facts.iter().any(|f| f.amount_sek == 2025));
    }

    #[test]
    fn extracts_a_realistic_balance_sheet() {
        let page = "\
BALANSRÄKNING
Materiella anläggningstillgångar   1 800 000
Varulager                            420 000
Kundfordringar                     2 100 000
Kassa och bank                     3 400 000
Summa tillgångar                   7 720 000
Summa eget kapital                 4 100 000
Obeskattade reserver               1 250 000
Kortfristiga skulder               2 370 000
Summa eget kapital och skulder     7 720 000
";
        let facts = extract_from_page(3, page, Scale::Kronor);
        let get = |kind: FactKind| facts.iter().find(|f| f.kind == kind).map(|f| f.amount_sek);

        assert_eq!(get(FactKind::TotalAssets), Some(7_720_000));
        assert_eq!(get(FactKind::TotalEquityAndLiabilities), Some(7_720_000));
        assert_eq!(get(FactKind::UntaxedReserves), Some(1_250_000));
        assert_eq!(get(FactKind::Equity), Some(4_100_000));
    }
}

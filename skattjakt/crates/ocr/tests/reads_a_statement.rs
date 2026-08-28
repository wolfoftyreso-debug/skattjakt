//! The engine against a real image, when the models are present.
//!
//! Skipped rather than failed when they are not, and the skip is printed:
//! a test that quietly passes because it did nothing is worse than one that
//! is not there. CI sets SKATTJAKT_OCR_MODELS, so this runs where it counts.

use skattjakt_ocr::{Models, Reader, Sign};

fn reader() -> Option<Reader> {
    // In CI the models are always there, so their absence is a broken build
    // rather than a reason to skip. Locally a skip is fine — but it is said
    // out loud, because a test that passes having done nothing is the most
    // expensive kind.
    let in_ci = std::env::var_os("CI").is_some();
    let Some(models) = Models::from_env() else {
        assert!(
            !in_ci,
            "SKATTJAKT_OCR_MODELS is unset in CI: this test read nothing"
        );
        eprintln!("SKIP: SKATTJAKT_OCR_MODELS is unset; the engine was not exercised");
        return None;
    };
    if !models.present() {
        assert!(
            !in_ci,
            "no OCR models at {} in CI: this test read nothing",
            models.detection.display()
        );
        eprintln!("SKIP: no models at {}", models.detection.display());
        return None;
    }
    Some(Reader::load(&models).expect("the models are present but would not load"))
}

fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/resultatrakning.png"
    ))
    .expect("fixture missing")
}

#[test]
fn a_scanned_statement_becomes_labelled_rows() {
    let Some(reader) = reader() else { return };
    let rows = reader.read_page(&fixture()).expect("reading failed");

    let labelled: Vec<_> = rows.iter().filter(|r| r.amount.is_some()).collect();
    assert!(
        labelled.len() >= 6,
        "expected the statement's rows, got {}: {rows:#?}",
        labelled.len()
    );

    // The pairing is the whole point: a figure with no label beside it is
    // exactly what the engine returns on its own, and is unusable.
    for row in &labelled {
        assert!(
            !row.label.trim().is_empty(),
            "a figure came back with no label: {row:?}"
        );
        assert!(
            !row.label.chars().any(|c| c.is_ascii_digit()),
            "the figure leaked into the label: {row:?}"
        );
    }

    let net = labelled
        .iter()
        .find(|r| r.label.to_lowercase().starts_with("nettoomsat"))
        .expect("no turnover row");
    assert_eq!(net.amount.as_ref().unwrap().digits, "12450000");
}

/// A figure the recogniser read without a sign must never come back as a
/// positive one. This is the failure that turns a cost into income.
#[test]
fn nothing_is_reported_as_positive() {
    let Some(reader) = reader() else { return };
    let rows = reader.read_page(&fixture()).expect("reading failed");
    for row in rows.iter().filter_map(|r| r.amount.as_ref()) {
        assert!(
            matches!(row.sign, Sign::Negative | Sign::Unsigned),
            "a third sign appeared"
        );
    }
}

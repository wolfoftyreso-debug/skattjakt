use skattjakt_store::notifications::NotificationKind;

use crate::render;

/// The single rule this crate exists to enforce.
#[test]
fn no_rendered_message_can_carry_an_amount() {
    // Structural, not a review convention: the rendering functions take a kind
    // and an identifier. There is no parameter an amount could arrive through.
    for kind in every_kind() {
        let email = render::email(kind, Some(uuid::Uuid::new_v4()));
        let push = render::push(kind, Some(uuid::Uuid::new_v4()));

        for text in [&email.subject, &email.body, &push.title, &push.body] {
            // A UUID contains digits; a currency amount contains a currency.
            assert!(!text.contains("kr"), "{kind:?} mentions kronor: {text}");
            assert!(
                !text.to_lowercase().contains("belopp"),
                "{kind:?} mentions an amount: {text}"
            );
        }
    }
}

#[test]
fn a_push_body_fits_on_a_lock_screen() {
    // A truncated notification is one the customer has to open the app to
    // understand, which defeats the point of sending it.
    for kind in every_kind() {
        let push = render::push(kind, None);
        assert!(
            push.body.chars().count() <= 60,
            "{kind:?} push body is {} characters: {}",
            push.body.chars().count(),
            push.body
        );
    }
}

#[test]
fn every_kind_renders_both_ways() {
    for kind in every_kind() {
        let email = render::email(kind, None);
        assert!(!email.subject.is_empty(), "{kind:?} has no subject");
        assert!(email.body.len() > 40, "{kind:?} has an empty-looking body");

        let push = render::push(kind, None);
        assert!(!push.title.is_empty());
        assert!(!push.body.is_empty());
    }
}

#[test]
fn every_email_carries_the_disclaimer() {
    // The same words as the in-product disclaimer. A customer who reads it in
    // two places must read the same thing.
    for kind in every_kind() {
        let email = render::email(kind, None);
        assert!(
            email.body.contains("preliminära"),
            "{kind:?} omits the disclaimer"
        );
    }
}

#[test]
fn a_deep_link_points_at_the_thing_that_happened() {
    let id = uuid::Uuid::new_v4();
    let push = render::push(NotificationKind::AnalysisCompleted, Some(id));
    assert_eq!(push.deep_link, Some(format!("skattjakt://analyses/{id}")));

    // A security alert sends the customer where they can act, not to the thing
    // that triggered it.
    let alert = render::push(NotificationKind::SecurityAlert, Some(id));
    assert_eq!(
        alert.deep_link,
        Some("skattjakt://settings/devices".to_string())
    );
}

#[test]
fn the_failure_message_tells_the_customer_what_to_try() {
    // The commonest cause is a scanned PDF, and saying so turns a dead end into
    // something the customer can fix themselves.
    let email = render::email(NotificationKind::AnalysisFailed, None);
    assert!(email.body.contains("inskannat"));
    assert!(email.body.to_lowercase().contains("pdf"));
}

#[test]
fn a_security_alert_says_what_to_do_if_it_was_not_you() {
    let email = render::email(NotificationKind::SecurityAlert, None);
    assert!(email.body.contains("byt lösenord"));
}

fn every_kind() -> [NotificationKind; 5] {
    [
        NotificationKind::AnalysisCompleted,
        NotificationKind::AnalysisFailed,
        NotificationKind::DocumentProcessed,
        NotificationKind::MemberInvited,
        NotificationKind::SecurityAlert,
    ]
}

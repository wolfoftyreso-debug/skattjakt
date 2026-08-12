//! Turning a notification kind into words.
//!
//! Swedish, because the product is Swedish. The strings live here rather than
//! in a catalogue for the reason stated in `SKATTJAKT_CLIENT_ARCHITECTURE.md`
//! §9: there is one market, and a catalogue would be machinery for a second one
//! that does not exist. What is kept from becoming a rewrite is that they are
//! all in one module behind one function.
//!
//! **Nothing here takes an amount.** The signatures do not admit one, which is
//! what makes "a notification never carries what was found" a property of the
//! code rather than a rule someone has to remember.

use skattjakt_store::notifications::NotificationKind;

/// An email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub subject: String,
    /// Plain text. No HTML, and that is deliberate: an HTML mail is a mail with
    /// remote images in it, and a remote image in a mail about someone's
    /// accounts is a tracking pixel that tells a third party when a Swedish
    /// business opened a message about its tax position.
    pub body: String,
}

/// A push notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPush {
    pub title: String,
    pub body: String,
    /// What the app should open. An identifier, never a value.
    pub deep_link: Option<String>,
}

/// The sender address a mail comes from.
pub const FROM_ADDRESS: &str = "Skattjakt <ingen-svar@skattjakt.se>";

/// The line every mail ends with.
///
/// The same words as the in-product disclaimer, for the same reason it exists
/// in one constant: a customer who reads it in two places must read the same
/// thing.
const FOOTER: &str = "\n\n—\nSkattjakt är ett analys- och upptäcktsverktyg. Resultaten är \
                      preliminära och ska inte betraktas som juridisk rådgivning eller \
                      skattebesked. Verifiera mot aktuella regler innan någon åtgärd vidtas.\n\n\
                      Du får det här mejlet för att du har ett konto hos Skattjakt. \
                      Inställningar för notiser finns i tjänsten.";

/// Renders an email for one notification kind.
///
/// Takes no amount and no company name — see the module note. `subject_id` is
/// an identifier the customer can use to find the thing in the product.
pub fn email(kind: NotificationKind, subject_id: Option<uuid::Uuid>) -> Rendered {
    let (subject, lead) = match kind {
        NotificationKind::AnalysisCompleted => (
            "Din analys är klar",
            "Analysen av ditt bokslut är färdig. Logga in för att se vad som hittades \
             och vilket underlag varje punkt vilar på.",
        ),
        NotificationKind::AnalysisFailed => (
            "Analysen kunde inte slutföras",
            "Vi kunde inte slutföra analysen av ditt underlag. Det vanligaste skälet är \
             att dokumentet är inskannat och saknar läsbar text — en textbaserad PDF \
             brukar lösa det. Logga in för att se vad som gick fel.",
        ),
        NotificationKind::DocumentProcessed => (
            "Ditt underlag är inläst",
            "Vi har läst in ditt uppladdade underlag och det är klart att analysera.",
        ),
        NotificationKind::MemberInvited => (
            "Du har fått tillgång till ett bolag i Skattjakt",
            "Någon har gett dig tillgång till ett bolags underlag i Skattjakt. \
             Logga in för att se vilket.",
        ),
        NotificationKind::SecurityAlert => (
            "Säkerhetshändelse på ditt konto",
            "Något som rör säkerheten på ditt konto har inträffat — till exempel en \
             inloggning från en ny enhet. Logga in och kontrollera dina enheter. \
             Var det inte du, byt lösenord direkt.",
        ),
    };

    let reference = subject_id
        .map(|id| format!("\n\nReferens: {id}"))
        .unwrap_or_default();

    Rendered {
        subject: subject.to_string(),
        body: format!("{lead}{reference}{FOOTER}"),
    }
}

/// Renders a push notification.
///
/// Shorter than the email, and shorter than a lock screen will truncate — a
/// truncated notification is one the customer has to open the app to
/// understand, which defeats the point of sending it.
pub fn push(kind: NotificationKind, subject_id: Option<uuid::Uuid>) -> RenderedPush {
    let (title, body) = match kind {
        NotificationKind::AnalysisCompleted => ("Skattjakt", "Din analys är klar."),
        NotificationKind::AnalysisFailed => {
            ("Skattjakt", "Analysen kunde inte slutföras. Öppna för mer.")
        }
        NotificationKind::DocumentProcessed => ("Skattjakt", "Ditt underlag är inläst."),
        NotificationKind::MemberInvited => ("Skattjakt", "Du har fått tillgång till ett bolag."),
        NotificationKind::SecurityAlert => {
            ("Skattjakt", "Säkerhetshändelse på ditt konto. Öppna appen.")
        }
    };

    RenderedPush {
        title: title.to_string(),
        body: body.to_string(),
        deep_link: subject_id.map(|id| match kind {
            NotificationKind::AnalysisCompleted | NotificationKind::AnalysisFailed => {
                format!("skattjakt://analyses/{id}")
            }
            NotificationKind::DocumentProcessed => format!("skattjakt://documents/{id}"),
            NotificationKind::MemberInvited => "skattjakt://companies".to_string(),
            NotificationKind::SecurityAlert => "skattjakt://settings/devices".to_string(),
        }),
    }
}

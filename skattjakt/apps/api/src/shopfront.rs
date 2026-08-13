//! The six things a Swedish payment scheme requires a webshop to publish.
//!
//! Why this is code and not a page somebody wrote once
//! ===================================================
//!
//! Applying for Swish Handel means ticking six boxes: prices, what is being
//! sold, terms of purchase, contact details, a returns policy, and returns
//! information. The bank checks. More to the point, the tick is the merchant's
//! signature — an attestation that these exist.
//!
//! Three of the six are facts nobody in this repository knows: the company's
//! registered name, its organisationsnummer, its address. Inventing them would
//! be the worst possible failure here, because the pages would look complete
//! and be false in exactly the way an attestation must not be.
//!
//! So the merchant details come from configuration, and a deployment that takes
//! payments without them **refuses to start**. There is no placeholder to
//! forget to replace.
//!
//! What is deliberately not claimed
//! ================================
//!
//! The terms below are a serious draft grounded in the statutes cited in the
//! source registry — not legal advice, and not reviewed by a lawyer. That is
//! stated on the page itself rather than only here, because the person who
//! needs to know is the one reading it.

use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use skattjakt_core::Money;
use skattjakt_payments::Product;

use crate::AppState;

/// Standard Swedish VAT. Digital services at the standard rate.
const VAT_RATE_BP: i64 = 2_500;

/// Who is selling, in the sense a customer and a bank both need.
///
/// Every field is required. `Option` on any of them would be a page that
/// renders with a gap, and a gap on a contact page is the specific failure
/// this type exists to prevent.
#[derive(Debug, Clone)]
pub struct Merchant {
    pub name: String,
    pub org_number: String,
    pub address: String,
    pub email: String,
    pub phone: Option<String>,
    /// Whether the business is registered for VAT. Below the registration
    /// threshold a business must **not** state VAT on a price, so this changes
    /// what the price page is allowed to say.
    pub vat_registered: bool,
}

impl Merchant {
    /// Reads the merchant from the environment, or says exactly what is
    /// missing.
    pub fn from_env() -> Result<Option<Self>, String> {
        let Ok(name) = std::env::var("SKATTJAKT_MERCHANT_NAME") else {
            return Ok(None);
        };
        if name.trim().is_empty() {
            return Ok(None);
        }

        let required = |key: &str| -> Result<String, String> {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    format!(
                        "{key} is required once SKATTJAKT_MERCHANT_NAME is set: the shop \
                             pages a payment scheme asks for cannot be published with a gap"
                    )
                })
        };

        Ok(Some(Self {
            name: name.trim().to_string(),
            org_number: required("SKATTJAKT_MERCHANT_ORG_NUMBER")?,
            address: required("SKATTJAKT_MERCHANT_ADDRESS")?,
            email: required("SKATTJAKT_MERCHANT_EMAIL")?,
            phone: std::env::var("SKATTJAKT_MERCHANT_PHONE")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            vat_registered: std::env::var("SKATTJAKT_MERCHANT_VAT_REGISTERED")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        }))
    }
}

/// The VAT inside a price that includes it.
///
/// Rounded half away from zero on the öre, because a customer's receipt has to
/// add up to the price they paid and a truncation would leave it one öre short.
pub fn vat_portion(gross: Money) -> Money {
    let gross = gross.ore();
    // gross = net * (1 + rate); vat = gross * rate / (10_000 + rate)
    let numerator = gross * VAT_RATE_BP;
    let denominator = 10_000 + VAT_RATE_BP;
    Money::from_ore((numerator + denominator / 2) / denominator)
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="sv">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — Skattjakt</title>
<link rel="stylesheet" href="/ui/app.css">
<link rel="stylesheet" href="/ui/index.css">
</head>
<body>
<main class="page">
<p class="meta"><a href="/">← Till Skattjakt</a></p>
<h1>{title}</h1>
{body}
<hr>
<nav class="meta">
  <a href="/tjanster">Tjänster</a> ·
  <a href="/priser">Priser</a> ·
  <a href="/villkor">Köpvillkor</a> ·
  <a href="/angerratt">Ångerrätt och återbetalning</a> ·
  <a href="/kontakt">Kontakt</a>
</nav>
</main>
</body>
</html>"#
    )
}

/// The page shown when the merchant has not been configured.
///
/// Deliberately unhelpful to a customer and very clear to an operator. The
/// alternative — rendering the page with the fields blank — is a page that
/// looks published and attests to nothing.
fn unconfigured(what: &str) -> String {
    page(
        what,
        "<div class=\"notice\"><strong>Den här sidan är inte konfigurerad.</strong> \
         Säljarens uppgifter saknas i den här installationen, och sidan visas hellre \
         tom än med platshållare.</div>",
    )
}

/// The merchant, or the page to send in its place.
///
/// The error side is the rendered page rather than a whole `Response`, which
/// keeps it small enough not to bloat every caller's return value, and borrows
/// rather than clones: nothing here needs to own the details, it only prints
/// them.
fn merchant_or_page<'a>(state: &'a AppState, what: &str) -> Result<&'a Merchant, Html<String>> {
    match &state.merchant {
        Some(merchant) => Ok(merchant),
        None => Err(Html(unconfigured(what))),
    }
}

/// A row for a product this build cannot deliver.
///
/// Shown rather than hidden. A price list that silently omits a service the
/// site describes elsewhere reads as an oversight; one that says the service is
/// not open yet is the truth, and it is the truth the customer needs before
/// they look for a way to pay for it.
fn unavailable_row(product: Product) -> String {
    format!(
        "<tr><td>{}</td><td colspan=\"3\">Inte öppen för köp ännu — \
         den här tjänsten har inget regelverk i den här versionen</td></tr>",
        escape(product_title(product))
    )
}

fn price_row(product: Product, vat_registered: bool) -> String {
    let gross = product.price();
    let vat = vat_portion(gross);
    let net = Money::from_ore(gross.ore() - vat.ore());
    let vat_cell = if vat_registered {
        format!("<td>{net}</td><td>{vat}</td>")
    } else {
        "<td colspan=\"2\">Säljaren är inte momsregistrerad</td>".to_string()
    };
    format!(
        "<tr><td>{}</td><td><strong>{gross}</strong></td>{vat_cell}</tr>",
        escape(product_title(product))
    )
}

pub(crate) fn product_title(product: Product) -> &'static str {
    match product {
        Product::PrivateAnalysis => "Privatanalys",
        Product::CompanyAnalysis => "Bolagsanalys",
        Product::ControlReview => "Skattjakt Kontroll",
    }
}

pub(crate) fn product_description(product: Product) -> &'static str {
    match product {
        Product::PrivateAnalysis => {
            "För dig som privatperson. Du laddar upp ditt eget underlag — \
             deklarationsunderlag, kontrolluppgifter, årsbesked — och får en genomgång \
             av vad som kan vara värt att kontrollera."
        }
        Product::CompanyAnalysis => {
            "För aktiebolag. Du laddar upp årsredovisning eller preliminärt bokslut och \
             får en genomgång av möjliga avdrag, periodiseringar och poster som bör \
             kontrolleras innan deklarationen lämnas in."
        }
        Product::ControlReview => {
            "För redovisningsbyrån. Samma analys, presenterad som en kontroll av ett \
             färdigt bokslut: vad som måste kontrolleras före inlämning, vad som kan \
             förbättras, vad som prövats och ser korrekt ut, och vad som är värt att ta \
             upp med kunden."
        }
    }
}

// ---------------------------------------------------------------------------
// The six pages
// ---------------------------------------------------------------------------

/// Prisuppgifter.
pub async fn prices(State(state): State<AppState>) -> Response {
    let merchant = match merchant_or_page(&state, "Priser") {
        Ok(merchant) => merchant,
        Err(page) => return page.into_response(),
    };

    let rows: String = Product::ALL
        .iter()
        .map(|p| {
            if state.engine.set().covers_audience(p.audience_key()) {
                price_row(*p, merchant.vat_registered)
            } else {
                unavailable_row(*p)
            }
        })
        .collect();

    let headers = if merchant.vat_registered {
        "<th>Tjänst</th><th>Pris</th><th>Varav exkl. moms</th><th>Varav moms 25 %</th>"
    } else {
        "<th>Tjänst</th><th>Pris</th><th colspan=\"2\">Moms</th>"
    };

    let vat_note = if merchant.vat_registered {
        "<p>Alla priser anges inklusive moms, vilket är det belopp du betalar. \
         Momssatsen är 25 procent.</p>"
    } else {
        "<p>Alla priser anges inklusive eventuell moms, vilket är det belopp du betalar. \
         Säljaren är inte momsregistrerad och tar därför inte ut moms.</p>"
    };

    Html(page(
        "Priser",
        &format!(
            "{vat_note}
<table>
<thead><tr>{headers}</tr></thead>
<tbody>{rows}</tbody>
</table>
<p>Priset gäller per analys. Det finns ingen prenumeration, ingen bindningstid och \
ingen återkommande debitering — du betalar en gång, för en analys.</p>
<p>Betalning sker med Swish. Analysen startar när betalningen är bekräftad.</p>
<p class=\"meta\">Priserna är de som gällde när sidan hämtades och är desamma som \
debiteras i kassan. Ändras ett pris påverkar det inte en order som redan skapats.</p>"
        ),
    ))
    .into_response()
}

/// Information om produkter och tjänster.
pub async fn services(State(state): State<AppState>) -> Response {
    let merchant = match merchant_or_page(&state, "Tjänster") {
        Ok(merchant) => merchant,
        Err(page) => return page.into_response(),
    };

    let items: String = Product::ALL
        .iter()
        .map(|p| {
            // A service description that does not say the service is closed is
            // an advertisement for something nobody can buy.
            let availability = if state.engine.set().covers_audience(p.audience_key()) {
                format!("<p class=\"meta\">Pris: {}</p>", p.price())
            } else {
                "<p class=\"notice\"><strong>Inte öppen för köp ännu.</strong> \
                 Skattjakt har ännu inget regelverk för den här sortens underlag, \
                 så en analys skulle inte hitta något — och det går inte att skilja \
                 från att det inte finns något att hitta. Tjänsten säljs inte förrän \
                 den kan svara på något.</p>"
                    .to_string()
            };
            format!(
                "<h2>{}</h2><p>{}</p>{availability}",
                escape(product_title(*p)),
                escape(product_description(*p)),
            )
        })
        .collect();

    Html(page(
        "Tjänster",
        &format!(
            "<p>Skattjakt är ett analys- och upptäcktsverktyg. Du laddar upp ditt underlag, \
och verktyget går igenom det mot ett regelverk och visar vad varje fynd bygger på.</p>
{items}
<h2>Vad du får</h2>
<ul>
<li>En prioriterad lista över vad som är värt att titta på först.</li>
<li>För varje fynd: vad det bygger på i ditt underlag, vilken regel som tillämpats, \
vilken lagkälla regeln vilar på, och vad som saknas för att kunna avgöra saken.</li>
<li>En rapport i nio avsnitt som går att spara och ta med till din redovisningskonsult.</li>
</ul>
<h2>Vad du inte får</h2>
<p>Skattjakt lämnar inte skatterådgivning, gör ingen revision och lämnar inget besked \
om vad som är rätt enligt lag. Beloppen anges som intervall, aldrig som en enskild \
siffra, och varje fynd ska stämmas av mot fullständigt underlag innan du agerar på det.</p>
<div class=\"notice\"><strong>Regelverket är ännu inte granskat av en kvalificerad \
rådgivare, och ingen av lagkällorna har hämtats och kontrollerats maskinellt.</strong> \
Inget fynd visas därför som fastställt.</div>
<h2>Så levereras tjänsten</h2>
<p>Analysen startar direkt när betalningen är bekräftad och tar normalt några minuter. \
Resultatet visas i webbläsaren och går att ladda ner. Ingen fysisk vara skickas.</p>
<p class=\"meta\">Säljare: {} (org.nr {}).</p>",
            escape(&merchant.name),
            escape(&merchant.org_number)
        ),
    ))
    .into_response()
}

/// Köpavtal.
pub async fn terms(State(state): State<AppState>) -> Response {
    let merchant = match merchant_or_page(&state, "Köpvillkor") {
        Ok(merchant) => merchant,
        Err(page) => return page.into_response(),
    };

    Html(page(
        "Köpvillkor",
        &format!(
            "<p class=\"meta\">Dessa villkor gäller mellan {} (org.nr {}), nedan Säljaren, \
och dig som köper en analys.</p>

<h2>1. Vad avtalet omfattar</h2>
<p>Avtalet omfattar en (1) analys av det underlag du laddar upp, av det slag du valt i \
kassan. Tjänsten är digital och levereras direkt.</p>

<h2>2. Pris och betalning</h2>
<p>Priset framgår i kassan och på <a href=\"/priser\">prissidan</a>, angivet i svenska \
kronor inklusive eventuell moms. Betalning sker med Swish. Analysen startar när \
betalningen är bekräftad av betaltjänsten.</p>
<p>Ett pris som ändras påverkar inte en order som redan skapats.</p>

<h2>3. Leverans</h2>
<p>Analysen påbörjas omedelbart efter bekräftad betalning och är normalt klar inom några \
minuter. Resultatet visas i webbläsaren. Om analysen inte kan slutföras får du besked \
om varför, och du har rätt till återbetalning enligt \
<a href=\"/angerratt\">ångerrätt och återbetalning</a>.</p>

<h2>4. Ångerrätt</h2>
<p>Vid distansköp har du som konsument normalt fjorton dagars ångerrätt. För digitalt \
innehåll som levereras omedelbart upphör ångerrätten när leveransen påbörjats, förutsatt \
att du uttryckligen samtyckt till det och bekräftat att du förlorar ångerrätten.</p>
<p><strong>Du lämnar det samtycket i kassan.</strong> Gör du inte det startar analysen \
inte förrän ångerfristen löpt ut. Se <a href=\"/angerratt\">ångerrätt och \
återbetalning</a>.</p>

<h2>5. Vad tjänsten inte är</h2>
<p>Skattjakt är ett analys- och upptäcktsverktyg. Resultaten är preliminära och utgör \
inte juridisk rådgivning, revisionsuttalande, skattebesked eller garanti om \
skatteåterbäring eller besparing. Säljaren ansvarar inte för beslut du fattar på \
resultatet utan att först stämma av det mot fullständigt underlag.</p>

<h2>6. Ditt underlag</h2>
<p>Du behåller alla rättigheter till det du laddar upp. Säljaren använder underlaget för \
att utföra analysen och för inget annat, och säljer eller lämnar det inte vidare. \
Underlaget raderas enligt gällande gallringsregler.</p>

<h2>7. Ansvarsbegränsning</h2>
<p>Säljarens ansvar är begränsat till det belopp du betalat för analysen, utom vid \
uppsåt eller grov vårdslöshet. Begränsningen inskränker inte tvingande konsumenträtt.</p>

<h2>8. Reklamation och tvist</h2>
<p>Kontakta oss först: <a href=\"mailto:{}\">{}</a>. Kommer vi inte överens kan du som \
konsument vända dig till Allmänna reklamationsnämnden (ARN) eller till EU-kommissionens \
plattform för tvistlösning online.</p>

<h2>9. Tillämplig lag</h2>
<p>Svensk lag tillämpas på avtalet.</p>

<div class=\"notice\"><strong>Dessa villkor är ett utkast som inte granskats av jurist.</strong> \
De är skrivna mot distansavtalslagen (2005:59), prisinformationslagen (2004:347) och \
lagen om elektronisk handel (2002:562), men bör läsas igenom av någon med \
konsumenträttslig kompetens innan de används skarpt.</div>",
            escape(&merchant.name),
            escape(&merchant.org_number),
            escape(&merchant.email),
            escape(&merchant.email),
        ),
    ))
    .into_response()
}

/// Kontaktuppgifter.
pub async fn contact(State(state): State<AppState>) -> Response {
    let merchant = match merchant_or_page(&state, "Kontakt") {
        Ok(merchant) => merchant,
        Err(page) => return page.into_response(),
    };

    let phone = match &merchant.phone {
        Some(phone) => format!("<dt>Telefon</dt><dd>{}</dd>", escape(phone)),
        None => String::new(),
    };

    Html(page(
        "Kontakt",
        &format!(
            "<dl>
<dt>Företag</dt><dd>{}</dd>
<dt>Organisationsnummer</dt><dd>{}</dd>
<dt>Adress</dt><dd>{}</dd>
<dt>E-post</dt><dd><a href=\"mailto:{}\">{}</a></dd>
{phone}
</dl>
<p>Vi svarar på e-post inom rimlig tid på vardagar. Gäller det en betalning, ange \
ordernumret du fick i kassan — det gör det möjligt att hitta ärendet direkt.</p>",
            escape(&merchant.name),
            escape(&merchant.org_number),
            escape(&merchant.address),
            escape(&merchant.email),
            escape(&merchant.email),
        ),
    ))
    .into_response()
}

/// Information om returpolicy och returer.
///
/// One page for both of the bank's boxes, because for a digital service they
/// are one subject: there is nothing to send back, so the returns policy *is*
/// the cancellation and refund policy. Two pages saying the same thing would be
/// two pages to keep in step.
pub async fn returns(State(state): State<AppState>) -> Response {
    let merchant = match merchant_or_page(&state, "Ångerrätt och återbetalning") {
        Ok(merchant) => merchant,
        Err(page) => return page.into_response(),
    };

    Html(page(
        "Ångerrätt och återbetalning",
        &format!(
            "<p>Skattjakt säljer en digital tjänst. Det finns ingen vara att skicka \
tillbaka, så det som motsvarar en returpolicy är rätten att ångra köpet och rätten till \
återbetalning.</p>

<h2>Ångerrätt</h2>
<p>Som konsument har du normalt fjorton dagars ångerrätt vid distansköp. För digitalt \
innehåll som levereras omedelbart upphör ångerrätten när leveransen påbörjats, om du \
uttryckligen samtyckt till att den påbörjas och bekräftat att du därmed förlorar \
ångerrätten.</p>
<p>I kassan väljer du själv, och valet spelas in mot köpet:</p>
<ul>
<li><strong>Starta analysen direkt.</strong> Du samtycker till omedelbar leverans och \
förlorar ångerrätten när analysen påbörjas.</li>
<li><strong>Vänta ut ångerfristen.</strong> Analysen startar efter fjorton dagar, och \
fram till dess kan du ångra köpet och få hela beloppet tillbaka.</li>
</ul>
<p>Väljer du att starta direkt är det den här meningen du bekräftar, ordagrant, och den \
sparas mot ditt köp tillsammans med tidpunkten:</p>
<blockquote class=\"notice\">{consent}</blockquote>
<p class=\"meta\">Ordalydelse version {consent_version}. Ändras den får senare köp en ny \
version; ditt köp behåller den du faktiskt fick se.</p>

<h2>När du får pengarna tillbaka ändå</h2>
<p>Ångerrätten är ett golv, inte ett tak. Du får pengarna tillbaka om:</p>
<ul>
<li>analysen inte kunde slutföras;</li>
<li>underlaget inte gick att läsa och ingen analys kunde göras;</li>
<li>du debiterats mer än en gång för samma analys;</li>
<li>tjänsten inte fungerade som beskrivet.</li>
</ul>
<p>Det sista är inte detsamma som att analysen inte hittade något. En analys som gått \
igenom ditt underlag och inte funnit något att flagga har utfört det den skulle — och \
säger det rakt ut i stället för att hitta på fynd.</p>

<h2>Så begär du återbetalning</h2>
<p>Skicka ett e-postmeddelande till <a href=\"mailto:{}\">{}</a> med ordernumret du fick \
i kassan. Vi återkommer inom rimlig tid och betalar tillbaka till samma Swish-nummer som \
betalningen kom från.</p>

<h2>Om du inte är nöjd med svaret</h2>
<p>Du kan vända dig till Allmänna reklamationsnämnden (ARN), Box 174, 101 23 Stockholm, \
eller till EU-kommissionens plattform för tvistlösning online. Vi följer ARN:s \
rekommendationer.</p>

<p class=\"meta\">Säljare: {} (org.nr {}).</p>",
            escape(&merchant.email),
            escape(&merchant.email),
            escape(&merchant.name),
            escape(&merchant.org_number),
            // Rendered from the constant the order records, not retyped. A page
            // that phrased it differently would make every stored consent a
            // claim about words the buyer never saw.
            consent = escape(skattjakt_payments::CONSENT_WORDING),
            consent_version = escape(skattjakt_payments::CONSENT_WORDING_VERSION),
        ),
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vat_inside_a_price_adds_back_up_to_the_price() {
        // A receipt that does not add up to what the customer paid is a receipt
        // that will be queried, and the query lands on a person.
        for ore in [2_900, 6_900, 1, 99, 12_345, 1_000_000] {
            let gross = Money::from_ore(ore);
            let vat = vat_portion(gross);
            let net = ore - vat.ore();
            assert_eq!(net + vat.ore(), ore, "{ore} öre does not reconcile");
            assert!(vat.ore() >= 0 && vat.ore() < ore.max(1));
        }
    }

    #[test]
    fn the_known_prices_carry_the_vat_a_person_would_compute_by_hand() {
        // 69 kr including 25 % VAT is 55,20 net and 13,80 VAT.
        assert_eq!(vat_portion(Money::from_ore(6_900)).ore(), 1_380);
        // 29 kr is 23,20 and 5,80.
        assert_eq!(vat_portion(Money::from_ore(2_900)).ore(), 580);
    }

    #[test]
    fn every_product_is_described_and_titled() {
        // A price list with a blank row is the specific failure the bank's
        // checkbox is asking about.
        for product in Product::ALL {
            assert!(!product_title(product).trim().is_empty());
            assert!(
                product_description(product).chars().count() > 80,
                "{} has no real description",
                product.as_str()
            );
        }
    }

    #[test]
    fn markup_in_a_merchant_name_cannot_reach_the_page() {
        assert_eq!(
            escape("<script>x</script>"),
            "&lt;script&gt;x&lt;/script&gt;"
        );
        assert_eq!(escape("Ström & Co \"AB\""), "Ström &amp; Co &quot;AB&quot;");
    }

    #[test]
    fn a_merchant_is_all_or_nothing() {
        // The failure this prevents: a contact page that renders with the
        // address missing, published, and attested to.
        let _ = Merchant {
            name: "x".into(),
            org_number: "x".into(),
            address: "x".into(),
            email: "x".into(),
            phone: None,
            vat_registered: true,
        };
        // Only `phone` is optional; the type has no other `Option`, which is
        // what makes a gap impossible rather than unlikely.
    }
}

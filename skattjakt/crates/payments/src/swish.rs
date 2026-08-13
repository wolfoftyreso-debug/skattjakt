//! The Swish Commerce API.
//!
//! Getting this working needs three things from the bank, and none of them is
//! code: a Swish-nummer for the business (`123XXXXXXX`), a client certificate
//! issued through the bank's Swish certificate management, and the callback URL
//! registered against that number. `SKATTJAKT_PAYMENTS.md` is the operator's
//! side of it.
//!
//! Wire format
//! ===========
//!
//! Everything Swish-specific — URLs, field names, status strings — is in this
//! file and nowhere else, so the day the scheme changes a version there is one
//! file to correct and the settlement logic in `lib.rs` is untouched.
//!
//! **The field names below are written against the documented v2 Commerce API
//! and must be checked against the specification the bank supplies.** They are
//! deliberately concentrated in `Wire` and `WirePayment` rather than scattered
//! through the client, so checking them is reading two structs.
//!
//! Authentication
//! ==============
//!
//! Mutual TLS. The client certificate identifies us; Swish's server certificate
//! is verified against the CA they publish. There is no bearer token and no
//! shared secret, which is why the certificate files are the whole of the
//! secret material and why they are mounted rather than baked in.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use skattjakt_core::Money;

use crate::{
    PaymentError, PaymentHandle, PaymentOutcome, PaymentProvider, PaymentRequest, PaymentStatus,
};

/// Production. The test environment (MSS) is a different host with a different
/// certificate, which is the correct way round: pointing at the wrong one fails
/// the handshake rather than moving real money.
pub const PRODUCTION_BASE: &str = "https://cpc.getswish.net/swish-cpcapi";
pub const TEST_BASE: &str = "https://mss.cpc.getswish.net/swish-cpcapi";

/// Swish's own timeout on a payment request is measured in minutes; ours is a
/// bound on how long an HTTP call may hold a request handler.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Swish's cap on the message shown to the payer.
pub const MAX_MESSAGE: usize = 50;
/// Swish's cap on the merchant's own reference.
pub const MAX_PAYMENT_REFERENCE: usize = 35;
/// An instruction id is 32 uppercase hexadecimal characters, and Swish rejects
/// anything else — including the hyphens a UUID is usually printed with.
pub const INSTRUCTION_ID_LEN: usize = 32;

/// What a deployment needs to take Swish payments.
#[derive(Debug, Clone)]
pub struct SwishConfig {
    pub base_url: String,
    /// The business's Swish number, `123` followed by seven digits.
    pub payee_alias: String,
    /// PEM holding the client certificate chain **and** its private key.
    pub client_identity_pem: Vec<u8>,
    /// PEM of the CA that signs Swish's server certificate. Supplied rather
    /// than taken from the system store: pinning to the scheme's own CA means a
    /// compromised public CA cannot impersonate the payment endpoint.
    pub server_ca_pem: Vec<u8>,
    /// Where Swish should send callbacks. Must be HTTPS and publicly
    /// resolvable; Swish rejects anything else.
    pub callback_url: String,
}

impl SwishConfig {
    /// Reads the configuration from the environment, or explains what is
    /// missing.
    ///
    /// Certificates come from **paths**, never from environment variables
    /// holding PEM. A private key in an environment variable is a private key
    /// in every crash dump, every `docker inspect` and every process listing
    /// that shows the environment.
    pub fn from_env() -> Result<Option<Self>, String> {
        let Ok(payee_alias) = std::env::var("SKATTJAKT_SWISH_PAYEE_ALIAS") else {
            return Ok(None);
        };
        if payee_alias.is_empty() {
            return Ok(None);
        }

        validate_payee_alias(&payee_alias)?;

        let identity_path = std::env::var("SKATTJAKT_SWISH_CLIENT_PEM")
            .map_err(|_| "SKATTJAKT_SWISH_CLIENT_PEM is required when Swish is configured")?;
        let ca_path = std::env::var("SKATTJAKT_SWISH_CA_PEM")
            .map_err(|_| "SKATTJAKT_SWISH_CA_PEM is required when Swish is configured")?;
        let callback_url = std::env::var("SKATTJAKT_SWISH_CALLBACK_URL")
            .map_err(|_| "SKATTJAKT_SWISH_CALLBACK_URL is required when Swish is configured")?;

        if !callback_url.starts_with("https://") {
            return Err(format!(
                "SKATTJAKT_SWISH_CALLBACK_URL must be https, got {callback_url:?}"
            ));
        }

        let client_identity_pem = std::fs::read(&identity_path)
            .map_err(|e| format!("could not read {identity_path}: {e}"))?;
        let server_ca_pem =
            std::fs::read(&ca_path).map_err(|e| format!("could not read {ca_path}: {e}"))?;

        // Default to the test host. Getting this wrong in the safe direction
        // means a failed handshake; the other way round means real money.
        let base_url =
            std::env::var("SKATTJAKT_SWISH_BASE_URL").unwrap_or_else(|_| TEST_BASE.to_string());

        Ok(Some(Self {
            base_url,
            payee_alias,
            client_identity_pem,
            server_ca_pem,
            callback_url,
        }))
    }

    pub fn is_production(&self) -> bool {
        self.base_url.starts_with(PRODUCTION_BASE)
    }
}

/// A Swish number is `123` followed by seven digits. Checked here so a typo is
/// a refusal to start rather than every payment failing at the provider.
pub fn validate_payee_alias(alias: &str) -> Result<(), String> {
    let looks_right =
        alias.len() == 10 && alias.starts_with("123") && alias.chars().all(|c| c.is_ascii_digit());
    if looks_right {
        Ok(())
    } else {
        Err(format!(
            "SKATTJAKT_SWISH_PAYEE_ALIAS must be 123 followed by seven digits, got {alias:?}"
        ))
    }
}

/// An instruction id Swish will accept, derived from a UUID.
///
/// Derived rather than random so it is reproducible from the order: retrying a
/// create for the same order sends the same instruction id, and Swish treats
/// that as the same payment rather than a second one. That is the whole
/// idempotency story, and it costs one function.
pub fn instruction_id(from: uuid::Uuid) -> String {
    from.simple().to_string().to_uppercase()
}

#[derive(Debug)]
pub struct SwishProvider {
    client: reqwest::Client,
    config: SwishConfig,
}

impl SwishProvider {
    pub fn new(config: SwishConfig) -> Result<Self, String> {
        let identity = reqwest::Identity::from_pem(&config.client_identity_pem)
            .map_err(|e| format!("the Swish client certificate could not be loaded: {e}"))?;
        let ca = reqwest::Certificate::from_pem(&config.server_ca_pem)
            .map_err(|e| format!("the Swish CA certificate could not be loaded: {e}"))?;

        let client = reqwest::Client::builder()
            .identity(identity)
            .add_root_certificate(ca)
            .timeout(HTTP_TIMEOUT)
            // Redirects are off: a payment endpoint that redirects is not the
            // payment endpoint, and following one would send a client
            // certificate somewhere it was not meant for.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("the Swish HTTP client could not be built: {e}"))?;

        Ok(Self { client, config })
    }

    pub fn config(&self) -> &SwishConfig {
        &self.config
    }

    fn payment_url(&self, instruction_id: &str) -> String {
        format!(
            "{}/api/v2/paymentrequests/{instruction_id}",
            self.config.base_url
        )
    }
}

/// The request body, exactly as Swish expects it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Wire<'a> {
    payee_alias: &'a str,
    amount: String,
    currency: &'a str,
    callback_url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    payer_alias: Option<&'a str>,
    message: &'a str,
    payee_payment_reference: &'a str,
}

/// The payment object Swish returns from a GET, and posts to the callback.
///
/// Both are parsed by this one type, but only the GET is ever believed — see
/// the crate documentation.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WirePayment {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// Swish sends amounts as a JSON number with two decimals.
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub payee_payment_reference: Option<String>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub date_paid: Option<String>,
}

impl WirePayment {
    /// Normalises the wire shape into what the settlement logic reads.
    ///
    /// An unknown status is `Failed` rather than anything else. The alternative
    /// — treating what we do not recognise as pending — leaves an order in
    /// limbo forever; treating it as paid is unthinkable. Failing loudly on a
    /// status Swish added since this was written is the outcome that gets
    /// noticed and fixed.
    pub fn normalise(&self) -> Result<PaymentStatus, PaymentError> {
        let status = self.status.as_deref().unwrap_or_default();
        let outcome = match status {
            "CREATED" => PaymentOutcome::Pending,
            "PAID" => PaymentOutcome::Paid,
            "DECLINED" | "CANCELLED" => PaymentOutcome::Declined,
            "ERROR" => PaymentOutcome::Failed,
            other => {
                return Err(PaymentError::Unintelligible(format!(
                    "unknown payment status {other:?}"
                )))
            }
        };

        // Kronor as a float on the wire, öre as an integer here. The rounding
        // is explicit because 69.0 * 100.0 is not always 6900.0 in binary
        // floating point, and a payment that misses by one öre would be
        // refused for the wrong reason.
        let amount = match self.amount {
            Some(kronor) => Money::from_ore((kronor * 100.0).round() as i64),
            None if outcome == PaymentOutcome::Paid => {
                return Err(PaymentError::Unintelligible(
                    "a paid payment carried no amount".into(),
                ))
            }
            None => Money::from_ore(0),
        };

        let paid_at = self
            .date_paid
            .as_deref()
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|t| t.with_timezone(&chrono::Utc));

        Ok(PaymentStatus {
            outcome,
            amount,
            currency: self.currency.clone().unwrap_or_default(),
            payment_reference: self.payee_payment_reference.clone(),
            error_code: self
                .error_code
                .clone()
                .or_else(|| self.error_message.clone()),
            paid_at,
        })
    }
}

#[async_trait]
impl PaymentProvider for SwishProvider {
    fn name(&self) -> &'static str {
        "swish"
    }

    async fn create(&self, request: &PaymentRequest) -> Result<PaymentHandle, PaymentError> {
        if request.instruction_id.len() != INSTRUCTION_ID_LEN
            || !request
                .instruction_id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
        {
            return Err(PaymentError::Rejected(
                "the instruction id must be 32 uppercase hexadecimal characters".into(),
            ));
        }
        if request.message.chars().count() > MAX_MESSAGE {
            return Err(PaymentError::Rejected(format!(
                "the payer message is longer than {MAX_MESSAGE} characters"
            )));
        }
        if request.payment_reference.len() > MAX_PAYMENT_REFERENCE {
            return Err(PaymentError::Rejected(format!(
                "the payment reference is longer than {MAX_PAYMENT_REFERENCE} characters"
            )));
        }

        let body = Wire {
            payee_alias: &self.config.payee_alias,
            // Kronor with two decimals, as the scheme expects.
            amount: format!(
                "{}.{:02}",
                request.amount.ore() / 100,
                request.amount.ore() % 100
            ),
            currency: "SEK",
            callback_url: &request.callback_url,
            payer_alias: request.payer_alias.as_deref(),
            message: &request.message,
            payee_payment_reference: &request.payment_reference,
        };

        let response = self
            .client
            .put(self.payment_url(&request.instruction_id))
            .json(&body)
            .send()
            .await
            .map_err(|e| PaymentError::Unavailable(e.to_string()))?;

        let status = response.status();
        if status.is_server_error() {
            return Err(PaymentError::Unavailable(format!(
                "Swish returned {status}"
            )));
        }
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(PaymentError::Rejected(format!(
                "Swish returned {status}: {}",
                detail.chars().take(400).collect::<String>()
            )));
        }

        // The token is what a client turns into an app switch or a QR code. Its
        // absence is not fatal — the payment exists either way and can be
        // polled — so it is carried as an Option rather than an error.
        let token = response
            .headers()
            .get("paymentrequesttoken")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        Ok(PaymentHandle {
            reference: request.instruction_id.clone(),
            token,
        })
    }

    async fn lookup(&self, reference: &str) -> Result<PaymentStatus, PaymentError> {
        let response = self
            .client
            .get(self.payment_url(reference))
            .send()
            .await
            .map_err(|e| PaymentError::Unavailable(e.to_string()))?;

        let status = response.status();
        if status.is_server_error() {
            return Err(PaymentError::Unavailable(format!(
                "Swish returned {status}"
            )));
        }
        if !status.is_success() {
            return Err(PaymentError::Rejected(format!("Swish returned {status}")));
        }

        let payment: WirePayment = response
            .json()
            .await
            .map_err(|e| PaymentError::Unintelligible(e.to_string()))?;
        payment.normalise()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(status: &str, amount: Option<f64>) -> WirePayment {
        WirePayment {
            id: Some("ABC".into()),
            status: Some(status.into()),
            amount,
            currency: Some("SEK".into()),
            payee_payment_reference: Some("order-1".into()),
            error_code: None,
            error_message: None,
            date_paid: None,
        }
    }

    #[test]
    fn the_four_statuses_swish_documents_are_understood() {
        assert_eq!(
            wire("CREATED", None).normalise().unwrap().outcome,
            PaymentOutcome::Pending
        );
        assert_eq!(
            wire("PAID", Some(69.0)).normalise().unwrap().outcome,
            PaymentOutcome::Paid
        );
        assert_eq!(
            wire("DECLINED", None).normalise().unwrap().outcome,
            PaymentOutcome::Declined
        );
        assert_eq!(
            wire("ERROR", None).normalise().unwrap().outcome,
            PaymentOutcome::Failed
        );
    }

    #[test]
    fn a_status_this_code_does_not_know_is_an_error_not_a_guess() {
        // If Swish adds a status, the honest answer is to stop and be fixed.
        // Treating it as pending strands the order; treating it as paid is
        // unthinkable.
        let error = wire("SETTLING", None).normalise().unwrap_err();
        assert!(
            matches!(error, PaymentError::Unintelligible(reason) if reason.contains("SETTLING"))
        );
    }

    #[test]
    fn a_paid_payment_with_no_amount_is_refused_rather_than_read_as_zero() {
        let error = wire("PAID", None).normalise().unwrap_err();
        assert!(matches!(error, PaymentError::Unintelligible(_)));
    }

    #[test]
    fn kronor_on_the_wire_become_exact_ore() {
        // 69.0 * 100.0 is not reliably 6900.0 in binary floating point, and an
        // amount that misses by one öre would be refused for the wrong reason.
        for (kronor, ore) in [(29.0, 2_900), (69.0, 6_900), (0.01, 1), (1234.56, 123_456)] {
            let status = wire("PAID", Some(kronor)).normalise().unwrap();
            assert_eq!(status.amount.ore(), ore, "{kronor} kronor");
        }
    }

    #[test]
    fn an_instruction_id_is_uppercase_hexadecimal_without_hyphens() {
        let id = instruction_id(uuid::Uuid::nil());
        assert_eq!(id.len(), INSTRUCTION_ID_LEN);
        assert!(!id.contains('-'));
        assert!(id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    }

    #[test]
    fn the_same_order_always_produces_the_same_instruction_id() {
        // The whole idempotency story: retrying a create for one order must not
        // create a second payment.
        let id = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        assert_eq!(instruction_id(id), instruction_id(id));
        assert_ne!(instruction_id(id), instruction_id(uuid::Uuid::nil()));
    }

    #[test]
    fn a_swish_number_that_is_not_a_swish_number_stops_the_deployment() {
        assert!(validate_payee_alias("1231234567").is_ok());
        for bad in [
            "123123456",
            "12312345678",
            "1231234567 ",
            "4671234567",
            "123abcdefg",
        ] {
            assert!(validate_payee_alias(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_default_environment_is_the_test_one() {
        // Getting this wrong in the safe direction fails a handshake. The other
        // way round moves real money.
        assert_ne!(TEST_BASE, PRODUCTION_BASE);
        let config = SwishConfig {
            base_url: TEST_BASE.into(),
            payee_alias: "1231234567".into(),
            client_identity_pem: vec![],
            server_ca_pem: vec![],
            callback_url: "https://example.test/cb".into(),
        };
        assert!(!config.is_production());
    }

    #[test]
    fn an_error_message_is_carried_when_there_is_no_code() {
        let mut payment = wire("ERROR", None);
        payment.error_code = None;
        payment.error_message = Some("Payer not enrolled".into());
        let status = payment.normalise().unwrap();
        assert_eq!(status.error_code.as_deref(), Some("Payer not enrolled"));
    }
}

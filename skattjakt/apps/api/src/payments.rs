//! Wiring the payment provider into the service.
//!
//! Two questions this type answers, and keeping them together is the point:
//! *which provider* and *whether payment is required at all*. A deployment with
//! no provider must not silently give analyses away, and a deployment with a
//! provider must not have a route that forgets to check.

use std::sync::Arc;

use skattjakt_payments::{swish, PaymentProvider, UnconfiguredProvider};
use skattjakt_telemetry::LogRecord;

#[derive(Debug)]
pub struct Payments {
    provider: Arc<dyn PaymentProvider>,
    callback_url: Option<String>,
    /// Whether an analysis needs a paid order.
    ///
    /// Separate from "is a provider configured" on purpose. The two are
    /// normally the same, but an operator running an internal deployment for a
    /// single customer has a legitimate reason to want the routes without the
    /// gate, and a hidden coupling would make that a code change.
    required: bool,
}

impl Payments {
    /// Reads the configuration, or explains what is wrong with it.
    ///
    /// A half-configured payment setup is a hard failure rather than a warning:
    /// a Swish number with no certificate is a deployment that will take orders
    /// and never be able to collect on them, and finding that out from a
    /// customer is worse than finding it out from a crash loop.
    pub fn from_env() -> Result<Self, String> {
        let required = std::env::var("SKATTJAKT_PAYMENTS_REQUIRED")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        match swish::SwishConfig::from_env()? {
            Some(config) => {
                let callback_url = config.callback_url.clone();
                let production = config.is_production();
                let provider = swish::SwishProvider::new(config)?;
                LogRecord::info("swish payments configured")
                    .internal(
                        "environment",
                        if production { "production" } else { "test" },
                    )
                    .emit();
                Ok(Self {
                    provider: Arc::new(provider),
                    callback_url: Some(callback_url),
                    required,
                })
            }
            None => {
                if required {
                    return Err(
                        "SKATTJAKT_PAYMENTS_REQUIRED is set but no payment provider is \
                         configured; analyses would be unbuyable"
                            .into(),
                    );
                }
                Ok(Self::unconfigured())
            }
        }
    }

    pub fn unconfigured() -> Self {
        Self {
            provider: Arc::new(UnconfiguredProvider),
            callback_url: None,
            required: false,
        }
    }

    pub fn provider(&self) -> &dyn PaymentProvider {
        self.provider.as_ref()
    }

    pub fn callback_url(&self) -> Option<&str> {
        self.callback_url.as_deref()
    }

    /// Whether an analysis must present a paid order.
    pub fn required(&self) -> bool {
        self.required
    }

    pub fn is_configured(&self) -> bool {
        self.callback_url.is_some()
    }
}

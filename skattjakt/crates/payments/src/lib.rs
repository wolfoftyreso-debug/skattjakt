//! Taking payment for one analysis, and refusing to run one that was not paid.
//!
//! The security position, stated before anything else because everything here
//! follows from it:
//!
//! > **A payment is settled by asking the payment provider, never by being told.**
//!
//! Swish delivers a callback when a payment resolves. That callback is a hint
//! that something happened — it is not evidence, and this crate never treats it
//! as evidence. Every settlement goes through [`PaymentProvider::lookup`],
//! which asks Swish over the same mutually-authenticated connection the payment
//! was created on, and the answer to *that* is what moves an order.
//!
//! The consequence is worth spelling out: a forged callback, a replayed
//! callback, or a callback from someone who guessed an order id achieves
//! nothing except causing us to ask Swish a question we would have asked
//! anyway. It cannot make an unpaid order paid. That is a much stronger
//! position than authenticating the callback and trusting its body, because it
//! does not depend on getting the authentication right.
//!
//! What is deliberately not here
//! =============================
//!
//! Refunds. Swish has an API for them and it needs the same certificate, but a
//! refund is a decision about a customer relationship rather than a mechanism,
//! and half a refund path is worse than none. [`OrderState::RefundOwed`] exists
//! so the system can *say* a refund is owed — for an analysis that was paid for
//! and then failed — without pretending it can make one.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod swish;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use skattjakt_core::Money;
use thiserror::Error;

/// What is being bought.
///
/// The price is part of the product rather than a database row, on purpose: a
/// price change is a product decision that should go through review and a
/// deploy, and be visible in a diff. The *charged* amount is copied onto the
/// order when it is created, so changing a price never rewrites what somebody
/// was actually asked to pay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    /// A private individual's own material.
    PrivateAnalysis,
    /// A limited company's accounts.
    CompanyAnalysis,
    /// The control review an accounting assistant runs over a client's closing.
    ControlReview,
}

impl Product {
    pub const ALL: [Product; 3] = [
        Product::PrivateAnalysis,
        Product::CompanyAnalysis,
        Product::ControlReview,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Product::PrivateAnalysis => "private_analysis",
            Product::CompanyAnalysis => "company_analysis",
            Product::ControlReview => "control_review",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "private_analysis" => Product::PrivateAnalysis,
            "company_analysis" => Product::CompanyAnalysis,
            "control_review" => Product::ControlReview,
            _ => return None,
        })
    }

    /// In öre, because every amount in this system is an integer of öre and a
    /// price is not the place to introduce a float.
    pub fn price(self) -> Money {
        Money::from_ore(match self {
            Product::PrivateAnalysis => 2_900,
            Product::CompanyAnalysis => 6_900,
            Product::ControlReview => 6_900,
        })
    }

    /// What the payer sees in the Swish app. Bounded to 50 characters by
    /// Swish, and checked here rather than discovered as a 422 in production.
    pub fn payment_message(self) -> &'static str {
        match self {
            Product::PrivateAnalysis => "Skattjakt privatanalys",
            Product::CompanyAnalysis => "Skattjakt bolagsanalys",
            Product::ControlReview => "Skattjakt Kontroll",
        }
    }

    /// Which presentation layer this product buys.
    pub fn audience(self) -> &'static str {
        match self {
            Product::PrivateAnalysis => "private",
            Product::CompanyAnalysis => "company",
            Product::ControlReview => "accountant",
        }
    }
}

/// Where an order is in its life.
///
/// Enumerated rather than left as a string for the same reason the analysis
/// state machine is: the states that must never coexist are the ones a string
/// lets you write. An order cannot be both `Paid` and `Failed`, and an order
/// that is `Consumed` has already bought an analysis and cannot buy another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderState {
    /// Created, no payment attempted yet.
    Created,
    /// A payment request is out with Swish and the payer has not resolved it.
    AwaitingPayment,
    /// Swish confirmed the payment, and we confirmed it with Swish. Not yet
    /// used for an analysis.
    Paid,
    /// The payer declined it in the app, or it expired.
    Declined,
    /// Swish reported an error, or we could not settle it.
    Failed,
    /// The analysis this order bought has been created. Terminal.
    Consumed,
    /// Paid, and the thing paid for could not be delivered. Terminal from the
    /// machine's point of view; a person owes the customer money.
    RefundOwed,
}

impl OrderState {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderState::Created => "created",
            OrderState::AwaitingPayment => "awaiting_payment",
            OrderState::Paid => "paid",
            OrderState::Declined => "declined",
            OrderState::Failed => "failed",
            OrderState::Consumed => "consumed",
            OrderState::RefundOwed => "refund_owed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "created" => OrderState::Created,
            "awaiting_payment" => OrderState::AwaitingPayment,
            "paid" => OrderState::Paid,
            "declined" => OrderState::Declined,
            "failed" => OrderState::Failed,
            "consumed" => OrderState::Consumed,
            "refund_owed" => OrderState::RefundOwed,
            _ => return None,
        })
    }

    /// Whether this order can still buy an analysis.
    pub fn is_spendable(self) -> bool {
        matches!(self, OrderState::Paid)
    }

    /// Whether anything further can happen to it.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderState::Declined
                | OrderState::Failed
                | OrderState::Consumed
                | OrderState::RefundOwed
        )
    }
}

/// What the provider says about a payment, normalised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentOutcome {
    /// Still with the payer.
    Pending,
    /// Money moved.
    Paid,
    /// The payer said no, or it timed out.
    Declined,
    /// The provider could not complete it.
    Failed,
}

/// One payment as the provider currently describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct PaymentStatus {
    pub outcome: PaymentOutcome,
    /// What was actually paid, as the provider reports it.
    pub amount: Money,
    /// ISO 4217, as the provider reports it.
    pub currency: String,
    /// Our own reference, echoed back. The link to the order.
    pub payment_reference: Option<String>,
    /// Provider-side error code, for the states that have one.
    pub error_code: Option<String>,
    pub paid_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// What is needed to start a payment.
#[derive(Debug, Clone, PartialEq)]
pub struct PaymentRequest {
    /// Client-generated, and the idempotency key: sending the same instruction
    /// id twice must not create two payments.
    pub instruction_id: String,
    pub amount: Money,
    pub message: String,
    /// Our order id, echoed back by the provider so a callback can be matched
    /// to an order without trusting anything in its body.
    pub payment_reference: String,
    /// Where the provider should tell us something happened. A hint channel —
    /// see the module documentation.
    pub callback_url: String,
    /// The payer's number, for a payment started somewhere other than the
    /// payer's own phone. `None` is the app-switch case, where the payer
    /// identifies themselves in the Swish app.
    pub payer_alias: Option<String>,
}

/// What came back when a payment was started.
#[derive(Debug, Clone, PartialEq)]
pub struct PaymentHandle {
    /// The provider's id for this payment, used to look it up later.
    pub reference: String,
    /// The token a client turns into an app switch or a QR code. Present for
    /// the app-switch flow.
    pub token: Option<String>,
}

#[derive(Debug, Error)]
pub enum PaymentError {
    /// No payment provider in this deployment. Not an error to retry.
    #[error("payments are not configured in this deployment")]
    NotConfigured,
    /// The provider rejected the request as invalid — a bad alias, an amount
    /// outside limits, a message too long. Retrying sends the same thing.
    #[error("the payment provider rejected the request: {0}")]
    Rejected(String),
    /// The provider could not be reached, or answered 5xx. Worth another go.
    #[error("the payment provider could not be reached: {0}")]
    Unavailable(String),
    /// The provider answered something this code cannot read. Never treated as
    /// a payment.
    #[error("the payment provider's answer could not be understood: {0}")]
    Unintelligible(String),
}

impl PaymentError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, PaymentError::Unavailable(_))
    }
}

/// One payment provider.
#[async_trait]
pub trait PaymentProvider: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &'static str;

    /// Starts a payment. Must be idempotent on `instruction_id`.
    async fn create(&self, request: &PaymentRequest) -> Result<PaymentHandle, PaymentError>;

    /// Asks the provider what actually happened. **This is the only thing that
    /// settles an order.**
    async fn lookup(&self, reference: &str) -> Result<PaymentStatus, PaymentError>;
}

/// The provider for a deployment with no payment configuration.
///
/// The honest form of "not built yet" (§31): a type that exists, is wired in,
/// and refuses — rather than one that returns success and lets unpaid analyses
/// run. Every route that needs payment gets `NotConfigured` and says so.
#[derive(Debug, Default)]
pub struct UnconfiguredProvider;

#[async_trait]
impl PaymentProvider for UnconfiguredProvider {
    fn name(&self) -> &'static str {
        "none"
    }

    async fn create(&self, _request: &PaymentRequest) -> Result<PaymentHandle, PaymentError> {
        Err(PaymentError::NotConfigured)
    }

    async fn lookup(&self, _reference: &str) -> Result<PaymentStatus, PaymentError> {
        Err(PaymentError::NotConfigured)
    }
}

// ---------------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------------

/// Why a payment that the provider called successful was still not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The amount paid is not the amount asked for.
    WrongAmount { expected: Money, actual: Money },
    /// Paid in something other than kronor.
    WrongCurrency(String),
    /// The payment does not carry our reference, so it is not this order's.
    WrongOrder {
        expected: String,
        actual: Option<String>,
    },
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::WrongAmount { expected, actual } => write!(
                f,
                "the payment was {actual} but the order was for {expected}"
            ),
            Rejection::WrongCurrency(currency) => {
                write!(f, "the payment was in {currency}, not SEK")
            }
            Rejection::WrongOrder { expected, actual } => write!(
                f,
                "the payment carries reference {actual:?}, not {expected:?}"
            ),
        }
    }
}

/// What an order should become, given what the provider says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// Nothing has happened yet; leave the order alone.
    Wait,
    /// Move the order to `Paid`.
    Accept,
    /// Move the order to `Declined`.
    Decline,
    /// Move the order to `Failed`, with a reason.
    Fail(String),
}

/// Decides what a provider's answer means for an order.
///
/// Pure, and separate from the HTTP client, because this is where the money
/// checks live and they should be testable without a network. Three things are
/// verified before an order is accepted as paid, and each of them is a real
/// attack or a real bug:
///
/// 1. **The reference matches.** Without it, a payment for one order could
///    settle another — which is exactly what happens if a callback body is
///    trusted to name its own order.
/// 2. **The amount matches.** A payer who edits the amount in the app, or a
///    price changed between order and payment, must not buy a 69-krona
///    analysis for 1 krona.
/// 3. **The currency is SEK.** Swish is a Swedish scheme and this should never
///    vary, which is precisely why an unexpected value here means something is
///    wrong enough to stop.
pub fn settle(expected_reference: &str, expected: Money, status: &PaymentStatus) -> Settlement {
    match status.outcome {
        PaymentOutcome::Pending => return Settlement::Wait,
        PaymentOutcome::Declined => return Settlement::Decline,
        PaymentOutcome::Failed => {
            return Settlement::Fail(
                status
                    .error_code
                    .clone()
                    .unwrap_or_else(|| "the provider reported a failure".to_string()),
            )
        }
        PaymentOutcome::Paid => {}
    }

    if status.payment_reference.as_deref() != Some(expected_reference) {
        return Settlement::Fail(
            Rejection::WrongOrder {
                expected: expected_reference.to_string(),
                actual: status.payment_reference.clone(),
            }
            .to_string(),
        );
    }
    if !status.currency.eq_ignore_ascii_case("SEK") {
        return Settlement::Fail(Rejection::WrongCurrency(status.currency.clone()).to_string());
    }
    if status.amount != expected {
        return Settlement::Fail(
            Rejection::WrongAmount {
                expected,
                actual: status.amount,
            }
            .to_string(),
        );
    }
    Settlement::Accept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paid(reference: &str, ore: i64, currency: &str) -> PaymentStatus {
        PaymentStatus {
            outcome: PaymentOutcome::Paid,
            amount: Money::from_ore(ore),
            currency: currency.into(),
            payment_reference: Some(reference.into()),
            error_code: None,
            paid_at: None,
        }
    }

    #[test]
    fn a_payment_of_the_right_amount_for_the_right_order_is_accepted() {
        let status = paid("order-1", 6_900, "SEK");
        assert_eq!(
            settle("order-1", Money::from_ore(6_900), &status),
            Settlement::Accept
        );
    }

    #[test]
    fn a_payment_for_a_different_order_never_settles_this_one() {
        // The attack a trusted callback body enables: pay 29 kr for your own
        // order, then post a callback naming somebody's 69-kr order.
        let status = paid("order-2", 6_900, "SEK");
        let outcome = settle("order-1", Money::from_ore(6_900), &status);
        assert!(matches!(outcome, Settlement::Fail(reason) if reason.contains("order-2")));
    }

    #[test]
    fn a_payment_with_no_reference_at_all_is_not_this_order() {
        let mut status = paid("order-1", 6_900, "SEK");
        status.payment_reference = None;
        assert!(matches!(
            settle("order-1", Money::from_ore(6_900), &status),
            Settlement::Fail(_)
        ));
    }

    #[test]
    fn paying_less_than_the_price_does_not_buy_the_product() {
        let status = paid("order-1", 100, "SEK");
        let outcome = settle("order-1", Money::from_ore(6_900), &status);
        assert!(matches!(outcome, Settlement::Fail(reason) if reason.contains("1,00")));
    }

    #[test]
    fn paying_more_than_the_price_is_also_refused() {
        // Not generosity: a mismatch in either direction means the order and
        // the payment disagree about what was bought, and quietly accepting the
        // larger one hides that.
        let status = paid("order-1", 10_000, "SEK");
        assert!(matches!(
            settle("order-1", Money::from_ore(6_900), &status),
            Settlement::Fail(_)
        ));
    }

    #[test]
    fn a_currency_that_is_not_kronor_stops_everything() {
        let status = paid("order-1", 6_900, "EUR");
        let outcome = settle("order-1", Money::from_ore(6_900), &status);
        assert!(matches!(outcome, Settlement::Fail(reason) if reason.contains("EUR")));
    }

    #[test]
    fn the_currency_check_does_not_care_about_case() {
        let status = paid("order-1", 6_900, "sek");
        assert_eq!(
            settle("order-1", Money::from_ore(6_900), &status),
            Settlement::Accept
        );
    }

    #[test]
    fn a_pending_payment_changes_nothing() {
        let mut status = paid("order-1", 6_900, "SEK");
        status.outcome = PaymentOutcome::Pending;
        assert_eq!(
            settle("order-1", Money::from_ore(6_900), &status),
            Settlement::Wait
        );
    }

    #[test]
    fn a_declined_payment_is_declined_rather_than_failed() {
        // Different words for the customer: "you cancelled it" and "something
        // broke" lead to different next screens.
        let mut status = paid("order-1", 6_900, "SEK");
        status.outcome = PaymentOutcome::Declined;
        assert_eq!(
            settle("order-1", Money::from_ore(6_900), &status),
            Settlement::Decline
        );
    }

    #[test]
    fn a_failure_carries_the_providers_code_when_there_is_one() {
        let mut status = paid("order-1", 6_900, "SEK");
        status.outcome = PaymentOutcome::Failed;
        status.error_code = Some("RF07".into());
        assert!(matches!(
            settle("order-1", Money::from_ore(6_900), &status),
            Settlement::Fail(reason) if reason == "RF07"
        ));
    }

    #[test]
    fn the_wrong_amount_is_checked_even_when_the_reference_is_right() {
        // Order of checks matters for the message, not for the verdict: every
        // one of the three must be able to stop a payment on its own.
        for (reference, ore, currency) in [
            ("order-x", 6_900, "SEK"),
            ("order-1", 1, "SEK"),
            ("order-1", 6_900, "NOK"),
        ] {
            let status = paid(reference, ore, currency);
            assert!(
                matches!(
                    settle("order-1", Money::from_ore(6_900), &status),
                    Settlement::Fail(_)
                ),
                "accepted {reference} {ore} {currency}"
            );
        }
    }

    #[test]
    fn prices_are_whole_kronor_and_the_three_products_are_priced() {
        for product in Product::ALL {
            let ore = product.price().ore();
            assert!(ore > 0, "{} is free", product.as_str());
            assert_eq!(
                ore % 100,
                0,
                "{} is priced in part-kronor",
                product.as_str()
            );
            // Swish caps the message a payer sees.
            assert!(product.payment_message().chars().count() <= 50);
        }
    }

    #[test]
    fn every_product_name_survives_a_round_trip() {
        for product in Product::ALL {
            assert_eq!(Product::parse(product.as_str()), Some(product));
        }
        assert_eq!(Product::parse("free_analysis"), None);
    }

    #[test]
    fn only_a_paid_order_can_buy_an_analysis() {
        for state in [
            OrderState::Created,
            OrderState::AwaitingPayment,
            OrderState::Declined,
            OrderState::Failed,
            OrderState::Consumed,
            OrderState::RefundOwed,
        ] {
            assert!(!state.is_spendable(), "{} could buy", state.as_str());
        }
        assert!(OrderState::Paid.is_spendable());
    }

    #[test]
    fn an_unconfigured_deployment_refuses_rather_than_succeeding() {
        // The failure mode this rules out: a provider stub that returns "paid"
        // and lets every analysis run free.
        let provider = UnconfiguredProvider;
        let request = PaymentRequest {
            instruction_id: "A".repeat(32),
            amount: Money::from_ore(6_900),
            message: "test".into(),
            payment_reference: "order-1".into(),
            callback_url: "https://example.test/cb".into(),
            payer_alias: None,
        };
        let error = futures_lite_block_on(provider.create(&request)).unwrap_err();
        assert!(matches!(error, PaymentError::NotConfigured));
        assert!(!error.is_retryable());
    }

    /// A one-poll executor, so this crate's tests do not need a runtime.
    fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Wake, Waker};
        struct Noop;
        impl Wake for Noop {
            fn wake(self: std::sync::Arc<Self>) {}
        }
        let waker = Waker::from(std::sync::Arc::new(Noop));
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
    }
}

//! Resolving payments the callback never resolved.
//!
//! Why this exists even though Swish sends a callback
//! ==================================================
//!
//! Because a callback is a network delivery, and network deliveries are lost.
//! A deploy during the thirty seconds a payer spends in the app, a partition, a
//! bad minute at either end — and a customer who has paid is left watching a
//! spinner while their money sits with us.
//!
//! The sweep is what turns the callback from a requirement into an
//! optimisation. With it, the callback makes settlement fast; without the
//! callback, settlement still happens, just a minute later. A design that needs
//! the callback to arrive is a design that breaks on an ordinary Tuesday.
//!
//! It is also the answer to the opposite failure. A payment the payer abandoned
//! never produces a callback at all — Swish simply lets it expire — so
//! something has to go and ask, or the order waits forever.

use std::time::Duration;

use skattjakt_payments::{OrderState, PaymentProvider, Settlement};
use skattjakt_store::Store;
use skattjakt_telemetry::{metrics, LabelSet, LogRecord, Registry};

/// How often to look for payments nobody resolved.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// How long to let the callback have first go.
///
/// A payment created two seconds ago is almost certainly still with the payer,
/// and asking about it would be a request against Swish for every payment in
/// flight. Thirty seconds is longer than a callback takes and far shorter than
/// a customer's patience.
pub const GRACE: Duration = Duration::from_secs(30);

/// How many to resolve per sweep. Bounded so a backlog is worked through
/// steadily rather than in one burst against the provider.
const BATCH: i64 = 50;

/// Asks the provider about every payment still pending.
pub async fn sweep(
    store: &Store,
    provider: &dyn PaymentProvider,
    metrics: &Registry,
) -> anyhow::Result<usize> {
    let grace = chrono::Duration::from_std(GRACE)?;
    let pending = store.unsettled_payments(grace, BATCH).await?;

    metrics.set(
        metrics::names::PAYMENTS_UNSETTLED,
        LabelSet::new(),
        pending.len() as u64,
    );

    let mut settled = 0usize;
    for (company_id, payment_id, _provider_name, reference) in pending {
        match resolve(store, provider, company_id, payment_id, &reference).await {
            Ok(true) => settled += 1,
            Ok(false) => {}
            Err(error) => {
                // One payment failing to resolve must not stop the rest: the
                // next one in the batch may be a customer who has been waiting
                // longer.
                LogRecord::warn("could not resolve a pending payment")
                    .internal("reference", reference)
                    .internal("error", error.to_string())
                    .emit();
            }
        }
    }

    if settled > 0 {
        LogRecord::info("resolved payments the callback did not")
            .internal("settled", settled.to_string())
            .emit();
    }
    Ok(settled)
}

/// Asks about one payment. Returns whether it reached a terminal state.
///
/// Deliberately the same three checks the callback path applies — reference,
/// currency, amount — because it is the same function. Two settlement paths
/// with two ideas of what counts as paid is how one of them ends up laxer.
async fn resolve(
    store: &Store,
    provider: &dyn PaymentProvider,
    company_id: skattjakt_core::CompanyId,
    payment_id: uuid::Uuid,
    reference: &str,
) -> anyhow::Result<bool> {
    let mut tenant = store.tenant(company_id).await?;
    let payment = tenant
        .payment_by_reference(provider.name(), reference)
        .await?;
    let order = tenant.order(payment.order_id).await?;
    tenant.commit().await?;

    if order.state.is_terminal() || order.state == OrderState::Paid {
        return Ok(false);
    }

    let status = match provider.lookup(reference).await {
        Ok(status) => status,
        Err(error) => {
            // Not a failed payment — a failed question. The order stays put and
            // the next sweep asks again. Failing it here would decline payments
            // customers actually made, every time the provider had a bad
            // minute.
            let mut tenant = store.tenant(company_id).await?;
            tenant.record_lookup(payment_id, None).await?;
            tenant.commit().await?;
            return Err(anyhow::anyhow!(error));
        }
    };

    let settlement =
        skattjakt_payments::settle(&order.id.simple().to_string(), order.amount, &status);

    let mut tenant = store.tenant(company_id).await?;
    tenant
        .record_lookup(payment_id, Some(&format!("{:?}", status.outcome)))
        .await?;

    let (state, note) = match &settlement {
        Settlement::Wait => {
            tenant.commit().await?;
            return Ok(false);
        }
        Settlement::Accept => (OrderState::Paid, None),
        Settlement::Decline => (OrderState::Declined, None),
        Settlement::Fail(reason) => (OrderState::Failed, Some(reason.as_str())),
    };

    tenant
        .settle_payment(payment_id, order.id, state, note)
        .await?;
    tenant.commit().await?;

    if let Settlement::Fail(reason) = &settlement {
        LogRecord::error("a payment was reported successful and refused")
            .internal("order_id", order.id.to_string())
            .internal("reason", reason.clone())
            .emit();
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grace_is_shorter_than_the_sweep_and_both_are_shorter_than_patience() {
        // A grace longer than the interval would mean a payment waits two
        // sweeps; a sweep slower than a person's patience defeats the point of
        // having one.
        assert!(GRACE <= SWEEP_INTERVAL);
        assert!(SWEEP_INTERVAL <= Duration::from_secs(120));
    }

    #[test]
    fn the_batch_is_bounded() {
        // A backlog is worked through steadily. Resolving everything at once
        // would be a burst of requests at the provider precisely when something
        // is already wrong.
        const { assert!(BATCH > 0 && BATCH <= 100) };
    }
}

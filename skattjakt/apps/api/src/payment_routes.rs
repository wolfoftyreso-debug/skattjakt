//! Orders, Swish payments, and the callback.
//!
//! Three routes, and the shape of them follows from one rule: **the client
//! never asserts that a payment happened.** It can start one, and it can ask
//! how it went. Both answers come from asking Swish.
//!
//! `POST /v1/orders`                    create an order, start a Swish payment
//! `GET  /v1/orders/{id}`               how far it has got
//! `POST /v1/payments/swish/callback`   Swish saying something changed
//!
//! The callback is unauthenticated, and that is deliberate rather than
//! conceded. It carries no authority: everything it can do is make the server
//! look up a payment it already knows about and ask Swish for the truth. See
//! `settle` below, and §4 of `SKATTJAKT_PAYMENTS.md`.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use skattjakt_core::CompanyId;
use skattjakt_payments::{
    DeliveryChoice, OrderState, PaymentError, PaymentRequest, Product, Settlement,
};
use skattjakt_store::payments::Order;
use skattjakt_telemetry::{metrics::names, LabelSet, LogRecord};

use crate::routes::{company_scope, internal, store};
use crate::{authorise, AppState, Problem};
use skattjakt_identity::Permission;

#[derive(Debug, Deserialize)]
pub struct CreateOrderRequest {
    pub product: String,
    /// The payer's Swish number, when the payment is started somewhere other
    /// than the payer's own phone — a desktop browser, typically. Absent means
    /// the app-switch flow, where the payer identifies themselves in the app.
    #[serde(default)]
    pub payer_alias: Option<String>,
    /// Whether the analysis starts now or after the cancellation period.
    ///
    /// Absent means the cautious one. A consumer who says nothing keeps their
    /// right of cancellation; nothing here may read silence as consent.
    #[serde(default)]
    pub delivery: Option<String>,
    /// The buyer's express acknowledgement that starting now costs them the
    /// right to cancel.
    ///
    /// Separate from `delivery` on purpose. Distansavtalslagen asks for two
    /// things — consent to immediate delivery *and* an acknowledgement that the
    /// right is lost — and one field could only ever record one of them.
    #[serde(default)]
    pub accepts_loss_of_cancellation_right: bool,
}

fn order_json(order: &Order, token: Option<&str>) -> serde_json::Value {
    json!({
        "order_id": order.id,
        "product": order.product.as_str(),
        "amount_ore": order.amount.ore(),
        "amount": order.amount.to_string(),
        "currency": "SEK",
        "state": order.state.as_str(),
        "audience": order.product.audience_key(),
        // What the buyer chose, and what follows from it. A client cannot show
        // an honest "you can still cancel until…" without these.
        "delivery_choice": order.delivery_choice.as_str(),
        "keeps_right_to_cancel": order.delivery_choice.keeps_right_to_cancel()
            && order.analysis_id.is_none(),
        "consented_at": order.consent_at,
        "deliverable_from": order.deliverable_from,
        "analysis_id": order.analysis_id.map(|id| id.0),
        "note": order.note,
        // Present only just after creation. The client turns it into an app
        // switch on a phone, or a QR code on a desktop.
        "swish_token": token,
    })
}

/// What this deployment sells, at what price, on what terms.
///
/// Public and unauthenticated, because it is the shop window: the interface
/// needs it before anyone has signed in, and everything in it is already
/// published on `/priser` and `/angerratt` in prose.
///
/// It exists so the interface does not carry its own copy of the prices, the
/// consent wording or the list of what is for sale. Three copies of a price is
/// two chances for a customer to be shown one thing and charged another.
pub async fn shop(State(state): State<AppState>) -> Response {
    let products: Vec<serde_json::Value> = Product::ALL
        .iter()
        .map(|p| {
            json!({
                "id": p.as_str(),
                "title": crate::shopfront::product_title(*p),
                "description": crate::shopfront::product_description(*p),
                "price": p.price().to_string(),
                "price_ore": p.price().ore(),
                "audience": p.audience_key(),
                // Whether it can actually be bought here. A product with no
                // rules behind it is listed and closed rather than hidden —
                // see `SKATTJAKT_PAYMENTS.md` §6.
                "available": state.engine.set().covers_audience(p.audience_key()),
            })
        })
        .collect();

    Json(json!({
        "payments_required": state.payments.required(),
        "can_take_payment": state.payments.callback_url().is_some(),
        "products": products,
        "cancellation_period_days": skattjakt_payments::CANCELLATION_PERIOD_DAYS,
        "consent": {
            "wording": skattjakt_payments::CONSENT_WORDING,
            "version": skattjakt_payments::CONSENT_WORDING_VERSION,
        },
    }))
    .into_response()
}

/// Creates an order and starts a Swish payment for it.
pub async fn create_order(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateOrderRequest>,
) -> Result<Response, Problem> {
    let company_id = company_scope(
        &authorise(&state, &headers).await?,
        Permission::StartAnalysis,
    )?;
    let store = store(&state)?.clone();

    let Some(product) = Product::parse(&request.product) else {
        return Err(Problem::bad_request(
            "unknown_product",
            format!(
                "{:?} is not one of: {}",
                request.product,
                Product::ALL
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    };

    // Nothing may be sold that this build cannot deliver.
    //
    // The three products are three presentation layers over one engine, and the
    // engine only has rules for the taxpayers somebody has written rules for.
    // Privatanalys is the live case: it has a price, a page and a payment
    // message, and the shipped rule set contains no private-individual rules at
    // all — so a customer paying 29 kronor would receive an empty report and no
    // way to tell "we found nothing" from "we looked at nothing".
    //
    // Asked of the rule set rather than of a constant, so the day the first
    // private rule lands the product becomes sellable on its own.
    if !state.engine.set().covers_audience(product.audience_key()) {
        return Err(Problem {
            status: StatusCode::SERVICE_UNAVAILABLE,
            title: "product_not_available".into(),
            detail: format!(
                "{} cannot be sold by this build: it has no rules for that kind of \
                 taxpayer, so the analysis would be empty",
                product.as_str()
            ),
        });
    }

    // The choice the terms page promises the buyer, actually offered.
    //
    // `/villkor` and `/angerratt` have said since they were written that the
    // buyer picks between starting at once and waiting out the fourteen days.
    // Until now the checkout offered neither, which made a purchase term into a
    // description of something that did not exist — the one kind of
    // documentation drift a customer can rely on to their cost.
    let delivery = match request.delivery.as_deref() {
        None => DeliveryChoice::default(),
        Some(value) => DeliveryChoice::parse(value).ok_or_else(|| {
            Problem::bad_request(
                "unknown_delivery_choice",
                format!("{value:?} is not one of: immediate, after_cancellation_period"),
            )
        })?,
    };

    // Consent is a thing the buyer does, not a thing a page says. Immediate
    // delivery without the acknowledgement is refused rather than downgraded
    // quietly to the safe option: a buyer who asked to start now and was
    // silently put in a two-week queue would find out a fortnight later.
    if delivery.needs_consent() && !request.accepts_loss_of_cancellation_right {
        return Err(Problem::bad_request(
            "consent_required",
            format!(
                "immediate delivery requires accepts_loss_of_cancellation_right: \"{}\"",
                skattjakt_payments::CONSENT_WORDING
            ),
        ));
    }

    let callback_url = match state.payments.callback_url() {
        Some(url) => url.to_string(),
        None => {
            return Err(Problem {
                status: StatusCode::SERVICE_UNAVAILABLE,
                title: "payments_not_configured".into(),
                detail: "this deployment cannot take payments".into(),
            })
        }
    };

    // The order and the payment row are written before Swish is called, so a
    // request that dies between the two leaves a record of what was attempted
    // rather than a payment nothing in this system knows about.
    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let order = tenant
        .create_order(product, delivery)
        .await
        .map_err(internal)?;
    let instruction = skattjakt_payments::swish::instruction_id(Uuid::new_v4());
    let payment = tenant
        .start_payment(&order, state.payments.provider().name(), &instruction)
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    let handle = state
        .payments
        .provider()
        .create(&PaymentRequest {
            instruction_id: instruction.clone(),
            amount: order.amount,
            message: product.payment_message().to_string(),
            // The order id is what comes back on the callback, and it is what
            // ties a payment to an order without trusting anything a caller
            // sends.
            payment_reference: order.id.simple().to_string(),
            callback_url,
            payer_alias: request.payer_alias.clone(),
        })
        .await;

    let handle = match handle {
        Ok(handle) => handle,
        Err(error) => {
            // The order exists and is unpayable. Record why, so the customer
            // sees a reason rather than a spinner.
            let mut tenant = store.tenant(company_id).await.map_err(internal)?;
            let _ = tenant
                .settle_payment(
                    payment.id,
                    order.id,
                    OrderState::Failed,
                    Some(&error.to_string()),
                )
                .await;
            tenant.commit().await.map_err(internal)?;

            state.metrics.increment(
                names::PAYMENTS_STARTED,
                LabelSet::new().enumerated("outcome", "rejected"),
            );
            return Err(match error {
                PaymentError::NotConfigured => Problem {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    title: "payments_not_configured".into(),
                    detail: "this deployment cannot take payments".into(),
                },
                PaymentError::Unavailable(detail) => Problem {
                    status: StatusCode::BAD_GATEWAY,
                    title: "payment_provider_unavailable".into(),
                    detail,
                },
                other => Problem::bad_request("payment_rejected", other.to_string()),
            });
        }
    };

    state.metrics.increment(
        names::PAYMENTS_STARTED,
        LabelSet::new().enumerated("outcome", "started"),
    );

    // Re-read rather than reporting the row as it was before the payment was
    // started: `start_payment` moves the order to `awaiting_payment`, and the
    // response said `created`. A client that polled immediately saw the state
    // change for no reason it could observe.
    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let order = tenant.order(order.id).await.map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    Ok((
        StatusCode::CREATED,
        Json(order_json(&order, handle.token.as_deref())),
    )
        .into_response())
}

/// How far an order has got.
///
/// Polled by the client while the payer is in the Swish app. Cheap: it reads
/// the order and does not call Swish. The reconciliation sweep and the callback
/// are what move it, so a client polling faster does not hammer the provider.
pub async fn get_order(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, Problem> {
    let company_id = company_scope(
        &authorise(&state, &headers).await?,
        Permission::StartAnalysis,
    )?;
    let store = store(&state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let order = tenant.order(id).await.map_err(|e| match e {
        skattjakt_store::StoreError::NotFound => Problem {
            status: StatusCode::NOT_FOUND,
            title: "not_found".into(),
            detail: "no such order".into(),
        },
        other => internal(other),
    })?;
    tenant.commit().await.map_err(internal)?;

    Ok(Json(order_json(&order, None)).into_response())
}

/// Cancels a paid order within the cancellation period.
///
/// This is the other half of the choice `/angerratt` offers. A buyer who kept
/// their right to cancel must have somewhere to exercise it, or the right is a
/// sentence on a page.
///
/// It does **not** move money. The system cannot make a refund — see the
/// `skattjakt-payments` crate documentation — so it records that one is owed
/// and says so plainly, rather than reporting a refund that no person has made.
pub async fn cancel_order(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, Problem> {
    let company_id = company_scope(
        &authorise(&state, &headers).await?,
        Permission::StartAnalysis,
    )?;
    let store = store(&state)?;

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let order = tenant.cancel_order(id).await.map_err(|e| match e {
        // Deliberately one answer for every way an order can fail to be
        // cancellable, with the reasons spelled out. Distinguishing them would
        // mean telling an unauthenticated guess which orders exist.
        skattjakt_store::StoreError::NotFound => Problem {
            status: StatusCode::CONFLICT,
            title: "order_not_cancellable".into(),
            detail: "that order cannot be cancelled: it does not exist, was not paid, has \
                     already bought an analysis, has already been cancelled, or was bought \
                     with a consent to immediate delivery"
                .into(),
        },
        other => internal(other),
    })?;
    tenant.commit().await.map_err(internal)?;

    LogRecord::info("an order was cancelled within the cancellation period")
        .internal("order_id", order.id.to_string())
        .emit();

    Ok(Json(json!({
        "order": order_json(&order, None),
        "refund": {
            "owed": true,
            "amount": order.amount.to_string(),
            "note": "Beloppet betalas tillbaka manuellt av säljaren. \
                     Systemet gör inga återbetalningar självt.",
        },
    }))
    .into_response())
}

/// Swish telling us something changed.
///
/// **The body is read for one field and otherwise discarded.** We take the
/// payment reference, find the payment it names, and then ask Swish what
/// actually happened. Nothing in the body can settle an order.
///
/// That is why this route needs no authentication and no signature check. A
/// forged callback causes one outbound lookup and no state change; a replayed
/// one causes the same lookup twice, and settlement is idempotent. Getting
/// callback authentication subtly wrong is a common way to lose money, and the
/// most reliable defence is not to depend on it.
///
/// Always answers 200. Swish retries anything else, and there is nothing a
/// retry can fix: if we could not settle it now, the reconciliation sweep will.
pub async fn swish_callback(
    State(state): State<AppState>,
    body: String,
) -> Result<Response, Problem> {
    // `id`, not `payeePaymentReference`.
    //
    // These are two different identifiers and the callback used the wrong one.
    // `payeePaymentReference` is *our* reference — the order id we asked Swish
    // to echo — while `payments.provider_reference` holds the instruction id,
    // which is what `id` carries. Looking the order id up among instruction ids
    // never matched, so the callback resolved nothing and settlement only ever
    // happened when the reconciliation sweep came round a minute later.
    //
    // Nothing was lost by it and nothing was paid twice: the sweep is the
    // guarantee and the callback is the optimisation. But the optimisation was
    // dead, and every test passed because both a forged callback and a real one
    // did exactly the same nothing.
    let reference = serde_json::from_str::<skattjakt_payments::swish::WirePayment>(&body)
        .ok()
        .and_then(|payment| payment.id);

    match reference {
        Some(reference) => {
            // Three outcomes, not two.
            //
            // A callback that named a payment we could not find used to be
            // counted as `accepted`, which is exactly how a callback that
            // resolved *nothing at all* — it read the wrong field — produced a
            // healthy-looking metric for as long as it existed. `unknown` is
            // what a forged callback looks like, and it is also what a broken
            // one looks like; a sustained rate of it means one or the other,
            // and both are worth knowing.
            let outcome = match settle_by_reference(&state, &reference).await {
                Ok(Some(_)) => "accepted",
                Ok(None) => "unknown",
                // Anything that failed here is picked up by the sweep, so it is
                // counted rather than reported: the caller is Swish, and there
                // is nothing useful to tell it.
                Err(_) => "failed",
            };
            state.metrics.increment(
                names::PAYMENT_CALLBACKS,
                LabelSet::new().enumerated("outcome", outcome),
            );
        }
        None => {
            state.metrics.increment(
                names::PAYMENT_CALLBACKS,
                LabelSet::new().enumerated("outcome", "unreadable"),
            );
            LogRecord::warn("a payment callback carried no reference we could read").emit();
        }
    }

    Ok(StatusCode::OK.into_response())
}

/// Asks the provider about one payment and moves the order to match.
///
/// The single settlement path. The callback calls it, the reconciliation sweep
/// calls it, and a client polling an order does not — because a client must
/// never be able to drive traffic at the payment provider.
/// `None` when the reference names nothing this system knows.
///
/// Distinguished from a settled order rather than folded into it, because the
/// two look identical from outside and one of them is a bug. A callback whose
/// reference resolved to nothing was counted as `accepted` — which is how a
/// callback that had never resolved anything at all went unnoticed. See the
/// note on `swish_callback`.
pub async fn settle_by_reference(
    state: &AppState,
    reference: &str,
) -> Result<Option<OrderState>, Problem> {
    let store = store(state)?.clone();
    let provider = state.payments.provider();

    // The one query outside a tenant scope, and it returns an id and nothing
    // else. See `store::payments::company_for_payment_reference`.
    let Some(company_id) = store
        .company_for_payment_reference(provider.name(), reference)
        .await
        .map_err(internal)?
    else {
        // An unknown reference is not an error worth reporting upward: it is
        // what a forged callback looks like, and the correct response is to do
        // nothing. It is still worth *counting* — see the caller.
        return Ok(None);
    };

    settle_payment(state, company_id, reference).await.map(Some)
}

async fn settle_payment(
    state: &AppState,
    company_id: CompanyId,
    reference: &str,
) -> Result<OrderState, Problem> {
    let store = store(state)?.clone();
    let provider = state.payments.provider();

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    let payment = tenant
        .payment_by_reference(provider.name(), reference)
        .await
        .map_err(internal)?;
    let order = tenant.order(payment.order_id).await.map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    // Already settled. Nothing to ask and nothing to change — this is the
    // redelivered-callback case, and it must be silent rather than an error.
    if order.state.is_terminal() || order.state == OrderState::Paid {
        return Ok(order.state);
    }

    let status = match provider.lookup(reference).await {
        Ok(status) => status,
        Err(error) => {
            // A provider we cannot reach is not a failed payment. Leaving the
            // order pending means the sweep tries again; failing it here would
            // decline a payment the customer may well have made.
            LogRecord::warn("could not ask the payment provider about a payment")
                .internal("reference", reference.to_string())
                .internal("error", error.to_string())
                .emit();
            let mut tenant = store.tenant(company_id).await.map_err(internal)?;
            let _ = tenant.record_lookup(payment.id, None).await;
            tenant.commit().await.map_err(internal)?;
            return Ok(order.state);
        }
    };

    let settlement =
        skattjakt_payments::settle(&order.id.simple().to_string(), order.amount, &status);

    let mut tenant = store.tenant(company_id).await.map_err(internal)?;
    tenant
        .record_lookup(payment.id, Some(&format!("{:?}", status.outcome)))
        .await
        .map_err(internal)?;

    let settled = match &settlement {
        Settlement::Wait => {
            tenant.commit().await.map_err(internal)?;
            return Ok(order.state);
        }
        Settlement::Accept => OrderState::Paid,
        Settlement::Decline => OrderState::Declined,
        Settlement::Fail(_) => OrderState::Failed,
    };
    let note = match &settlement {
        Settlement::Fail(reason) => Some(reason.as_str()),
        _ => None,
    };

    let updated = tenant
        .settle_payment(payment.id, order.id, settled, note)
        .await
        .map_err(internal)?;
    tenant.commit().await.map_err(internal)?;

    if let Settlement::Fail(reason) = &settlement {
        // A payment the provider called successful and we refused. Always worth
        // a person's attention: it is either an attack or a bug in the amount
        // handling, and both matter.
        LogRecord::error("a payment was reported successful and refused")
            .internal("order_id", order.id.to_string())
            .internal("reason", reason.clone())
            .emit();
    }

    state.metrics.increment(
        names::PAYMENTS_SETTLED,
        LabelSet::new().enumerated(
            "outcome",
            match settled {
                OrderState::Paid => "paid",
                OrderState::Declined => "declined",
                _ => "failed",
            },
        ),
    );

    Ok(updated.state)
}

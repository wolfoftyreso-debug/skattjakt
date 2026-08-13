//! Orders and payments.
//!
//! Every method here runs inside a `Tenant` transaction except the two that
//! cannot: resolving a provider reference to its company, which a callback
//! needs before it knows whose tenant to open, and the reconciliation sweep,
//! which is maintenance across all tenants. Both are marked and both are
//! narrow.
//!
//! The one method worth reading closely is [`Tenant::redeem_order`]. It is the
//! whole double-spend defence, and it is one statement on purpose.

use chrono::{DateTime, Duration, Utc};
use sqlx::Row;
use uuid::Uuid;

use skattjakt_core::{AnalysisId, CompanyId, Money};
use skattjakt_payments::{OrderState, Product};

use crate::{Store, StoreError, StoreResult, Tenant};

#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub id: Uuid,
    pub company_id: CompanyId,
    pub product: Product,
    pub amount: Money,
    pub state: OrderState,
    pub analysis_id: Option<AnalysisId>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub paid_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Payment {
    pub id: Uuid,
    pub order_id: Uuid,
    pub company_id: CompanyId,
    pub provider: String,
    pub provider_reference: String,
    pub state: String,
    pub amount: Money,
    pub provider_status: Option<String>,
    pub error_code: Option<String>,
    pub lookups: i32,
}

fn order_from(row: &sqlx::postgres::PgRow) -> StoreResult<Order> {
    let product: String = row.get("product");
    let state: String = row.get("state");
    Ok(Order {
        id: row.get("id"),
        company_id: CompanyId::from_uuid(row.get("company_id")),
        product: Product::parse(&product)
            .ok_or_else(|| StoreError::Invalid(format!("unknown product {product}")))?,
        amount: Money::from_ore(row.get::<i64, _>("amount_ore")),
        state: OrderState::parse(&state)
            .ok_or_else(|| StoreError::Invalid(format!("unknown order state {state}")))?,
        analysis_id: row
            .get::<Option<Uuid>, _>("analysis_id")
            .map(AnalysisId::from_uuid),
        note: row.get("note"),
        created_at: row.get("created_at"),
        paid_at: row.get("paid_at"),
    })
}

fn payment_from(row: &sqlx::postgres::PgRow) -> Payment {
    Payment {
        id: row.get("id"),
        order_id: row.get("order_id"),
        company_id: CompanyId::from_uuid(row.get("company_id")),
        provider: row.get("provider"),
        provider_reference: row.get("provider_reference"),
        state: row.get("state"),
        amount: Money::from_ore(row.get::<i64, _>("amount_ore")),
        provider_status: row.get("provider_status"),
        error_code: row.get("error_code"),
        lookups: row.get("lookups"),
    }
}

const ORDER_COLUMNS: &str = "id, company_id, product, amount_ore, state, analysis_id, note, \
                             created_at, paid_at";
const PAYMENT_COLUMNS: &str = "id, order_id, company_id, provider, provider_reference, state, \
                               amount_ore, provider_status, error_code, lookups";

impl Tenant<'_> {
    /// Creates an order at the product's current price.
    pub async fn create_order(&mut self, product: Product) -> StoreResult<Order> {
        let row = sqlx::query(&format!(
            "INSERT INTO orders (company_id, product, amount_ore)
             VALUES (current_company_id(), $1, $2)
             RETURNING {ORDER_COLUMNS}"
        ))
        .bind(product.as_str())
        .bind(product.price().ore())
        .fetch_one(&mut *self.tx)
        .await?;
        order_from(&row)
    }

    pub async fn order(&mut self, id: Uuid) -> StoreResult<Order> {
        let row = sqlx::query(&format!("SELECT {ORDER_COLUMNS} FROM orders WHERE id = $1"))
            .bind(id)
            .fetch_optional(&mut *self.tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        order_from(&row)
    }

    /// Records a payment attempt against an order and moves it to
    /// `awaiting_payment`.
    pub async fn start_payment(
        &mut self,
        order: &Order,
        provider: &str,
        provider_reference: &str,
    ) -> StoreResult<Payment> {
        let row = sqlx::query(&format!(
            "INSERT INTO payments (order_id, company_id, provider, provider_reference, amount_ore)
             VALUES ($1, current_company_id(), $2, $3, $4)
             RETURNING {PAYMENT_COLUMNS}"
        ))
        .bind(order.id)
        .bind(provider)
        .bind(provider_reference)
        .bind(order.amount.ore())
        .fetch_one(&mut *self.tx)
        .await?;

        sqlx::query(
            "UPDATE orders SET state = 'awaiting_payment', updated_at = now()
             WHERE id = $1 AND state = 'created'",
        )
        .bind(order.id)
        .execute(&mut *self.tx)
        .await?;

        Ok(payment_from(&row))
    }

    pub async fn payment_by_reference(
        &mut self,
        provider: &str,
        provider_reference: &str,
    ) -> StoreResult<Payment> {
        let row = sqlx::query(&format!(
            "SELECT {PAYMENT_COLUMNS} FROM payments
             WHERE provider = $1 AND provider_reference = $2"
        ))
        .bind(provider)
        .bind(provider_reference)
        .fetch_optional(&mut *self.tx)
        .await?
        .ok_or(StoreError::NotFound)?;
        Ok(payment_from(&row))
    }

    /// Records that the provider was asked, whatever the answer.
    pub async fn record_lookup(
        &mut self,
        payment_id: Uuid,
        provider_status: Option<&str>,
    ) -> StoreResult<()> {
        sqlx::query(
            "UPDATE payments
             SET lookups = lookups + 1, provider_status = $2, updated_at = now()
             WHERE id = $1",
        )
        .bind(payment_id)
        .bind(provider_status)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    /// Settles a payment and its order together.
    ///
    /// Idempotent by construction: the order update only fires while the order
    /// is still unsettled, so a callback delivered three times settles once and
    /// the two later ones change nothing. Swish does redeliver, and a design
    /// that needed exactly-once delivery would be a design that breaks in
    /// normal operation.
    pub async fn settle_payment(
        &mut self,
        payment_id: Uuid,
        order_id: Uuid,
        state: OrderState,
        note: Option<&str>,
    ) -> StoreResult<Order> {
        let payment_state = match state {
            OrderState::Paid => "paid",
            OrderState::Declined => "declined",
            _ => "failed",
        };

        sqlx::query(
            "UPDATE payments
             SET state = $2, settled_at = coalesce(settled_at, now()),
                 error_code = coalesce($3, error_code), updated_at = now()
             WHERE id = $1 AND state = 'pending'",
        )
        .bind(payment_id)
        .bind(payment_state)
        .bind(note)
        .execute(&mut *self.tx)
        .await?;

        sqlx::query(
            "UPDATE orders
             SET state = $2,
                 paid_at = CASE WHEN $2 = 'paid' THEN coalesce(paid_at, now()) ELSE paid_at END,
                 note = coalesce($3, note),
                 updated_at = now()
             WHERE id = $1 AND state IN ('created', 'awaiting_payment')",
        )
        .bind(order_id)
        .bind(state.as_str())
        .bind(note)
        .execute(&mut *self.tx)
        .await?;

        self.order(order_id).await
    }

    /// Spends a paid order on one analysis.
    ///
    /// **The double-spend defence, and the reason it is one statement.** The
    /// `WHERE state = 'paid'` and the move to `consumed` happen in the same
    /// row lock, so two requests racing on the same order cannot both observe
    /// `paid`. The loser updates nothing and gets `NotFound`, which the caller
    /// turns into a clear refusal.
    ///
    /// A check-then-act in the handler would let both through under exactly the
    /// load where it matters — a customer double-tapping on a slow connection.
    pub async fn redeem_order(
        &mut self,
        order_id: Uuid,
        analysis_id: AnalysisId,
    ) -> StoreResult<Order> {
        let row = sqlx::query(&format!(
            "UPDATE orders
             SET state = 'consumed', analysis_id = $2, consumed_at = now(), updated_at = now()
             WHERE id = $1 AND state = 'paid'
             RETURNING {ORDER_COLUMNS}"
        ))
        .bind(order_id)
        .bind(analysis_id.as_uuid())
        .fetch_optional(&mut *self.tx)
        .await?
        .ok_or(StoreError::NotFound)?;
        order_from(&row)
    }

    /// Marks a paid order as owing a refund, because what it bought could not
    /// be delivered.
    ///
    /// The system cannot make a refund — see the `skattjakt-payments` crate
    /// documentation — so this is how it says one is owed rather than pretending
    /// otherwise.
    pub async fn mark_refund_owed(&mut self, order_id: Uuid, reason: &str) -> StoreResult<()> {
        sqlx::query(
            "UPDATE orders SET state = 'refund_owed', note = $2, updated_at = now()
             WHERE id = $1 AND state IN ('paid', 'consumed')",
        )
        .bind(order_id)
        .bind(reason)
        .execute(&mut *self.tx)
        .await?;
        Ok(())
    }

    pub async fn orders(&mut self, limit: i64) -> StoreResult<Vec<Order>> {
        let rows = sqlx::query(&format!(
            "SELECT {ORDER_COLUMNS} FROM orders ORDER BY created_at DESC LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(&mut *self.tx)
        .await?;
        rows.iter().map(order_from).collect()
    }
}

impl Store {
    /// Which company owns a provider reference.
    ///
    /// The one query in the payment path that runs outside a tenant scope,
    /// because a callback arrives with no session and cannot know whose tenant
    /// to open until it has asked. It returns an id and nothing else; every
    /// subsequent read and write happens inside that company's scope.
    ///
    /// Safe to expose to an unauthenticated caller because the reference is 32
    /// hexadecimal characters we generated, and knowing one buys the ability to
    /// make us ask Swish a question — not to change anything.
    pub async fn company_for_payment_reference(
        &self,
        provider: &str,
        provider_reference: &str,
    ) -> StoreResult<Option<CompanyId>> {
        let row = sqlx::query(
            "SELECT company_id FROM payments WHERE provider = $1 AND provider_reference = $2",
        )
        .bind(provider)
        .bind(provider_reference)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|row| CompanyId::from_uuid(row.get("company_id"))))
    }

    /// Payments nobody has resolved, oldest first.
    ///
    /// The reconciliation sweep reads this. A callback that never arrives — a
    /// deploy during the payer's thirty seconds, a network partition, Swish
    /// having a bad minute — must not leave a customer who paid staring at a
    /// spinner. Polling is what turns the callback from a requirement into an
    /// optimisation.
    ///
    /// Cross-tenant by necessity: it is maintenance over every company's
    /// payments, and it returns only the identifiers needed to go and ask.
    pub async fn unsettled_payments(
        &self,
        older_than: Duration,
        limit: i64,
    ) -> StoreResult<Vec<(CompanyId, Uuid, String, String)>> {
        let rows = sqlx::query(
            "SELECT company_id, id, provider, provider_reference
             FROM payments
             WHERE state = 'pending' AND updated_at < now() - $1::interval
             ORDER BY updated_at
             LIMIT $2",
        )
        .bind(older_than)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    CompanyId::from_uuid(row.get("company_id")),
                    row.get("id"),
                    row.get("provider"),
                    row.get("provider_reference"),
                )
            })
            .collect())
    }
}

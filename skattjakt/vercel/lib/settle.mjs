// The single settlement path. The callback calls it, the cron sweep calls it,
// and a client polling an order does not — because a client must never be able
// to drive traffic at the payment provider.
import { asTenant, db } from './db.mjs';
import { lookupPayment } from './payments.mjs';

export async function settleByReference(reference) {
  const [row] = await db()`
    SELECT company_for_payment_reference('swish', ${reference}) AS company_id`;
  if (!row?.company_id) return 'unknown';
  return settle(row.company_id, reference);
}

async function settle(companyId, reference) {
  const order = await asTenant(companyId, async (tx) => {
    const [p] = await tx`
      SELECT p.id, p.order_id, o.state, o.amount_ore
        FROM payments p JOIN orders o ON o.id = p.order_id
       WHERE p.provider = 'swish' AND p.provider_reference = ${reference}`;
    return p;
  });
  if (!order) return 'unknown';
  // Redelivered callback. Silent, not an error.
  if (order.state === 'paid' || order.state === 'declined' || order.state === 'failed') {
    return 'accepted';
  }

  let status;
  try {
    status = await lookupPayment(reference);
  } catch (error) {
    // A provider we cannot reach is not a failed payment. Leaving it pending
    // means the sweep tries again; failing it here would decline a payment the
    // customer may well have made.
    console.warn('could not ask swish about a payment', error.message);
    return 'failed';
  }

  const paid = status.status === 'PAID';
  const amountMatches = Math.round(Number(status.amount) * 100) === order.amount_ore;
  let next;
  if (!paid) {
    next = status.status === 'DECLINED' || status.status === 'ERROR' ? 'declined' : null;
  } else if (!amountMatches) {
    // Reported successful and refused. Always worth a person's attention: it is
    // either an attack or a bug in the amount handling, and both matter.
    console.error(JSON.stringify({
      event: 'payment_refused', order_id: order.order_id,
      reason: 'amount does not match the order',
    }));
    next = 'failed';
  } else {
    next = 'paid';
  }
  if (!next) return 'accepted';   // still pending; the sweep will ask again

  await asTenant(companyId, async (tx) => {
    await tx`UPDATE orders SET state = ${next}, settled_at = now() WHERE id = ${order.order_id}`;
    await tx`UPDATE payments SET last_lookup_at = now(), last_status = ${status.status}
              WHERE id = ${order.id}`;
  });
  return 'accepted';
}

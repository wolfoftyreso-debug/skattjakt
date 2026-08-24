// POST /api/analyse — one analysis, inline.
//
// There is no job, no queue and nothing to poll. That is not a simplification
// for the platform's sake: a hundred generated scenarios ran at a median of
// 3 ms natively and 1.7 ms through the WebAssembly module, so the request
// finishes faster than a poll would have taken to set up. The queue in the
// original service existed for a pipeline whose slow step was a model call.
import { analyse } from '../lib/engine.mjs';
import { problem, secure, tooLarge } from '../lib/http.mjs';
import { paymentsRequired } from '../lib/payments.mjs';
import { asTenant } from '../lib/db.mjs';

export const config = { runtime: 'nodejs', maxDuration: 30 };

export default async function handler(req, res) {
  if (req.method !== 'POST') {
    return problem(res, 405, 'method_not_allowed', 'POST an analysis request.');
  }
  if (tooLarge(req)) {
    return problem(res, 413, 'payload_too_large',
      'Underlaget är för stort. Ladda upp det som separata dokument.');
  }

  const body = req.body ?? {};
  const { documents, profile, audience, accounts_state: accountsState, order_id: orderId } = body;
  if (!Array.isArray(documents) || documents.length === 0) {
    return problem(res, 400, 'no_documents', 'Ange minst ett dokument.');
  }
  if (!profile || typeof profile !== 'object') {
    return problem(res, 400, 'no_profile', 'Ange bolagets uppgifter i "profile".');
  }

  // The payment gate, before any work is done. `paymentsRequired()` defaults to
  // ON when a provider is configured — the opposite of what the Rust service
  // did, and the reason is in lib/payments.mjs.
  if (paymentsRequired()) {
    if (!orderId) {
      return problem(res, 402, 'payment_required',
        'Analysen måste dras mot en betald order.');
    }
    const redeemed = await redeemOrder(orderId, audience);
    if (redeemed.error) {
      return problem(res, redeemed.status, redeemed.error, redeemed.detail);
    }
    // The audience comes from the order, never from the request: a customer who
    // paid 29 kr for Privatanalys must not be able to ask for the accountant's
    // layer by changing a query parameter.
    body.audience = redeemed.audience;
  }

  try {
    const report = await analyse({
      documents,
      profile,
      audience: body.audience ?? audience,
      accounts_state: accountsState,
    });
    return secure(res).status(200).json({ report });
  } catch (error) {
    if (error.fromEngine) {
      // The engine's messages name the document and the reason and carry no
      // internals; they are written to be read by the person who uploaded it.
      return problem(res, 422, 'analysis_failed', error.message);
    }
    console.error('analysis failed', error);
    return problem(res, 500, 'internal_error', 'Analysen kunde inte genomföras.');
  }
}

/**
 * Spends one paid order on one analysis, atomically.
 *
 * A single conditional UPDATE, so two requests racing on the same order cannot
 * both win, and an order that already names an analysis answers with it rather
 * than with a refusal — a customer whose request timed out and who pressed the
 * button again has already paid.
 */
async function redeemOrder(orderId, requestedAudience) {
  try {
    return await asTenant(await companyForOrder(orderId), async (tx) => {
      const [row] = await tx`
        UPDATE orders
           SET redeemed_at = now()
         WHERE id = ${orderId}
           AND state = 'paid'
           AND redeemed_at IS NULL
        RETURNING audience`;
      if (row) return { audience: row.audience };

      const [existing] = await tx`
        SELECT state, audience, redeemed_at FROM orders WHERE id = ${orderId}`;
      if (!existing) {
        return { status: 404, error: 'unknown_order', detail: 'Ordern finns inte.' };
      }
      if (existing.state !== 'paid') {
        return {
          status: 402, error: 'payment_required',
          detail: `Ordern är inte betald (${existing.state}).`,
        };
      }
      // Already spent — and it bought them something. Answer with the layer it
      // bought rather than refusing a customer who has paid.
      return { audience: existing.audience };
    });
  } catch (error) {
    console.error('order redemption failed', error);
    return { status: 503, error: 'order_unavailable', detail: 'Ordern kunde inte läsas.' };
  } finally {
    void requestedAudience;
  }
}

/**
 * The one query outside a tenant scope, and it returns an id and nothing else.
 *
 * Same shape as `company_for_payment_reference` in migration 0012: a narrow
 * SECURITY DEFINER function with a pinned search_path, revoked from PUBLIC.
 */
async function companyForOrder(orderId) {
  const { db } = await import('../lib/db.mjs');
  const [row] = await db()`SELECT company_for_order(${orderId}) AS company_id`;
  if (!row?.company_id) throw new Error('unknown order');
  return row.company_id;
}

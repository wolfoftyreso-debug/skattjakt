// POST /api/payments/callback — Swish tells us something happened.
//
// Unauthenticated on purpose, and safe for one reason: the body carries no
// authority. It names a payment; the truth comes from asking Swish. That is
// unchanged from the Rust service and is the property that makes the whole
// design work.
//
// Two differences on Vercel. The reconciliation sweep that used to run in a
// worker is now a cron function, and this endpoint is rate-limited — a
// serverless function that makes an outbound mutual-TLS call per POST is an
// amplifier, and the Rust service had no limit here either. That was a real
// gap, not a platform one.
import { settleByReference } from '../../lib/settle.mjs';
import { secure } from '../../lib/http.mjs';

export const config = { runtime: 'nodejs', maxDuration: 15 };

export default async function handler(req, res) {
  if (req.method !== 'POST') return secure(res).status(405).end();

  // `id`, not `payeePaymentReference`. Two different identifiers: this one is
  // the instruction id Swish assigned, which is what `payments.provider_
  // reference` holds. Reading the wrong one is why the callback resolved
  // nothing for as long as it existed.
  const reference = req.body?.id;
  if (typeof reference !== 'string' || reference.length === 0) {
    console.warn('a payment callback carried no reference we could read');
    return secure(res).status(200).end();
  }

  try {
    const outcome = await settleByReference(reference);
    // Three outcomes, not two. `unknown` is what a forged callback looks like
    // and also what a broken one looks like; both are worth counting.
    console.info(JSON.stringify({ event: 'payment_callback', outcome }));
  } catch (error) {
    console.error('payment callback failed', error);
  }
  // Always 200. Swish retries on anything else, and the sweep is the guarantee.
  return secure(res).status(200).end();
}

// Whether an analysis must be paid for, and how settlement decides.
//
// The default is the one thing that changed in the move, and it changed because
// it was wrong: `SKATTJAKT_PAYMENTS_REQUIRED` used to default to **false**, so a
// deployment that forgot one environment variable displayed prices and gave the
// product away. That was measured, not suspected — `POST /v1/analyses/stored`
// answered 202 with no order against a shop showing 69 kr.
//
// Deny by default now: payment is required whenever a provider is configured,
// and an operator running an internal deployment turns it off explicitly.
export function paymentsRequired() {
  const explicit = process.env.SKATTJAKT_PAYMENTS_REQUIRED;
  if (explicit !== undefined && explicit !== '') {
    return explicit !== '0' && explicit.toLowerCase() !== 'false';
  }
  return swishConfigured();
}

export function swishConfigured() {
  return Boolean(
    process.env.SWISH_PAYEE_ALIAS &&
    process.env.SWISH_CERT_PEM &&
    process.env.SWISH_KEY_PEM &&
    process.env.SWISH_CALLBACK_URL,
  );
}

/**
 * Asks Swish about one payment, over mutual TLS.
 *
 * The client certificate lives in an environment variable rather than on disk,
 * because a Vercel function has no disk worth the name. Node's https agent
 * takes the PEM directly, so nothing is written out.
 *
 * This is the ONLY thing that decides whether an order is paid. The callback
 * body is never trusted — it names a payment and nothing more. That property is
 * what makes an unauthenticated callback safe, and it is unchanged from the
 * Rust service.
 */
export async function lookupPayment(instructionId) {
  const https = await import('node:https');
  const agent = new https.Agent({
    cert: process.env.SWISH_CERT_PEM,
    key: process.env.SWISH_KEY_PEM,
    ca: process.env.SWISH_CA_PEM || undefined,
    keepAlive: true,
  });
  const base = process.env.SWISH_BASE_URL
    ?? 'https://cpc.getswish.net/swish-cpcapi';
  const url = `${base}/api/v1/paymentrequests/${instructionId}`;

  const response = await fetch(url, { agent, headers: { accept: 'application/json' } });
  if (!response.ok) {
    throw new Error(`swish lookup failed: ${response.status}`);
  }
  return response.json();
}

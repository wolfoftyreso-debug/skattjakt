// GET /api/health — is this deployment able to serve, and with what rules.
import { ruleSetVersion } from '../lib/engine.mjs';
import { paymentsRequired, swishConfigured } from '../lib/payments.mjs';
import { secure } from '../lib/http.mjs';

export const config = { runtime: 'nodejs' };

export default function handler(_req, res) {
  let rules = null;
  let ok = true;
  try {
    rules = ruleSetVersion();
  } catch (error) {
    ok = false;
    rules = error.message;
  }
  // Stated rather than implied. A deployment that cannot say which rule set it
  // is running is one whose reports cannot be reproduced, and one that gives
  // analyses away without saying so is the defect this replaced.
  return secure(res).status(ok ? 200 : 503).json({
    ok,
    rule_set: rules,
    database_configured: Boolean(process.env.DATABASE_URL),
    payments_required: paymentsRequired(),
    swish_configured: swishConfigured(),
    model_configured: Boolean(process.env.ANTHROPIC_API_KEY),
  });
}

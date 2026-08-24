// The bits every function needs, in one place so they cannot drift.

/** Security headers, matching what the Rust service served. */
export function secure(res) {
  res.setHeader('content-security-policy',
    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; " +
    "form-action 'self'; frame-ancestors 'none'; base-uri 'none'");
  res.setHeader('strict-transport-security', 'max-age=31536000; includeSubDomains');
  res.setHeader('x-content-type-options', 'nosniff');
  res.setHeader('referrer-policy', 'no-referrer');
  res.setHeader('permissions-policy', 'geolocation=(), microphone=(), camera=()');
  return res;
}

export function problem(res, status, title, detail) {
  secure(res).status(status).json({ title, detail });
}

/**
 * Reads a JSON body, bounded.
 *
 * Vercel parses the body for us, but the limit is the one thing worth stating
 * rather than inheriting: a 30 MB scanned annual report posted as base64 JSON
 * is a request that should be refused at the edge, not buffered.
 */
export const MAX_BODY_BYTES = 8 * 1024 * 1024;

export function tooLarge(req) {
  const declared = Number(req.headers['content-length'] ?? 0);
  return declared > MAX_BODY_BYTES;
}

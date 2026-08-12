//! Response headers that make a browser enforce what the server intends.
//!
//! There were none. The API serves two HTML pages and set no policy at all —
//! no `Content-Security-Policy`, no `nosniff`, no framing rule, no referrer
//! rule. Every one of those is a browser-side control the server has to ask
//! for, and a browser that is not asked does the permissive thing.
//!
//! ## Why the policy can be this strict
//!
//! The interface was built to load nothing from anywhere else — no CDN, no font
//! service, no analytics — which is what makes `default-src 'none'` possible
//! rather than aspirational. The one thing standing in the way was that both
//! pages carried their script inline, and an inline `<script>` needs either
//! `'unsafe-inline'` — which defeats the point, since an injected script is
//! also inline — or a hash that has to be recomputed on every edit. So the
//! scripts moved into files and the policy needs no exception:
//!
//! ```text
//! default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:;
//! connect-src 'self'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'
//! ```
//!
//! `style-src 'self'` also blocks inline `style="…"` attributes, of which there
//! were fourteen. They became four utility classes. The one that genuinely
//! varies with data — a bar's width in the sensitivity chart — is set through
//! the CSSOM, which the policy does not govern.
//!
//! ## The API responses get it too
//!
//! A JSON response is not rendered, so a policy on it looks pointless. It is
//! not: a browser navigated directly to an endpoint, or a response mistaken for
//! HTML by a sniffing browser, is exactly the case `default-src 'none'` and
//! `nosniff` exist for. The cost is 300 bytes a response.

use axum::extract::Request;
use axum::http::{header, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

/// The policy for the two HTML pages.
///
/// `frame-ancestors 'none'` is the one that stops clickjacking, and it is a
/// header rather than a meta tag because a meta tag is ignored for it.
/// `form-action 'none'` is deliberate: every form on these pages is submitted
/// by script through `fetch`, so a form that navigates anywhere is not
/// something the interface does and is something an injection would.
const PAGE_POLICY: &str = "default-src 'none'; \
     script-src 'self'; \
     style-src 'self'; \
     img-src 'self' data:; \
     font-src 'self'; \
     connect-src 'self'; \
     form-action 'none'; \
     frame-ancestors 'none'; \
     base-uri 'none'; \
     object-src 'none'";

/// The policy for everything that is not a page.
///
/// Nothing at all is allowed to load, because nothing should be loading: this
/// is JSON, YAML or Prometheus text.
const DATA_POLICY: &str = "default-src 'none'; frame-ancestors 'none'; base-uri 'none'; \
     sandbox";

/// How long a browser should refuse to talk to this host over plain HTTP.
///
/// One year, with subdomains, and `preload` deliberately absent: preloading is
/// a submission to a list baked into browsers, it is slow to undo, and it is
/// not a decision a header should make on an operator's behalf.
///
/// Sent unconditionally, including over plain HTTP. That looked wrong at first
/// and is not: RFC 6797 §8.1 requires a user agent to *ignore* this header when
/// it arrives over a non-secure transport, so a development server on loopback
/// cannot pin anything. The alternative — gating it on the development switch
/// that turns off `Secure` cookies — would add a way for the header to be
/// silently absent in production, which is the failure that actually matters.
/// In production the API speaks plain HTTP to a TLS-terminating ingress anyway,
/// so any condition on the API's own transport would be answering the wrong
/// question.
const HSTS: &str = "max-age=31536000; includeSubDomains";

/// Sets the headers on every response.
pub async fn secure(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    let is_page = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));

    let policy = if is_page { PAGE_POLICY } else { DATA_POLICY };
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(policy),
    );

    // Stops a browser guessing a content type from the bytes. Without it, a
    // JSON response holding text a customer supplied can be sniffed as HTML and
    // rendered — which turns a stored string into stored script.
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );

    // `frame-ancestors` above covers modern browsers; this covers the rest.
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));

    // No URL leaves in a `Referer`. The paths here carry company, analysis and
    // simulation identifiers, and a referrer sends them to whatever the user
    // clicks through to.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );

    // Nothing here needs a camera, a microphone, a location or a payment
    // handler, so nothing may ask — including anything injected into the page.
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static(
            "accelerometer=(), camera=(), geolocation=(), gyroscope=(), \
             magnetometer=(), microphone=(), payment=(), usb=()",
        ),
    );

    // Isolates the browsing context, so a window this page opened — or one that
    // opened it — cannot reach into it.
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );

    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static(HSTS),
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_policy_has_no_unsafe_directive() {
        // The point of moving the scripts out of the HTML. If this ever fails,
        // an injected `<script>` executes and the policy is decoration.
        assert!(!PAGE_POLICY.contains("unsafe-inline"));
        assert!(!PAGE_POLICY.contains("unsafe-eval"));
        assert!(!PAGE_POLICY.contains('*'));
    }

    #[test]
    fn the_page_policy_denies_by_default_and_names_every_source_it_allows() {
        assert!(PAGE_POLICY.starts_with("default-src 'none'"));
        for directive in [
            "script-src 'self'",
            "style-src 'self'",
            "connect-src 'self'",
            "frame-ancestors 'none'",
            "form-action 'none'",
            "base-uri 'none'",
            "object-src 'none'",
        ] {
            assert!(PAGE_POLICY.contains(directive), "missing {directive}");
        }
    }

    #[test]
    fn the_data_policy_allows_nothing_at_all() {
        assert!(DATA_POLICY.starts_with("default-src 'none'"));
        assert!(DATA_POLICY.contains("sandbox"));
        assert!(!DATA_POLICY.contains("'self'"));
    }

    #[test]
    fn hsts_is_a_year_and_does_not_preload() {
        assert!(HSTS.contains("max-age=31536000"));
        assert!(HSTS.contains("includeSubDomains"));
        assert!(
            !HSTS.contains("preload"),
            "preloading is an operator's decision, not a header's"
        );
    }
}

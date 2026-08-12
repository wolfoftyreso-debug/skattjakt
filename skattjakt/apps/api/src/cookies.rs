//! Session cookies for the web client, and the CSRF defence they require.
//!
//! ## Why cookies rather than a token in JavaScript
//!
//! `SKATTJAKT_CLIENT_ARCHITECTURE.md` §2 states the rule and this is where it
//! is enforced: a refresh token that JavaScript can read is a refresh token an
//! XSS can steal, and a stolen refresh token is weeks of access. An `HttpOnly`
//! cookie is not readable by script at all, so the same XSS gets whatever it
//! can do inside the page while it is open — bad, but bounded, and it ends when
//! the tab closes.
//!
//! iOS and Android do *not* get cookies. They have the Keychain and the
//! Keystore, which are a better place than a cookie jar, and a native client
//! handling `Set-Cookie` is a native client reimplementing a browser.
//!
//! ## The cost cookies bring, and how it is paid
//!
//! A cookie is sent by the browser automatically, which is the whole point and
//! also the entire CSRF problem: another site can cause a request that carries
//! the customer's session. Two things stop it here, and both are needed.
//!
//! **`SameSite=Strict`.** The browser does not attach the cookie to any request
//! that did not originate from this site — not a form post, not an image, not a
//! top-level navigation. This is the strong half.
//!
//! **A required custom header.** Cookie authentication is accepted *only* when
//! the request also carries `x-skattjakt-client`. A cross-origin request cannot
//! set a custom header without a CORS preflight, and this API grants no CORS
//! permission, so the preflight fails and the request never happens. This is
//! the half that still works on a browser whose `SameSite` handling is old or
//! quirky, and it costs one header the clients already send.
//!
//! The bearer path is unaffected: a mobile client sends `Authorization`, which
//! a browser never attaches automatically, so CSRF does not apply to it.

use axum::http::{header, HeaderMap, HeaderValue};
use chrono::{DateTime, Utc};

/// The access token cookie.
pub const ACCESS_COOKIE: &str = "skattjakt_session";
/// The refresh token cookie.
///
/// Scoped by `Path` to the one endpoint that consumes it, so it is not attached
/// to every request. A credential that is sent a hundred times a session has a
/// hundred chances to end up somewhere it should not.
pub const REFRESH_COOKIE: &str = "skattjakt_refresh";
const REFRESH_PATH: &str = "/v1/auth/refresh";

/// The header whose presence is required before a cookie is trusted.
pub const CLIENT_HEADER: &str = "x-skattjakt-client";

/// Whether cookies should be marked `Secure`.
///
/// On unless explicitly disabled, and disabling it is only for a local HTTP
/// development server: a `Secure` cookie is simply not sent over plain HTTP, so
/// the alternative to this switch is developers turning off something worse.
fn secure() -> bool {
    std::env::var("SKATTJAKT_INSECURE_COOKIES")
        .map(|v| v != "1" && !v.eq_ignore_ascii_case("true"))
        .unwrap_or(true)
}

fn attributes(path: &str, expires: DateTime<Utc>) -> String {
    let mut out = format!(
        "; Path={path}; HttpOnly; SameSite=Strict; Expires={}",
        // RFC 7231 date format. A cookie with an unparseable Expires is treated
        // as a session cookie, which would silently survive longer than the
        // token inside it.
        expires.format("%a, %d %b %Y %H:%M:%S GMT")
    );
    if secure() {
        out.push_str("; Secure");
    }
    out
}

/// The `Set-Cookie` values for a freshly issued session.
pub fn issue(
    access: &str,
    access_expires: DateTime<Utc>,
    refresh: &str,
    refresh_expires: DateTime<Utc>,
) -> Vec<String> {
    vec![
        format!(
            "{ACCESS_COOKIE}={access}{}",
            attributes("/", access_expires)
        ),
        format!(
            "{REFRESH_COOKIE}={refresh}{}",
            attributes(REFRESH_PATH, refresh_expires)
        ),
    ]
}

/// The `Set-Cookie` values that clear a session.
///
/// Both cookies, with the same `Path` they were set with — a clear on the wrong
/// path leaves the cookie in place and the customer signed in after pressing
/// sign out.
pub fn clear() -> Vec<String> {
    let expired = DateTime::from_timestamp(0, 0).unwrap_or_default();
    vec![
        format!("{ACCESS_COOKIE}={}", attributes("/", expired)),
        format!("{REFRESH_COOKIE}={}", attributes(REFRESH_PATH, expired)),
    ]
}

/// Reads one cookie from a request.
///
/// Hand-parsed rather than through a cookie crate: the `Cookie` header is a
/// `;`-separated list of `name=value`, this reads two names, and a dependency
/// for that is a dependency to audit and keep patched (§33).
pub fn read(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())?
        .split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim().to_string())
}

/// Reads the access token from a cookie, if the request is allowed to use one.
///
/// Returns `None` without the client header even when the cookie is present.
/// That is the CSRF defence: a cross-origin request cannot set a custom header
/// without a preflight this API never grants, so a request that arrives with a
/// cookie and no header did not come from our own page.
pub fn access_token(headers: &HeaderMap) -> Option<String> {
    if !headers.contains_key(CLIENT_HEADER) {
        return None;
    }
    read(headers, ACCESS_COOKIE)
}

/// Reads the refresh token from a cookie, under the same rule.
pub fn refresh_token(headers: &HeaderMap) -> Option<String> {
    if !headers.contains_key(CLIENT_HEADER) {
        return None;
    }
    read(headers, REFRESH_COOKIE)
}

/// Appends `Set-Cookie` headers to a response.
pub fn apply(headers: &mut HeaderMap, cookies: Vec<String>) {
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            headers.append(header::SET_COOKIE, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn with_cookie(value: &str, client: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(value).unwrap());
        if client {
            headers.insert(CLIENT_HEADER, HeaderValue::from_static("web"));
        }
        headers
    }

    #[test]
    fn a_cookie_is_read_from_a_list() {
        let headers = with_cookie("other=1; skattjakt_session=abc123; third=x", true);
        assert_eq!(access_token(&headers).as_deref(), Some("abc123"));
    }

    #[test]
    fn whitespace_around_a_pair_is_tolerated() {
        // Browsers send `; ` between pairs; a parser that does not trim finds
        // nothing and the customer is silently signed out.
        let headers = with_cookie("a=1;  skattjakt_session=xyz  ; b=2", true);
        assert_eq!(access_token(&headers).as_deref(), Some("xyz"));
    }

    #[test]
    fn a_cookie_without_the_client_header_is_refused() {
        // The CSRF defence. Another site can cause the browser to send the
        // cookie; it cannot make it send a custom header without a preflight
        // this API never grants.
        let headers = with_cookie("skattjakt_session=abc123", false);
        assert_eq!(access_token(&headers), None);
        // The cookie is there — it is the header that is missing.
        assert_eq!(read(&headers, ACCESS_COOKIE).as_deref(), Some("abc123"));
    }

    #[test]
    fn the_refresh_cookie_is_under_the_same_rule() {
        let headers = with_cookie("skattjakt_refresh=r1", false);
        assert_eq!(refresh_token(&headers), None);
        let allowed = with_cookie("skattjakt_refresh=r1", true);
        assert_eq!(refresh_token(&allowed).as_deref(), Some("r1"));
    }

    /// Serialises the tests that touch `SKATTJAKT_INSECURE_COOKIES`.
    ///
    /// An environment variable is process-global and `cargo test` runs tests on
    /// parallel threads, so without this the test that switches the flag on can
    /// be running while the test that asserts cookies are `Secure` reads it.
    /// That failure is rare, looks exactly like a real regression in the
    /// strongest security property these cookies have, and would eventually be
    /// dismissed as flaky — which is the worst outcome available.
    static ENVIRONMENT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn an_issued_cookie_is_httponly_samesite_strict_and_secure() {
        let _guard = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SKATTJAKT_INSECURE_COOKIES");
        let cookies = issue(
            "a",
            Utc::now() + Duration::minutes(15),
            "r",
            Utc::now() + Duration::hours(12),
        );
        for cookie in &cookies {
            assert!(cookie.contains("HttpOnly"), "not HttpOnly: {cookie}");
            assert!(cookie.contains("SameSite=Strict"), "not Strict: {cookie}");
            assert!(cookie.contains("Secure"), "not Secure: {cookie}");
            assert!(cookie.contains("Expires="), "no expiry: {cookie}");
        }
    }

    #[test]
    fn the_refresh_cookie_is_scoped_to_the_endpoint_that_consumes_it() {
        // A credential attached to every request has many more chances to end
        // up somewhere it should not.
        let cookies = issue("a", Utc::now(), "r", Utc::now());
        let refresh = cookies
            .iter()
            .find(|c| c.starts_with(REFRESH_COOKIE))
            .unwrap();
        assert!(refresh.contains("Path=/v1/auth/refresh"));
        let access = cookies
            .iter()
            .find(|c| c.starts_with(ACCESS_COOKIE))
            .unwrap();
        assert!(access.contains("Path=/"));
    }

    #[test]
    fn clearing_uses_the_same_paths_as_issuing() {
        // A clear on the wrong path leaves the cookie in place, and the
        // customer is still signed in after pressing sign out.
        let issued = issue("a", Utc::now(), "r", Utc::now());
        let cleared = clear();
        for name in [ACCESS_COOKIE, REFRESH_COOKIE] {
            let path_of = |set: &[String]| {
                set.iter()
                    .find(|c| c.starts_with(name))
                    .and_then(|c| c.split("; ").find(|a| a.starts_with("Path=")))
                    .map(str::to_string)
            };
            assert_eq!(
                path_of(&issued),
                path_of(&cleared),
                "path differs for {name}"
            );
        }
    }

    #[test]
    fn the_expiry_is_in_the_format_a_browser_parses() {
        // An unparseable Expires makes it a session cookie, which would outlive
        // the token inside it.
        let cookies = issue(
            "a",
            DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            "r",
            Utc::now(),
        );
        assert!(
            cookies[0].contains("Expires=Tue, 14 Nov 2023 22:13:20 GMT"),
            "{}",
            cookies[0]
        );
    }

    #[test]
    fn secure_can_be_switched_off_only_explicitly() {
        let _guard = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SKATTJAKT_INSECURE_COOKIES", "1");
        assert!(!issue("a", Utc::now(), "r", Utc::now())[0].contains("Secure"));
        std::env::remove_var("SKATTJAKT_INSECURE_COOKIES");
        assert!(issue("a", Utc::now(), "r", Utc::now())[0].contains("Secure"));
    }
}

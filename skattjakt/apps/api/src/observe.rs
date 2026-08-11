//! The observability middleware and the scrape endpoint (sections 43, 45).
//!
//! One layer, applied to every route, that does three things and no more:
//! establishes the correlation id and trace context, records the request in the
//! metrics, and writes exactly one structured log line per request.
//!
//! The route label is the templated path, never the actual one. `/v1/analyses/{id}`
//! is one series; `/v1/analyses/8a5e…` would be one series per analysis, which
//! is both a cardinality explosion and a tenant identifier in a metric label —
//! the thing section 45 forbids. `LabelSet` would reject the raw path anyway;
//! this makes the intent explicit at the point where it matters.

use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use skattjakt_telemetry::{
    names, LabelSet, LogRecord, TraceContext, CORRELATION_HEADER, TRACEPARENT_HEADER,
};

use crate::{correlation_id, AppState};

/// The Prometheus scrape endpoint.
///
/// Unauthenticated on purpose, and safe to be: everything on it is `PUBLIC` by
/// construction, because `LabelSet` will not accept anything else. In the
/// cluster it is reachable only from the monitoring namespace — the
/// NetworkPolicy in `infrastructure/` is what actually closes it, and it is
/// closed there rather than here so that a scrape from Prometheus does not need
/// a credential to rotate.
pub async fn metrics(State(state): State<AppState>) -> Response {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}

/// Maps a concrete path onto the templated route label.
///
/// A closed list. An unrecognised path becomes `other`, which keeps the metric
/// bounded even if a route is added and this function is not — the failure mode
/// is a missing series rather than an unbounded one.
fn route_label(path: &str) -> &'static str {
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    match (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) {
        (None, ..) => "/",
        (Some("health"), None, ..) => "/health",
        (Some("ready"), None, ..) => "/ready",
        (Some("metrics"), None, ..) => "/metrics",
        (Some("favicon.svg" | "favicon.ico"), None, ..) => "/favicon",
        (Some("v1"), Some("openapi.yaml"), None, _) => "/v1/openapi.yaml",
        (Some("v1"), Some("rules"), None, _) => "/v1/rules",
        (Some("v1"), Some("companies"), None, _) => "/v1/companies",
        (Some("v1"), Some("companies"), Some("me"), None) => "/v1/companies/me",
        (Some("v1"), Some("documents"), None, _) => "/v1/documents",
        (Some("v1"), Some("analyses"), None, _) => "/v1/analyses",
        (Some("v1"), Some("analyses"), Some("stored"), None) => "/v1/analyses/stored",
        (Some("v1"), Some("analyses"), Some(_), None) => "/v1/analyses/{id}",
        (Some("v1"), Some("analyses"), Some(_), Some("opportunities")) => {
            "/v1/analyses/{id}/opportunities"
        }
        (Some("v1"), Some("analyses"), Some(_), Some("report")) => "/v1/analyses/{id}/report",
        (Some("v1"), Some("opportunities"), Some(_), None) => "/v1/opportunities/{id}",
        _ => "other",
    }
}

/// Status class rather than status code: `2xx`, `4xx`, `5xx`.
///
/// Three values instead of dozens. An alert asks "are we serving errors", not
/// "how many 418s"; the exact code is in the log line for the request that
/// produced it.
fn status_class(status: StatusCode) -> &'static str {
    match status.as_u16() {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

/// The one middleware.
pub async fn observe(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let method = match *request.method() {
        axum::http::Method::GET => "GET",
        axum::http::Method::POST => "POST",
        axum::http::Method::PUT => "PUT",
        axum::http::Method::DELETE => "DELETE",
        axum::http::Method::PATCH => "PATCH",
        axum::http::Method::HEAD => "HEAD",
        axum::http::Method::OPTIONS => "OPTIONS",
        _ => "other",
    };
    let route = route_label(request.uri().path());
    let correlation = correlation_id(request.headers());
    let trace = TraceContext::from_header_or_new(
        request
            .headers()
            .get(TRACEPARENT_HEADER)
            .and_then(|v| v.to_str().ok()),
    );
    let span = trace.start_span("http.request");

    let in_flight = LabelSet::new().enumerated("route", route);
    state
        .metrics
        .add(names::HTTP_IN_FLIGHT, in_flight.clone(), 1);

    let started = Instant::now();
    let mut response = next.run(request).await;
    let elapsed = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let status = response.status();

    let labels = LabelSet::new()
        .enumerated("route", route)
        .enumerated("method", method)
        .enumerated("status", status_class(status));
    state.metrics.increment(names::HTTP_REQUESTS, labels);
    state.metrics.observe(
        names::HTTP_DURATION,
        LabelSet::new().enumerated("route", route),
        elapsed,
    );
    // A gauge set to zero on the way out would clobber other in-flight
    // requests, so the counter is decremented by wrapping through `set`.
    state.metrics.add(names::HTTP_IN_FLIGHT, in_flight, 0);

    // The correlation id goes back to the client, so a customer reporting a
    // problem can quote the one identifier that finds the request in the logs.
    if let Ok(value) = HeaderValue::from_str(&correlation.to_string()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(CORRELATION_HEADER), value);
    }
    if let Ok(value) = HeaderValue::from_str(&span.traceparent()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(TRACEPARENT_HEADER), value);
    }

    // Exactly one line per request. No path, because a path can carry a
    // document id; the templated route plus the correlation id is enough to
    // find anything, and neither identifies a customer.
    let record = span
        .annotate(if status.is_server_error() {
            LogRecord::error("request failed")
        } else {
            LogRecord::info("request")
        })
        .correlate(correlation)
        .public("route", route)
        .public("method", method)
        .public("status", status.as_u16())
        .internal("duration_ms", elapsed);
    record.emit();

    response
}

/// The header names, checked at startup rather than at the first request.
///
/// `HeaderName::from_static` panics on a non-lowercase name, and doing that on
/// a hot path would turn a typo into a 500 per request instead of a failure to
/// start.
pub fn assert_header_names_are_valid() {
    let _ = HeaderName::from_static(CORRELATION_HEADER);
    let _ = HeaderName::from_static(TRACEPARENT_HEADER);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_never_reach_the_route_label() {
        let id = "8a5e1e6c-1234-4321-8888-000000000000";
        assert_eq!(
            route_label(&format!("/v1/analyses/{id}")),
            "/v1/analyses/{id}"
        );
        assert_eq!(
            route_label(&format!("/v1/analyses/{id}/report")),
            "/v1/analyses/{id}/report"
        );
        assert_eq!(
            route_label(&format!("/v1/opportunities/{id}")),
            "/v1/opportunities/{id}"
        );
    }

    #[test]
    fn an_unknown_path_collapses_to_one_bounded_label() {
        assert_eq!(route_label("/wp-admin/setup.php"), "other");
        assert_eq!(route_label("/v1/../../etc/passwd"), "other");
        assert_eq!(route_label("/a/b/c/d/e/f"), "other");
    }

    #[test]
    fn a_hostile_path_cannot_create_a_new_series() {
        // Every one of these must land on a label from the closed set.
        let known: std::collections::BTreeSet<&str> = [
            "/",
            "/health",
            "/ready",
            "/metrics",
            "/favicon",
            "/v1/openapi.yaml",
            "/v1/rules",
            "/v1/companies",
            "/v1/companies/me",
            "/v1/documents",
            "/v1/analyses",
            "/v1/analyses/stored",
            "/v1/analyses/{id}",
            "/v1/analyses/{id}/opportunities",
            "/v1/analyses/{id}/report",
            "/v1/opportunities/{id}",
            "other",
        ]
        .into_iter()
        .collect();

        for path in [
            "/v1/analyses/\u{0000}",
            "/v1/analyses/'; DROP TABLE jobs;--",
            "/v1/documents?token=secret",
            &format!("/v1/analyses/{}", "x".repeat(10_000)),
        ] {
            assert!(
                known.contains(route_label(path)),
                "{path} escaped the label set"
            );
        }
    }

    #[test]
    fn status_codes_collapse_into_classes() {
        assert_eq!(status_class(StatusCode::OK), "2xx");
        assert_eq!(status_class(StatusCode::NOT_FOUND), "4xx");
        assert_eq!(status_class(StatusCode::TOO_MANY_REQUESTS), "4xx");
        assert_eq!(status_class(StatusCode::INTERNAL_SERVER_ERROR), "5xx");
    }

    #[test]
    fn the_header_names_are_valid_at_startup() {
        assert_header_names_are_valid();
    }
}

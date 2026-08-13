use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::*;

const TOKEN: &str = "test-token";

const STATEMENT: &str = "\
RESULTATRÄKNING
Nettoomsättning                12 500 000
Personalkostnader              -5 800 000
Pensionskostnader                -240 000
Övriga externa kostnader       -2 100 000
Av- och nedskrivningar           -150 000
Skattemässigt resultat          3 000 000
Materiella anläggningstillgångar 1 800 000
Summa tillgångar                7 720 000
Summa eget kapital och skulder  7 720 000
";

fn state() -> AppState {
    let provider: Arc<dyn skattjakt_model::ModelProvider> = Arc::new(
        ScriptedProvider::new()
            .with(
                skattjakt_model::ReasoningTask::OpportunityDiscovery,
                json!({"candidates": []}),
            )
            .with(
                skattjakt_model::ReasoningTask::ContradictionCheck,
                json!({"verdicts": []}),
            ),
    );

    AppState {
        engine: Arc::new(RuleEngine::load_embedded().unwrap()),
        // The tests go through the real gateway, so they exercise the fence
        // check and the fallback check rather than a shortcut around them.
        gateway: Arc::new(skattjakt_gateway::ModelGateway::for_testing(
            provider.clone(),
        )),
        provider,
        password_verifier: Arc::new(skattjakt_identity::PasswordVerifier::new()),
        spans: skattjakt_telemetry::otlp::SpanExporter::disabled(),
        config: PipelineConfig::default(),
        api_token: Some(TOKEN.to_string()),
        admin_token: None,
        model_configured: true,
        // The stateless surface: analyses are computed and returned, never stored.
        store: None,
        blobs: Arc::new(skattjakt_store::FilesystemBlobStore::new(
            std::env::temp_dir().join("skattjakt-tests"),
        )),
        // Unconfigured: the stateless surface has no database to hold an order
        // in, and a provider that refuses is the honest state for it.
        payments: Arc::new(crate::payments::Payments::unconfigured()),
        merchant: Some(crate::shopfront::Merchant {
            name: "Testbolaget AB".into(),
            org_number: "556016-0680".into(),
            address: "Testgatan 1, 111 22 Stockholm".into(),
            email: "hej@example.test".into(),
            phone: None,
            vat_registered: true,
        }),
        // No database, so no queue: the stateless surface computes inline.
        queue: None,
        metrics: {
            let registry = skattjakt_telemetry::Registry::new();
            skattjakt_telemetry::metrics::register_all(&registry);
            registry
        },
    }
}

fn analysis_body(document_text: &str, fiscal_year: (&str, &str)) -> Value {
    json!({
        "company": {
            "name": "Testbolaget AB",
            "org_number": "556016-0680",
            "fiscal_year": {"start": fiscal_year.0, "end": fiscal_year.1},
            "employee_count": 8,
            "owner_count": 2,
            "in_group": false,
            "has_vehicles": false,
            "does_development_work": false,
            "owners_active_in_company": true
        },
        "documents": [{
            "filename": "bokslut.txt",
            "mime_type": "text/plain",
            "text": document_text
        }],
        "accounts_state": "preliminary"
    })
}

async fn send(state: AppState, request: Request<Body>) -> (StatusCode, Value) {
    let response = router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn post_analysis(body: Value, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/analyses")
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn health_needs_no_credentials_and_touches_nothing() {
    let request = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(state(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn readiness_reports_why_it_is_not_ready() {
    let mut degraded = state();
    degraded.model_configured = false;
    degraded.api_token = None;

    let request = Request::builder()
        .uri("/ready")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(degraded, request).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["ready"], false);
    assert_eq!(body["reasons"].as_array().unwrap().len(), 2);
    // The rule set version is reported even when not ready, so a failing probe
    // is diagnosable without shell access.
    assert_eq!(body["rule_set_version"], "se-2025.1");
}

#[tokio::test]
async fn readiness_is_green_when_everything_is_configured() {
    let request = Request::builder()
        .uri("/ready")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(state(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ready"], true);
    assert!(body["reasons"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn the_contract_is_served_from_the_running_build() {
    let request = Request::builder()
        .uri("/v1/openapi.yaml")
        .body(Body::empty())
        .unwrap();
    let response = router(state()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.starts_with("openapi: 3.1.0"));
    assert!(text.contains("/v1/analyses"));
}

#[tokio::test]
async fn the_analysis_route_rejects_a_missing_token() {
    let (status, body) = send(
        state(),
        post_analysis(analysis_body(STATEMENT, ("2025-01-01", "2025-12-31")), None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["title"], "unauthorized");
}

#[tokio::test]
async fn the_analysis_route_rejects_a_wrong_token() {
    let (status, _) = send(
        state(),
        post_analysis(
            analysis_body(STATEMENT, ("2025-01-01", "2025-12-31")),
            Some("wrong"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_unconfigured_token_closes_the_route_rather_than_opening_it() {
    let mut open = state();
    open.api_token = None;
    let (status, _) = send(
        open,
        post_analysis(
            analysis_body(STATEMENT, ("2025-01-01", "2025-12-31")),
            Some("anything"),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "no token must mean closed, not open"
    );
}

#[tokio::test]
async fn a_valid_analysis_returns_findings_with_evidence_and_a_disclaimer() {
    let (status, body) = send(
        state(),
        post_analysis(
            analysis_body(STATEMENT, ("2025-01-01", "2025-12-31")),
            Some(TOKEN),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(!body["disclaimer"].as_str().unwrap().is_empty());
    assert!(
        body["summary"]["identified_opportunities"]
            .as_u64()
            .unwrap()
            > 0
    );

    let opportunities = body["opportunities"].as_array().unwrap();
    for opportunity in opportunities {
        // Money is always an interval, never a single figure.
        assert!(opportunity["impact"]["low"].is_i64());
        assert!(opportunity["impact"]["high"].is_i64());
        assert!(!opportunity["recommended_action"]
            .as_str()
            .unwrap()
            .is_empty());
        // Nothing may be presented as established while the rule set is unreviewed.
        assert_ne!(opportunity["status"], "identified");
    }

    // Evidence is exposed, not summarised away.
    let with_rule = opportunities
        .iter()
        .find(|o| !o["rule_ids"].as_array().unwrap().is_empty())
        .expect("expected a rule-backed finding");
    let evidence = with_rule["evidence"].as_array().unwrap();
    assert!(evidence.iter().any(|e| e["type"] == "document_value"));
    assert!(evidence.iter().any(|e| e["type"] == "rule"));
}

#[tokio::test]
async fn an_invalid_organisationsnummer_is_rejected_at_the_edge() {
    let mut body = analysis_body(STATEMENT, ("2025-01-01", "2025-12-31"));
    body["company"]["org_number"] = json!("556016-0681");
    let (status, problem) = send(state(), post_analysis(body, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["title"], "invalid organisationsnummer");
}

#[tokio::test]
async fn a_tax_year_the_rule_set_does_not_cover_is_an_explicit_error() {
    // Section 31: better a clear refusal than a confident empty analysis.
    let (status, problem) = send(
        state(),
        post_analysis(
            analysis_body(STATEMENT, ("2030-01-01", "2030-12-31")),
            Some(TOKEN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["title"], "tax year not covered");
    assert!(problem["detail"].as_str().unwrap().contains("2030"));
}

#[tokio::test]
async fn a_document_that_does_not_match_its_declared_type_is_rejected() {
    let mut body = analysis_body(STATEMENT, ("2025-01-01", "2025-12-31"));
    body["documents"][0]["mime_type"] = json!("application/pdf");
    let (status, problem) = send(state(), post_analysis(body, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["title"], "content does not match its declared type");
}

#[tokio::test]
async fn an_unsupported_content_type_is_named_rather_than_guessed_at() {
    let mut body = analysis_body(STATEMENT, ("2025-01-01", "2025-12-31"));
    body["documents"][0]["mime_type"] = json!("application/x-msdownload");
    let (status, problem) = send(state(), post_analysis(body, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["title"], "unsupported document type");
}

#[tokio::test]
async fn an_empty_document_list_is_rejected() {
    let mut body = analysis_body(STATEMENT, ("2025-01-01", "2025-12-31"));
    body["documents"] = json!([]);
    let (status, problem) = send(state(), post_analysis(body, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["title"], "no documents");
}

#[tokio::test]
async fn a_base64_document_is_decoded_and_analysed() {
    let mut body = analysis_body(STATEMENT, ("2025-01-01", "2025-12-31"));
    body["documents"][0] = json!({
        "filename": "bokslut.txt",
        "mime_type": "text/plain",
        "content_base64": encode_base64(STATEMENT.as_bytes())
    });
    let (status, response) = send(state(), post_analysis(body, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        response["summary"]["identified_opportunities"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[tokio::test]
async fn supplying_both_text_and_base64_is_ambiguous_and_refused() {
    let mut body = analysis_body(STATEMENT, ("2025-01-01", "2025-12-31"));
    body["documents"][0]["content_base64"] = json!(encode_base64(b"other"));
    let (status, problem) = send(state(), post_analysis(body, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["title"], "ambiguous document");
}

#[tokio::test]
async fn the_rule_listing_discloses_which_rules_are_unreviewed() {
    let request = Request::builder()
        .uri("/v1/rules")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(state(), request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], "se-2025.1");
    let rules = body["rules"].as_array().unwrap();
    assert!(!rules.is_empty());
    assert_eq!(
        body["unreviewed_count"].as_u64().unwrap() as usize,
        rules.len()
    );
    for rule in rules {
        let sources = rule["sources"].as_array().unwrap();
        assert!(
            !sources.is_empty(),
            "{} cites no source",
            rule["rule_id"].as_str().unwrap()
        );
        for source in sources {
            for field in ["id", "reference", "authority", "url", "claim"] {
                assert!(
                    !source[field].as_str().unwrap_or_default().is_empty(),
                    "a source of {} has no {field}",
                    rule["rule_id"].as_str().unwrap()
                );
            }
            // The state is what a caller decides how much to trust the rule on,
            // so it has to be a state the caller can interpret rather than a
            // free-form string, and `verified` has to carry the retrieval that
            // earned it — a state without a timestamp is a claim, not a check.
            let state = source["state"].as_str().unwrap();
            assert!(
                ["unretrieved", "unreachable", "mismatch", "verified"].contains(&state),
                "unknown source state {state}"
            );
            if state == "verified" {
                assert!(
                    source["retrieved_at"].is_string(),
                    "a source claims verified with no retrieval behind it"
                );
            }
        }
        // The rule's own state is the weakest of its sources: a rule resting on
        // one checked paragraph and one unchecked one is unchecked.
        let weakest = sources
            .iter()
            .map(|s| match s["state"].as_str().unwrap() {
                "unretrieved" => 0,
                "unreachable" => 1,
                "mismatch" => 2,
                _ => 3,
            })
            .min()
            .unwrap();
        let named = ["unretrieved", "unreachable", "mismatch", "verified"][weakest];
        assert_eq!(
            rule["source_state"],
            named,
            "{} reports a source state stronger than its sources",
            rule["rule_id"].as_str().unwrap()
        );
        assert_eq!(rule["reviewed"], false);
    }
}

// --- base64 -----------------------------------------------------------------

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[test]
fn base64_round_trips() {
    for case in [&b""[..], b"a", b"ab", b"abc", b"abcd", STATEMENT.as_bytes()] {
        let encoded = encode_base64(case);
        assert_eq!(
            decode_base64(&encoded).as_deref(),
            Some(case),
            "failed on {case:?}"
        );
    }
}

#[test]
fn base64_rejects_invalid_input() {
    assert!(decode_base64("!!!!").is_none());
    assert!(
        decode_base64("a").is_none(),
        "a single character carries no whole byte"
    );
}

#[test]
fn base64_tolerates_whitespace_from_wrapped_payloads() {
    let encoded = encode_base64(b"hello world");
    let wrapped = format!("{}\n{}", &encoded[..4], &encoded[4..]);
    assert_eq!(
        decode_base64(&wrapped).as_deref(),
        Some(&b"hello world"[..])
    );
}

#[test]
fn token_comparison_is_length_safe() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"abcd"));
    assert!(!constant_time_eq(b"", b"a"));
}

#[tokio::test]
async fn the_interface_is_served_and_carries_the_disclaimer() {
    let request = Request::builder().uri("/").body(Body::empty()).unwrap();
    let response = router(state()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(bytes.to_vec()).unwrap();

    assert!(html.contains("<html lang=\"sv\">"));
    // The disclaimer must be present even before an analysis has run.
    // In the markup, not in a script. This assertion used to pass only because
    // the page carried its JavaScript inline, so the constant that set the text
    // happened to be part of the HTML. Extracting the script for the
    // Content-Security-Policy broke it and exposed the real problem: the
    // disclaimer was only ever on screen if a script ran.
    assert!(
        html.contains("Skattjakt är ett analys- och upptäcktsverktyg"),
        "the disclaimer must be in the document, not injected by script"
    );
    assert!(
        !html.contains("<script>"),
        "an inline script would need 'unsafe-inline' in the policy"
    );
    // And the unreviewed-rule caveat must be on the first screen, not buried.
    assert!(html.contains("Regelverket är ännu inte granskat"));
    // No bundler, no CDN. The scripts the page loads are its own, served from
    // this binary, which is what lets the policy be `script-src 'self'` with
    // nothing else in it. This used to forbid `<script src=` outright, which
    // was a proxy for "nothing remote" and stopped being one the moment the
    // page legitimately loaded a local file.
    for reference in ["src=\"http", "src=\"//", "href=\"http", "href=\"//"] {
        assert!(
            !html.contains(reference),
            "the interface loads {reference}… from somewhere that is not this server"
        );
    }
    assert!(
        !html.contains("http://") && !html.contains("https://"),
        "the interface must not reference external hosts"
    );
}

#[tokio::test]
async fn the_interface_needs_no_credentials_to_load() {
    // The page is public; the token is entered into it and used per request.
    let request = Request::builder().uri("/").body(Body::empty()).unwrap();
    let response = router(state()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Observability (sections 43, 45)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_scrape_endpoint_serves_the_prometheus_text_format() {
    let request = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = router(state()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains("# TYPE skattjakt_http_requests_total counter"));
    assert!(body.contains("# HELP skattjakt_analyses_started_total"));
}

#[tokio::test]
async fn requests_are_counted_by_templated_route_not_by_path() {
    let state = state();
    let id = "8a5e1e6c-1234-4321-8888-000000000000";

    let request = Request::builder()
        .uri(format!("/v1/analyses/{id}"))
        .body(Body::empty())
        .unwrap();
    let _ = router(state.clone()).oneshot(request).await.unwrap();

    let request = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = router(state).oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();

    assert!(
        !body.contains("8a5e1e6c"),
        "an identifier reached a metric label"
    );
    assert!(body.contains("route=\"/v1/analyses/{id}\""));
}

#[tokio::test]
async fn no_metric_label_carries_a_tenant_identifier() {
    let state = state();
    for uri in ["/health", "/ready", "/v1/rules", "/v1/openapi.yaml"] {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let _ = router(state.clone()).oneshot(request).await.unwrap();
    }

    let request = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = router(state).oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();

    for forbidden in [
        "company_id=",
        "company=",
        "tenant=",
        "correlation_id=",
        "org_number=",
        "user=",
    ] {
        assert!(
            !body.contains(forbidden),
            "{forbidden} appeared in the scrape body"
        );
    }
}

#[tokio::test]
async fn every_response_carries_a_correlation_id() {
    let request = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let response = router(state()).oneshot(request).await.unwrap();
    let header = response
        .headers()
        .get("x-correlation-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .expect("no correlation id on the response");
    assert!(uuid::Uuid::parse_str(&header).is_ok());
}

#[tokio::test]
async fn a_client_supplied_correlation_id_is_honoured_when_it_parses() {
    let supplied = "018f4d5e-0000-7000-8000-000000000000";
    let request = Request::builder()
        .uri("/health")
        .header("x-correlation-id", supplied)
        .body(Body::empty())
        .unwrap();
    let response = router(state()).oneshot(request).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get("x-correlation-id")
            .and_then(|v| v.to_str().ok()),
        Some(supplied)
    );
}

#[tokio::test]
async fn a_forged_correlation_id_is_replaced_rather_than_echoed() {
    // An arbitrary header value would be an injection point into the log store.
    let request = Request::builder()
        .uri("/health")
        .header(
            "x-correlation-id",
            "\" }{ \"level\": \"info\", \"msg\": \"forged",
        )
        .body(Body::empty())
        .unwrap();
    let response = router(state()).oneshot(request).await.unwrap();
    let header = response
        .headers()
        .get("x-correlation-id")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert!(!header.contains("forged"));
    assert!(uuid::Uuid::parse_str(header).is_ok());
}

#[tokio::test]
async fn the_trace_context_is_continued_rather_than_restarted() {
    let inbound = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let request = Request::builder()
        .uri("/health")
        .header("traceparent", inbound)
        .body(Body::empty())
        .unwrap();
    let response = router(state()).oneshot(request).await.unwrap();
    let outbound = response
        .headers()
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .expect("no traceparent on the response");
    // Same trace, new span.
    assert!(outbound.contains("4bf92f3577b34da6a3ce929d0e0e4736"));
    assert!(!outbound.contains("00f067aa0ba902b7"));
}

#[tokio::test]
async fn background_analyses_are_refused_without_a_queue() {
    // The stateless deployment has no database and therefore no queue. It must
    // say so rather than accept work it cannot durably hold.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/analyses/stored")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"document_version_ids": ["8a5e1e6c-1234-4321-8888-000000000000"]}).to_string(),
        ))
        .unwrap();
    let response = router(state()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
}

// ---------------------------------------------------------------------------
// The error contract (sections 19, 20)
// ---------------------------------------------------------------------------

/// Every error code the API can emit.
///
/// This list is the contract. Three clients branch on these, and one of them
/// ships only when Apple approves it — so a code changing underneath them is a
/// break they cannot hot-fix. Collected from the source rather than written by
/// hand, so a new `Problem` cannot be added without appearing here.
#[test]
fn the_error_codes_are_the_set_the_contract_promises() {
    // Every source file in the crate, read at test time.
    //
    // A hardcoded list was the first version, and it had exactly the hole this
    // test exists to prevent: a new route file was not on the list, so its
    // error codes were invisible and the contract "passed" while it had
    // silently changed. A test that only inspects what it already knows about
    // is a test that passes the day it matters.
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(&directory).expect("the source directory is readable") {
        let path = entry.expect("a readable entry").path();
        // This file is skipped: it is the scanner, and its own marker
        // literals would register as error codes. Nothing here serves a route,
        // so nothing here can raise one.
        let is_this_file = path.file_name().and_then(|n| n.to_str()) == Some("api_tests.rs");
        if path.extension().and_then(|e| e.to_str()) == Some("rs") && !is_this_file {
            sources.push(std::fs::read_to_string(&path).expect("a readable source file"));
        }
    }
    assert!(
        sources.len() >= 5,
        "only {} source files were read; the glob is broken",
        sources.len()
    );

    // Both ways a Problem is built. Scanning only the struct literal was the
    // second version of this hole: every code raised through the bad-request
    // constructor was invisible, so four real codes never reached the contract
    // and nobody noticed. The same failure the comment above describes, one
    // construction form along.
    //
    // The constructor is matched across whitespace, because the argument is
    // usually on the next line and a scan that only handled the one-line form
    // would be a third version of the same hole.
    // Comment lines are dropped before scanning. A word-count heuristic was
    // tried first and ate `the_upload_does_not_match_its_ticket`, which is
    // seven words and entirely real — a filter that guesses at prose will
    // eventually guess wrong about code.
    let code_only: Vec<String> = sources
        .iter()
        .map(|source| {
            source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();

    let mut found: Vec<String> = Vec::new();
    for source in &code_only {
        for (marker, skip_whitespace) in [("title: \"", false), ("bad_request(", true)] {
            let mut rest = source.as_str();
            while let Some(at) = rest.find(marker) {
                rest = &rest[at + marker.len()..];
                let body = if skip_whitespace {
                    let trimmed = rest.trim_start();
                    match trimmed.strip_prefix('"') {
                        Some(body) => body,
                        // A call whose first argument is not a literal — a
                        // variable, or a `format!`. Nothing to record.
                        None => continue,
                    }
                } else {
                    rest
                };
                if let Some(stop) = body.find('"') {
                    found.push(
                        Problem {
                            status: StatusCode::BAD_REQUEST,
                            title: body[..stop].to_string(),
                            detail: String::new(),
                        }
                        .code(),
                    );
                }
            }
        }
    }
    found.sort();
    found.dedup();

    let expected = [
        "account_temporarily_locked",
        "admin_credential_required",
        "already_exists",
        "ambiguous_document",
        "analysis_failed",
        "analysis_is_not_finished",
        "authentication_unavailable",
        "content_does_not_match_its_declared_type",
        "document_too_large",
        "empty_document",
        "insufficient_permission",
        "internal_error",
        "invalid_accounts_state",
        "invalid_base64",
        "invalid_credentials",
        "invalid_cursor",
        "invalid_fiscal_year",
        "invalid_idempotency_key",
        "invalid_organisationsnummer",
        "invalid_request",
        "invalid_simulation_model",
        "no_company",
        "no_documents",
        "not_a_session",
        "not_found",
        "nothing_was_uploaded",
        "order_not_payable",
        "password_rejected",
        "payment_provider_unavailable",
        "payment_rejected",
        "payment_required",
        "payments_not_configured",
        "persistence_is_not_configured",
        "provider_required",
        "rate_limited",
        "simulation_cancelled",
        "simulation_cannot_run",
        "simulation_too_slow_to_answer_directly",
        "storage_failure",
        "tax_year_not_covered",
        "the_job_queue_is_not_configured",
        "the_session_cannot_be_refreshed",
        "the_upload_does_not_match_its_ticket",
        "too_many_simulations_at_once",
        "unauthorized",
        "unknown_audience",
        "unknown_document",
        "unknown_product",
        "unknown_push_provider",
        "unknown_role",
        "unknown_value",
        "unreadable_document",
        "unsupported_document_type",
        "wrong_credential",
    ];

    assert_eq!(
        found, expected,
        "\nthe set of error codes changed. Every client branches on these, and \
         one of them ships when Apple says so. If this is deliberate, update \
         the list, the OpenAPI contract and SKATTJAKT_CLIENT_ARCHITECTURE.md \
         together."
    );
}

#[test]
fn a_code_is_derived_deterministically_and_is_url_safe() {
    let problem = Problem {
        status: StatusCode::FORBIDDEN,
        title: "insufficient permission".to_string(),
        detail: "irrelevant".to_string(),
    };
    assert_eq!(problem.code(), "insufficient_permission");
    assert!(problem
        .code()
        .chars()
        .all(|c| c.is_ascii_lowercase() || c == '_'));
}

#[tokio::test]
async fn an_error_response_carries_the_problem_media_type_and_a_code() {
    let app = router(state());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/documents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );

    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();

    assert_eq!(body["code"], "unauthorized");
    assert_eq!(body["status"], 401);
    // The body must stay uninformative about whether a token exists.
    assert!(!body["detail"].as_str().unwrap().contains("expired"));
}

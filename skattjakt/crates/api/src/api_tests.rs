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
    AppState {
        engine: Arc::new(RuleEngine::load_embedded().unwrap()),
        provider: Arc::new(
            ScriptedProvider::new()
                .with(
                    skattjakt_model::ReasoningTask::OpportunityDiscovery,
                    json!({"candidates": []}),
                )
                .with(
                    skattjakt_model::ReasoningTask::ContradictionCheck,
                    json!({"verdicts": []}),
                ),
        ),
        config: PipelineConfig::default(),
        api_token: Some(TOKEN.to_string()),
        admin_token: None,
        model_configured: true,
        // The stateless surface: analyses are computed and returned, never stored.
        store: None,
        blobs: Arc::new(skattjakt_store::FilesystemBlobStore::new(
            std::env::temp_dir().join("skattjakt-tests"),
        )),
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
        assert!(
            !rule["source"].as_str().unwrap().is_empty(),
            "every rule cites a source"
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

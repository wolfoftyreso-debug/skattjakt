//! The analysis engine, compiled for the Vercel function that serves it.
//!
//! # Why this exists at all
//!
//! Vercel runs no long-lived processes. The original service was an axum server
//! plus two workers leasing jobs off a Postgres queue, and none of that has
//! anywhere to run there. The reason the product fits anyway is a measurement
//! rather than an argument: a hundred generated scenarios ran with a **median
//! of 3 ms and a maximum of 7 ms**. The queue existed for a pipeline whose slow
//! step was a model call. Without one — and the rules-only analysis is the
//! whole product until a reviewer signs the rule set — there is nothing to
//! queue. It runs inline, inside the request, and finishes before the network
//! round-trip would have.
//!
//! So the engine crosses to wasm32 and the function calls it directly. What
//! does not cross is everything that opens a socket: the Anthropic client and
//! the OTLP exporter are behind the `native` feature and simply absent here.
//! The rules, the extraction, the confidence model and the report are the same
//! code the native binary runs, and the same tests cover both.
//!
//! # The contract
//!
//! One entry point, JSON in and JSON out, because a wasm boundary is a bad
//! place for a rich type and a good place for a schema:
//!
//! ```text
//!   analyse({ documents: [{ filename, content_base64 }], profile, audience })
//!     → { report } | { error }
//! ```
//!
//! Errors come back as a value, never as a panic across the boundary: a panic
//! in wasm poisons the instance, and the function would then serve one broken
//! request and every request after it from the same warm instance.

use wasm_bindgen::prelude::*;

mod decode;

use skattjakt_core::AnalysisId;
use skattjakt_core::company::{CompanyProfile, FiscalYear, OrgNumber};
use skattjakt_core::document::{AccountsState, MimeType};
use skattjakt_core::CompanyId;
use skattjakt_gateway::{GatewayConfig, ModelGateway};
use skattjakt_model::ScriptedProvider;
use skattjakt_pipeline::pipeline::SilentObserver;
use skattjakt_pipeline::{AnalysisInput, AnalysisPipeline, Audience, DocumentInput, PipelineConfig};
use skattjakt_rules::RuleEngine;
use skattjakt_telemetry::metrics::Registry;

use std::collections::BTreeMap;
use std::sync::Arc;

/// Panics become readable in the function's log instead of a bare
/// `unreachable executed`. Called once, from `analyse`.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(console_error_panic_hook::set_once);
}

#[derive(serde::Deserialize)]
struct Request {
    documents: Vec<Document>,
    profile: serde_json::Value,
    #[serde(default)]
    audience: Option<String>,
    #[serde(default)]
    accounts_state: Option<String>,
}

#[derive(serde::Deserialize)]
struct Document {
    filename: String,
    /// Standard base64. The function reads the upload as bytes and encodes it;
    /// passing bytes across the boundary directly would mean a copy either way.
    content_base64: String,
}

/// The rule set, loaded once per instance.
///
/// Parsing and validating it is the most expensive thing this module does —
/// every rule's expressions are checked against the constants for each year it
/// covers. A warm Vercel instance serves many requests, and doing that work
/// once instead of per request is the difference between a 3 ms analysis and a
/// 3 ms analysis behind a 40 ms load.
fn engine() -> Result<Arc<RuleEngine>, String> {
    use std::cell::RefCell;
    thread_local! {
        static ENGINE: RefCell<Option<Arc<RuleEngine>>> = const { RefCell::new(None) };
    }
    ENGINE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if let Some(engine) = slot.as_ref() {
            return Ok(engine.clone());
        }
        let built = Arc::new(RuleEngine::load_embedded().map_err(|e| e.to_string())?);
        *slot = Some(built.clone());
        Ok(built)
    })
}

/// Runs one analysis. Never panics across the boundary; failures are a value.
///
/// Returns a JSON **string** rather than a JS object. `serde-wasm-bindgen`
/// renders a serde map as a JS `Map`, not a plain object, so `result.report`
/// reads `undefined` on the other side and every field access after it is a
/// guess. A string has one meaning, `JSON.parse` is the only step, and the
/// function on the JS side never has to know how the bridge encodes a map.
#[wasm_bindgen]
pub async fn analyse(request: JsValue) -> String {
    install_panic_hook();
    let outcome = match run(request).await {
        Ok(report) => serde_json::json!({ "report": report }),
        Err(message) => serde_json::json!({ "error": message }),
    };
    serde_json::to_string(&outcome)
        .unwrap_or_else(|e| format!(r#"{{"error":"could not serialise the report: {e}"}}"#))
}

/// The rule set version this module was built with, for the function's health
/// endpoint. A deployment that cannot say which rules it is running is a
/// deployment whose reports cannot be reproduced.
#[wasm_bindgen]
pub fn rule_set_version() -> String {
    engine()
        .map(|e| e.version().to_string())
        .unwrap_or_else(|e| format!("unloadable: {e}"))
}

async fn run(request: JsValue) -> Result<serde_json::Value, String> {
    let request: Request =
        serde_wasm_bindgen::from_value(request).map_err(|e| format!("bad request: {e}"))?;

    if request.documents.is_empty() {
        return Err("no documents were supplied".to_string());
    }

    let engine = engine()?;
    let profile = profile_from(&request.profile)?;
    let audience = match request.audience.as_deref() {
        None => Audience::Company,
        Some(value) => Audience::parse(value)
            .ok_or_else(|| format!("unknown audience {value:?}: private, company, accountant"))?,
    };
    let accounts_state = match request.accounts_state.as_deref() {
        Some("final") => AccountsState::Final,
        Some("unknown") => AccountsState::Unknown,
        Some("preliminary") | None => AccountsState::Preliminary,
        Some(other) => return Err(format!("unknown accounts_state {other:?}")),
    };

    let mut documents = Vec::new();
    for doc in &request.documents {
        documents.push(read_document(doc)?);
    }

    let input = AnalysisInput {
        analysis_id: AnalysisId::new(),
        company: profile,
        documents,
        accounts_state,
        source_states: BTreeMap::new(),
    };

    // The gateway with no provider behind it. Every model call the pipeline
    // makes still goes through pricing, budget and prompt-fence checks — the
    // same code the native build runs — and returns `NotConfigured`, which the
    // pipeline records as a failed run and continues past.
    let gateway = Arc::new(ModelGateway::new(
        Arc::new(ScriptedProvider::new()),
        GatewayConfig::from_env().map_err(|e| e.to_string())?,
        Registry::new(),
    ));

    let pipeline = AnalysisPipeline::new(engine.clone(), gateway, PipelineConfig::default());
    let (result, _runs) = pipeline
        .run(&input, &SilentObserver)
        .await
        .map_err(|e| e.to_string())?;

    let mut report = skattjakt_pipeline::build_report_for(
        audience,
        &result,
        &input.company.name,
        &input.company.fiscal_year.label(),
        engine.version(),
    );
    // Said in the report, not only in a log nobody reads. A rules-only analysis
    // is complete as far as the rules go and finds fewer things than one with a
    // model; the reader is entitled to know which they are holding.
    report.sections.limitations.push(
        "Analysen kördes utan språkmodell. Regelmotorns fynd är fullständiga, men modellens \
         genomgång av underlaget och dess motsägelsekontroll har inte utförts, så färre \
         möjligheter kan ha hittats."
            .to_string(),
    );

    serde_json::to_value(&report).map_err(|e| e.to_string())
}

fn read_document(doc: &Document) -> Result<DocumentInput, String> {
    let bytes = decode::base64(&doc.content_base64)
        .ok_or_else(|| format!("{}: content_base64 is not valid base64", doc.filename))?;
    let mime = mime_for(&doc.filename)
        .ok_or_else(|| format!("{}: unsupported file type", doc.filename))?;
    if !mime.matches_content(&bytes) {
        return Err(format!(
            "{}: the bytes do not look like {}",
            doc.filename,
            mime.as_content_type()
        ));
    }
    let extracted =
        skattjakt_extract::extract(&bytes, mime).map_err(|e| format!("{}: {e}", doc.filename))?;
    Ok(DocumentInput {
        document_id: skattjakt_core::DocumentId::new(),
        document_version_id: skattjakt_core::DocumentVersionId::new(),
        extracted,
    })
}

fn mime_for(filename: &str) -> Option<MimeType> {
    let ext = filename.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => Some(MimeType::Pdf),
        "csv" => Some(MimeType::Csv),
        "xlsx" => Some(MimeType::Xlsx),
        "se" | "si" | "sie" => Some(MimeType::Sie),
        "txt" | "text" => Some(MimeType::PlainText),
        _ => None,
    }
}

/// Builds the profile from the request's JSON.
///
/// The four required fields are read by name and validated — the org number's
/// checksum included — and everything else is merged through the domain type,
/// so a field the profile gains is accepted here without anyone remembering to
/// add it.
fn profile_from(value: &serde_json::Value) -> Result<CompanyProfile, String> {
    let obj = value.as_object().ok_or("profile must be an object")?;
    let get = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_string);

    let name = get("name").ok_or("profile.name is required")?;
    let org_number = OrgNumber::parse(&get("org_number").ok_or("profile.org_number is required")?)
        .map_err(|e| format!("profile.org_number: {e}"))?;
    let start = get("fiscal_year_start").ok_or("profile.fiscal_year_start is required")?;
    let end = get("fiscal_year_end").ok_or("profile.fiscal_year_end is required")?;
    let parse_date = |s: &str| {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|e| format!("{s:?} is not a date (YYYY-MM-DD): {e}"))
    };
    let fiscal_year = FiscalYear::new(parse_date(&start)?, parse_date(&end)?)
        .map_err(|e| format!("profile fiscal year: {e}"))?;

    let mut merged = serde_json::json!({
        "id": CompanyId::new(),
        "name": name,
        "org_number": org_number,
        "fiscal_year": fiscal_year,
    });
    if let (Some(base), serde_json::Value::Object(extra)) = (merged.as_object_mut(), value.clone()) {
        for (key, v) in extra {
            if !matches!(
                key.as_str(),
                "name" | "org_number" | "fiscal_year_start" | "fiscal_year_end" | "id"
            ) {
                base.insert(key, v);
            }
        }
    }
    serde_json::from_value(merged).map_err(|e| format!("profile: {e}"))
}

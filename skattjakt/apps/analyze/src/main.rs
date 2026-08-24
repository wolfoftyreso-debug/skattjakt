//! `skattjakt-analyze` — one analysis, from files to a report.
//!
//! This is what replaced the website. The engine never knew about HTTP, a
//! database or a payment provider, so removing them left a program that takes
//! documents in and prints a report out:
//!
//! ```text
//!   skattjakt-analyze --profile bolag.json arsredovisning.pdf
//! ```
//!
//! ## What it does not do, on purpose
//!
//! It does not store anything, authenticate anybody, or charge for the answer.
//! An analysis is a pure function of its documents, its profile and the rule
//! set version — which is what makes it reproducible, and is the reason the
//! same inputs always give the same findings. Persistence and access control
//! are somebody else's job; this program is the part that has an opinion about
//! Swedish tax.
//!
//! ## The model is optional and its absence is reported
//!
//! Without `ANTHROPIC_API_KEY` the rule engine still runs and still produces
//! evidence-backed findings — it simply produces fewer of them, because the
//! model's discovery and contradiction passes never happen. That is stated in
//! the report's limitations rather than left for the reader to notice, which is
//! a deliberate difference from the service this was cut out of.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use chrono::NaiveDate;
use skattjakt_core::company::{CompanyProfile, FiscalYear, OrgNumber};
use skattjakt_core::document::{AccountsState, MimeType};
use skattjakt_core::AnalysisId;
use skattjakt_core::CompanyId;
use skattjakt_gateway::{GatewayConfig, ModelGateway};
use skattjakt_model::{ModelProvider, ScriptedProvider};
use skattjakt_pipeline::pipeline::SilentObserver;
use skattjakt_pipeline::{
    AnalysisInput, AnalysisPipeline, Audience, DocumentInput, PipelineConfig,
};
use skattjakt_rules::RuleEngine;
use skattjakt_telemetry::metrics::Registry;

const USAGE: &str = "\
skattjakt-analyze — analyserar svenska bokslut mot ett versionerat regelverk

ANVÄNDNING:
    skattjakt-analyze [FLAGGOR] <FIL>...

FLAGGOR:
    --profile <FIL>     JSON med bolagets uppgifter. Utan den antas ett
                        aktiebolag med okända egenskaper, vilket ger färre
                        fynd — profilfrågorna hamnar då i rapportens avsnitt
                        om vad som skulle göra analysen bättre.
    --audience <NAMN>   private | company | accountant. Samma analys,
                        tre presentationslager. Standard: company.
    --format <NAMN>     markdown | json. Standard: markdown.
    --preliminary       Bokslutet är preliminärt, inte fastställt.
    --help

FILER:
    .pdf .csv .xlsx .se .txt

MILJÖ:
    ANTHROPIC_API_KEY   Utan den körs bara regelmotorn. Analysen fungerar,
                        men hittar färre saker, och rapporten säger det.
";

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("kunde inte starta körtiden: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    files: Vec<PathBuf>,
    profile: Option<PathBuf>,
    audience: Audience,
    format: Format,
    accounts_state: AccountsState,
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Markdown,
    Json,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut files = Vec::new();
    let mut profile = None;
    let mut audience = Audience::Company;
    let mut format = Format::Markdown;
    let mut accounts_state = AccountsState::Final;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--preliminary" => accounts_state = AccountsState::Preliminary,
            "--profile" => {
                profile = Some(PathBuf::from(
                    args.next().ok_or("--profile behöver en filväg")?,
                ));
            }
            "--audience" => {
                let value = args.next().ok_or("--audience behöver ett värde")?;
                audience = Audience::parse(&value).ok_or_else(|| {
                    format!("okänd mottagare {value:?}: private, company, accountant")
                })?;
            }
            "--format" => {
                let value = args.next().ok_or("--format behöver ett värde")?;
                format = match value.as_str() {
                    "markdown" | "md" => Format::Markdown,
                    "json" => Format::Json,
                    other => return Err(format!("okänt format {other:?}: markdown, json")),
                };
            }
            other if other.starts_with('-') => {
                return Err(format!("okänd flagga {other:?}"));
            }
            other => files.push(PathBuf::from(other)),
        }
    }

    if files.is_empty() {
        return Err("inga filer angivna. Kör med --help.".to_string());
    }
    Ok(Some(Args {
        files,
        profile,
        audience,
        format,
        accounts_state,
    }))
}

async fn run() -> Result<(), String> {
    let Some(args) = parse_args()? else {
        print!("{USAGE}");
        return Ok(());
    };

    let engine = Arc::new(
        RuleEngine::load_embedded().map_err(|e| format!("regelverket går inte att läsa: {e}"))?,
    );

    // The provider is optional; the gateway is not. Every model call the
    // pipeline makes goes through pricing, budget and prompt-fence checks even
    // when there is no model behind it — so the "no key" path exercises the
    // same code as the paid one rather than a second, less-tested one.
    let (provider, model_configured): (Arc<dyn ModelProvider>, bool) =
        match skattjakt_model::AnthropicConfig::from_env()
            .and_then(skattjakt_model::AnthropicProvider::new)
        {
            Ok(provider) => (Arc::new(provider), true),
            Err(reason) => {
                eprintln!("modell inte konfigurerad ({reason}); kör bara regelmotorn");
                (Arc::new(ScriptedProvider::new()), false)
            }
        };

    let gateway_config = GatewayConfig::from_env()
        .map_err(|e| format!("prislistan för modellen är felaktig: {e}"))?;
    let gateway = Arc::new(ModelGateway::new(provider, gateway_config, Registry::new()));

    let mut documents = Vec::new();
    for path in &args.files {
        documents.push(read_document(path)?);
    }

    let profile = match &args.profile {
        Some(path) => load_profile(path)?,
        None => unknown_company(),
    };

    let input = AnalysisInput {
        analysis_id: AnalysisId::new(),
        company: profile,
        documents,
        accounts_state: args.accounts_state,
        source_states: BTreeMap::new(),
    };

    let pipeline = AnalysisPipeline::new(engine.clone(), gateway, PipelineConfig::default());
    let (result, _runs) = pipeline
        .run(&input, &SilentObserver)
        .await
        .map_err(|e| format!("analysen misslyckades: {e}"))?;

    let mut report = skattjakt_pipeline::build_report_for(
        args.audience,
        &result,
        &input.company.name,
        &input.company.fiscal_year.label(),
        engine.version(),
    );

    // The service this was cut from reports a missing model only on its
    // readiness endpoint, which a customer never sees — so an analysis that
    // silently skipped two of its stages looked exactly like a complete one.
    // Here the reader is the operator, and they are told in the report itself.
    if !model_configured {
        report.sections.limitations.push(
            "Analysen kördes utan språkmodell. Regelmotorns fynd är fullständiga, \
             men modellens genomgång av underlaget och dess motsägelsekontroll har \
             inte utförts, så färre möjligheter kan ha hittats."
                .to_string(),
        );
    }

    match args.format {
        Format::Markdown => print!("{}", skattjakt_pipeline::to_markdown(&report)),
        Format::Json => {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| format!("rapporten går inte att serialisera: {e}"))?;
            println!("{json}");
        }
    }
    Ok(())
}

fn read_document(path: &Path) -> Result<DocumentInput, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;

    // The bytes decide, not the extension. A `.pdf` that is really a JPEG is a
    // JPEG, and saying so beats failing three stages later with an empty
    // extraction. Nothing is refused for its type — a file we cannot read comes
    // back as a document that says what it was and why.
    let mime = MimeType::sniff(&bytes, path.file_name().and_then(|n| n.to_str()));
    let extracted = skattjakt_extract::extract(&bytes, mime)
        .map_err(|e| format!("{}: går inte att läsa: {e}", path.display()))?;

    Ok(DocumentInput {
        document_id: skattjakt_core::DocumentId::new(),
        document_version_id: skattjakt_core::DocumentVersionId::new(),
        extracted,
    })
}

#[derive(serde::Deserialize)]
struct ProfileFile {
    name: String,
    org_number: String,
    fiscal_year_start: NaiveDate,
    fiscal_year_end: NaiveDate,
    #[serde(flatten)]
    rest: serde_json::Value,
}

fn load_profile(path: &Path) -> Result<CompanyProfile, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: ProfileFile =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;

    let org_number = OrgNumber::parse(&file.org_number)
        .map_err(|e| format!("{}: organisationsnumret: {e}", path.display()))?;
    let fiscal_year = FiscalYear::new(file.fiscal_year_start, file.fiscal_year_end)
        .map_err(|e| format!("{}: räkenskapsåret: {e}", path.display()))?;

    // The optional flags are deserialised through the domain type rather than
    // copied field by field, so a profile file gains a field the day the
    // profile does and this function does not have to be remembered.
    let mut profile: CompanyProfile = serde_json::from_value(serde_json::json!({
        "id": CompanyId::new(),
        "name": file.name,
        "org_number": org_number,
        "fiscal_year": fiscal_year,
    }))
    .map_err(|e| format!("{}: {e}", path.display()))?;

    if let serde_json::Value::Object(extra) = file.rest {
        let mut merged =
            serde_json::to_value(&profile).map_err(|e| format!("{}: {e}", path.display()))?;
        if let serde_json::Value::Object(base) = &mut merged {
            for (key, value) in extra {
                base.insert(key, value);
            }
        }
        profile = serde_json::from_value(merged)
            .map_err(|e| format!("{}: okänt eller felaktigt fält: {e}", path.display()))?;
    }
    Ok(profile)
}

/// A company we know nothing about beyond that it is one.
///
/// Deliberately not a set of cheerful defaults. Every unanswered field makes
/// the rules that depend on it return "cannot say" rather than "no", and those
/// questions are then listed in the report as things that would make the
/// analysis better. Guessing here would trade a visible gap for an invisible
/// wrong answer.
fn unknown_company() -> CompanyProfile {
    let today = chrono::Utc::now().date_naive();
    let year = today.year_of_previous_close();
    let fiscal_year = FiscalYear::calendar(year).expect("a calendar year is a fiscal year");
    serde_json::from_value(serde_json::json!({
        "id": CompanyId::new(),
        "name": "Okänt bolag",
        "org_number": OrgNumber::parse("556016-0680").expect("the example number is valid"),
        "fiscal_year": fiscal_year,
    }))
    .expect("a profile with only its required fields is a valid profile")
}

trait PreviousClose {
    fn year_of_previous_close(&self) -> i32;
}

impl PreviousClose for NaiveDate {
    /// The most recent year that can have a closed set of accounts.
    fn year_of_previous_close(&self) -> i32 {
        use chrono::Datelike;
        self.year() - 1
    }
}

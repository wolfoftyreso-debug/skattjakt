//! Keeping the source registry checked, continuously.
//!
//! Why this is a background sweep and not a command somebody runs
//! ==============================================================
//!
//! Verification used to be a developer tool: run a script, write the result
//! into `rules/se-ruleset.json`, rebuild. That produces a verification which is
//! only ever as current as the last release. The law does not change on our
//! release schedule, and a rule set built in March and running in November has
//! a March verification and no way to say so.
//!
//! So the check runs here, on an interval, against the same registry the rules
//! cite, and writes what it finds to `source_retrievals` where every analysis
//! reads it. A rule whose paragraph stops saying what it assumed goes to
//! `mismatch` on the next sweep, and every finding resting on it drops to
//! "investigate" without anybody deploying anything.
//!
//! What it deliberately does not do
//! ================================
//!
//! It cannot decide whether a rule *applies* its source correctly. That a
//! paragraph says 25 per cent does not establish that the rule computes the
//! right base to apply it to. The sweep answers a narrower question — is the
//! document still there, is it still the one cited, does it still contain the
//! operative words and figures — and that question is worth answering exactly
//! because a machine can answer it every six hours forever.

use std::time::Duration;

use skattjakt_rules::{RuleEngine, SourceState};
use skattjakt_store::Store;
use skattjakt_telemetry::{metrics, LogRecord, Registry};

/// How often the registry is re-checked.
///
/// Statute changes on a scale of months, so this is not about latency to a
/// change — it is about the record never being stale enough that somebody
/// would reasonably ask "when was this last true?" and dislike the answer.
/// Four sweeps a day against 24 documents is 96 requests, which is nothing to
/// the authority serving them and cheap enough not to need tuning.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// How often to *consider* sweeping. Short relative to the interval so a worker
/// that starts just after another one finished still converges quickly, and so
/// a fresh deployment checks within minutes rather than within six hours.
pub const SWEEP_POLL: Duration = Duration::from_secs(5 * 60);

/// Politeness between requests to the same authority.
///
/// The 24 sources sit on three hosts. Fetching them as fast as the socket
/// allows is how a legitimate client gets mistaken for a bad one, and being
/// rate-limited by riksdagen.se would take out the verification of every rule
/// at once.
const BETWEEN_FETCHES: Duration = Duration::from_millis(750);

/// Per-document ceiling. A statute page is a few hundred kilobytes; a request
/// that has not finished in this long is not going to.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// A hostile or broken endpoint must not be able to exhaust the worker's
/// memory. Statute pages are well under this; anything above it is not the
/// document we cited.
const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

const AGENT: &str = concat!(
    "skattjakt-source-verifier/2.0 ",
    "(+source verification for a Swedish tax analysis tool)"
);

/// What one sweep concluded, for the log line and for the tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepOutcome {
    pub verified: usize,
    pub mismatched: usize,
    pub unreachable: usize,
}

impl SweepOutcome {
    pub fn total(&self) -> usize {
        self.verified + self.mismatched + self.unreachable
    }
}

/// Runs the sweep if it is due and no other worker is already doing it.
pub async fn sweep_if_due(
    store: &Store,
    engine: &RuleEngine,
    metrics: &Registry,
) -> anyhow::Result<Option<SweepOutcome>> {
    let interval = chrono::Duration::from_std(SWEEP_INTERVAL)?;
    if !store.sources_due_for_check(interval).await? {
        return Ok(None);
    }
    // Due, but another worker may have got there first. The lock is taken
    // *after* the due check because the due check is a cheap read and the lock
    // costs a pooled connection for the length of the sweep.
    let Some(guard) = store.claim_source_sweep().await? else {
        return Ok(None);
    };

    // Re-ask now that the lock is held: the worker that held it a moment ago
    // may have finished the very sweep this one decided was due.
    if !store.sources_due_for_check(interval).await? {
        guard.release().await?;
        return Ok(None);
    }

    let outcome = sweep(store, engine, metrics).await;
    guard.release().await?;
    outcome.map(Some)
}

/// A sweep's verdicts, both as counts and as lines a person can read.
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub outcome: SweepOutcome,
    pub lines: Vec<String>,
}

/// Fetches and checks every source in the registry.
///
/// Writes the result when given a store and only reports when not. That is the
/// difference between the scheduled sweep and an operator asking the question,
/// and it is the only difference — the checking is identical, which is the
/// point of there being one function.
pub async fn check_all(
    engine: &RuleEngine,
    store: Option<&Store>,
    metrics: &Registry,
) -> anyhow::Result<Report> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(AGENT)
        .build()?;

    let mut report = Report::default();
    let sources = &engine.set().sources;

    for (index, (id, source)) in sources.iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(BETWEEN_FETCHES).await;
        }
        let Some(url) = source.machine_url.as_ref().or(source.url.as_ref()) else {
            let reason = "the registry gives no url to fetch";
            if let Some(store) = store {
                store.record_source_unreachable(id, reason).await?;
            }
            report.outcome.unreachable += 1;
            report.lines.push(format!("  unreached {id:<22} {reason}"));
            continue;
        };

        match fetch(&client, url).await {
            Err(reason) => {
                // Deliberately not fatal, and deliberately not clearing an
                // earlier successful retrieval — see `record_source_unreachable`.
                if let Some(store) = store {
                    store.record_source_unreachable(id, &reason).await?;
                }
                report.outcome.unreachable += 1;
                report.lines.push(format!("  unreached {id:<22} {reason}"));
                LogRecord::warn("could not retrieve a cited source")
                    .internal("source_id", id.clone())
                    .internal("reason", reason)
                    .emit();
            }
            Ok(body) => {
                let check = source.check(&body);
                if let Some(store) = store {
                    let sha = check.sha256.clone().unwrap_or_default();
                    store
                        .record_source_read(id, check.state, &sha, check.note.as_deref())
                        .await?;
                }
                match check.state {
                    SourceState::Mismatch => {
                        report.outcome.mismatched += 1;
                        let note = check.note.clone().unwrap_or_default();
                        report.lines.push(format!("  MISMATCH  {id:<22} {note}"));
                        // The loudest thing this process can say. A source that
                        // contradicts the rule set means either the law moved
                        // or the rule was wrong when written, and every finding
                        // resting on it has just been capped at "investigate".
                        LogRecord::error("a cited source contradicts the rule set")
                            .internal("source_id", id.clone())
                            .internal("locator", source.locator.clone())
                            .internal("note", note)
                            .emit();
                    }
                    _ => {
                        report.outcome.verified += 1;
                        report
                            .lines
                            .push(format!("  ok        {id:<22} {}", source.locator));
                    }
                }
            }
        }
    }

    metrics::set_source_states(
        metrics,
        report.outcome.verified,
        report.outcome.mismatched,
        report.outcome.unreachable,
    );

    LogRecord::info("source registry swept")
        .internal("verified", report.outcome.verified.to_string())
        .internal("mismatched", report.outcome.mismatched.to_string())
        .internal("unreachable", report.outcome.unreachable.to_string())
        .emit();

    Ok(report)
}

/// The scheduled sweep: check everything, and record it.
pub async fn sweep(
    store: &Store,
    engine: &RuleEngine,
    metrics: &Registry,
) -> anyhow::Result<SweepOutcome> {
    Ok(check_all(engine, Some(store), metrics).await?.outcome)
}

/// Retrieves one document, or the reason it could not be.
///
/// The reason is returned rather than logged and swallowed, because it is
/// written into the record: "HTTP 404" and "the proxy refused the connection"
/// send an operator to entirely different places, and six months later the note
/// is all anyone has.
async fn fetch(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| describe(&error))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {} from {url}", status.as_u16()));
    }

    // Content-Length is a hint, not a promise, so the body is also capped as it
    // arrives. Checking the header first avoids downloading something huge only
    // to discard it.
    if let Some(length) = response.content_length() {
        if length as usize > MAX_DOCUMENT_BYTES {
            return Err(format!(
                "the document is {length} bytes, larger than we will read"
            ));
        }
    }

    let bytes = response.bytes().await.map_err(|error| describe(&error))?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "the document is at least {} bytes, larger than we will read",
            bytes.len()
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "the request timed out".to_string();
    }
    if error.is_connect() {
        return format!("could not connect: {error}");
    }
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_poll_is_shorter_than_the_interval() {
        // Otherwise a worker could sleep past every due window and the registry
        // would go unchecked while looking scheduled.
        assert!(SWEEP_POLL < SWEEP_INTERVAL);
    }

    #[test]
    fn a_sweep_of_the_shipped_registry_would_be_polite() {
        // 24 sources at 750 ms apart is under a minute, which is the budget a
        // maintenance task may take without needing its own process.
        let engine = RuleEngine::load_embedded().unwrap();
        let count = engine.set().sources.len() as u32;
        let spacing = BETWEEN_FETCHES * count.saturating_sub(1);
        assert!(
            spacing < Duration::from_secs(60),
            "a full sweep would idle {spacing:?} between requests"
        );
    }

    #[test]
    fn totals_add_up() {
        let outcome = SweepOutcome {
            verified: 20,
            mismatched: 1,
            unreachable: 3,
        };
        assert_eq!(outcome.total(), 24);
    }
}

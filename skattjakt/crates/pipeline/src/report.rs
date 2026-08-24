//! The beta report (section 28).
//!
//! Nine sections, in the order the build order sets: what was found, what to
//! look at first, the opportunities themselves, warnings, missing information,
//! economic potential, evidence, next steps, limitations.
//!
//! The report is derived entirely from the analysis result — it adds no facts
//! and does no arithmetic of its own, so it cannot disagree with the findings
//! it is reporting.

use serde::{Deserialize, Serialize};
use skattjakt_core::analysis::AnalysisResult;
use skattjakt_core::opportunity::EffectKind;
use skattjakt_core::{EvidenceItem, MoneyRange, OpportunityStatus, PriorityBand};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub analysis_id: skattjakt_core::AnalysisId,
    pub company_name: String,
    pub fiscal_year: String,
    pub rule_set_version: String,
    pub sections: ReportSections,
    pub disclaimer: String,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSections {
    /// 1. What did Skattjakt find?
    pub summary: Summary,
    /// 2. What should the company look at first?
    pub start_here: Vec<Highlight>,
    /// 3. The opportunities.
    pub opportunities: Vec<Highlight>,
    /// 4. Things to check.
    pub warnings: Vec<skattjakt_core::analysis::Warning>,
    /// 5. What would make the analysis better.
    pub missing_information: Vec<skattjakt_core::analysis::MissingInformation>,
    /// 6. Economic potential, as an interval.
    pub economic_potential: EconomicPotential,
    /// 7. Sources and documents.
    pub evidence: EvidenceSummary,
    /// 8. Concrete next steps.
    pub next_steps: Vec<String>,
    /// 9. What the system cannot determine.
    pub limitations: Vec<String>,
    /// Who this rendering was written for.
    pub audience: String,
    /// The ordered "do this now" list, across all findings.
    pub action_plan: Vec<ActionStep>,
    /// Present only for `Audience::Accountant`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_review: Option<ControlReview>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub headline: String,
    pub found_count: usize,
    pub high_priority_count: usize,
    pub should_investigate_count: usize,
    pub needs_more_evidence_count: usize,
    pub warnings_count: usize,
    pub areas_checked: Vec<skattjakt_core::analysis::CoveredArea>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Highlight {
    pub title: String,
    pub category: String,
    pub status: String,
    pub status_label: String,
    pub impact: MoneyRange,
    pub impact_display: String,
    /// `reduction` or `deferral`. Carried into the report because a reader who
    /// sees an amount needs to know whether it is tax saved or tax postponed,
    /// and the headline total answers that only by omission.
    pub effect: EffectKind,
    pub confidence: u8,
    pub priority_band: String,
    pub rationale: String,
    pub recommended_action: String,
    pub missing_information: Vec<String>,
    pub supporting_values: Vec<SupportingValue>,
    pub rules: Vec<CitedRule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupportingValue {
    pub kind: String,
    pub amount: String,
    pub page: Option<u32>,
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CitedRule {
    pub rule_id: String,
    pub title: String,
    pub source: String,
    /// How far the weakest of this rule's sources has been checked.
    ///
    /// In the report on purpose. A reader shown `30 kap. 5 §` beside a figure
    /// reasonably assumes somebody opened it; the honest version of that line
    /// says whether anybody did.
    pub source_state: String,
}

/// The Swedish the report prints for a retrieval state.
fn source_state_label(state: &str) -> &'static str {
    match state {
        "verified" => "källa kontrollerad",
        "mismatch" => "källan motsäger regeln",
        "unreachable" => "källan kunde inte hämtas",
        _ => "källa ej kontrollerad",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EconomicPotential {
    /// Tax that would not be paid at all. Deferrals are excluded — see
    /// `deferred`, and `Opportunity::countable_impact`.
    pub total: MoneyRange,
    pub display: String,
    pub note: String,
    /// Tax that would be postponed rather than saved, on its own line.
    ///
    /// Split out because adding the two together answered a question nobody
    /// asked. A reader who sees one figure reads it as money to be had; a
    /// periodiseringsfond allocation is the same money paid later, with a
    /// schablonintäkt charged every year the fund stands.
    #[serde(default)]
    pub deferred: MoneyRange,
    #[serde(default)]
    pub deferred_display: String,
    /// What the deferred figure means, or empty when there is none.
    #[serde(default)]
    pub deferred_note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceSummary {
    pub document_versions_used: usize,
    pub values_cited: usize,
    pub rules_cited: Vec<CitedRule>,
    pub assumptions: Vec<String>,
}

/// Who the report is being written for.
///
/// One engine, three presentation layers — not three products. The analysis is
/// identical in all three: the same rules, the same evidence, the same status
/// ladder. What changes is what gets emphasised, what gets grouped, and what a
/// reader is assumed to already know.
///
/// Splitting this into separate products would mean three rule sets to keep in
/// step, and the one that fell behind would be the one nobody noticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// Someone reading about their own affairs, who does not know the
    /// vocabulary and needs the next step spelled out.
    Private,
    /// A company owner or director reading about their own company.
    Company,
    /// An accounting assistant reviewing a client's closing before it is filed.
    /// Reads the same findings as a checklist, and — the part that makes this a
    /// different product rather than a different font — wants the checks that
    /// *passed* as well as the ones that did not.
    Accountant,
}

impl Audience {
    pub fn as_str(self) -> &'static str {
        match self {
            Audience::Private => "private",
            Audience::Company => "company",
            Audience::Accountant => "accountant",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "private" => Audience::Private,
            "company" => Audience::Company,
            "accountant" => Audience::Accountant,
            _ => return None,
        })
    }
}

/// One step in the ordered "do this now" list.
///
/// The list exists because a report is not a deliverable — a decision is. Seven
/// findings each with a "recommended action" leaves the reader to work out what
/// to do first; this says it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionStep {
    /// 1-based, and the order is the point.
    pub order: usize,
    pub action: String,
    /// The finding it came from, so a reader can go back to the evidence.
    pub title: String,
    /// `high`, `should_investigate`, `needs_more_evidence`.
    pub priority_band: String,
    /// Present when the finding carries a computed effect. A range, never a
    /// single figure — see `EconomicPotential`.
    pub impact: Option<MoneyRange>,
    /// True when the step is a control rather than an opportunity: something to
    /// check before filing, not money to go and get.
    pub is_control: bool,
}

/// What an accounting assistant sees instead of a list of opportunities.
///
/// The four bands are the review they would otherwise write by hand. The last
/// one is the reason a firm would pay for this: a review that only lists
/// problems creates work; a review that also says "these four are worth raising
/// with the client" creates billable work.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ControlReview {
    /// Must be checked before filing. Findings the engine could not settle.
    pub must_check: Vec<Highlight>,
    /// A possible improvement, with an amount where one could be computed.
    pub possible_improvement: Vec<Highlight>,
    /// Checked against values from the documents, and did not apply.
    ///
    /// Deliberately **not** everything that failed to fire: a rule whose
    /// trigger fact was never found decided nothing, and listing it here would
    /// be reassurance in place of a look. Those appear under what is missing.
    pub looks_correct: Vec<ClearedLine>,
    /// Areas worth raising with the client, most valuable first.
    pub worth_raising: Vec<TalkingPoint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClearedLine {
    pub title: String,
    pub category: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TalkingPoint {
    pub area: String,
    pub summary: String,
    /// A range. An accountant quoting a single figure to a client owns that
    /// figure.
    pub impact: MoneyRange,
    pub impact_display: String,
}

/// Builds the report from a finished analysis.
pub fn build(
    result: &AnalysisResult,
    company_name: &str,
    fiscal_year: &str,
    rule_set_version: &str,
) -> Report {
    build_for(
        Audience::Company,
        result,
        company_name,
        fiscal_year,
        rule_set_version,
    )
}

/// Builds the report for a particular reader.
pub fn build_for(
    audience: Audience,
    result: &AnalysisResult,
    company_name: &str,
    fiscal_year: &str,
    rule_set_version: &str,
) -> Report {
    let presented: Vec<_> = result
        .opportunities
        .iter()
        .filter(|o| o.status.is_presented())
        .collect();

    let opportunities: Vec<Highlight> = presented.iter().map(|o| highlight(o)).collect();

    let start_here: Vec<Highlight> = opportunities
        .iter()
        .filter(|h| h.priority_band == "high")
        .cloned()
        .collect();

    let mut rules_cited: Vec<CitedRule> =
        opportunities.iter().flat_map(|h| h.rules.clone()).collect();
    rules_cited.sort_by(|a, b| a.rule_id.cmp(&b.rule_id));
    rules_cited.dedup_by(|a, b| a.rule_id == b.rule_id);

    let assumptions: Vec<String> = presented
        .iter()
        .flat_map(|o| o.evidence.assumptions().into_iter().map(str::to_string))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    let document_versions_used = presented
        .iter()
        .flat_map(|o| o.evidence.document_versions())
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    let values_cited = opportunities
        .iter()
        .map(|h| h.supporting_values.len())
        .sum();

    let headline = if result.summary.found_nothing {
        "Skattjakten hittade inga tydliga möjligheter på det underlag vi fått. \
         Det betyder inte att det inte finns möjligheter — det betyder att vi inte har \
         tillräckligt stark evidens för att flagga dem."
            .to_string()
    } else {
        // Both words inflect, not just the noun. With one finding this read
        // "1 sak som kan vara värda att undersöka" — the product's opening
        // sentence, ungrammatical, on every analysis that found exactly one
        // thing. Found by running a hundred scenarios; no test had a
        // single-finding case.
        format!(
            "Vi hittade {} {}.",
            result.summary.identified_opportunities,
            if result.summary.identified_opportunities == 1 {
                "sak som kan vara värd att undersöka"
            } else {
                "saker som kan vara värda att undersöka"
            }
        )
    };

    let total = result.summary.estimated_total;
    // Summed here rather than on `AnalysisSummary`, because a deferral is a
    // presentation concern: the analysis found one number and the report is
    // what decides it must not be added to the other.
    let deferred = result
        .opportunities
        .iter()
        .map(skattjakt_core::Opportunity::deferred_impact)
        .try_fold(MoneyRange::ZERO, |acc, r| acc.checked_add(r))
        .unwrap_or(MoneyRange::ZERO);

    // Computed before the list is moved into the report.
    let action_plan = action_plan(&opportunities);
    let control_review = match audience {
        Audience::Accountant => Some(control_review(&opportunities, result)),
        _ => None,
    };

    Report {
        analysis_id: result.analysis_id,
        company_name: company_name.to_string(),
        fiscal_year: fiscal_year.to_string(),
        rule_set_version: rule_set_version.to_string(),
        sections: ReportSections {
            summary: Summary {
                headline,
                found_count: result.summary.identified_opportunities,
                high_priority_count: result.summary.high_priority_count,
                should_investigate_count: result.summary.needs_investigation_count,
                needs_more_evidence_count: result.summary.missing_information_count,
                warnings_count: result.summary.warnings_count,
                areas_checked: result.covered_areas.clone(),
            },
            start_here,
            opportunities,
            warnings: result.warnings.clone(),
            missing_information: result.missing_information.clone(),
            economic_potential: EconomicPotential {
                total,
                display: total.to_string(),
                note: if total.is_zero() {
                    "Ingen beräknad sänkning av skatten på det underlag som lämnats.".to_string()
                } else {
                    "Ett intervall, inte ett besked. Beloppen bygger på det underlag som \
                     lämnats och ska verifieras innan någon åtgärd vidtas."
                        .to_string()
                },
                deferred,
                deferred_display: deferred.to_string(),
                deferred_note: if deferred.is_zero() {
                    String::new()
                } else {
                    "Uppskjuten skatt, inte sänkt skatt. Beloppet betalas senare — en \
                     periodiseringsfond ska återföras senast det sjätte året, och en \
                     schablonintäkt tas upp varje år fonden står kvar. Det är därför \
                     inte medräknat i beloppet ovan."
                        .to_string()
                },
            },
            evidence: EvidenceSummary {
                document_versions_used,
                values_cited,
                rules_cited,
                assumptions,
            },
            next_steps: result.recommended_actions.clone(),
            limitations: result
                .limitations
                .iter()
                .map(|l| l.statement.clone())
                .collect(),
            audience: audience.as_str().to_string(),
            action_plan,
            control_review,
        },
        disclaimer: result.disclaimer.clone(),
        generated_at: result.generated_at,
    }
}

/// Orders every finding's recommended action into one list.
///
/// Ranked by priority band, then by the top of the money range. Ties keep the
/// order the pipeline produced, which is already sorted by priority score — so
/// two findings in the same band with no computed amount stay in the engine's
/// order rather than in an arbitrary one.
///
/// Deduplicated on the action text: several rules can land on the same next
/// step ("ask your accountant about X"), and a numbered list that says the same
/// thing twice reads as though the machine was not paying attention.
fn action_plan(opportunities: &[Highlight]) -> Vec<ActionStep> {
    fn band_rank(band: &str) -> u8 {
        match band {
            "high" => 0,
            "should_investigate" => 1,
            "needs_more_evidence" => 2,
            _ => 3,
        }
    }

    let mut ranked: Vec<&Highlight> = opportunities.iter().collect();
    ranked.sort_by(|a, b| {
        band_rank(&a.priority_band)
            .cmp(&band_rank(&b.priority_band))
            .then(b.impact.high.ore().cmp(&a.impact.high.ore()))
    });

    let mut seen = std::collections::BTreeSet::new();
    let mut steps = Vec::new();
    for highlight in ranked {
        let action = highlight.recommended_action.trim();
        if action.is_empty() || !seen.insert(action.to_string()) {
            continue;
        }
        steps.push(ActionStep {
            order: steps.len() + 1,
            action: action.to_string(),
            title: highlight.title.clone(),
            priority_band: highlight.priority_band.clone(),
            impact: (!highlight.impact.is_zero()).then_some(highlight.impact),
            // A warning or a finding the engine could not settle is something to
            // check before filing; the rest is money to go and look for. The
            // distinction is what lets a reader tell "you may be owed this" from
            // "do not file until you have looked at this".
            is_control: highlight.status == "warning" || highlight.status == "investigate",
        });
    }
    steps
}

/// The four bands an accounting assistant reviews a closing by.
fn control_review(opportunities: &[Highlight], result: &AnalysisResult) -> ControlReview {
    let mut review = ControlReview::default();

    for highlight in opportunities {
        match highlight.status.as_str() {
            // `verify` belongs here, and putting it under "possible
            // improvement" was the wrong way round.
            //
            // The band means *the engine could not settle this*, and `verify`
            // says exactly that: the rule or the calculation needs checking. It
            // is not a nuance either — in a build where no legal source has been
            // retrieved and no rule professionally reviewed, every finding is
            // capped at `verify`. So an assistant who bought Skattjakt Kontroll
            // got a review whose "must be checked before filing" section was
            // empty, and six improvements each resting on an unverified rule.
            //
            // That is the precise failure the source-state ladder exists to
            // prevent, reintroduced one layer above it.
            "warning" | "investigate" | "verify" => review.must_check.push(highlight.clone()),
            // Only `identified` — strong evidence, reviewed rule — is an
            // improvement somebody can act on without checking first.
            _ => review.possible_improvement.push(highlight.clone()),
        }
    }

    review.looks_correct = result
        .cleared
        .iter()
        .map(|check| ClearedLine {
            title: check.title.clone(),
            category: check.category.clone(),
            reason: check.reason.clone(),
        })
        .collect();

    // What is worth a phone call, biggest first. Only findings with a computed
    // amount: "we found something, no idea how much" is not a conversation an
    // assistant can charge for, and putting it here would dilute the ones that
    // are.
    let mut points: Vec<TalkingPoint> = opportunities
        .iter()
        .filter(|h| !h.impact.is_zero())
        .map(|h| TalkingPoint {
            area: h.category.clone(),
            summary: h.title.clone(),
            impact: h.impact,
            impact_display: h.impact.to_string(),
        })
        .collect();
    points.sort_by(|a, b| b.impact.high.ore().cmp(&a.impact.high.ore()));
    review.worth_raising = points;

    review
}

fn highlight(opportunity: &skattjakt_core::Opportunity) -> Highlight {
    let mut supporting_values = Vec::new();
    let mut rules = Vec::new();

    for item in opportunity.evidence.items() {
        match item {
            EvidenceItem::DocumentValue {
                kind,
                value,
                page,
                excerpt,
                ..
            } => {
                supporting_values.push(SupportingValue {
                    kind: kind.key(),
                    amount: value.to_string(),
                    page: *page,
                    excerpt: excerpt.clone(),
                });
            }
            EvidenceItem::Rule {
                rule_id,
                title,
                source,
                citations,
                ..
            } => {
                // The weakest citation, for the same reason the engine takes
                // the weakest: a rule half-checked is not half-trustworthy.
                let weakest = citations
                    .iter()
                    .map(|c| match c.state.as_str() {
                        "unretrieved" => (0, "unretrieved"),
                        "unreachable" => (1, "unreachable"),
                        "mismatch" => (2, "mismatch"),
                        _ => (3, "verified"),
                    })
                    .min_by_key(|(rank, _)| *rank)
                    .map(|(_, name)| name)
                    .unwrap_or("unretrieved");
                rules.push(CitedRule {
                    rule_id: rule_id.clone(),
                    title: title.clone(),
                    source: source.clone(),
                    source_state: weakest.to_string(),
                });
            }
            _ => {}
        }
    }

    Highlight {
        title: opportunity.title.clone(),
        category: opportunity.category.label_sv().to_string(),
        status: status_key(opportunity.status),
        status_label: opportunity.status.label_sv().to_string(),
        impact: opportunity.impact,
        impact_display: if opportunity.impact.is_zero() {
            "Ingen beräknad ekonomisk effekt".to_string()
        } else if opportunity.effect.is_deferral() {
            format!("{} uppskjuten skatt", opportunity.impact)
        } else {
            opportunity.impact.to_string()
        },
        effect: opportunity.effect,
        confidence: opportunity.confidence.score,
        priority_band: priority_key(opportunity.priority.band),
        rationale: opportunity.rationale.clone(),
        recommended_action: opportunity.recommended_action.clone(),
        missing_information: opportunity.missing_information.clone(),
        supporting_values,
        rules,
    }
}

fn status_key(status: OpportunityStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn priority_key(band: PriorityBand) -> String {
    serde_json::to_value(band)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Renders the report as Markdown, for export.
pub fn to_markdown(report: &Report) -> String {
    let s = &report.sections;
    let mut out = String::new();

    out.push_str(&format!(
        "# Din Skattjakt\n\n**{}** · räkenskapsår {} · regelverk {}\n\n",
        report.company_name, report.fiscal_year, report.rule_set_version
    ));

    out.push_str("## 1. Sammanfattning\n\n");
    out.push_str(&format!("{}\n\n", s.summary.headline));
    out.push_str(&format!(
        "- {} hög prioritet\n- {} bör undersökas\n- {} kräver mer underlag\n- {} varningar\n\n",
        s.summary.high_priority_count,
        s.summary.should_investigate_count,
        s.summary.needs_more_evidence_count,
        s.summary.warnings_count
    ));

    out.push_str("## 2. Börja här\n\n");
    if s.start_here.is_empty() {
        out.push_str("Inget fynd har nått hög prioritet på det här underlaget.\n\n");
    }
    for item in &s.start_here {
        out.push_str(&format!(
            "- **{}** — {}\n",
            item.title, item.recommended_action
        ));
    }
    out.push('\n');

    out.push_str("## 3. Potentiella möjligheter\n\n");
    if s.opportunities.is_empty() {
        out.push_str("Inga.\n\n");
    }
    for item in &s.opportunities {
        out.push_str(&format!(
            "### {} ({})\n\n{}\n\n- Potentiell ekonomisk påverkan: {}\n- Status: {}\n- Confidence: {} %\n",
            item.title, item.category, item.rationale, item.impact_display, item.status_label, item.confidence
        ));
        if !item.supporting_values.is_empty() {
            out.push_str("- Underlag:\n");
            for value in &item.supporting_values {
                out.push_str(&format!(
                    "  - {} {}{}\n",
                    value.kind,
                    value.amount,
                    value
                        .page
                        .map(|p| format!(" (sida {p})"))
                        .unwrap_or_default()
                ));
            }
        }
        if !item.rules.is_empty() {
            out.push_str("- Regler:\n");
            for rule in &item.rules {
                out.push_str(&format!(
                    "  - {} — {} ({})\n",
                    rule.title,
                    rule.source,
                    source_state_label(&rule.source_state)
                ));
            }
        }
        if !item.missing_information.is_empty() {
            out.push_str("- Saknas:\n");
            for missing in &item.missing_information {
                out.push_str(&format!("  - {missing}\n"));
            }
        }
        out.push_str(&format!(
            "- Rekommenderad åtgärd: {}\n\n",
            item.recommended_action
        ));
    }

    out.push_str("## 4. Varningar\n\n");
    if s.warnings.is_empty() {
        out.push_str("Inga.\n\n");
    }
    for warning in &s.warnings {
        out.push_str(&format!("- {}\n", warning.message));
    }
    out.push('\n');

    out.push_str("## 5. Saknad information\n\n");
    if s.missing_information.is_empty() {
        out.push_str("Inget ytterligare underlag efterfrågas.\n\n");
    }
    for missing in &s.missing_information {
        out.push_str(&format!(
            "- {} — {}\n",
            missing.description, missing.unlocks
        ));
    }
    out.push('\n');

    out.push_str("## 6. Ekonomisk potential\n\n");
    out.push_str(&format!(
        "**Lägre skatt:** {}\n\n{}\n\n",
        s.economic_potential.display, s.economic_potential.note
    ));
    if !s.economic_potential.deferred.is_zero() {
        out.push_str(&format!(
            "**Uppskjuten skatt:** {}\n\n{}\n\n",
            s.economic_potential.deferred_display, s.economic_potential.deferred_note
        ));
    }

    out.push_str("## 7. Evidens\n\n");
    out.push_str(&format!(
        "{} dokumentversion(er), {} citerade värden, {} regler.\n\n",
        s.evidence.document_versions_used,
        s.evidence.values_cited,
        s.evidence.rules_cited.len()
    ));
    for rule in &s.evidence.rules_cited {
        out.push_str(&format!(
            "- {} — {} ({})\n",
            rule.title,
            rule.source,
            source_state_label(&rule.source_state)
        ));
    }
    if !s.evidence.assumptions.is_empty() {
        out.push_str("\nAntaganden:\n");
        for assumption in &s.evidence.assumptions {
            out.push_str(&format!("- {assumption}\n"));
        }
    }
    out.push('\n');

    out.push_str("## 8. Gör detta nu\n\n");
    if s.action_plan.is_empty() {
        out.push_str("Inga åtgärder föreslås på det här underlaget.\n\n");
    }
    for step in &s.action_plan {
        // The marker distinguishes "look at this before you file" from "there
        // may be money here". A numbered list that mixes the two silently makes
        // the reader rank them, which is the work the list exists to do.
        let marker = if step.is_control {
            "Kontroll"
        } else {
            "Möjlighet"
        };
        let amount = match step.impact {
            Some(range) => format!(" — {range}"),
            None => String::new(),
        };
        out.push_str(&format!(
            "{}. **{}** ({}{})\n   {}\n",
            step.order, step.title, marker, amount, step.action
        ));
    }
    out.push('\n');

    if let Some(review) = &s.control_review {
        out.push_str("## 8b. Kontroll av bokslutet\n\n");

        out.push_str(&format!(
            "### Måste kontrolleras ({})\n\n",
            review.must_check.len()
        ));
        for item in &review.must_check {
            out.push_str(&format!(
                "- **{}** — {}\n",
                item.title, item.recommended_action
            ));
        }
        if review.must_check.is_empty() {
            out.push_str("Inget som måste kontrolleras på det här underlaget.\n");
        }
        out.push('\n');

        out.push_str(&format!(
            "### Möjlig förbättring ({})\n\n",
            review.possible_improvement.len()
        ));
        for item in &review.possible_improvement {
            out.push_str(&format!(
                "- **{}** — {} ({})\n",
                item.title, item.impact_display, item.status_label
            ));
        }
        out.push('\n');

        out.push_str(&format!(
            "### Ser korrekt ut ({})\n\n",
            review.looks_correct.len()
        ));
        for line in &review.looks_correct {
            out.push_str(&format!("- **{}** — {}\n", line.title, line.reason));
        }
        if review.looks_correct.is_empty() {
            out.push_str(
                "Ingen regel kunde prövas mot avlästa värden och avfärdas. Det betyder \
                 inte att något är fel — det betyder att underlaget inte räckte för att \
                 friskförklara något.\n",
            );
        }
        out.push('\n');

        out.push_str(&format!(
            "### Värt att ta upp med kunden ({})\n\n",
            review.worth_raising.len()
        ));
        for point in &review.worth_raising {
            out.push_str(&format!(
                "- {} — {} ({})\n",
                point.summary, point.impact_display, point.area
            ));
        }
        out.push('\n');
    }

    out.push_str("## 9. Begränsningar\n\n");
    for limitation in &s.limitations {
        out.push_str(&format!("- {limitation}\n"));
    }

    out.push_str(&format!("\n---\n\n_{}_\n", report.disclaimer));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use skattjakt_core::analysis::AnalysisResult;
    use skattjakt_core::{AnalysisId, CompanyId};

    fn empty_result() -> AnalysisResult {
        AnalysisResult::summarise(
            AnalysisId::new(),
            CompanyId::new(),
            vec![],
            vec![],
            vec![],
            vec![skattjakt_core::analysis::CoveredArea {
                category: "Skatt".into(),
                rules_evaluated: 5,
                findings: 0,
            }],
            vec![],
            vec![skattjakt_core::analysis::Limitation {
                statement: "Underlaget är preliminärt.".into(),
            }],
            vec![],
        )
    }

    #[test]
    fn a_report_with_nothing_found_says_so_and_still_shows_what_was_checked() {
        let report = build(&empty_result(), "Testbolaget AB", "2025", "se-2025.1");
        assert!(report
            .sections
            .summary
            .headline
            .contains("inga tydliga möjligheter"));
        assert!(report
            .sections
            .summary
            .headline
            .contains("Det betyder inte att det inte finns möjligheter"));
        assert_eq!(report.sections.summary.areas_checked.len(), 1);
        assert!(report.sections.economic_potential.total.is_zero());
        assert!(!report.disclaimer.is_empty());
    }

    #[test]
    fn the_report_has_all_nine_sections_in_markdown() {
        let markdown = to_markdown(&build(
            &empty_result(),
            "Testbolaget AB",
            "2025",
            "se-2025.1",
        ));
        for heading in [
            "## 1. Sammanfattning",
            "## 2. Börja här",
            "## 3. Potentiella möjligheter",
            "## 4. Varningar",
            "## 5. Saknad information",
            "## 6. Ekonomisk potential",
            "## 7. Evidens",
            "## 8. Gör detta nu",
            "## 9. Begränsningar",
        ] {
            assert!(markdown.contains(heading), "missing {heading}");
        }
    }

    #[test]
    fn the_disclaimer_is_always_in_the_rendered_report() {
        let markdown = to_markdown(&build(&empty_result(), "X AB", "2025", "v"));
        assert!(markdown.contains("Skattjakt är ett analys- och upptäcktsverktyg"));
    }

    #[test]
    fn every_empty_section_says_so_rather_than_rendering_blank() {
        let markdown = to_markdown(&build(&empty_result(), "X AB", "2025", "v"));
        assert!(markdown.contains("Inget fynd har nått hög prioritet"));
        assert!(markdown.contains("Inga åtgärder föreslås"));
    }
}

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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EconomicPotential {
    pub total: MoneyRange,
    pub display: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceSummary {
    pub document_versions_used: usize,
    pub values_cited: usize,
    pub rules_cited: Vec<CitedRule>,
    pub assumptions: Vec<String>,
}

/// Builds the report from a finished analysis.
pub fn build(
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
        format!(
            "Vi hittade {} {} som kan vara värda att undersöka.",
            result.summary.identified_opportunities,
            if result.summary.identified_opportunities == 1 {
                "sak"
            } else {
                "saker"
            }
        )
    };

    let total = result.summary.estimated_total;

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
                    "Ingen beräknad ekonomisk effekt på det underlag som lämnats.".to_string()
                } else {
                    "Ett intervall, inte ett besked. Beloppen bygger på det underlag som \
                     lämnats och ska verifieras innan någon åtgärd vidtas."
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
        },
        disclaimer: result.disclaimer.clone(),
        generated_at: result.generated_at,
    }
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
                ..
            } => {
                rules.push(CitedRule {
                    rule_id: rule_id.clone(),
                    title: title.clone(),
                    source: source.clone(),
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
        } else {
            opportunity.impact.to_string()
        },
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
                out.push_str(&format!("  - {} — {}\n", rule.title, rule.source));
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
        "{}\n\n{}\n\n",
        s.economic_potential.display, s.economic_potential.note
    ));

    out.push_str("## 7. Evidens\n\n");
    out.push_str(&format!(
        "{} dokumentversion(er), {} citerade värden, {} regler.\n\n",
        s.evidence.document_versions_used,
        s.evidence.values_cited,
        s.evidence.rules_cited.len()
    ));
    for rule in &s.evidence.rules_cited {
        out.push_str(&format!("- {} — {}\n", rule.title, rule.source));
    }
    if !s.evidence.assumptions.is_empty() {
        out.push_str("\nAntaganden:\n");
        for assumption in &s.evidence.assumptions {
            out.push_str(&format!("- {assumption}\n"));
        }
    }
    out.push('\n');

    out.push_str("## 8. Nästa steg\n\n");
    if s.next_steps.is_empty() {
        out.push_str("Inga åtgärder föreslås på det här underlaget.\n\n");
    }
    for (i, step) in s.next_steps.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, step));
    }
    out.push('\n');

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
            "## 8. Nästa steg",
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

//! Versioned prompts and output schemas.
//!
//! Prompts are versioned alongside the code (section 35, git first) and the
//! version is recorded on every model run, so an analysis can be reproduced
//! from document version + rule version + prompt version + model run.
//!
//! What the prompts deliberately do *not* contain: tax rules, thresholds, or
//! amounts. Those live in the rule engine. A prompt that quoted a percentage
//! would put the rule set in two places, and the copy in the prompt is the one
//! nobody updates.

use serde_json::{json, Value};

use crate::provider::ReasoningTask;

pub const PROMPT_VERSION: &str = "2025.1";

/// The framing shared by every task.
const SHARED_FRAMING: &str = "\
Du analyserar underlag från ett svenskt aktiebolag åt Skattjakt, ett verktyg som \
hjälper företagare att upptäcka frågor att ta vidare till sin redovisningskonsult.

Tre saker gäller genomgående:

Du avgör inte vad som är rätt enligt lag. En separat regelmotor prövar varje \
regels villkor, undantag och beräkning mot verifierade källor. Ange därför aldrig \
procentsatser, beloppsgränser eller skatteeffekter — beskriv iakttagelsen och \
vilken fråga den väcker.

Du får bara bygga på det som faktiskt står i underlaget. Om en siffra inte finns \
där ska du säga att den saknas, inte uppskatta den. Ett tomt svar är ett giltigt \
svar.

Skilj på vad du ser och vad du drar för slutsats. Formulera dig så att en \
redovisningskonsult kan följa resonemanget tillbaka till en post i underlaget.";

/// The system prompt for a task.
pub fn system_prompt(task: ReasoningTask) -> String {
    let specific = match task {
        ReasoningTask::FinancialExtraction => "\
Din uppgift nu: läs den extraherade texten och identifiera de ekonomiska poster \
du kan hitta. För varje post anger du vilken sida den stod på och exakt vilken \
textrad du läste den ur, så att beloppet kan stämmas av mot originalet. \
Belopp anges i hela kronor. Ta bara med poster du faktiskt ser.",

        ReasoningTask::OpportunityDiscovery => "\
Din uppgift nu: leta brett efter sådant som kan vara värt att undersöka — \
skattemässiga positioner, kostnader som kan vara felklassificerade, \
periodiseringar, momsfrågor, personalrelaterade frågor, investeringar, \
utvecklingsarbete, och sådant som ser ovanligt ut utan att nödvändigtvis vara fel.

Var generös i det här steget. Ett nästa steg kommer att försöka motbevisa varje \
kandidat, så det är bättre att lyfta något som senare faller än att missa det. \
Men varje kandidat måste peka på en konkret post i underlaget.",

        ReasoningTask::ContradictionCheck => "\
Din uppgift nu: försök motbevisa varje kandidat. Du är skeptikern, inte \
förespråkaren.

För var och en: finns underlaget faktiskt? Är iakttagelsen rimlig givet \
verksamheten? Finns en enklare förklaring? Är posten helt normal för ett bolag av \
den här typen? Har någon tolkat en siffra fel? Saknas något avgörande?

Utgå från att kandidaten är fel tills underlaget visar något annat. Om du är \
osäker ska den falla. Ett fynd som inte överlever är billigare än ett som inte \
håller.",

        ReasoningTask::FinalSynthesis => "\
Din uppgift nu: sammanfatta analysen för företagaren. Skriv på svenska, konkret \
och utan överdrifter. Säg vad som hittades, vad som bör tas vidare först, och vad \
som skulle göra analysen bättre. Lova ingenting om utfall.",

        ReasoningTask::DocumentUnderstanding => "\
Din uppgift nu: avgör vilken sorts dokument detta är, vilken period det avser och \
om bokslutet verkar preliminärt eller färdigt.",

        ReasoningTask::TaxReasoning => "\
Din uppgift nu: beskriv vilken skattemässig fråga iakttagelsen väcker och vilket \
underlag som skulle behövas för att avgöra den. Beräkna ingenting.",

        ReasoningTask::EvidenceValidation => "\
Din uppgift nu: bedöm om det angivna underlaget faktiskt bär slutsatsen, eller om \
kopplingen är svagare än den ser ut.",

        ReasoningTask::CalculationReview => "\
Din uppgift nu: granska om beräkningens ingångsvärden är de rimliga posterna att \
utgå från. Räkna inte om beloppet.",
    };

    format!("{SHARED_FRAMING}\n\n{specific}")
}

/// The output schema for a task. Every task returns structured data; none
/// returns prose the pipeline has to interpret.
pub fn output_schema(task: ReasoningTask) -> Value {
    match task {
        ReasoningTask::FinancialExtraction => extraction_schema(),
        ReasoningTask::OpportunityDiscovery => discovery_schema(),
        ReasoningTask::ContradictionCheck => skeptic_schema(),
        ReasoningTask::FinalSynthesis => synthesis_schema(),
        _ => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["observations"],
            "properties": {
                "observations": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            }
        }),
    }
}

fn extraction_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["facts", "unreadable_pages"],
        "properties": {
            "facts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "amount_sek", "source_page", "source_text", "reading_confidence"],
                    "properties": {
                        "kind": {"type": "string"},
                        "amount_sek": {"type": "integer"},
                        "account": {"type": "string"},
                        "source_page": {"type": "integer"},
                        "source_text": {"type": "string"},
                        "reading_confidence": {"type": "number"}
                    }
                }
            },
            "unreadable_pages": {
                "type": "array",
                "items": {"type": "integer"}
            },
            "accounts_state": {
                "type": "string",
                "enum": ["preliminary", "final", "unknown"]
            }
        }
    })
}

fn discovery_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["candidates"],
        "properties": {
            "candidates": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["title", "category", "observation", "question", "supporting_facts"],
                    "properties": {
                        "title": {"type": "string"},
                        "category": {
                            "type": "string",
                            "enum": ["tax", "costs", "vat", "personnel", "investments", "research_and_development", "risk"]
                        },
                        "observation": {"type": "string"},
                        "question": {"type": "string"},
                        "supporting_facts": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "missing_information": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "suggested_rule_ids": {
                            "type": "array",
                            "items": {"type": "string"}
                        }
                    }
                }
            }
        }
    })
}

fn skeptic_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["verdicts"],
        "properties": {
            "verdicts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["title", "survives", "reasoning", "objection_strength"],
                    "properties": {
                        "title": {"type": "string"},
                        "survives": {"type": "boolean"},
                        "reasoning": {"type": "string"},
                        // 0.0 = no objection found, 1.0 = the candidate is clearly wrong.
                        "objection_strength": {"type": "number"},
                        "alternative_explanation": {"type": "string"},
                        "missing_information": {
                            "type": "array",
                            "items": {"type": "string"}
                        }
                    }
                }
            }
        }
    })
}

fn synthesis_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "start_here", "limitations"],
        "properties": {
            "summary": {"type": "string"},
            "start_here": {
                "type": "array",
                "items": {"type": "string"}
            },
            "limitations": {
                "type": "array",
                "items": {"type": "string"}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_TASKS: [ReasoningTask; 8] = [
        ReasoningTask::DocumentUnderstanding,
        ReasoningTask::FinancialExtraction,
        ReasoningTask::OpportunityDiscovery,
        ReasoningTask::TaxReasoning,
        ReasoningTask::ContradictionCheck,
        ReasoningTask::EvidenceValidation,
        ReasoningTask::CalculationReview,
        ReasoningTask::FinalSynthesis,
    ];

    #[test]
    fn every_task_has_a_prompt_and_a_schema() {
        for task in ALL_TASKS {
            let prompt = system_prompt(task);
            assert!(prompt.len() > 200, "{task} has no task-specific prompt");
            let schema = output_schema(task);
            assert_eq!(schema["type"], "object", "{task} schema is not an object");
        }
    }

    #[test]
    fn no_prompt_contains_a_number_or_a_percentage() {
        // The rule set is not allowed to live in a prompt (section 10). Rates,
        // thresholds, amounts and statute references are all numeric, so the
        // sharpest possible guard is that a prompt contains no digits at all.
        for task in ALL_TASKS {
            let prompt = system_prompt(task);
            assert!(!prompt.contains('%'), "{task} prompt contains a percentage");
            if let Some(digit) = prompt.chars().find(char::is_ascii_digit) {
                panic!("{task} prompt contains the digit '{digit}' — a rule value has leaked into a prompt");
            }
        }
    }

    #[test]
    fn the_skeptic_prompt_asks_for_refutation_not_confirmation() {
        let prompt = system_prompt(ReasoningTask::ContradictionCheck).to_lowercase();
        assert!(prompt.contains("motbevisa"));
        assert!(prompt.contains("osäker"));
    }

    #[test]
    fn the_discovery_schema_requires_every_candidate_to_cite_support() {
        let schema = output_schema(ReasoningTask::OpportunityDiscovery);
        let required = schema["properties"]["candidates"]["items"]["required"]
            .as_array()
            .unwrap();
        assert!(required.contains(&json!("supporting_facts")));
        assert!(required.contains(&json!("observation")));
    }

    #[test]
    fn schemas_forbid_additional_properties_so_invented_fields_are_caught() {
        for task in ALL_TASKS {
            let schema = output_schema(task);
            assert_eq!(
                schema["additionalProperties"],
                json!(false),
                "{task} schema permits invented top-level fields"
            );
        }
    }

    #[test]
    fn the_extraction_schema_demands_page_and_verbatim_text() {
        let required = output_schema(ReasoningTask::FinancialExtraction)["properties"]["facts"]
            ["items"]["required"]
            .as_array()
            .unwrap()
            .clone();
        assert!(required.contains(&json!("source_page")));
        assert!(required.contains(&json!("source_text")));
    }

    #[test]
    fn the_shared_framing_tells_the_model_an_empty_answer_is_allowed() {
        // Section 32: finding nothing is a result. A prompt that implies the
        // model must produce findings manufactures them.
        assert!(SHARED_FRAMING.contains("Ett tomt svar är ett giltigt svar"));
    }
}

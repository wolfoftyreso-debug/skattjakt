//! # skattjakt-core
//!
//! The domain model shared by every other crate: money, company identity,
//! financial facts, evidence chains, opportunities, confidence and priority.
//!
//! This crate has no I/O, no database and no model client. It holds the rules
//! about *meaning* — what may be called actionable, what counts as evidence,
//! how confidence is arrived at — so those rules are testable in isolation and
//! cannot be quietly bypassed by a caller in a hurry.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod analysis;
pub mod classification;
pub mod company;
pub mod confidence;
pub mod document;
pub mod error;
pub mod evidence;
pub mod evidence_graph;
pub mod fact;
pub mod ids;
pub mod money;
pub mod opportunity;
pub mod state_machine;

pub use classification::{Classification, Classified, REDACTED};
pub use company::{CompanyProfile, FiscalYear, OrgNumber};
pub use confidence::{
    Confidence, ConfidenceBand, ConfidenceFactors, ConfidenceThresholds, ConfidenceWeights,
    UnitInterval,
};
pub use error::{CoreError, CoreResult};
pub use evidence::{CalculationInput, EvidenceChain, EvidenceItem};
pub use evidence_graph::{EdgeKind, EvidenceGraph, NodeId};
pub use fact::{Contradiction, FactKind, FactSet, FinancialFact};
pub use ids::*;
pub use money::{Money, MoneyRange, SEK};
pub use opportunity::{
    InvestigationEffort, Opportunity, OpportunityCategory, OpportunityStatus, Priority,
    PriorityBand, PriorityWeights, RiskLevel, Urgency,
};
pub use state_machine::{AnalysisEvent, AnalysisState, InvalidTransition, StateTransition};

/// The disclaimer from section 27. Held in code, in one place, so every surface
/// that shows results shows the same words.
pub const DISCLAIMER_SV: &str = "Skattjakt är ett analys- och upptäcktsverktyg. \
Resultaten är preliminära och ska inte betraktas som juridisk rådgivning, revisionsuttalande, \
skattebesked eller garanti om skatteåterbäring eller besparing. Identifierade möjligheter bör \
verifieras mot aktuella regler och företagets fullständiga underlag innan någon åtgärd vidtas.";

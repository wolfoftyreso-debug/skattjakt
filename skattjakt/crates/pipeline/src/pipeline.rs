//! The analysis orchestrator.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use skattjakt_core::analysis::{
    AnalysisResult, AnalysisStage, CoveredArea, Limitation, MissingInformation, Warning,
};
use skattjakt_core::document::AccountsState;
use skattjakt_core::{
    AnalysisId, CalculationId, CalculationInput, CompanyProfile, Confidence, ConfidenceFactors,
    ConfidenceThresholds, ConfidenceWeights, EvidenceChain, EvidenceItem, FactKind, FactSet,
    InvestigationEffort, MoneyRange, Opportunity, OpportunityCategory, OpportunityId,
    OpportunityStatus, PriorityWeights, RiskLevel, UnitInterval, Urgency,
};
use skattjakt_gateway::{wrap_document, Budget, GatewayError, InjectionScan, ModelGateway};
use skattjakt_model::{ModelRequest, ModelRunRecord, ModelRunStatus, ProviderError, ReasoningTask};
use skattjakt_rules::{
    ImpactSpec, ReviewState, Rule, RuleEngine, RuleEvaluation, RuleOutcome, SourceState,
};
use skattjakt_telemetry::{CorrelationId, SpanContext, TraceContext};
use thiserror::Error;

use crate::facts::{build_fact_set, DocumentInput};

/// Tunables for one deployment.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub confidence_weights: ConfidenceWeights,
    pub confidence_thresholds: ConfidenceThresholds,
    pub priority_weights: PriorityWeights,
    pub max_tokens: u32,
    pub effort: skattjakt_model::Effort,
    /// Characters of document text sent to the model per analysis. Bounded so
    /// a large upload cannot silently become a very large prompt.
    pub max_document_chars: usize,
    /// When true, a rule that has not been professionally reviewed can never
    /// produce a finding presented as established — the best it can reach is
    /// "needs verification". Defaults to on, and should stay on until the rule
    /// set has been through a qualified adviser.
    pub require_reviewed_rules_for_identified: bool,
    /// Whether a rule whose every cited source has been machine-verified may
    /// satisfy the gate above without a reviewer's signature.
    ///
    /// What verification establishes, precisely: a retrieval fetched each
    /// cited paragraph from the authority that publishes it, and
    /// the operative words and figures the rule depends on were present in the
    /// retrieved text. It recorded a timestamp and a hash, so the check is
    /// repeatable and the day the law moves the check fails.
    ///
    /// What it does not establish: that the rule applies its source correctly.
    /// A paragraph saying 25 percent does not establish that this rule computes
    /// the right base to apply it to. That question needs a person — but the
    /// arithmetic between the two is deterministic Rust with its own tests,
    /// which is a stronger guarantee than a signature on a document nobody can
    /// re-check afterwards.
    ///
    /// A cited source in the `mismatch` state overrides all of this: the
    /// finding drops to `investigate` whatever the flag says, because we then
    /// hold positive evidence the rule's basis is wrong.
    pub accept_verified_sources_in_place_of_review: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            confidence_weights: ConfidenceWeights::default(),
            confidence_thresholds: ConfidenceThresholds::default(),
            priority_weights: PriorityWeights::default(),
            max_tokens: 16_000,
            effort: skattjakt_model::Effort::High,
            max_document_chars: 60_000,
            require_reviewed_rules_for_identified: true,
            accept_verified_sources_in_place_of_review: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnalysisInput {
    pub analysis_id: AnalysisId,
    pub company: CompanyProfile,
    pub documents: Vec<DocumentInput>,
    pub accounts_state: AccountsState,
    /// How far each cited source had been checked when this analysis ran,
    /// keyed by registry id.
    ///
    /// An input rather than engine configuration, because it is part of what
    /// the answer rested on: the same accounts analysed before and after a
    /// verification sweep are two different analyses, and the evidence chain
    /// has to be able to say which one a reader is looking at.
    ///
    /// Empty means "nothing known beyond what shipped", and the embedded
    /// registry's own retrieval record is used. That is the honest default: a
    /// deployment with no database, or one whose worker has never swept, has
    /// not checked anything, and the embedded records say `unretrieved`.
    #[allow(clippy::doc_markdown)]
    pub source_states: BTreeMap<String, skattjakt_rules::Retrieval>,
}

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("no documents were supplied for analysis")]
    NoDocuments,

    #[error("the rule set has no version covering tax year {0}; the analysis cannot be trusted")]
    TaxYearNotCovered(i32),

    #[error("model provider failed: {0}")]
    Provider(#[from] ProviderError),
}

/// Reports progress so the UI can show the stages of section 3.
pub trait StageObserver: Send + Sync {
    fn stage(&self, stage: AnalysisStage);
}

/// Used when nobody is watching.
#[derive(Debug, Default)]
pub struct SilentObserver;

impl StageObserver for SilentObserver {
    fn stage(&self, _stage: AnalysisStage) {}
}

pub struct AnalysisPipeline {
    engine: Arc<RuleEngine>,
    /// Every model call goes through the gateway, never through a provider
    /// directly. That is not a style preference: the gateway is where the cost
    /// ceiling is checked, where a fallback is detected, and where the
    /// document-data fence is verified. A pipeline holding a bare
    /// `ModelProvider` would bypass all three, and would do so invisibly —
    /// which is exactly what it did before this type changed.
    gateway: Arc<ModelGateway>,
    config: PipelineConfig,
}

impl std::fmt::Debug for AnalysisPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalysisPipeline")
            .field("rule_set", &self.engine.version())
            .field("model", &self.gateway.model_id())
            .finish()
    }
}

/// What one model discovery candidate looked like.
#[derive(Debug, Clone)]
struct Candidate {
    title: String,
    category: OpportunityCategory,
    observation: String,
    question: String,
    supporting_facts: Vec<String>,
    missing_information: Vec<String>,
    suggested_rule_ids: Vec<String>,
}

/// The skeptic's verdict on one candidate.
#[derive(Debug, Clone)]
struct Verdict {
    survives: bool,
    reasoning: String,
    objection_strength: f64,
    missing_information: Vec<String>,
}

impl AnalysisPipeline {
    pub fn new(
        engine: Arc<RuleEngine>,
        gateway: Arc<ModelGateway>,
        config: PipelineConfig,
    ) -> Self {
        Self {
            engine,
            gateway,
            config,
        }
    }

    pub fn rule_set_version(&self) -> &str {
        self.engine.version()
    }

    /// How far a rule's sources have been checked, live state first.
    ///
    /// The folding rule — weakest wins, except that a contradiction dominates —
    /// lives in `skattjakt_rules::engine::combine` so the pipeline, the engine
    /// and the API cannot answer this question three different ways.
    fn source_state_of(&self, rule: &Rule, input: &AnalysisInput) -> SourceState {
        skattjakt_rules::engine::combine(rule.sources.iter().map(|id| {
            match input.source_states.get(id) {
                Some(live) => live.state,
                None => self
                    .engine
                    .set()
                    .source_by_id(id)
                    .map_or(SourceState::Unretrieved, |source| source.state()),
            }
        }))
    }

    /// Runs a complete analysis.
    ///
    /// Returns the result and the model-run records to persist. Model runs are
    /// returned rather than written here so the pipeline stays free of I/O.
    pub async fn run(
        &self,
        input: &AnalysisInput,
        observer: &dyn StageObserver,
    ) -> Result<(AnalysisResult, Vec<ModelRunRecord>), PipelineError> {
        let mut budget = self.gateway.config().budget();
        self.run_with_budget(input, observer, &mut budget, CorrelationId::new(), None)
            .await
    }

    /// Runs an analysis against a caller-supplied budget and trace.
    ///
    /// The worker uses this so a retried attempt resumes against what earlier
    /// attempts already spent — three attempts must cost one budget, not three
    /// — and so the model calls join the trace that started at the request.
    pub async fn run_with_budget(
        &self,
        input: &AnalysisInput,
        observer: &dyn StageObserver,
        budget: &mut Budget,
        correlation_id: CorrelationId,
        trace: Option<TraceContext>,
    ) -> Result<(AnalysisResult, Vec<ModelRunRecord>), PipelineError> {
        if input.documents.is_empty() {
            return Err(PipelineError::NoDocuments);
        }

        let span = trace
            .unwrap_or_else(TraceContext::root)
            .start_span("analysis.pipeline");

        let mut runs = Vec::new();
        let mut warnings = Vec::new();

        observer.stage(AnalysisStage::ReadingDocuments);
        let facts = build_fact_set(
            input.company.id,
            input.company.fiscal_year,
            &input.documents,
        );

        for document in &input.documents {
            for page in &document.extracted.unreadable_pages {
                warnings.push(Warning {
                    code: "unreadable_page".to_string(),
                    message: format!("Sida {page} innehöll ingen läsbar text."),
                    detail: Some(
                        "Sidan kan vara inskannad. En textbaserad PDF ger en bättre analys."
                            .to_string(),
                    ),
                });
            }
        }

        observer.stage(AnalysisStage::UnderstandingStructure);
        for contradiction in facts.contradictions() {
            warnings.push(Warning {
                code: "conflicting_values".to_string(),
                message: format!(
                    "Två olika värden för {} i underlaget: {} och {}.",
                    contradiction.kind.key(),
                    contradiction.a,
                    contradiction.b
                ),
                detail: Some("Kontrollera vilket underlag som är aktuellt.".to_string()),
            });
        }

        let tax_year = input.company.fiscal_year.tax_year();
        if !self.engine.covers_tax_year(tax_year) {
            // Section 31: a rule set with no version for the year is a failure
            // the user must be told about, not an empty result.
            return Err(PipelineError::TaxYearNotCovered(tax_year));
        }

        observer.stage(AnalysisStage::IdentifyingAreas);
        let constants = self
            .engine
            .constants_for(tax_year)
            .expect("coverage was just checked")
            .clone();
        let context = skattjakt_rules::context(
            &facts,
            &input.company,
            &constants,
            tax_year,
            input.accounts_state,
        );

        observer.stage(AnalysisStage::SearchingOpportunities);
        let (candidates, discovery_run, gateway_scan) = self
            .discover(input, &facts, budget, correlation_id, span)
            .await;

        // Both scans, taking the worse of each. The gateway sees the assembled
        // request; this sees the raw pages before `wrap_document` escaped a
        // forged fence out of existence. Neither alone is complete.
        let raw_scan = self.scan_documents(input);
        let injection = InjectionScan {
            instruction_like: gateway_scan.instruction_like.max(raw_scan.instruction_like),
            delimiter_attempts: gateway_scan
                .delimiter_attempts
                .max(raw_scan.delimiter_attempts),
            role_impersonation: gateway_scan
                .role_impersonation
                .max(raw_scan.role_impersonation),
            capability_probes: gateway_scan
                .capability_probes
                .max(raw_scan.capability_probes),
        };

        // A document that contains text addressed to an automated reader is
        // worth telling the customer about: they may not know it is there. The
        // threshold is deliberately above one match, because an accountant's
        // "ignorera ovanstående jämförelsetal" is ordinary bookkeeping language.
        if injection.warrants_a_warning() {
            warnings.push(Warning {
                code: "instruction_like_content".to_string(),
                message: "Underlaget innehåller text som är formulerad som en instruktion \
                          till ett automatiskt system."
                    .to_string(),
                detail: Some(
                    "Skattjakt behandlar allt innehåll i uppladdade dokument som underlag, \
                     aldrig som instruktioner, och analysen påverkas inte. Kontrollera gärna \
                     varifrån dokumentet kommer."
                        .to_string(),
                ),
            });
        }
        if let Some(run) = discovery_run {
            runs.push(run);
        }

        observer.stage(AnalysisStage::CheckingRules);
        let evaluations = self.engine.evaluate_all(&context);

        observer.stage(AnalysisStage::Falsifying);
        let (verdicts, skeptic_run) = self
            .falsify(
                input,
                &evaluations,
                &candidates,
                budget,
                correlation_id,
                span,
            )
            .await;
        if let Some(run) = skeptic_run {
            runs.push(run);
        }

        observer.stage(AnalysisStage::VerifyingCalculations);
        let mut opportunities = Vec::new();
        let mut rejected = Vec::new();
        let mut cleared: Vec<skattjakt_core::analysis::ClearedCheck> = Vec::new();

        for evaluation in &evaluations {
            if let RuleOutcome::RuleError { reason } = &evaluation.outcome {
                warnings.push(Warning {
                    code: "rule_error".to_string(),
                    message: format!("Regeln {} kunde inte utvärderas.", evaluation.rule_id),
                    detail: Some(reason.clone()),
                });
                continue;
            }
            let Some(rule) = self.engine.rule(&evaluation.rule_id) else {
                continue;
            };

            if !evaluation.outcome.produces_finding() {
                // A rule that did not fire is not automatically a clean bill of
                // health. It is only worth reporting as checked if it was
                // evaluated against values we actually read: a rule whose
                // trigger fact was never found decided nothing, and saying
                // "looks correct" about it would be the most damaging kind of
                // wrong — reassurance in place of a look.
                let referenced = rule.referenced_facts();
                let readable = referenced.iter().all(|kind| facts.get(kind).is_some());
                if readable {
                    let reason = match &evaluation.outcome {
                        RuleOutcome::NotApplicable { reason } => reason.clone(),
                        RuleOutcome::ExceptionApplies { explanation, .. } => explanation.clone(),
                        _ => continue,
                    };
                    cleared.push(skattjakt_core::analysis::ClearedCheck {
                        rule_id: rule.rule_id.clone(),
                        title: rule.title.clone(),
                        category: format!("{:?}", rule.category).to_lowercase(),
                        reason,
                    });
                }
                continue;
            }

            // A rule that could not read a single one of the values it depends
            // on has not found anything — it has failed to look. Presenting it
            // would be a finding with no document behind it, which section 7
            // forbids. What is missing still reaches the user, through the
            // missing-information section built from the same evaluations.
            // A rule driven purely by the company profile references no facts
            // and is exempt; it simply cannot reach "actionable", because the
            // confidence engine caps a finding with no document evidence.
            let referenced = rule.referenced_facts();
            if !referenced.is_empty() && !referenced.iter().any(|kind| facts.get(kind).is_some()) {
                continue;
            }

            let corroborated = candidates.iter().any(|c| {
                c.suggested_rule_ids
                    .iter()
                    .any(|id| id == &evaluation.rule_id)
                    || c.category == evaluation.category
            });

            let verdict = verdicts.get(&evaluation.title);
            let opportunity =
                self.build_rule_opportunity(input, &facts, rule, evaluation, corroborated, verdict);

            if opportunity.status == OpportunityStatus::Rejected {
                rejected.push(opportunity);
            } else {
                opportunities.push(opportunity);
            }
        }

        // Candidates the rule set had nothing to say about. They are still
        // worth surfacing as questions, but with no rule behind them they can
        // never be presented as established or carry a figure.
        for candidate in &candidates {
            let already_covered = evaluations.iter().any(|e| {
                candidate.suggested_rule_ids.contains(&e.rule_id) && e.outcome.produces_finding()
            });
            if already_covered {
                continue;
            }

            let verdict = verdicts.get(&candidate.title);
            if verdict.is_some_and(|v| !v.survives) {
                rejected.push(self.build_candidate_opportunity(input, &facts, candidate, verdict));
                continue;
            }

            opportunities.push(self.build_candidate_opportunity(input, &facts, candidate, verdict));
        }

        observer.stage(AnalysisStage::Prioritising);
        opportunities.sort_by(|a, b| {
            b.priority
                .score
                .partial_cmp(&a.priority.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let covered_areas = self.covered_areas(&evaluations, &opportunities);
        let missing_information =
            self.missing_information(&evaluations, &opportunities, &input.company);
        let limitations = self.limitations(input, &evaluations);

        observer.stage(AnalysisStage::Done);

        let result = AnalysisResult::summarise(
            input.analysis_id,
            input.company.id,
            opportunities,
            warnings,
            missing_information,
            covered_areas,
            cleared,
            limitations,
            rejected,
        );

        Ok((result, runs))
    }

    /// Pass 1: broad discovery (section 11).
    ///
    /// A model failure here degrades the analysis to rules-only rather than
    /// failing it: the rule engine does not need the model to produce findings,
    /// and a rules-only result is still a useful, evidence-backed one.
    async fn discover(
        &self,
        input: &AnalysisInput,
        facts: &FactSet,
        budget: &mut Budget,
        correlation_id: CorrelationId,
        span: SpanContext,
    ) -> (Vec<Candidate>, Option<ModelRunRecord>, InjectionScan) {
        let request = ModelRequest {
            task: ReasoningTask::OpportunityDiscovery,
            prompt_version: skattjakt_model::PROMPT_VERSION.to_string(),
            system: skattjakt_model::system_prompt(ReasoningTask::OpportunityDiscovery),
            user_content: self.describe_company(input, facts),
            schema: skattjakt_model::output_schema(ReasoningTask::OpportunityDiscovery),
            max_tokens: self.config.max_tokens,
            effort: self.config.effort,
        };

        let started = Utc::now();
        match self
            .gateway
            .run(&request, budget, correlation_id, span)
            .await
        {
            Ok(outcome) => {
                let candidates = parse_candidates(&outcome.response.output);
                let record = self.record(
                    input,
                    &request,
                    ModelRunStatus::Succeeded,
                    started,
                    Some(&outcome.response),
                    None,
                );
                (candidates, Some(record), outcome.injection)
            }
            Err(error) => {
                let record = self.record(
                    input,
                    &request,
                    match &error {
                        GatewayError::Provider(ProviderError::Refused { .. }) => {
                            ModelRunStatus::Refused
                        }
                        _ => ModelRunStatus::Failed,
                    },
                    started,
                    None,
                    // The error *kind*, not its message. A provider message can
                    // quote the request, and the request contains the customer's
                    // figures.
                    Some(error.kind().to_string()),
                );
                (Vec::new(), Some(record), InjectionScan::default())
            }
        }
    }

    /// Pass 2: the skeptic (section 11).
    ///
    /// When the skeptic cannot run, findings are not silently promoted. The
    /// contradiction factor stays unknown, which the confidence engine treats
    /// as an absence of corroboration rather than as a clean bill of health.
    async fn falsify(
        &self,
        input: &AnalysisInput,
        evaluations: &[RuleEvaluation],
        candidates: &[Candidate],
        budget: &mut Budget,
        correlation_id: CorrelationId,
        span: SpanContext,
    ) -> (BTreeMap<String, Verdict>, Option<ModelRunRecord>) {
        let mut listing = String::new();
        for evaluation in evaluations.iter().filter(|e| e.outcome.produces_finding()) {
            listing.push_str(&format!(
                "- {} (regel {}): {}\n",
                evaluation.title,
                evaluation.rule_id,
                match &evaluation.outcome {
                    RuleOutcome::Matched => "regelns villkor är uppfyllda".to_string(),
                    RuleOutcome::Indeterminate { reason } => reason.clone(),
                    _ => String::new(),
                }
            ));
        }
        for candidate in candidates {
            listing.push_str(&format!(
                "- {}: {}\n",
                candidate.title, candidate.observation
            ));
        }

        if listing.is_empty() {
            return (BTreeMap::new(), None);
        }

        let request = ModelRequest {
            task: ReasoningTask::ContradictionCheck,
            prompt_version: skattjakt_model::PROMPT_VERSION.to_string(),
            system: skattjakt_model::system_prompt(ReasoningTask::ContradictionCheck),
            user_content: format!(
                "Kandidater att pröva:\n{listing}\nUnderlag om bolaget:\n{}",
                self.describe_profile(&input.company)
            ),
            schema: skattjakt_model::output_schema(ReasoningTask::ContradictionCheck),
            max_tokens: self.config.max_tokens,
            effort: self.config.effort,
        };

        let started = Utc::now();
        match self
            .gateway
            .run(&request, budget, correlation_id, span)
            .await
        {
            Ok(outcome) => {
                let verdicts = parse_verdicts(&outcome.response.output);
                let record = self.record(
                    input,
                    &request,
                    ModelRunStatus::Succeeded,
                    started,
                    Some(&outcome.response),
                    None,
                );
                (verdicts, Some(record))
            }
            Err(error) => {
                let record = self.record(
                    input,
                    &request,
                    match &error {
                        GatewayError::Provider(ProviderError::Refused { .. }) => {
                            ModelRunStatus::Refused
                        }
                        _ => ModelRunStatus::Failed,
                    },
                    started,
                    None,
                    Some(error.kind().to_string()),
                );
                (BTreeMap::new(), Some(record))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_rule_opportunity(
        &self,
        input: &AnalysisInput,
        facts: &FactSet,
        rule: &Rule,
        evaluation: &RuleEvaluation,
        corroborated: bool,
        verdict: Option<&Verdict>,
    ) -> Opportunity {
        let mut evidence = EvidenceChain::new();

        // Document values the rule actually reads.
        let needed = rule.referenced_facts();
        let mut traceable = 0usize;
        for kind in &needed {
            if let Some(fact) = facts.get(kind) {
                if fact.is_traceable() {
                    traceable += 1;
                }
                evidence.push(EvidenceItem::DocumentValue {
                    document_id: input
                        .documents
                        .iter()
                        .find(|d| d.document_version_id == fact.document_version_id)
                        .map(|d| d.document_id)
                        .unwrap_or_else(skattjakt_core::DocumentId::new),
                    document_version_id: fact.document_version_id,
                    fact_id: fact.id,
                    kind: fact.kind.clone(),
                    value: fact.value,
                    page: fact.source_page,
                    excerpt: fact.source_text.clone(),
                });
            }
        }

        // Every authority the rule rests on, each with how far it has been
        // checked. The single citation string this replaced could say a rule
        // came from `30 kap. 5–6 §§` without anyone ever having opened either.
        let citations: Vec<skattjakt_core::Citation> = rule
            .sources
            .iter()
            .filter_map(|id| {
                self.engine
                    .set()
                    .source_by_id(id)
                    .map(|source| (id, source))
            })
            .map(|(id, source)| {
                // The live record when a sweep has produced one, the embedded
                // record otherwise. The registry supplies what the source *is*;
                // the database supplies how far anyone has got with it.
                let retrieval = input.source_states.get(id).unwrap_or(&source.retrieval);
                skattjakt_core::Citation {
                    reference: source.citation(),
                    url: source.url.clone(),
                    claim: source.asserted_claim.clone(),
                    state: retrieval.state.as_str().to_string(),
                    retrieved_at: retrieval.at.clone(),
                }
            })
            .collect();

        evidence.push(EvidenceItem::Rule {
            rule_id: rule.rule_id.clone(),
            rule_version: rule.version.clone(),
            title: rule.title.clone(),
            source: citations
                .iter()
                .map(|c| c.reference.as_str())
                .collect::<Vec<_>>()
                .join("; "),
            citations,
        });

        if let Some(calculation) = &evaluation.calculation {
            evidence.push(EvidenceItem::Calculation {
                calculation_id: CalculationId::new(),
                method: calculation.method.clone(),
                inputs: calculation
                    .inputs
                    .iter()
                    .map(|i| CalculationInput {
                        name: i.name.clone(),
                        value: i.value,
                        fact_id: None,
                    })
                    .collect(),
                result_low: calculation.result_low,
                result_high: calculation.result_high,
            });
        }

        if let ReviewState::AwaitingProfessionalReview { note } = &rule.review {
            if !note.is_empty() {
                evidence.push(EvidenceItem::Assumption {
                    statement: note.clone(),
                });
            }
        }

        if corroborated {
            evidence.push(EvidenceItem::ModelJudgement {
                model_run_id: skattjakt_core::ModelRunId::new(),
                task: ReasoningTask::OpportunityDiscovery.key().to_string(),
                rationale_summary: "Modellens genomgång pekade ut samma område.".to_string(),
            });
        }

        // Confidence factors — measured, not asserted (section 12).
        let document_evidence = if needed.is_empty() {
            UnitInterval::ZERO
        } else {
            UnitInterval::saturating(traceable as f64 / needed.len() as f64)
        };

        let rule_match = match &evaluation.outcome {
            RuleOutcome::Matched => UnitInterval::ONE,
            RuleOutcome::Indeterminate { .. } => UnitInterval::saturating(0.4),
            _ => UnitInterval::ZERO,
        };

        let calculation_certainty = match (&evaluation.calculation, &rule.impact) {
            (Some(_), _) => UnitInterval::ONE,
            // A rule with nothing to compute is not uncertain about its
            // arithmetic; it simply has none.
            (None, ImpactSpec::None) => UnitInterval::saturating(0.7),
            (None, _) => UnitInterval::saturating(0.2),
        };

        let gaps = evaluation.missing_facts.len() + evaluation.unanswered_questions.len();
        let missing_information = UnitInterval::saturating(
            gaps as f64 / (needed.len().max(1) + evaluation.unanswered_questions.len()) as f64,
        );

        let contradiction_score =
            UnitInterval::saturating(verdict.map(|v| v.objection_strength).unwrap_or(0.0));

        let model_agreement = if corroborated {
            UnitInterval::ONE
        } else {
            UnitInterval::saturating(0.5)
        };

        let confidence = Confidence::compute(
            &ConfidenceFactors {
                document_evidence,
                rule_match,
                calculation_certainty,
                missing_information,
                contradiction_score,
                model_agreement,
            },
            &self.config.confidence_weights,
            &self.config.confidence_thresholds,
        );

        let impact = evaluation.impact.unwrap_or(MoneyRange::ZERO);

        let priority = if rule.category == OpportunityCategory::Risk || impact.is_zero() {
            Opportunity::compute_risk_priority(confidence, rule.risk)
        } else {
            Opportunity::compute_priority(
                impact,
                confidence,
                rule.effort,
                rule.urgency,
                rule.relevance,
                &self.config.priority_weights,
            )
        };

        let status = self.status_for(rule, evaluation, verdict, &evidence, confidence, input);

        let mut missing = evaluation
            .missing_facts
            .iter()
            .map(humanise_fact)
            .collect::<Vec<_>>();
        missing.extend(rule.missing_information_hints.iter().cloned());
        missing.extend(
            evaluation
                .unanswered_questions
                .iter()
                .map(|q| q.question_sv().to_string()),
        );
        if let Some(v) = verdict {
            missing.extend(v.missing_information.iter().cloned());
        }
        missing.dedup();

        Opportunity {
            id: OpportunityId::new(),
            analysis_id: input.analysis_id,
            category: rule.category,
            status,
            title: rule.title.clone(),
            rationale: self.rationale_for(rule, evaluation, verdict),
            impact,
            effect: rule.effect,
            evidence,
            missing_information: missing,
            recommended_action: rule.recommended_action.clone(),
            confidence,
            risk: rule.risk,
            effort: rule.effort,
            urgency: rule.urgency,
            priority,
            rule_ids: vec![rule.rule_id.clone()],
            rejection_reason: verdict.filter(|v| !v.survives).map(|v| v.reasoning.clone()),
            created_at: Utc::now(),
        }
    }

    /// The status ladder, including the review gate.
    fn status_for(
        &self,
        rule: &Rule,
        evaluation: &RuleEvaluation,
        verdict: Option<&Verdict>,
        evidence: &EvidenceChain,
        confidence: Confidence,
        input: &AnalysisInput,
    ) -> OpportunityStatus {
        if verdict.is_some_and(|v| !v.survives) {
            return OpportunityStatus::Rejected;
        }
        if rule.category == OpportunityCategory::Risk {
            return OpportunityStatus::Warning;
        }
        if matches!(evaluation.outcome, RuleOutcome::Indeterminate { .. }) {
            return OpportunityStatus::Investigate;
        }
        // No actionable claim without a document value and a rule (section 7).
        if evidence.validate_actionable().is_err() || !confidence.is_actionable() {
            return OpportunityStatus::Investigate;
        }
        // A cited source that contradicts the rule set is not a weaker form of
        // no evidence — it is evidence pointing the other way, and it outranks
        // both the review state and the flag below. Either the law changed or
        // the rule was wrong when it was written; both need a person.
        let sources = self.source_state_of(rule, input);
        if sources == SourceState::Mismatch {
            return OpportunityStatus::Investigate;
        }
        // The review gate: an unverified rule cannot produce an established
        // finding, however well its conditions matched. Two things can satisfy
        // it — a reviewer's signature, or every cited source having been
        // fetched and found to say what the rule assumes. See the flag's
        // documentation for what the second of those does and does not prove.
        let grounded = rule.review.is_reviewed()
            || (self.config.accept_verified_sources_in_place_of_review
                && sources == SourceState::Verified);
        if self.config.require_reviewed_rules_for_identified && !grounded {
            return OpportunityStatus::Verify;
        }
        OpportunityStatus::Identified
    }

    fn rationale_for(
        &self,
        rule: &Rule,
        evaluation: &RuleEvaluation,
        verdict: Option<&Verdict>,
    ) -> String {
        let mut text = rule.description.clone();
        if let RuleOutcome::Indeterminate { reason } = &evaluation.outcome {
            text.push_str(&format!(" {reason}."));
        }
        for note in &evaluation.exception_notes {
            text.push_str(&format!(" {note}"));
        }
        if let Some(v) = verdict.filter(|v| v.objection_strength > 0.0 && v.survives) {
            text.push_str(&format!(" Invändning att beakta: {}", v.reasoning));
        }
        text
    }

    /// A finding the model raised that no rule covers.
    fn build_candidate_opportunity(
        &self,
        input: &AnalysisInput,
        facts: &FactSet,
        candidate: &Candidate,
        verdict: Option<&Verdict>,
    ) -> Opportunity {
        let mut evidence = EvidenceChain::new();
        let mut anchored = 0usize;

        for name in &candidate.supporting_facts {
            if let Some(kind) = fact_kind_from_key(name) {
                if let Some(fact) = facts.get(&kind) {
                    anchored += 1;
                    evidence.push(EvidenceItem::DocumentValue {
                        document_id: input
                            .documents
                            .iter()
                            .find(|d| d.document_version_id == fact.document_version_id)
                            .map(|d| d.document_id)
                            .unwrap_or_else(skattjakt_core::DocumentId::new),
                        document_version_id: fact.document_version_id,
                        fact_id: fact.id,
                        kind: fact.kind.clone(),
                        value: fact.value,
                        page: fact.source_page,
                        excerpt: fact.source_text.clone(),
                    });
                }
            }
        }

        evidence.push(EvidenceItem::ModelJudgement {
            model_run_id: skattjakt_core::ModelRunId::new(),
            task: ReasoningTask::OpportunityDiscovery.key().to_string(),
            rationale_summary: candidate.observation.clone(),
        });

        let confidence = Confidence::compute(
            &ConfidenceFactors {
                document_evidence: UnitInterval::saturating(
                    anchored as f64 / candidate.supporting_facts.len().max(1) as f64,
                ),
                // No rule matched — this is what caps the finding.
                rule_match: UnitInterval::ZERO,
                calculation_certainty: UnitInterval::ZERO,
                missing_information: UnitInterval::saturating(
                    if candidate.missing_information.is_empty() {
                        0.4
                    } else {
                        0.8
                    },
                ),
                contradiction_score: UnitInterval::saturating(
                    verdict.map(|v| v.objection_strength).unwrap_or(0.0),
                ),
                model_agreement: UnitInterval::ONE,
            },
            &self.config.confidence_weights,
            &self.config.confidence_thresholds,
        );

        Opportunity {
            id: OpportunityId::new(),
            analysis_id: input.analysis_id,
            category: candidate.category,
            status: if verdict.is_some_and(|v| !v.survives) {
                OpportunityStatus::Rejected
            } else {
                OpportunityStatus::Investigate
            },
            title: candidate.title.clone(),
            rationale: candidate.observation.clone(),
            // No rule, no calculation, therefore no figure. Section 13 does not
            // allow a number that nothing computed.
            impact: MoneyRange::ZERO,
            // A model-only finding carries no amount, so the distinction
            // between saved and postponed tax has nothing to apply to.
            effect: skattjakt_core::opportunity::EffectKind::Reduction,
            evidence,
            missing_information: candidate.missing_information.clone(),
            recommended_action: candidate.question.clone(),
            confidence,
            risk: RiskLevel::Medium,
            effort: InvestigationEffort::Medium,
            urgency: Urgency::Routine,
            priority: Opportunity::compute_risk_priority(confidence, RiskLevel::Low),
            rule_ids: Vec::new(),
            rejection_reason: verdict.filter(|v| !v.survives).map(|v| v.reasoning.clone()),
            created_at: Utc::now(),
        }
    }

    fn covered_areas(
        &self,
        evaluations: &[RuleEvaluation],
        opportunities: &[Opportunity],
    ) -> Vec<CoveredArea> {
        use OpportunityCategory::*;
        [
            Tax,
            Costs,
            Vat,
            Personnel,
            Investments,
            ResearchAndDevelopment,
            Risk,
        ]
        .into_iter()
        .map(|category| CoveredArea {
            category: category.label_sv().to_string(),
            rules_evaluated: evaluations
                .iter()
                .filter(|e| e.category == category)
                .count(),
            findings: opportunities
                .iter()
                .filter(|o| o.category == category)
                .count(),
        })
        .collect()
    }

    /// What would make the analysis better, gathered in one place.
    ///
    /// Three sources, and the third was missing for as long as this existed.
    /// A profile question nobody answered and a fact we could not read both
    /// land here — but the rules' own `missing_information_hints`, which are
    /// literally requests for a document ("Föregående års K10-blankett"), only
    /// ever reached the individual finding.
    ///
    /// The effect was that the section went **empty exactly when the analysis
    /// worked**: a complete profile and readable documents leave nothing in the
    /// first two sources, while every finding that fired carries two or three
    /// things a person could go and fetch. On the run this was found in, twenty
    /// such requests sat inside eight findings and the section a customer reads
    /// to know what to do next had nothing in it.
    ///
    /// `SKATTJAKT_ENGINEERING_DECISIONS.md` already promised this: "The
    /// information is not lost — it surfaces in `missing_information`, which is
    /// where a request for a document belongs." Half of it was true.
    fn missing_information(
        &self,
        evaluations: &[RuleEvaluation],
        opportunities: &[Opportunity],
        profile: &CompanyProfile,
    ) -> Vec<MissingInformation> {
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();

        for evaluation in evaluations {
            for question in &evaluation.unanswered_questions {
                if seen.insert(question.question_sv().to_string()) {
                    out.push(MissingInformation {
                        code: "profile_question".to_string(),
                        description: question.question_sv().to_string(),
                        unlocks: format!("Gör att regeln \"{}\" kan prövas.", evaluation.title),
                    });
                }
            }
            for fact in &evaluation.missing_facts {
                let described = humanise_fact(fact);
                if seen.insert(described.clone()) {
                    out.push(MissingInformation {
                        code: "missing_fact".to_string(),
                        description: described,
                        unlocks: format!("Behövs för att bedöma \"{}\".", evaluation.title),
                    });
                }
            }
        }

        // What the findings themselves say would make them stronger.
        //
        // `seen` is shared with the two loops above deliberately: a finding's
        // list already contains the humanised missing facts and the unanswered
        // questions, and those are better described where they were added — an
        // unanswered question says which rule it would unlock, which is more
        // useful than saying which finding it would strengthen.
        //
        // Ordered by how many findings each unlocks, because a document that
        // strengthens three findings is the one to fetch first. Alphabetical
        // within a tie, so the order is stable between runs.
        let mut strengthens: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for opportunity in opportunities.iter().filter(|o| o.status.is_presented()) {
            for item in &opportunity.missing_information {
                if seen.contains(item) {
                    continue;
                }
                let titles = strengthens.entry(item.clone()).or_default();
                if !titles.contains(&opportunity.title) {
                    titles.push(opportunity.title.clone());
                }
            }
        }
        let mut ranked: Vec<(String, Vec<String>)> = strengthens.into_iter().collect();
        ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
        for (description, titles) in ranked {
            out.push(MissingInformation {
                code: "would_strengthen".to_string(),
                unlocks: format!("Stärker {}.", quoted_list(&titles)),
                description,
            });
        }

        if profile.unanswered_fields().len() > 4 {
            out.push(MissingInformation {
                code: "sparse_profile".to_string(),
                description: "Företagsprofilen är ofullständig.".to_string(),
                unlocks: "Fler frågor om verksamheten gör att fler regler kan prövas.".to_string(),
            });
        }

        out
    }

    fn limitations(
        &self,
        input: &AnalysisInput,
        evaluations: &[RuleEvaluation],
    ) -> Vec<Limitation> {
        let mut limitations = vec![
            Limitation {
                statement: format!(
                    "Analysen bygger på {} uppladdat underlag och på de uppgifter som lämnats om bolaget. Den har inte tillgång till bokföringen i övrigt.",
                    input.documents.len()
                ),
            },
            Limitation {
                statement: "Skattjakt avgör inte vad som är rätt enligt lag. Varje möjlighet behöver stämmas av mot fullständigt underlag.".to_string(),
            },
        ];

        if evaluations.iter().any(|e| e.awaiting_review) {
            limitations.push(Limitation {
                statement: "Delar av regelverket i den här versionen har inte granskats av en kvalificerad rådgivare. Sådana fynd visas som \"Verifiera\" och ska kontrolleras innan de används.".to_string(),
            });
        }

        if input.accounts_state == AccountsState::Preliminary {
            limitations.push(Limitation {
                statement: "Bokslutet är preliminärt. Siffrorna kan komma att ändras.".to_string(),
            });
        }

        limitations
    }

    /// The description sent to the model.
    ///
    /// Section 21: only what the task needs. The company is named because the
    /// analysis is about it; the document text is bounded.
    fn describe_company(&self, input: &AnalysisInput, facts: &FactSet) -> String {
        let mut text = self.describe_profile(&input.company);

        text.push_str("\n\nExtraherade ekonomiska poster:\n");
        if facts.is_empty() {
            text.push_str("(inga poster kunde läsas ur underlaget)\n");
        }
        for fact in facts.iter() {
            text.push_str(&format!(
                "- {}: {} (sida {}, \"{}\")\n",
                fact.kind.key(),
                fact.value,
                fact.source_page
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".into()),
                fact.source_text.as_deref().unwrap_or("")
            ));
        }

        // Document text is the one part of this prompt an attacker controls.
        // It goes in wrapped as data, through the only function permitted to
        // put it there, and the gateway verifies the fence is intact before the
        // request leaves. Concatenating it directly — which is what this did
        // before — is exactly the mistake section 51 names.
        let mut raw = String::new();
        let mut budget = self.config.max_document_chars;
        let mut truncated = false;
        for document in &input.documents {
            for page in &document.extracted.pages {
                if budget == 0 {
                    truncated = true;
                    break;
                }
                let take = page.text.chars().take(budget).collect::<String>();
                budget = budget.saturating_sub(take.chars().count());
                raw.push_str(&format!("\n--- sida {} ---\n{}\n", page.number, take));
            }
            if truncated {
                break;
            }
        }
        if truncated {
            raw.push_str("\n[underlaget är avkortat här]\n");
        }

        text.push_str("\nUnderlagets text:\n");
        text.push_str(&wrap_document("uppladdat underlag", &raw));

        text
    }

    /// Scans the raw document text, before wrapping.
    ///
    /// Before, not after, and the order is the point: `wrap_document` escapes a
    /// forged fence out of existence, so a document whose only hostile feature
    /// was an attempt to close the block would look clean by the time the
    /// gateway sees it. The gateway's scan catches everything else; this
    /// catches the one thing wrapping removes.
    fn scan_documents(&self, input: &AnalysisInput) -> InjectionScan {
        let mut total = InjectionScan::default();
        for document in &input.documents {
            for page in &document.extracted.pages {
                let scan = skattjakt_gateway::scan(&page.text);
                total.instruction_like += scan.instruction_like;
                total.delimiter_attempts += scan.delimiter_attempts;
                total.role_impersonation += scan.role_impersonation;
                total.capability_probes += scan.capability_probes;
            }
        }
        total
    }

    fn describe_profile(&self, profile: &CompanyProfile) -> String {
        let describe = |value: Option<bool>| match value {
            Some(true) => "ja",
            Some(false) => "nej",
            None => "ej besvarat",
        };

        format!(
            "Bolag: {} ({})\nRäkenskapsår: {}\nBransch: {}\nAntal anställda: {}\nAntal ägare: {}\n\
             Ingår i koncern: {}\nVerksamhet utanför Sverige: {}\nBedriver utvecklingsarbete: {}\n\
             Äger lokaler: {}\nHar fordon: {}\nÄgare verksamma i bolaget: {}",
            profile.name,
            profile.org_number,
            profile.fiscal_year.label(),
            profile.industry.as_deref().unwrap_or("ej angiven"),
            profile
                .employee_count
                .map(|c| c.to_string())
                .unwrap_or_else(|| "ej angivet".into()),
            profile
                .owner_count
                .map(|c| c.to_string())
                .unwrap_or_else(|| "ej angivet".into()),
            describe(profile.in_group),
            describe(profile.operations_outside_sweden),
            describe(profile.does_development_work),
            describe(profile.owns_premises),
            describe(profile.has_vehicles),
            describe(profile.owners_active_in_company),
        )
    }

    fn record(
        &self,
        input: &AnalysisInput,
        request: &ModelRequest,
        status: ModelRunStatus,
        started: chrono::DateTime<Utc>,
        response: Option<&skattjakt_model::ModelResponse>,
        error: Option<String>,
    ) -> ModelRunRecord {
        ModelRunRecord {
            id: skattjakt_core::ModelRunId::new(),
            analysis_id: input.analysis_id,
            company_id: input.company.id,
            provider: "gateway".to_string(),
            model: self.gateway.model_id().to_string(),
            task: request.task,
            prompt_version: request.prompt_version.clone(),
            document_version_ids: input
                .documents
                .iter()
                .map(|d| d.document_version_id)
                .collect(),
            status,
            usage: response.map(|r| r.usage).unwrap_or_default(),
            latency_ms: response.map(|r| r.latency_ms).unwrap_or(0),
            output: response
                .map(|r| r.output.clone())
                .unwrap_or(serde_json::Value::Null),
            started_at: started,
            finished_at: Utc::now(),
            error,
        }
    }
}

fn parse_candidates(output: &serde_json::Value) -> Vec<Candidate> {
    output
        .get("candidates")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(Candidate {
                        title: item.get("title")?.as_str()?.to_string(),
                        category: serde_json::from_value(item.get("category")?.clone()).ok()?,
                        observation: item.get("observation")?.as_str()?.to_string(),
                        question: item.get("question")?.as_str()?.to_string(),
                        supporting_facts: string_list(item.get("supporting_facts")),
                        missing_information: string_list(item.get("missing_information")),
                        suggested_rule_ids: string_list(item.get("suggested_rule_ids")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_verdicts(output: &serde_json::Value) -> BTreeMap<String, Verdict> {
    let mut map = BTreeMap::new();
    let Some(items) = output.get("verdicts").and_then(|v| v.as_array()) else {
        return map;
    };
    for item in items {
        let Some(title) = item.get("title").and_then(|v| v.as_str()) else {
            continue;
        };
        map.insert(
            title.to_string(),
            Verdict {
                survives: item
                    .get("survives")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                reasoning: item
                    .get("reasoning")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                objection_strength: item
                    .get("objection_strength")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
                missing_information: string_list(item.get("missing_information")),
            },
        );
    }
    map
}

fn string_list(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn fact_kind_from_key(key: &str) -> Option<FactKind> {
    serde_json::from_value(serde_json::Value::String(key.to_string())).ok()
}

/// `"A"`, `"A" och "B"`, `"A", "B" och "C"` — Swedish list punctuation.
///
/// Worth a function rather than a `join`: a list that reads "A, B, C" in a
/// sentence a customer is meant to act on looks like a machine wrote it, and
/// this text is the product telling them what to go and fetch.
fn quoted_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("\"{i}\"")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} och {last}", rest.join(", ")),
    }
}

fn humanise_fact(kind: &FactKind) -> String {
    let label = match kind {
        FactKind::TaxableResult => "skattemässigt resultat",
        FactKind::CarriedForwardLoss => "outnyttjat underskott från tidigare år",
        FactKind::TaxAllocationReserve => "befintliga periodiseringsfonder",
        FactKind::TaxAllocationReserveThisYear => "årets avsättning till periodiseringsfond",
        FactKind::Depreciation => "årets avskrivningar",
        FactKind::FixedAssets => "materiella anläggningstillgångar",
        FactKind::PersonnelCosts => "personalkostnader",
        FactKind::PensionCosts => "pensionskostnader",
        FactKind::InterestExpense => "räntekostnader",
        FactKind::UntaxedReserves => "obeskattade reserver",
        FactKind::TotalAssets => "summa tillgångar",
        FactKind::TotalEquityAndLiabilities => "summa eget kapital och skulder",
        FactKind::ExternalCosts => "övriga externa kostnader",
        other => return format!("uppgift saknas: {}", other.key()),
    };
    format!("Uppgift om {label} saknas i underlaget.")
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;

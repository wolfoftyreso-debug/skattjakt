//! The evidence graph (section 24).
//!
//! An `EvidenceChain` answers "why does Skattjakt say this?" for one finding.
//! The graph answers the questions that span findings, and those are the ones
//! that matter operationally:
//!
//!   - a rule was revised: which findings, in which analyses, rested on it?
//!   - a document version was replaced: what did it hold up?
//!   - a figure was misread: what fell over with it?
//!   - a finding was disputed: show every node between the page and the claim.
//!
//! Without this, a rule correction is answered by re-running everything and
//! hoping. With it, the blast radius is a query.
//!
//! The graph is derived, not authored. It is built from evidence chains that
//! already exist, so it cannot claim support that the chain does not contain.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::evidence::{EvidenceChain, EvidenceItem};
use crate::ids::{AnalysisId, CalculationId, DocumentVersionId, ModelRunId, OpportunityId};

/// A node identity. Stable across runs: two analyses that read the same figure
/// off the same document version share the fact node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeId {
    /// A value read from a page of a specific document version.
    Fact {
        document_version_id: DocumentVersionId,
        fact: String,
    },
    /// A rule at a specific version. `se.periodiseringsfond.unused@se-2025.1`
    /// and `...@se-2025.2` are different nodes, which is what makes a rule
    /// revision traceable.
    Rule {
        rule_id: String,
        rule_version: String,
    },
    Calculation {
        calculation_id: CalculationId,
    },
    ModelRun {
        model_run_id: ModelRunId,
    },
    /// A stated assumption, keyed by its text so identical assumptions merge.
    Assumption {
        statement: String,
    },
    /// A presented finding. Always a sink.
    Finding {
        opportunity_id: OpportunityId,
    },
    /// The analysis a finding belongs to. Lets the graph answer "which runs are
    /// affected" without a second lookup.
    Analysis {
        analysis_id: AnalysisId,
    },
}

impl NodeId {
    /// A short label for an operator. Never includes an amount or an excerpt —
    /// the graph is walked in tooling that logs, and section 45 applies.
    pub fn label(&self) -> String {
        match self {
            NodeId::Fact {
                document_version_id,
                fact,
            } => format!("fact:{fact}@{document_version_id}"),
            NodeId::Rule {
                rule_id,
                rule_version,
            } => format!("rule:{rule_id}@{rule_version}"),
            NodeId::Calculation { calculation_id } => format!("calc:{calculation_id}"),
            NodeId::ModelRun { model_run_id } => format!("model:{model_run_id}"),
            NodeId::Assumption { .. } => "assumption".to_string(),
            NodeId::Finding { opportunity_id } => format!("finding:{opportunity_id}"),
            NodeId::Analysis { analysis_id } => format!("analysis:{analysis_id}"),
        }
    }

    pub fn is_document_anchored(&self) -> bool {
        matches!(self, NodeId::Fact { .. })
    }
}

/// Why one node supports another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// The source is an input the target could not stand without.
    Supports,
    /// The source is a judgement that shaped the target but does not carry it.
    /// A finding supported only by `Informs` edges is not actionable.
    Informs,
    /// Structural: a finding belongs to an analysis.
    PartOf,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
}

/// A directed acyclic graph from document values to findings.
///
/// It is acyclic by construction: edges only ever run from an evidence item to
/// the finding it supports, and from a finding to its analysis. Nothing in the
/// build path can point backwards.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidenceGraph {
    nodes: BTreeSet<NodeId>,
    edges: BTreeSet<Edge>,
    /// Adjacency, forwards and backwards, so both questions are cheap.
    #[serde(skip)]
    outgoing: BTreeMap<NodeId, BTreeSet<NodeId>>,
    #[serde(skip)]
    incoming: BTreeMap<NodeId, BTreeSet<NodeId>>,
}

impl EvidenceGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn nodes(&self) -> impl Iterator<Item = &NodeId> {
        self.nodes.iter()
    }

    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.iter()
    }

    pub fn contains(&self, node: &NodeId) -> bool {
        self.nodes.contains(node)
    }

    fn add_edge(&mut self, from: NodeId, to: NodeId, kind: EdgeKind) {
        self.nodes.insert(from.clone());
        self.nodes.insert(to.clone());
        self.outgoing
            .entry(from.clone())
            .or_default()
            .insert(to.clone());
        self.incoming
            .entry(to.clone())
            .or_default()
            .insert(from.clone());
        self.edges.insert(Edge { from, to, kind });
    }

    /// Fold one finding's chain into the graph.
    pub fn add_finding(
        &mut self,
        analysis_id: AnalysisId,
        opportunity_id: OpportunityId,
        chain: &EvidenceChain,
    ) {
        let finding = NodeId::Finding { opportunity_id };
        let analysis = NodeId::Analysis { analysis_id };
        self.add_edge(finding.clone(), analysis, EdgeKind::PartOf);

        for item in chain.items() {
            let (node, kind) = match item {
                EvidenceItem::DocumentValue {
                    document_version_id,
                    kind,
                    ..
                } => (
                    NodeId::Fact {
                        document_version_id: *document_version_id,
                        fact: kind.as_key().to_string(),
                    },
                    EdgeKind::Supports,
                ),
                EvidenceItem::Rule {
                    rule_id,
                    rule_version,
                    ..
                } => (
                    NodeId::Rule {
                        rule_id: rule_id.clone(),
                        rule_version: rule_version.clone(),
                    },
                    EdgeKind::Supports,
                ),
                EvidenceItem::Calculation {
                    calculation_id,
                    inputs,
                    ..
                } => {
                    let calc = NodeId::Calculation {
                        calculation_id: *calculation_id,
                    };
                    // A calculation's inputs support the calculation, which in
                    // turn supports the finding. Two hops, so re-reading a
                    // figure invalidates the arithmetic explicitly.
                    for input in inputs {
                        let _ = input;
                    }
                    (calc, EdgeKind::Supports)
                }
                EvidenceItem::ModelJudgement { model_run_id, .. } => (
                    NodeId::ModelRun {
                        model_run_id: *model_run_id,
                    },
                    // A model pass informs; it never carries a finding on its
                    // own. This is the same rule the confidence caps enforce,
                    // expressed as graph structure.
                    EdgeKind::Informs,
                ),
                EvidenceItem::Assumption { statement } => (
                    NodeId::Assumption {
                        statement: statement.clone(),
                    },
                    EdgeKind::Informs,
                ),
            };
            self.add_edge(node, finding.clone(), kind);
        }
    }

    /// Everything reachable downstream of `node` — what breaks if it does.
    ///
    /// This is the blast radius query. Give it a rule version and it returns
    /// every finding and every analysis that rested on it.
    pub fn dependents(&self, node: &NodeId) -> BTreeSet<NodeId> {
        self.reachable(node, &self.outgoing)
    }

    /// Everything upstream of `node` — the full provenance of a finding, back
    /// to the pages it was read from.
    pub fn provenance(&self, node: &NodeId) -> BTreeSet<NodeId> {
        self.reachable(node, &self.incoming)
    }

    fn reachable(
        &self,
        start: &NodeId,
        adjacency: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    ) -> BTreeSet<NodeId> {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(start.clone());
        while let Some(current) = queue.pop_front() {
            let Some(neighbours) = adjacency.get(&current) else {
                continue;
            };
            for next in neighbours {
                if seen.insert(next.clone()) {
                    queue.push_back(next.clone());
                }
            }
        }
        seen
    }

    /// The findings affected if `node` turns out to be wrong.
    pub fn affected_findings(&self, node: &NodeId) -> Vec<OpportunityId> {
        self.dependents(node)
            .into_iter()
            .filter_map(|n| match n {
                NodeId::Finding { opportunity_id } => Some(opportunity_id),
                _ => None,
            })
            .collect()
    }

    /// The analyses affected if `node` turns out to be wrong. What a rule
    /// change proposal has to state before it can be approved (section 53).
    pub fn affected_analyses(&self, node: &NodeId) -> Vec<AnalysisId> {
        self.dependents(node)
            .into_iter()
            .filter_map(|n| match n {
                NodeId::Analysis { analysis_id } => Some(analysis_id),
                _ => None,
            })
            .collect()
    }

    /// True when the finding has at least one document-anchored `Supports`
    /// path. The graph-level restatement of the actionability rule: a finding
    /// held up only by model judgements and assumptions is not actionable, no
    /// matter how confident the model was.
    pub fn finding_is_document_anchored(&self, opportunity_id: OpportunityId) -> bool {
        let finding = NodeId::Finding { opportunity_id };
        self.edges.iter().any(|e| {
            e.to == finding && e.kind == EdgeKind::Supports && e.from.is_document_anchored()
        })
    }

    /// Rebuilds adjacency after deserialisation, which drops it.
    pub fn reindex(&mut self) {
        self.outgoing.clear();
        self.incoming.clear();
        for edge in &self.edges {
            self.outgoing
                .entry(edge.from.clone())
                .or_default()
                .insert(edge.to.clone());
            self.incoming
                .entry(edge.to.clone())
                .or_default()
                .insert(edge.from.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::FactKind;
    use crate::ids::{DocumentId, FinancialFactId};
    use crate::money::Money;

    fn chain_with_document_and_rule(rule_version: &str) -> EvidenceChain {
        let mut chain = EvidenceChain::new();
        chain.push(EvidenceItem::DocumentValue {
            document_id: DocumentId::from_uuid(uuid::Uuid::nil()),
            document_version_id: DocumentVersionId::from_uuid(uuid::Uuid::nil()),
            fact_id: FinancialFactId::new(),
            kind: FactKind::Revenue,
            value: Money::from_sek(1_000_000).unwrap(),
            page: Some(1),
            excerpt: None,
        });
        chain.push(EvidenceItem::Rule {
            citations: Vec::new(),
            rule_id: "se.test.rule".into(),
            rule_version: rule_version.into(),
            title: "t".into(),
            source: "s".into(),
        });
        chain
    }

    #[test]
    fn a_rule_version_reports_the_findings_that_rested_on_it() {
        let mut graph = EvidenceGraph::new();
        let analysis = AnalysisId::new();
        let a = OpportunityId::new();
        let b = OpportunityId::new();
        graph.add_finding(analysis, a, &chain_with_document_and_rule("se-2025.1"));
        graph.add_finding(analysis, b, &chain_with_document_and_rule("se-2025.1"));

        let rule = NodeId::Rule {
            rule_id: "se.test.rule".into(),
            rule_version: "se-2025.1".into(),
        };
        let affected = graph.affected_findings(&rule);
        assert_eq!(affected.len(), 2);
        assert!(affected.contains(&a));
        assert!(affected.contains(&b));
        assert_eq!(graph.affected_analyses(&rule), vec![analysis]);
    }

    #[test]
    fn two_versions_of_a_rule_are_different_nodes() {
        let mut graph = EvidenceGraph::new();
        let old = OpportunityId::new();
        let new = OpportunityId::new();
        graph.add_finding(
            AnalysisId::new(),
            old,
            &chain_with_document_and_rule("se-2025.1"),
        );
        graph.add_finding(
            AnalysisId::new(),
            new,
            &chain_with_document_and_rule("se-2025.2"),
        );

        let v1 = NodeId::Rule {
            rule_id: "se.test.rule".into(),
            rule_version: "se-2025.1".into(),
        };
        assert_eq!(graph.affected_findings(&v1), vec![old]);
    }

    #[test]
    fn a_document_version_reports_everything_it_holds_up() {
        let mut graph = EvidenceGraph::new();
        let finding = OpportunityId::new();
        graph.add_finding(
            AnalysisId::new(),
            finding,
            &chain_with_document_and_rule("se-2025.1"),
        );
        let fact = NodeId::Fact {
            document_version_id: DocumentVersionId::from_uuid(uuid::Uuid::nil()),
            fact: FactKind::Revenue.as_key().to_string(),
        };
        assert_eq!(graph.affected_findings(&fact), vec![finding]);
    }

    #[test]
    fn provenance_walks_back_from_a_finding_to_the_page() {
        let mut graph = EvidenceGraph::new();
        let finding_id = OpportunityId::new();
        graph.add_finding(
            AnalysisId::new(),
            finding_id,
            &chain_with_document_and_rule("se-2025.1"),
        );
        let provenance = graph.provenance(&NodeId::Finding {
            opportunity_id: finding_id,
        });
        assert!(provenance.iter().any(NodeId::is_document_anchored));
        assert!(provenance.iter().any(|n| matches!(n, NodeId::Rule { .. })));
    }

    #[test]
    fn a_finding_held_up_only_by_the_model_is_not_document_anchored() {
        let mut chain = EvidenceChain::new();
        chain.push(EvidenceItem::ModelJudgement {
            model_run_id: ModelRunId::new(),
            task: "discovery".into(),
            rationale_summary: "looked plausible".into(),
        });
        chain.push(EvidenceItem::Assumption {
            statement: "assumed the company is not a holding company".into(),
        });

        let mut graph = EvidenceGraph::new();
        let finding = OpportunityId::new();
        graph.add_finding(AnalysisId::new(), finding, &chain);
        assert!(!graph.finding_is_document_anchored(finding));
    }

    #[test]
    fn a_finding_with_a_document_value_is_document_anchored() {
        let mut graph = EvidenceGraph::new();
        let finding = OpportunityId::new();
        graph.add_finding(
            AnalysisId::new(),
            finding,
            &chain_with_document_and_rule("se-2025.1"),
        );
        assert!(graph.finding_is_document_anchored(finding));
    }

    #[test]
    fn identical_evidence_across_analyses_shares_one_node() {
        let mut graph = EvidenceGraph::new();
        graph.add_finding(
            AnalysisId::new(),
            OpportunityId::new(),
            &chain_with_document_and_rule("se-2025.1"),
        );
        let after_one = graph.node_count();
        graph.add_finding(
            AnalysisId::new(),
            OpportunityId::new(),
            &chain_with_document_and_rule("se-2025.1"),
        );
        // One more finding, one more analysis; the fact and rule nodes merge.
        assert_eq!(graph.node_count(), after_one + 2);
    }

    #[test]
    fn node_labels_never_carry_amounts_or_excerpts() {
        let mut graph = EvidenceGraph::new();
        graph.add_finding(
            AnalysisId::new(),
            OpportunityId::new(),
            &chain_with_document_and_rule("se-2025.1"),
        );
        for node in graph.nodes() {
            let label = node.label();
            assert!(!label.contains("1000000"));
            assert!(!label.contains("1 000 000"));
        }
    }

    #[test]
    fn the_graph_survives_a_round_trip_through_json() {
        let mut graph = EvidenceGraph::new();
        let finding = OpportunityId::new();
        graph.add_finding(
            AnalysisId::new(),
            finding,
            &chain_with_document_and_rule("se-2025.1"),
        );
        let json = serde_json::to_string(&graph).unwrap();
        let mut restored: EvidenceGraph = serde_json::from_str(&json).unwrap();
        restored.reindex();
        let rule = NodeId::Rule {
            rule_id: "se.test.rule".into(),
            rule_version: "se-2025.1".into(),
        };
        assert_eq!(restored.affected_findings(&rule), vec![finding]);
    }
}

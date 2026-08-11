//! Data classification (section 9).
//!
//! Every field that leaves the process — into a log line, a metric label, a
//! trace attribute, a model request, an error message — carries a
//! classification. The classification is not a comment: it is a value the
//! emitting code must hold, and the emitters in `skattjakt-telemetry` refuse to
//! render anything above their own ceiling.
//!
//! The point is that "do not log customer financial data" stops being a rule
//! people remember and becomes a rule the type system applies.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Four levels, ordered. Higher means more damage if it escapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// Safe anywhere: rule ids, versions, stage names, HTTP status codes.
    Public,
    /// Internal operations: queue depths, durations, error kinds, host names.
    Internal,
    /// Tied to a tenant but not itself financial: company id, document id,
    /// org number, file name, user id.
    Confidential,
    /// The customer's economy and the people in it: amounts, extracted facts,
    /// document text, model prompts and completions, personal data.
    HighlyConfidential,
}

impl Classification {
    /// The highest level a destination may render. Anything above is redacted.
    ///
    /// Logs stop at `Internal` on purpose. A company id in a log line is still
    /// a tenant identifier in a store that operators, shipping pipelines and
    /// log aggregators can all read; correlation ids do that job without
    /// naming the customer.
    pub const LOG_CEILING: Classification = Classification::Internal;

    /// Metric labels are worse than logs: they are unbounded cardinality *and*
    /// they end up in a time-series database with a different retention policy
    /// and a different access list. Public only (section 45).
    pub const METRIC_LABEL_CEILING: Classification = Classification::Public;

    /// Trace attributes sit with the logs.
    pub const TRACE_CEILING: Classification = Classification::Internal;

    /// What may cross into the model provider. `HighlyConfidential` is allowed
    /// here — sending the financial figures is the entire point of the product
    /// — but it is allowed *explicitly*, at one named boundary, rather than by
    /// nobody having thought about it.
    pub const MODEL_REQUEST_CEILING: Classification = Classification::HighlyConfidential;

    /// What may appear in an error body returned over HTTP to a caller who has
    /// already been authorised for the tenant.
    pub const ERROR_BODY_CEILING: Classification = Classification::Confidential;

    pub fn as_str(self) -> &'static str {
        match self {
            Classification::Public => "PUBLIC",
            Classification::Internal => "INTERNAL",
            Classification::Confidential => "CONFIDENTIAL",
            Classification::HighlyConfidential => "HIGHLY_CONFIDENTIAL",
        }
    }

    /// True when a value at this level may be rendered to a destination whose
    /// ceiling is `ceiling`.
    pub fn permitted_at(self, ceiling: Classification) -> bool {
        self <= ceiling
    }

    /// How long data at this level may be kept by default, in days, absent a
    /// tenant-specific retention policy (section 65). `None` means the level
    /// carries no time limit of its own.
    pub fn default_retention_days(self) -> Option<u32> {
        match self {
            Classification::Public | Classification::Internal => None,
            // Tenant-identifying data lives as long as the tenant does.
            Classification::Confidential => None,
            // Documents and extracted facts expire unless the tenant renews.
            Classification::HighlyConfidential => Some(365 * 2),
        }
    }
}

impl fmt::Display for Classification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A value paired with its classification.
///
/// `Classified<T>` deliberately has no `Display` and no `Deref`. The only way
/// to get at the inner value is to name what you are doing: `expose_for` with a
/// ceiling, or `into_inner` when the value is leaving the classified world on
/// purpose (into the database, into the response body).
#[derive(Clone, PartialEq, Eq)]
pub struct Classified<T> {
    level: Classification,
    value: T,
}

impl<T> Classified<T> {
    pub const fn new(level: Classification, value: T) -> Self {
        Self { level, value }
    }

    pub fn public(value: T) -> Self {
        Self::new(Classification::Public, value)
    }

    pub fn internal(value: T) -> Self {
        Self::new(Classification::Internal, value)
    }

    pub fn confidential(value: T) -> Self {
        Self::new(Classification::Confidential, value)
    }

    pub fn highly_confidential(value: T) -> Self {
        Self::new(Classification::HighlyConfidential, value)
    }

    pub const fn level(&self) -> Classification {
        self.level
    }

    /// The value, if this destination is cleared for it. `None` is the caller's
    /// signal to render a redaction marker instead.
    pub fn expose_for(&self, ceiling: Classification) -> Option<&T> {
        self.level.permitted_at(ceiling).then_some(&self.value)
    }

    /// Leave the classified world on purpose — persisting to the tenant's own
    /// row, or building the model request at the one boundary that allows it.
    pub fn into_inner(self) -> T {
        self.value
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Classified<U> {
        Classified {
            level: self.level,
            value: f(self.value),
        }
    }
}

/// Debug is safe to derive on the wrapper only if it does not print the value.
/// It does not.
impl<T> fmt::Debug for Classified<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Classified<{}>(<redacted>)", self.level)
    }
}

/// What a redacted field renders as. One string, everywhere, so a grep for it
/// finds every place data was withheld.
pub const REDACTED: &str = "[redacted]";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_ordered_by_severity() {
        assert!(Classification::Public < Classification::Internal);
        assert!(Classification::Internal < Classification::Confidential);
        assert!(Classification::Confidential < Classification::HighlyConfidential);
    }

    #[test]
    fn logs_refuse_tenant_identifying_data_and_above() {
        assert!(Classification::Internal.permitted_at(Classification::LOG_CEILING));
        assert!(!Classification::Confidential.permitted_at(Classification::LOG_CEILING));
        assert!(!Classification::HighlyConfidential.permitted_at(Classification::LOG_CEILING));
    }

    #[test]
    fn metric_labels_accept_public_only() {
        assert!(Classification::Public.permitted_at(Classification::METRIC_LABEL_CEILING));
        assert!(!Classification::Internal.permitted_at(Classification::METRIC_LABEL_CEILING));
    }

    #[test]
    fn the_model_boundary_is_the_only_one_cleared_for_financial_data() {
        let ceilings = [
            Classification::LOG_CEILING,
            Classification::METRIC_LABEL_CEILING,
            Classification::TRACE_CEILING,
            Classification::ERROR_BODY_CEILING,
        ];
        for ceiling in ceilings {
            assert!(
                !Classification::HighlyConfidential.permitted_at(ceiling),
                "{ceiling} should not accept HIGHLY_CONFIDENTIAL"
            );
        }
        assert!(
            Classification::HighlyConfidential.permitted_at(Classification::MODEL_REQUEST_CEILING)
        );
    }

    #[test]
    fn a_classified_value_is_withheld_from_a_destination_below_it() {
        let amount = Classified::highly_confidential("1 250 000 kr".to_string());
        assert!(amount.expose_for(Classification::LOG_CEILING).is_none());
        assert_eq!(
            amount.expose_for(Classification::MODEL_REQUEST_CEILING),
            Some(&"1 250 000 kr".to_string())
        );
    }

    #[test]
    fn debug_never_prints_the_value() {
        let secret = Classified::highly_confidential("org number 556677-8899");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("556677"));
        assert!(rendered.contains("HIGHLY_CONFIDENTIAL"));
    }

    #[test]
    fn documents_have_a_default_expiry_and_operational_data_does_not() {
        assert!(Classification::HighlyConfidential
            .default_retention_days()
            .is_some());
        assert!(Classification::Internal.default_retention_days().is_none());
    }
}

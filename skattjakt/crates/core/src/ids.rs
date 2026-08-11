//! Typed identifiers.
//!
//! Every entity gets its own newtype over `Uuid`. A `DocumentId` that can be
//! passed where a `CompanyId` is expected is exactly how cross-tenant leaks are
//! written, so the compiler is made to care.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! typed_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }
    };
}

typed_id!(
    /// A tenant. Every row in the system is reachable only through one of these.
    CompanyId
);
typed_id!(UserId);
typed_id!(DocumentId);
typed_id!(DocumentVersionId);
typed_id!(FinancialFactId);
typed_id!(AnalysisId);
typed_id!(ModelRunId);
typed_id!(OpportunityId);
typed_id!(CalculationId);
typed_id!(AuditEventId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialise_as_bare_uuid_strings() {
        let id = CompanyId::from_uuid(Uuid::nil());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"00000000-0000-0000-0000-000000000000\"");
    }

    #[test]
    fn distinct_ids_are_distinct_types() {
        // Compile-time property; the assertion just keeps the test meaningful.
        let a = CompanyId::new();
        let b = CompanyId::new();
        assert_ne!(a, b);
    }
}

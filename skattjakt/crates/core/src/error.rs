use thiserror::Error;

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("monetary amount overflowed the representable range")]
    MoneyOverflow,

    #[error("confidence factor `{factor}` must be within 0.0..=1.0, got {value}")]
    ConfidenceOutOfRange { factor: &'static str, value: String },

    #[error("invalid organisationsnummer: {reason}")]
    InvalidOrgNumber { reason: String },

    #[error("invalid fiscal year: {reason}")]
    InvalidFiscalYear { reason: String },

    #[error("an opportunity must carry at least one piece of evidence")]
    EvidenceRequired,
}

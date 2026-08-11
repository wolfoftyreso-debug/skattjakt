//! # skattjakt-gateway
//!
//! The model gateway of section 15: the single boundary between Skattjakt and
//! any model provider.
//!
//! Nothing else in the workspace holds a `ModelProvider`. Routing that through
//! one type is what makes cost accounting, budget ceilings, fallback
//! visibility, injection defence and the capability rules of section 52 hold
//! everywhere rather than in most places.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod cost;
pub mod gateway;
pub mod injection;

pub use cost::{Budget, BudgetError, ModelPrice, PriceError, PriceList, MICRO_ORE_PER_SEK};
pub use gateway::{
    GatewayConfig, GatewayError, GatewayOutcome, ModelGateway, MODEL_REQUEST_CLASSIFICATION,
};
pub use injection::{scan, scan_wrapped, wrap_document, InjectionScan, DATA_FRAMING};

//! Reading a set of accounts that arrived as pixels.
//!
//! This crate is deliberately *not* a dependency of `skattjakt-extract`.
//! Extraction crosses to wasm32 and runs in the browser; an OCR engine and
//! its models are eleven megabytes and cannot go there. So the split is:
//! extract reports that a page yielded no text, and this crate — server-side
//! only — reads the pixels when a reader is available.
//!
//! [`layout`] holds the part worth testing hardest, and holds no engine
//! dependency: rebuilding statement rows out of recognised words.

pub mod layout;

pub use layout::{rows_from_words, Amount, Row, Sign, Word};

//! Identity: who someone is, what they may do, and how a signed-in device
//! keeps proving it.
//!
//! This crate holds the decisions and none of the I/O. What a token looks like,
//! how long a session lives, when a refresh is a theft signal, and which role
//! may do what are all answerable — and testable — without a database.
//! `skattjakt-store` does the persisting.
//!
//! ## The four concepts, kept apart (section 13)
//!
//! ```text
//!   Authentication   proving you are the holder of a credential
//!         ↓
//!   Identity         which person that credential belongs to
//!         ↓
//!   Verification     whether that person's identity has been confirmed
//!         ↓
//!   Authorization    what that person may do in this company
//! ```
//!
//! They are separate types here because collapsing them is how a system ends up
//! deciding that anyone who can log in must be allowed to delete the accounts.
//! In particular `Verification` is deliberately its own axis: a person can be
//! authenticated and authorised while their identity remains unverified, and a
//! product that files tax positions needs to be able to say so.

pub mod authorization;
pub mod credential;
pub mod session;
pub mod token;

pub use authorization::{Permission, Role, VerificationLevel};
pub use credential::{CredentialError, CredentialMethod, PasswordPolicy, PasswordVerifier};
pub use session::{RefreshOutcome, SessionPolicy, SessionState};
pub use token::{ClientKind, SecretToken, TokenHash};

#[cfg(test)]
mod identity_tests;

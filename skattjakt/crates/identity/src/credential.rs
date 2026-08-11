//! Verifying that someone holds the credential they claim.
//!
//! This is the seam. `Skattjakt` does not want to be in the business of storing
//! passwords: the identity method this market actually wants is Swedish
//! BankID, and an organisation running this on its own platform will already
//! have an identity provider. Both are [`CredentialMethod::Federated`], which
//! this crate models and does not implement, because implementing it against a
//! provider that is not reachable would be a guess.
//!
//! What is implemented is [`CredentialMethod::Password`], because it is
//! self-contained, testable, and enough to run the product. Everything above it
//! — sessions, devices, rotation, roles — is independent of which method was
//! used, so replacing this with BankID or OIDC changes one verifier and no
//! client.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialMethod {
    Password,
    /// An external provider asserted the subject. No secret is stored here.
    Federated,
}

impl CredentialMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialMethod::Password => "password",
            CredentialMethod::Federated => "federated",
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CredentialError {
    /// Deliberately one variant for "no such user" and "wrong password".
    ///
    /// Distinguishing them tells an attacker which email addresses are
    /// customers, which for a product whose customers are identifiable
    /// businesses is a disclosure worth avoiding. The caller must also take the
    /// same time in both cases — see [`PasswordVerifier::verify`].
    #[error("the credentials are not valid")]
    Invalid,
    #[error("the account is temporarily locked")]
    Locked { until: DateTime<Utc> },
    #[error("the password does not meet the policy: {0}")]
    PolicyViolation(&'static str),
    #[error("the stored credential is unusable")]
    StoredCredentialCorrupt,
}

/// What a password has to satisfy.
///
/// Length and a denylist, not a character-class rule. Forcing a symbol and a
/// digit produces `Password1!` — which satisfies every classic rule and is in
/// every cracking dictionary — while blocking the passphrase that is actually
/// strong. Length is the property that correlates with strength.
#[derive(Debug, Clone, Copy)]
pub struct PasswordPolicy {
    pub min_length: usize,
    pub max_length: usize,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            min_length: 12,
            // Bounded because Argon2 hashes whatever it is given, and an
            // unbounded password is an unbounded amount of work per login
            // attempt — a denial of service with a text field for an interface.
            max_length: 256,
        }
    }
}

impl PasswordPolicy {
    pub fn check(&self, password: &str) -> Result<(), CredentialError> {
        // Counted in characters, not bytes: "räkenskapsår2025" is 16 characters
        // and 18 bytes, and a customer told their 12-character password is too
        // short has been told something false.
        let length = password.chars().count();
        if length < self.min_length {
            return Err(CredentialError::PolicyViolation(
                "a password must be at least 12 characters",
            ));
        }
        if length > self.max_length {
            return Err(CredentialError::PolicyViolation(
                "a password may be at most 256 characters",
            ));
        }
        if COMMON_PASSWORDS
            .iter()
            .any(|common| password.eq_ignore_ascii_case(common))
        {
            return Err(CredentialError::PolicyViolation(
                "this password is among the most commonly used and is not accepted",
            ));
        }
        Ok(())
    }
}

/// A very small denylist of passwords that are guessed first.
///
/// Not a substitute for a breach corpus, and does not pretend to be — a real
/// deployment should check against one. It is here because the top few are
/// tried before anything else, and blocking them costs nothing.
const COMMON_PASSWORDS: &[&str] = &[
    "password123!",
    "passwordpassword",
    "123456789012",
    "qwertyuiopas",
    "administrator",
    "skattjakt123",
    "sommar2025!!",
];

/// How many failed attempts before an account is locked, and for how long.
///
/// Locking the account rather than the address: an attacker spraying one
/// password across many accounts is not slowed by per-address limits, and the
/// per-address limiter already exists at the ingress for the other shape of
/// attack.
pub const MAX_FAILED_ATTEMPTS: i32 = 8;

pub fn lockout_duration(failed_attempts: i32) -> Duration {
    // Doubling, capped. The cap matters: an uncapped backoff means an attacker
    // who fails often enough locks a customer out permanently, turning a
    // defence into a denial of service against the person it protects.
    let excess = (failed_attempts - MAX_FAILED_ATTEMPTS).clamp(0, 6);
    Duration::minutes(5 * 2i64.pow(excess as u32))
}

/// Argon2id password hashing.
pub struct PasswordVerifier {
    argon: Argon2<'static>,
}

impl Default for PasswordVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl PasswordVerifier {
    /// Argon2id at OWASP's second recommended configuration: 19 MiB, 2 passes,
    /// 1 degree of parallelism.
    ///
    /// Argon2**id** rather than Argon2i or Argon2d because it is the hybrid,
    /// and the one to use absent a specific reason for the others. The memory
    /// cost is what does the work — it is what a GPU cannot parallelise
    /// cheaply — and 19 MiB per concurrent login is affordable at this scale
    /// while being expensive at an attacker's.
    pub fn new() -> Self {
        let params = Params::new(19 * 1024, 2, 1, None).expect("the Argon2 parameters are valid");
        Self {
            argon: Argon2::new(Algorithm::Argon2id, Version::V0x13, params),
        }
    }

    pub fn hash(&self, password: &str) -> Result<String, CredentialError> {
        let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
        self.argon
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|_| CredentialError::StoredCredentialCorrupt)
    }

    /// Verifies a password against a stored hash.
    ///
    /// Returns `Invalid` for a corrupt stored hash as well as for a wrong
    /// password, so a caller cannot use the error to distinguish them.
    pub fn verify(&self, password: &str, stored: &str) -> Result<(), CredentialError> {
        let parsed = PasswordHash::new(stored).map_err(|_| CredentialError::Invalid)?;
        self.argon
            .verify_password(password.as_bytes(), &parsed)
            .map_err(|_| CredentialError::Invalid)
    }

    /// Spends the same work as a real verification, for the case where no user
    /// exists.
    ///
    /// Without this, "no such account" returns in microseconds and "wrong
    /// password" in tens of milliseconds, and the difference enumerates which
    /// businesses are customers. The hash below is of a password nobody holds;
    /// verifying against it costs exactly what a real attempt costs.
    pub fn spend_equivalent_work(&self) {
        let _ = self.verify("a password nobody holds", DECOY_HASH);
    }
}

/// Argon2id hash of a random string, used only to burn equivalent time.
const DECOY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$\
                          c2thdHRqYWt0ZGVjb3lzYWx0$\
                          8YQ0mgFjqzQFvJqTZ8vJZ4mQmXvBz9Y1mZ8XcJ0kL3s";

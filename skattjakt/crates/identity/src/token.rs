//! Opaque tokens, and the hash that is all the database ever sees.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How many bytes of operating-system entropy a token carries.
///
/// 32 bytes — 256 bits. Not a round number chosen for looks: a token is the
/// entire credential, there is no second factor behind it on a device, and it
/// sits in storage for weeks. The cost of the extra bytes is nothing.
const TOKEN_BYTES: usize = 32;

/// A token in the clear.
///
/// Exists for exactly as long as it takes to return it to the client. It does
/// not implement `Display`, `Debug` prints a placeholder, and `Serialize` is
/// deliberately absent — a token reaches a response body by an explicit call to
/// [`SecretToken::expose`], which is greppable, rather than by a struct field
/// happening to be serialised into a log line.
pub struct SecretToken(String);

impl SecretToken {
    /// Mints a token from the operating system's CSPRNG.
    ///
    /// `getrandom` rather than a userspace generator: a userspace PRNG can be
    /// seeded identically in two processes, and two API replicas minting the
    /// same session token is not a failure mode worth having.
    pub fn generate() -> Self {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).expect("the operating system CSPRNG must be available");
        Self(hex(&bytes))
    }

    /// Reads the secret. Every call site is a place a token escapes, so this is
    /// named to be conspicuous in review.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The hash to store. The clear token is never persisted anywhere.
    pub fn hash(&self) -> TokenHash {
        TokenHash::of(&self.0)
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken([redacted])")
    }
}

/// The SHA-256 of a token, lowercase hex.
///
/// SHA-256 without a work factor, unlike a password. That is a deliberate
/// difference and worth stating: a password is low-entropy and human-chosen, so
/// it needs Argon2 to make guessing expensive. A token is 256 bits of CSPRNG
/// output, so there is nothing to guess and a slow hash would only add latency
/// to every authenticated request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenHash(String);

impl TokenHash {
    pub fn of(presented: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(presented.as_bytes());
        Self(hex(&hasher.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Which kind of client a session belongs to.
///
/// Not cosmetic. It selects the session lifetimes in
/// [`crate::SessionPolicy`], it decides which push provider a device's token
/// belongs to, and it lets a customer's device list say something they
/// recognise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Web,
    Ios,
    Android,
}

impl ClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientKind::Web => "web",
            ClientKind::Ios => "ios",
            ClientKind::Android => "android",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "web" => Some(ClientKind::Web),
            "ios" => Some(ClientKind::Ios),
            "android" => Some(ClientKind::Android),
            _ => None,
        }
    }

    /// Whether the platform can hold a credential in hardware-backed storage.
    ///
    /// iOS has the Keychain and Android has the Keystore; a browser has
    /// `localStorage`, which any script on the page can read. That difference
    /// is why a web session is short and a mobile session is long — see
    /// [`crate::SessionPolicy`].
    pub fn has_secure_storage(self) -> bool {
        matches!(self, ClientKind::Ios | ClientKind::Android)
    }
}

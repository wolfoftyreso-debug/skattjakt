//! Documents and their immutable versions.
//!
//! An uploaded file is never mutated (section 15). Re-uploading produces a new
//! `DocumentVersion`, so an analysis can always name the exact bytes it read.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{CompanyId, DocumentId, DocumentVersionId};

/// What kind of material was uploaded. The beta ingests PDF; the rest of the
/// taxonomy exists so the pipeline can be routed by type from day one rather
/// than retrofitted (section 2 of the document pipeline).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// Årsredovisning or bokslut, preliminary or final.
    AnnualAccounts,
    IncomeStatement,
    BalanceSheet,
    GeneralLedger,
    JournalList,
    TaxAccountStatement,
    TaxReturn,
    FixedAssetRegister,
    PayrollSummary,
    InvoiceBundle,
    /// Recognised as financial material but not classified further.
    Unknown,
}

/// Whether the accounts are final or still moving. A preliminary year-end is
/// the primary case Skattjakt is built for, and it changes what may be
/// concluded — so it is modelled, not inferred at read time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountsState {
    Preliminary,
    Final,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MimeType {
    Pdf,
    Csv,
    Xlsx,
    Sie,
    PlainText,
}

impl MimeType {
    /// Maps a declared content type to a supported kind. Unsupported types are
    /// rejected at the edge rather than discovered mid-pipeline.
    pub fn from_content_type(value: &str) -> Option<Self> {
        let normalised = value.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        match normalised.as_str() {
            "application/pdf" => Some(MimeType::Pdf),
            "text/csv" | "application/csv" => Some(MimeType::Csv),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Some(MimeType::Xlsx),
            "application/sie" | "text/sie" => Some(MimeType::Sie),
            "text/plain" => Some(MimeType::PlainText),
            _ => None,
        }
    }

    pub fn as_content_type(self) -> &'static str {
        match self {
            MimeType::Pdf => "application/pdf",
            MimeType::Csv => "text/csv",
            MimeType::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            MimeType::Sie => "application/sie",
            MimeType::PlainText => "text/plain",
        }
    }

    /// The leading bytes a file of this type must start with, when the format
    /// defines one. A declared content type is a claim; this is a check.
    pub fn magic_prefix(self) -> Option<&'static [u8]> {
        match self {
            MimeType::Pdf => Some(b"%PDF-"),
            // ZIP container.
            MimeType::Xlsx => Some(b"PK\x03\x04"),
            MimeType::Csv | MimeType::Sie | MimeType::PlainText => None,
        }
    }

    /// Whether the bytes are consistent with the declared type.
    pub fn matches_content(self, bytes: &[u8]) -> bool {
        match self.magic_prefix() {
            Some(prefix) => bytes.starts_with(prefix),
            None => true,
        }
    }
}

/// A logical document belonging to one company.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: DocumentId,
    pub company_id: CompanyId,
    pub kind: DocumentKind,
    pub original_filename: String,
    pub created_at: DateTime<Utc>,
}

/// Immutable bytes, addressed by hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentVersion {
    pub id: DocumentVersionId,
    pub document_id: DocumentId,
    pub company_id: CompanyId,
    /// 1-based, increasing per document.
    pub version: i32,
    pub mime_type: MimeType,
    pub byte_size: i64,
    /// Lowercase hex SHA-256 of the stored bytes.
    pub sha256: String,
    /// Key in object storage. Never a path the client controls.
    pub storage_key: String,
    pub page_count: Option<i32>,
    pub accounts_state: AccountsState,
    pub uploaded_at: DateTime<Utc>,
}

impl DocumentVersion {
    /// The storage key layout. Tenant-prefixed so an object listing cannot
    /// return another company's material even by accident.
    pub fn build_storage_key(company_id: CompanyId, document_id: DocumentId, version: i32, sha256: &str) -> String {
        format!("companies/{company_id}/documents/{document_id}/v{version}-{sha256}")
    }

    pub fn verify_hash(&self, bytes: &[u8]) -> bool {
        crate::document::sha256_hex(bytes) == self.sha256
    }
}

/// Lowercase hex SHA-256. Implemented here rather than pulled from the store
/// layer so the domain can verify integrity without an I/O dependency.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = sha2_digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// The core crate stays dependency-light; SHA-256 is small enough to carry.
fn sha2_digest(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Longer than one 64-byte block, to exercise the chunk loop.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn content_type_mapping_ignores_parameters_and_case() {
        assert_eq!(MimeType::from_content_type("application/PDF; charset=binary"), Some(MimeType::Pdf));
        assert_eq!(MimeType::from_content_type("text/csv"), Some(MimeType::Csv));
        assert_eq!(MimeType::from_content_type("application/x-msdownload"), None);
    }

    #[test]
    fn declared_pdf_must_actually_start_like_a_pdf() {
        assert!(MimeType::Pdf.matches_content(b"%PDF-1.7\n..."));
        assert!(!MimeType::Pdf.matches_content(b"MZ\x90\x00"));
        // Text formats have no signature to check.
        assert!(MimeType::Csv.matches_content(b"anything"));
    }

    #[test]
    fn storage_keys_are_tenant_prefixed() {
        let company = CompanyId::from_uuid(uuid::Uuid::nil());
        let doc = DocumentId::from_uuid(uuid::Uuid::nil());
        let key = DocumentVersion::build_storage_key(company, doc, 2, "deadbeef");
        assert!(key.starts_with("companies/00000000-0000-0000-0000-000000000000/"));
        assert!(key.ends_with("v2-deadbeef"));
    }

    #[test]
    fn hash_verification_detects_altered_bytes() {
        let version = DocumentVersion {
            id: DocumentVersionId::new(),
            document_id: DocumentId::new(),
            company_id: CompanyId::new(),
            version: 1,
            mime_type: MimeType::Pdf,
            byte_size: 3,
            sha256: sha256_hex(b"abc"),
            storage_key: "k".into(),
            page_count: Some(1),
            accounts_state: AccountsState::Preliminary,
            uploaded_at: Utc::now(),
        };
        assert!(version.verify_hash(b"abc"));
        assert!(!version.verify_hash(b"abd"));
    }
}

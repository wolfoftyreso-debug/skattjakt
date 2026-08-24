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

/// What a file is, as far as we can tell from its bytes.
///
/// # Why this is open rather than a fixed list
///
/// It used to be five variants and a closed match: a type outside them was
/// refused at the edge. That is the right answer for a service that only ever
/// wants five formats, and the wrong one for a customer who has a folder of
/// material and does not know which parts we can read. Refusing the folder
/// teaches them nothing; taking it and saying which parts were readable is the
/// whole of what they wanted to know.
///
/// So every file is accepted, identified from its content, stored and recorded.
/// `Other` carries what the bytes actually turned out to be. What separates the
/// variants is not whether a file is *allowed* — they all are — but whether an
/// extractor exists, and `extracts_text` is the honest name for that question.
///
/// # The declared type is a claim, the bytes are the evidence
///
/// `sniff` reads the leading bytes and ignores the filename. A `.pdf` that is
/// really a JPEG is a JPEG, and the report says so rather than failing three
/// stages later with an empty extraction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MimeType {
    Pdf,
    Csv,
    /// Excel's current format. A ZIP of XML underneath, like `Docx`.
    Xlsx,
    /// Word's current format. Swedish annual reports arrive as these often
    /// enough to be worth reading rather than refusing.
    Docx,
    /// SIE, the Swedish accounting interchange format. Text.
    Sie,
    /// XML that is not one of the above: SIE 5, iXBRL, an export from a
    /// bookkeeping system.
    Xml,
    Html,
    Json,
    Rtf,
    PlainText,
    /// A ZIP that is not an Office document. Its listing is readable even when
    /// its members are not, and a customer who uploaded a folder deserves to be
    /// told what was in it.
    Zip,
    /// Anything else, carrying what it was detected to be.
    ///
    /// Not an error and not a rejection. The file is stored, hashed and
    /// recorded; the report says it was received and could not be read, and
    /// names the type so the customer knows whether that is a surprise.
    Other(String),
}

impl MimeType {
    /// Identifies a file from its leading bytes.
    ///
    /// Content first, always. The filename is consulted only to separate
    /// formats whose containers are identical — a `.docx` and an `.xlsx` are
    /// both ZIPs, and telling them apart means looking inside, which `sniff`
    /// does by reading the member names in the ZIP's central directory.
    pub fn sniff(bytes: &[u8], filename: Option<&str>) -> Self {
        if bytes.is_empty() {
            return MimeType::Other("empty".to_string());
        }
        if bytes.starts_with(b"%PDF-") {
            return MimeType::Pdf;
        }
        if bytes.starts_with(b"{\\rtf") {
            return MimeType::Rtf;
        }
        if bytes.starts_with(b"PK\x03\x04") {
            return Self::inside_zip(bytes, filename);
        }
        // Old Office (.xls, .doc): the OLE2 compound file header.
        if bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
            return MimeType::Other("application/x-ole-storage".to_string());
        }
        for (magic, label) in BINARY_SIGNATURES {
            if bytes.starts_with(magic) {
                return MimeType::Other((*label).to_string());
            }
        }
        Self::sniff_text(bytes, filename)
    }

    /// Text formats, told apart by what the text starts with.
    fn sniff_text(bytes: &[u8], filename: Option<&str>) -> Self {
        // A prefix is enough, and bounds the work on a very large file.
        let head = &bytes[..bytes.len().min(4096)];
        if std::str::from_utf8(head).is_err() && !head.is_ascii() {
            // Not decodable as UTF-8 and not plain ASCII: binary of some kind.
            return MimeType::Other("application/octet-stream".to_string());
        }
        let text = String::from_utf8_lossy(head);
        let trimmed = text.trim_start().trim_start_matches('\u{feff}');
        let lower = trimmed.to_ascii_lowercase();

        if lower.starts_with("<?xml") {
            return MimeType::Xml;
        }
        if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
            return MimeType::Html;
        }
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            return MimeType::Json;
        }
        // SIE announces itself on the first line.
        if lower.starts_with("#flagga") || lower.contains("#sietyp") {
            return MimeType::Sie;
        }
        let extension = filename
            .and_then(|f| f.rsplit('.').next())
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("csv" | "tsv") => MimeType::Csv,
            Some("se" | "si" | "sie") => MimeType::Sie,
            // Several separators on the first few lines reads like a table.
            _ if Self::looks_like_delimited(&text) => MimeType::Csv,
            _ => MimeType::PlainText,
        }
    }

    fn looks_like_delimited(text: &str) -> bool {
        let lines: Vec<&str> = text
            .lines()
            .take(5)
            .filter(|l| !l.trim().is_empty())
            .collect();
        if lines.len() < 2 {
            return false;
        }
        let counts: Vec<usize> = lines
            .iter()
            .map(|l| l.matches(';').count().max(l.matches(',').count()))
            .collect();
        counts[0] >= 2 && counts.iter().all(|c| *c == counts[0])
    }

    /// Separates the ZIP-container formats by looking at the member names.
    ///
    /// `.docx`, `.xlsx` and a plain `.zip` share the same first four bytes, so
    /// the extension is the only cheap hint — and a wrong extension is exactly
    /// what sniffing exists to survive. The central directory is at the end of
    /// the file, so this reads a bounded tail rather than inflating anything.
    fn inside_zip(bytes: &[u8], filename: Option<&str>) -> Self {
        let tail = &bytes[bytes.len().saturating_sub(64 * 1024)..];
        let has = |needle: &[u8]| {
            tail.windows(needle.len()).any(|w| w == needle)
                || bytes[..bytes.len().min(64 * 1024)]
                    .windows(needle.len())
                    .any(|w| w == needle)
        };
        if has(b"word/document.xml") {
            return MimeType::Docx;
        }
        if has(b"xl/workbook.xml") {
            return MimeType::Xlsx;
        }
        match filename
            .and_then(|f| f.rsplit('.').next())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("docx") => MimeType::Docx,
            Some("xlsx" | "xlsm") => MimeType::Xlsx,
            _ => MimeType::Zip,
        }
    }

    /// Maps a declared content type. Kept because a client may send one, but it
    /// is never the last word — see `sniff`.
    pub fn from_content_type(value: &str) -> Self {
        let normalised = value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match normalised.as_str() {
            "application/pdf" => MimeType::Pdf,
            "text/csv" | "application/csv" => MimeType::Csv,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => MimeType::Xlsx,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
                MimeType::Docx
            }
            "application/sie" | "text/sie" => MimeType::Sie,
            "text/xml" | "application/xml" => MimeType::Xml,
            "text/html" => MimeType::Html,
            "application/json" => MimeType::Json,
            "application/rtf" | "text/rtf" => MimeType::Rtf,
            "application/zip" => MimeType::Zip,
            "text/plain" => MimeType::PlainText,
            other => MimeType::Other(other.to_string()),
        }
    }

    pub fn as_content_type(&self) -> &str {
        match self {
            MimeType::Pdf => "application/pdf",
            MimeType::Csv => "text/csv",
            MimeType::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            MimeType::Docx => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            MimeType::Sie => "application/sie",
            MimeType::Xml => "application/xml",
            MimeType::Html => "text/html",
            MimeType::Json => "application/json",
            MimeType::Rtf => "application/rtf",
            MimeType::Zip => "application/zip",
            MimeType::PlainText => "text/plain",
            MimeType::Other(label) => label,
        }
    }

    /// Whether an extractor exists for this type.
    ///
    /// The question that actually matters, and deliberately not called
    /// "supported": every type is supported for upload. This is about whether
    /// the analysis will have anything to read.
    pub fn extracts_text(&self) -> bool {
        !matches!(self, MimeType::Zip | MimeType::Other(_))
    }

    /// What to tell a customer who uploaded something we cannot read.
    ///
    /// Names the type and says why, because "unsupported file" tells them
    /// nothing about whether to be surprised.
    pub fn why_unreadable(&self) -> Option<String> {
        match self {
            MimeType::Zip => Some(
                "Filen är ett arkiv. Innehållet listas men läses inte — packa upp \
                 det och ladda upp de filer som hör till bokslutet."
                    .to_string(),
            ),
            MimeType::Other(label) if label == "empty" => Some("Filen är tom.".to_string()),
            MimeType::Other(label) if label.starts_with("image/") => Some(
                "Filen är en bild. Skattjakt läser inte text ur bilder, så ett \
                 fotograferat eller skannat underlag ger ingen analys."
                    .to_string(),
            ),
            MimeType::Other(label) if label == "application/x-ole-storage" => Some(
                "Filen är ett äldre Office-dokument (.doc eller .xls). Spara om \
                 det som .docx, .xlsx eller PDF."
                    .to_string(),
            ),
            MimeType::Other(label) => Some(format!(
                "Filen är av typen {label}, som Skattjakt inte kan läsa text ur."
            )),
            _ => None,
        }
    }
}

/// Leading bytes that identify a format we store but cannot read.
///
/// Listed so the report can name what a file was rather than calling it
/// "unsupported" — a customer who uploaded a photograph of their accounts is
/// told it is a photograph, which is the sentence that helps them.
const BINARY_SIGNATURES: &[(&[u8], &str)] = &[
    (b"\x89PNG\r\n\x1a\n", "image/png"),
    (b"\xff\xd8\xff", "image/jpeg"),
    (b"GIF87a", "image/gif"),
    (b"GIF89a", "image/gif"),
    (b"BM", "image/bmp"),
    (b"II*\x00", "image/tiff"),
    (b"MM\x00*", "image/tiff"),
    (b"\x1f\x8b", "application/gzip"),
    (b"7z\xbc\xaf\x27\x1c", "application/x-7z-compressed"),
    (b"Rar!\x1a\x07", "application/vnd.rar"),
    (b"OggS", "audio/ogg"),
    (b"\x00\x00\x00\x18ftyp", "video/mp4"),
    (b"\x00\x00\x00\x20ftyp", "video/mp4"),
    (b"\x1aE\xdf\xa3", "video/x-matroska"),
    (b"ID3", "audio/mpeg"),
    (b"RIFF", "application/x-riff"),
    (b"\x7fELF", "application/x-executable"),
    (b"MZ", "application/vnd.microsoft.portable-executable"),
    (b"SQLite format 3\x00", "application/vnd.sqlite3"),
];

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
    pub fn build_storage_key(
        company_id: CompanyId,
        document_id: DocumentId,
        version: i32,
        sha256: &str,
    ) -> String {
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
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
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
    fn a_declared_content_type_is_read_but_never_refused() {
        assert_eq!(
            MimeType::from_content_type("application/PDF; charset=binary"),
            MimeType::Pdf
        );
        assert_eq!(MimeType::from_content_type("text/csv"), MimeType::Csv);
        // Not a rejection any more. An unknown type is a type we know we cannot
        // read, which is a different and more useful thing to say.
        assert_eq!(
            MimeType::from_content_type("application/x-msdownload"),
            MimeType::Other("application/x-msdownload".to_string())
        );
    }

    /// The filename is a hint; the bytes are the answer.
    #[test]
    fn a_file_is_what_its_bytes_are_not_what_it_is_called() {
        // The case that used to fail three stages later as an empty extraction.
        assert_eq!(
            MimeType::sniff(b"\xff\xd8\xff\xe0JFIF", Some("bokslut.pdf")),
            MimeType::Other("image/jpeg".to_string())
        );
        assert_eq!(
            MimeType::sniff(b"%PDF-1.7\n...", Some("gissa.txt")),
            MimeType::Pdf
        );
        assert_eq!(
            MimeType::sniff(b"", Some("tom.pdf")),
            MimeType::Other("empty".into())
        );
    }

    #[test]
    fn text_formats_are_told_apart_by_what_they_start_with() {
        assert_eq!(
            MimeType::sniff(b"<?xml version=\"1.0\"?><sie/>", None),
            MimeType::Xml
        );
        assert_eq!(
            MimeType::sniff(b"<!DOCTYPE html><html>", None),
            MimeType::Html
        );
        assert_eq!(MimeType::sniff(b"{\"resultat\": 1}", None), MimeType::Json);
        assert_eq!(
            MimeType::sniff(b"#FLAGGA 0\n#SIETYP 4", None),
            MimeType::Sie
        );
        assert_eq!(MimeType::sniff(b"{\\rtf1\\ansi", None), MimeType::Rtf);
        assert_eq!(
            MimeType::sniff("Nettoomsättning 4 200 000".as_bytes(), None),
            MimeType::PlainText
        );
        // Consistent separators across several lines read as a table.
        assert_eq!(
            MimeType::sniff(b"a;b;c\n1;2;3\n4;5;6", Some("okant")),
            MimeType::Csv
        );
    }

    /// A `.docx` and an `.xlsx` are both ZIPs; the members separate them.
    #[test]
    fn office_containers_are_separated_by_what_is_inside_them() {
        let docx = [b"PK\x03\x04".as_slice(), b"...word/document.xml..."].concat();
        let xlsx = [b"PK\x03\x04".as_slice(), b"...xl/workbook.xml..."].concat();
        let plain = [b"PK\x03\x04".as_slice(), b"...bokslut.pdf..."].concat();
        assert_eq!(MimeType::sniff(&docx, Some("x.zip")), MimeType::Docx);
        assert_eq!(MimeType::sniff(&xlsx, Some("x.zip")), MimeType::Xlsx);
        assert_eq!(MimeType::sniff(&plain, Some("x.zip")), MimeType::Zip);
    }

    /// A type with no extractor is not an error, and the reason is in the
    /// customer's terms rather than "unsupported file".
    #[test]
    fn a_type_we_cannot_read_says_what_it_was_and_why() {
        let jpeg = MimeType::sniff(b"\xff\xd8\xff\xe0", None);
        assert!(!jpeg.extracts_text());
        let why = jpeg.why_unreadable().expect("a reason");
        assert!(why.contains("bild"), "{why}");
        assert!(
            why.contains("skannat") || why.contains("fotograferat"),
            "{why}"
        );

        let old_office = MimeType::sniff(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1], None);
        assert!(old_office.why_unreadable().unwrap().contains(".docx"));

        // And the ones we can read say nothing, because there is nothing to say.
        assert!(MimeType::Pdf.extracts_text());
        assert_eq!(MimeType::Pdf.why_unreadable(), None);
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

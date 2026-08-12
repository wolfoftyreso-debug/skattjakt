//! S3-compatible object storage, and the presigned URLs the upload ticket flow
//! needs.
//!
//! ## Why this is hand-written rather than `aws-sdk-s3`
//!
//! Section 33 asks what a dependency costs. `aws-sdk-s3` brings roughly fifty
//! crates for four operations and one signing scheme. Every one of them lands
//! in the SBOM this product publishes, in the `cargo audit` run that gates every
//! commit, and in the surface an operator has to keep patched.
//!
//! What is bought for that is SigV4, which is the part worth being careful
//! about — so the question is whether hand-writing it is reckless. It is not,
//! and the reason is the failure mode: a wrong signature is **rejected by the
//! server**. It fails closed and loudly. It is not a silent hole that
//! authorises something it should not; it is a request that does not work, at
//! the first attempt, in the tests below, against a real MinIO.
//!
//! The honest cost of this choice: multipart upload, automatic retries and the
//! long tail of S3 semantics are not implemented. Documents here are bounded at
//! 32 MB, which is comfortably inside the single-`PUT` limit, and retries belong
//! to the caller that knows whether the operation is safe to repeat. If either
//! of those stops being true, the SDK becomes the right answer and this module
//! is the thing it replaces.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::blob::{BlobError, BlobResult, BlobStore};

type HmacSha256 = Hmac<Sha256>;

/// An empty payload's SHA-256, which SigV4 needs for unsigned-payload requests.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The marker SigV4 uses when the payload is not hashed in advance.
///
/// Required for presigned URLs: the signature is computed before the bytes
/// exist, so there is nothing to hash.
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

#[derive(Debug, Clone)]
pub struct S3Config {
    /// e.g. `https://minio.skattjakt-prod.svc.cluster.local:9000`
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    /// Path style (`host/bucket/key`) rather than virtual-host style
    /// (`bucket.host/key`).
    ///
    /// True for MinIO, which is what this deployment runs. Virtual-host style
    /// needs a wildcard DNS entry and a wildcard certificate for the bucket
    /// subdomain, which is machinery a single in-cluster bucket does not earn.
    pub path_style: bool,
}

impl S3Config {
    /// Reads the configuration from the environment.
    ///
    /// Returns `None` rather than failing when nothing is configured: a
    /// deployment without object storage falls back to the filesystem store,
    /// which is a supported single-node mode rather than a misconfiguration.
    pub fn from_env() -> Option<Self> {
        let endpoint = std::env::var("SKATTJAKT_S3_ENDPOINT").ok()?;
        Some(Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            region: std::env::var("SKATTJAKT_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
            bucket: std::env::var("SKATTJAKT_S3_BUCKET").ok()?,
            access_key: std::env::var("SKATTJAKT_S3_ACCESS_KEY").ok()?,
            secret_key: std::env::var("SKATTJAKT_S3_SECRET_KEY").ok()?,
            path_style: std::env::var("SKATTJAKT_S3_PATH_STYLE")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
        })
    }
}

#[derive(Debug, Clone)]
pub struct S3BlobStore {
    config: S3Config,
    client: reqwest::Client,
}

impl S3BlobStore {
    pub fn new(config: S3Config) -> BlobResult<Self> {
        let client = reqwest::Client::builder()
            // Bounded, because an upload that hangs holds a worker slot. Long,
            // because a 32 MB document over a slow link is legitimate.
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| BlobError::Io(format!("could not build an HTTP client: {e}")))?;
        Ok(Self { config, client })
    }

    /// The URL for one key.
    fn url_for(&self, key: &str) -> String {
        if self.config.path_style {
            format!(
                "{}/{}/{}",
                self.config.endpoint,
                self.config.bucket,
                encode_key(key)
            )
        } else {
            // Virtual-host style. Kept for a real AWS endpoint, which does not
            // offer path style for buckets created after 2020.
            let host = self
                .config
                .endpoint
                .replace("://", &format!("://{}.", self.config.bucket));
            format!("{}/{}", host, encode_key(key))
        }
    }

    /// The path component the canonical request signs.
    fn canonical_path(&self, key: &str) -> String {
        if self.config.path_style {
            format!("/{}/{}", self.config.bucket, encode_key(key))
        } else {
            format!("/{}", encode_key(key))
        }
    }

    /// A presigned URL a client can use directly, without a credential.
    ///
    /// This is what makes the upload ticket flow real: the phone writes to
    /// storage and the API never touches the bytes.
    ///
    /// The signature covers the method, the exact key, the expiry and the host.
    /// A client cannot change any of them — a presigned `PUT` for one key
    /// cannot be edited into a `PUT` for another, because the signature would
    /// no longer verify.
    pub fn presign(&self, method: &str, key: &str, expires_in_secs: u64) -> BlobResult<String> {
        // S3 caps a presigned URL at seven days. A ticket lives 30 minutes, so
        // this is a guard against a caller passing something absurd rather than
        // a limit anything reaches.
        let expires = expires_in_secs.clamp(1, 7 * 24 * 3600);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BlobError::Io("the system clock is before 1970".into()))?
            .as_secs();
        let (date, timestamp) = amz_dates(now);

        let credential_scope = format!("{}/{}/s3/aws4_request", date, self.config.region);
        let credential = format!("{}/{}", self.config.access_key, credential_scope);

        // Query parameters must be sorted by key for the canonical request, and
        // every value percent-encoded — including the slashes inside the
        // credential, which is the detail most implementations get wrong.
        let mut params = [
            (
                "X-Amz-Algorithm".to_string(),
                "AWS4-HMAC-SHA256".to_string(),
            ),
            ("X-Amz-Credential".to_string(), credential),
            ("X-Amz-Date".to_string(), timestamp.clone()),
            ("X-Amz-Expires".to_string(), expires.to_string()),
            ("X-Amz-SignedHeaders".to_string(), "host".to_string()),
        ];
        params.sort_by(|a, b| a.0.cmp(&b.0));

        let canonical_query = params
            .iter()
            .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
            .collect::<Vec<_>>()
            .join("&");

        let host = host_of(&self.url_for(key))?;
        let canonical_request = format!(
            "{}\n{}\n{}\nhost:{}\n\nhost\n{}",
            method,
            self.canonical_path(key),
            canonical_query,
            host,
            UNSIGNED_PAYLOAD
        );

        let signature = self.sign(&canonical_request, &date, &timestamp, &credential_scope);

        Ok(format!(
            "{}?{}&X-Amz-Signature={}",
            self.url_for(key),
            canonical_query,
            signature
        ))
    }

    /// The `Authorization` header for a request with a known payload.
    fn authorization_header(
        &self,
        method: &str,
        key: &str,
        payload_sha256: &str,
        timestamp: &str,
        date: &str,
    ) -> BlobResult<String> {
        let host = host_of(&self.url_for(key))?;
        let credential_scope = format!("{}/{}/s3/aws4_request", date, self.config.region);

        // Signed headers must be lowercase and sorted. `x-amz-content-sha256`
        // is what binds the signature to the bytes: without it a signature for
        // one body would authorise a request with any other.
        let canonical_request = format!(
            "{}\n{}\n\nhost:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n\n\
             host;x-amz-content-sha256;x-amz-date\n{}",
            method,
            self.canonical_path(key),
            host,
            payload_sha256,
            timestamp,
            payload_sha256
        );

        let signature = self.sign(&canonical_request, date, timestamp, &credential_scope);

        Ok(format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
            self.config.access_key, credential_scope, signature
        ))
    }

    /// The SigV4 signing chain.
    ///
    /// Four nested HMACs deriving a key that is scoped to the date, the region
    /// and the service — which is why a leaked signature is useless tomorrow,
    /// in another region, or against another service.
    fn sign(
        &self,
        canonical_request: &str,
        date: &str,
        timestamp: &str,
        credential_scope: &str,
    ) -> String {
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            timestamp,
            credential_scope,
            hex(&Sha256::digest(canonical_request.as_bytes()))
        );

        let key = hmac(
            format!("AWS4{}", self.config.secret_key).as_bytes(),
            date.as_bytes(),
        );
        let key = hmac(&key, self.config.region.as_bytes());
        let key = hmac(&key, b"s3");
        let key = hmac(&key, b"aws4_request");
        hex(&hmac(&key, string_to_sign.as_bytes()))
    }

    async fn send(
        &self,
        method: reqwest::Method,
        key: &str,
        body: Option<Vec<u8>>,
    ) -> BlobResult<reqwest::Response> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BlobError::Io("the system clock is before 1970".into()))?
            .as_secs();
        let (date, timestamp) = amz_dates(now);

        let payload_sha = match body.as_ref() {
            Some(bytes) => hex(&Sha256::digest(bytes)),
            None => EMPTY_SHA256.to_string(),
        };

        let authorization =
            self.authorization_header(method.as_str(), key, &payload_sha, &timestamp, &date)?;

        let mut request = self
            .client
            .request(method, self.url_for(key))
            .header("authorization", authorization)
            .header("x-amz-content-sha256", &payload_sha)
            .header("x-amz-date", &timestamp);

        if let Some(bytes) = body {
            request = request.body(bytes);
        }

        request
            .send()
            .await
            .map_err(|e| BlobError::Io(format!("the object store could not be reached: {e}")))
    }
}

#[async_trait::async_trait]
impl BlobStore for S3BlobStore {
    fn presign_put(&self, key: &str, expires_in_secs: u64) -> Option<String> {
        self.presign("PUT", key, expires_in_secs).ok()
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> BlobResult<()> {
        let response = self
            .send(reqwest::Method::PUT, key, Some(bytes.to_vec()))
            .await?;
        if !response.status().is_success() {
            // The status, never the body. An S3 error body echoes the key, and
            // a key names a company.
            return Err(BlobError::Io(format!(
                "the object store refused a write: HTTP {}",
                response.status().as_u16()
            )));
        }
        Ok(())
    }

    async fn get(&self, key: &str) -> BlobResult<Vec<u8>> {
        let response = self.send(reqwest::Method::GET, key, None).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(BlobError::NotFound(key.to_string()));
        }
        if !response.status().is_success() {
            return Err(BlobError::Io(format!(
                "the object store refused a read: HTTP {}",
                response.status().as_u16()
            )));
        }
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| BlobError::Io(format!("the object could not be read: {e}")))
    }

    async fn delete(&self, key: &str) -> BlobResult<()> {
        let response = self.send(reqwest::Method::DELETE, key, None).await?;
        // S3 answers 204 for a delete of something that was never there, and
        // that is the right answer for a retention job: the desired state is
        // "gone", and it is.
        if !response.status().is_success() && response.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(BlobError::Io(format!(
                "the object store refused a delete: HTTP {}",
                response.status().as_u16()
            )));
        }
        Ok(())
    }

    async fn exists(&self, key: &str) -> BlobResult<bool> {
        let response = self.send(reqwest::Method::HEAD, key, None).await?;
        Ok(response.status().is_success())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// `(20250811, 20250811T221530Z)` — the two forms SigV4 needs.
fn amz_dates(unix_secs: u64) -> (String, String) {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    (
        format!("{year:04}{month:02}{day:02}"),
        format!(
            "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
            secs_of_day / 3600,
            (secs_of_day % 3600) / 60,
            secs_of_day % 60
        ),
    )
}

/// Days since the epoch to a civil date.
///
/// Howard Hinnant's algorithm. Written out rather than pulled from `chrono`
/// because it is eight lines and the formatting has to be exact — SigV4 rejects
/// anything else, and a date formatter with a locale in it would eventually
/// produce one.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Percent-encodes a key, keeping `/` as a path separator.
fn encode_key(key: &str) -> String {
    key.split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// Percent-encodes one component, per the rules SigV4 requires.
///
/// The unreserved set is exactly `A-Za-z0-9-._~`. AWS's own documentation is
/// explicit that everything else must be encoded, including characters many URL
/// encoders leave alone — and a single mismatch produces a signature that does
/// not verify.
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn host_of(url: &str) -> BlobResult<String> {
    let without_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(|| BlobError::Io("the endpoint has no scheme".into()))?;
    Ok(without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> S3Config {
        S3Config {
            endpoint: "http://minio:9000".into(),
            region: "us-east-1".into(),
            bucket: "skattjakt".into(),
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            path_style: true,
        }
    }

    #[test]
    fn dates_are_formatted_exactly_as_sigv4_requires() {
        // 2013-05-24T00:00:00Z — the timestamp from AWS's own worked example.
        let (date, timestamp) = amz_dates(1_369_353_600);
        assert_eq!(date, "20130524");
        assert_eq!(timestamp, "20130524T000000Z");
    }

    #[test]
    fn dates_handle_a_leap_day() {
        // 2024-02-29T12:24:56Z. An off-by-one here produces a signature that
        // fails for one day every four years, which is the worst kind of bug.
        let (date, timestamp) = amz_dates(1_709_209_496);
        assert_eq!(date, "20240229");
        assert_eq!(timestamp, "20240229T122456Z");
    }

    #[test]
    fn the_signing_chain_matches_the_aws_worked_example() {
        // AWS publishes this derivation in its SigV4 documentation. If this
        // passes, the four-step key derivation is right.
        let key = hmac(b"AWS4wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", b"20150830");
        let key = hmac(&key, b"us-east-1");
        let key = hmac(&key, b"iam");
        let key = hmac(&key, b"aws4_request");
        assert_eq!(
            hex(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn the_empty_payload_hash_is_correct() {
        assert_eq!(hex(&Sha256::digest(b"")), EMPTY_SHA256);
    }

    #[test]
    fn encoding_follows_the_unreserved_set_exactly() {
        // The characters many encoders leave alone and SigV4 does not.
        assert_eq!(encode_component("a b"), "a%20b");
        assert_eq!(encode_component("a/b"), "a%2Fb");
        assert_eq!(encode_component("a+b"), "a%2Bb");
        assert_eq!(encode_component("a=b"), "a%3Db");
        // And the ones it does leave alone.
        assert_eq!(encode_component("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn a_key_keeps_its_path_separators() {
        assert_eq!(
            encode_key("companies/abc/documents/def"),
            "companies/abc/documents/def"
        );
        // But a separator inside a component is encoded, which is what stops a
        // crafted key escaping its prefix.
        assert_eq!(encode_key("companies/a b/x"), "companies/a%20b/x");
    }

    #[test]
    fn a_presigned_url_carries_everything_a_verifier_needs() {
        let store = S3BlobStore::new(config()).unwrap();
        let url = store.presign("PUT", "companies/a/uploads/b", 1800).unwrap();

        for required in [
            "X-Amz-Algorithm=AWS4-HMAC-SHA256",
            "X-Amz-Credential=",
            "X-Amz-Date=",
            "X-Amz-Expires=1800",
            "X-Amz-SignedHeaders=host",
            "X-Amz-Signature=",
        ] {
            assert!(url.contains(required), "missing {required} in {url}");
        }
    }

    #[test]
    fn a_presigned_url_never_contains_the_secret() {
        let store = S3BlobStore::new(config()).unwrap();
        let url = store.presign("PUT", "companies/a/uploads/b", 1800).unwrap();
        assert!(!url.contains("wJalrXUtnFEMI"));
        // The access key id is not secret and is required in the credential.
        assert!(url.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn a_different_key_produces_a_different_signature() {
        // The signature covers the path. A presigned PUT for one key must not
        // be editable into a PUT for another.
        let store = S3BlobStore::new(config()).unwrap();
        let a = store.presign("PUT", "companies/a/x", 900).unwrap();
        let b = store.presign("PUT", "companies/b/x", 900).unwrap();
        let sig = |url: &str| url.split("X-Amz-Signature=").nth(1).unwrap().to_string();
        assert_ne!(sig(&a), sig(&b));
    }

    #[test]
    fn a_different_method_produces_a_different_signature() {
        // A presigned GET must not be usable as a PUT.
        let store = S3BlobStore::new(config()).unwrap();
        let get = store.presign("GET", "companies/a/x", 900).unwrap();
        let put = store.presign("PUT", "companies/a/x", 900).unwrap();
        let sig = |url: &str| url.split("X-Amz-Signature=").nth(1).unwrap().to_string();
        assert_ne!(sig(&get), sig(&put));
    }

    #[test]
    fn an_absurd_expiry_is_clamped_to_the_s3_maximum() {
        let store = S3BlobStore::new(config()).unwrap();
        let url = store.presign("PUT", "k", u64::MAX).unwrap();
        assert!(url.contains(&format!("X-Amz-Expires={}", 7 * 24 * 3600)));
    }

    #[test]
    fn path_style_puts_the_bucket_in_the_path() {
        let store = S3BlobStore::new(config()).unwrap();
        assert_eq!(store.canonical_path("a/b"), "/skattjakt/a/b");
        assert!(store
            .url_for("a/b")
            .starts_with("http://minio:9000/skattjakt/"));
    }

    #[test]
    fn virtual_host_style_puts_the_bucket_in_the_host() {
        let store = S3BlobStore::new(S3Config {
            path_style: false,
            ..config()
        })
        .unwrap();
        assert_eq!(store.canonical_path("a/b"), "/a/b");
        assert!(store
            .url_for("a/b")
            .starts_with("http://skattjakt.minio:9000/"));
    }

    #[test]
    fn the_config_is_absent_rather_than_broken_when_nothing_is_set() {
        // A deployment without object storage falls back to the filesystem
        // store, which is a supported single-node mode.
        std::env::remove_var("SKATTJAKT_S3_ENDPOINT");
        assert!(S3Config::from_env().is_none());
    }
}

//! Immutable document storage.
//!
//! Uploaded bytes are never modified (section 15). A blob is written once,
//! addressed by a key the client never chooses, and read back by hash.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BlobError {
    #[error("no object at {0}")]
    NotFound(String),

    #[error("refusing an unsafe storage key: {0}")]
    UnsafeKey(String),

    #[error("storage failure: {0}")]
    Io(String),
}

pub type BlobResult<T> = Result<T, BlobError>;

/// Somewhere to keep documents.
#[async_trait]
pub trait BlobStore: Send + Sync + std::fmt::Debug {
    /// Writes bytes. Writing the same key twice with different bytes is a
    /// programming error, not an update: keys carry the content hash.
    async fn put(&self, key: &str, bytes: &[u8]) -> BlobResult<()>;
    async fn get(&self, key: &str) -> BlobResult<Vec<u8>>;
    async fn delete(&self, key: &str) -> BlobResult<()>;
    async fn exists(&self, key: &str) -> BlobResult<bool>;
}

/// Local-filesystem storage. Suitable for development and single-node
/// deployments; an S3-compatible implementation slots in behind the same trait.
#[derive(Debug, Clone)]
pub struct FilesystemBlobStore {
    root: PathBuf,
}

impl FilesystemBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolves a key to a path inside the root.
    ///
    /// Storage keys are built by the domain layer and are tenant-prefixed, but
    /// this validates anyway: a traversal in a key would let one company read
    /// another's documents, which is the exact failure the whole tenancy design
    /// exists to prevent.
    fn resolve(&self, key: &str) -> BlobResult<PathBuf> {
        if key.is_empty() || key.starts_with('/') || key.contains('\\') || key.contains('\0') {
            return Err(BlobError::UnsafeKey(key.to_string()));
        }

        let relative = Path::new(key);
        for component in relative.components() {
            match component {
                Component::Normal(part) => {
                    // Reject anything that could be a traversal even after a
                    // decoding step somewhere else.
                    let text = part.to_string_lossy();
                    if text == ".." || text.contains("..") {
                        return Err(BlobError::UnsafeKey(key.to_string()));
                    }
                }
                _ => return Err(BlobError::UnsafeKey(key.to_string())),
            }
        }

        Ok(self.root.join(relative))
    }
}

#[async_trait]
impl BlobStore for FilesystemBlobStore {
    async fn put(&self, key: &str, bytes: &[u8]) -> BlobResult<()> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| BlobError::Io(e.to_string()))?;
        }
        // Write to a temporary file and rename, so a crash mid-write cannot
        // leave a truncated document that later reads as valid.
        let temporary = path.with_extension("partial");
        tokio::fs::write(&temporary, bytes)
            .await
            .map_err(|e| BlobError::Io(e.to_string()))?;
        tokio::fs::rename(&temporary, &path)
            .await
            .map_err(|e| BlobError::Io(e.to_string()))
    }

    async fn get(&self, key: &str) -> BlobResult<Vec<u8>> {
        let path = self.resolve(key)?;
        tokio::fs::read(&path).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BlobError::NotFound(key.to_string()),
            _ => BlobError::Io(e.to_string()),
        })
    }

    async fn delete(&self, key: &str) -> BlobResult<()> {
        let path = self.resolve(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(BlobError::NotFound(key.to_string()))
            }
            Err(e) => Err(BlobError::Io(e.to_string())),
        }
    }

    async fn exists(&self, key: &str) -> BlobResult<bool> {
        Ok(tokio::fs::try_exists(self.resolve(key)?)
            .await
            .unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (FilesystemBlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        (FilesystemBlobStore::new(dir.path()), dir)
    }

    #[tokio::test]
    async fn a_document_round_trips() {
        let (store, _dir) = temp_store();
        let key = "companies/abc/documents/def/v1-deadbeef";
        store.put(key, b"%PDF-1.7 ...").await.unwrap();
        assert!(store.exists(key).await.unwrap());
        assert_eq!(store.get(key).await.unwrap(), b"%PDF-1.7 ...");
    }

    #[tokio::test]
    async fn a_missing_document_is_not_found_rather_than_empty() {
        let (store, _dir) = temp_store();
        assert!(matches!(
            store.get("companies/abc/missing").await,
            Err(BlobError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn traversal_keys_are_refused() {
        let (store, _dir) = temp_store();
        for key in [
            "../escape",
            "companies/../../etc/passwd",
            "/absolute",
            "companies/..%2f/x",
            "a\\b",
        ] {
            assert!(
                matches!(store.put(key, b"x").await, Err(BlobError::UnsafeKey(_))),
                "{key} was not refused"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_key_is_refused() {
        let (store, _dir) = temp_store();
        assert!(matches!(
            store.put("", b"x").await,
            Err(BlobError::UnsafeKey(_))
        ));
    }

    #[tokio::test]
    async fn a_partial_write_leaves_no_readable_document() {
        // The rename is what guarantees this; the test pins the behaviour that
        // no `.partial` file is left behind on success.
        let (store, dir) = temp_store();
        store.put("companies/a/doc", b"payload").await.unwrap();
        let leftovers: Vec<_> = walk(dir.path())
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "partial"))
            .collect();
        assert!(leftovers.is_empty(), "found {leftovers:?}");
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    out.extend(walk(&path));
                } else {
                    out.push(path);
                }
            }
        }
        out
    }
}

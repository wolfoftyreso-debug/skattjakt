//! The S3 blob store against a real object store.
//!
//! Skipped unless `SKATTJAKT_S3_ENDPOINT` is set, so `cargo test` stays
//! self-contained. `tests/integration/s3-blobstore.sh` starts a MinIO and sets
//! it.
//!
//! These are the assertions a unit test cannot make. A signature is only
//! correct if a server that verifies it says so.

use skattjakt_store::blob::BlobStore;
use skattjakt_store::s3::{S3BlobStore, S3Config};

fn store() -> Option<S3BlobStore> {
    let config = S3Config::from_env()?;
    S3BlobStore::new(config).ok()
}

fn key(suffix: &str) -> String {
    format!("companies/test/live/{}-{suffix}", std::process::id())
}

#[tokio::test]
async fn a_document_round_trips() {
    let Some(store) = store() else { return };
    let k = key("roundtrip");
    let bytes = "Nettoomsättning 12 500 000\n".as_bytes();

    store.put(&k, bytes).await.expect("the write was refused");
    let read = store.get(&k).await.expect("the read was refused");
    assert_eq!(read, bytes, "the bytes came back different");

    store.delete(&k).await.expect("the delete was refused");
}

#[tokio::test]
async fn a_missing_object_is_not_found_rather_than_an_error() {
    let Some(store) = store() else { return };
    // The distinction matters: a retention job deleting something already gone
    // has succeeded, and a read of a document that should exist has not.
    let result = store.get(&key("never-written")).await;
    assert!(
        matches!(result, Err(skattjakt_store::blob::BlobError::NotFound(_))),
        "expected NotFound, got {result:?}"
    );
}

#[tokio::test]
async fn exists_answers_both_ways() {
    let Some(store) = store() else { return };
    let k = key("exists");
    assert!(!store.exists(&k).await.expect("the HEAD was refused"));
    store.put(&k, b"x").await.unwrap();
    assert!(store.exists(&k).await.expect("the HEAD was refused"));
    store.delete(&k).await.unwrap();
}

#[tokio::test]
async fn deleting_something_that_is_already_gone_succeeds() {
    let Some(store) = store() else { return };
    // The desired state is "gone", and it is. A retention job must not fail
    // because it is running for the second time.
    store
        .delete(&key("already-gone"))
        .await
        .expect("a delete of a missing object failed");
}

#[tokio::test]
async fn a_key_with_awkward_characters_round_trips() {
    let Some(store) = store() else { return };
    // Percent-encoding is where a hand-written SigV4 goes wrong, and it goes
    // wrong exactly here: a space, a plus, an equals.
    let k = format!("companies/test/live/{}-a b+c=d.pdf", std::process::id());
    store.put(&k, b"awkward").await.expect("the write failed");
    assert_eq!(store.get(&k).await.unwrap(), b"awkward");
    store.delete(&k).await.unwrap();
}

#[tokio::test]
async fn a_large_document_round_trips() {
    let Some(store) = store() else { return };
    // Comfortably inside the single-PUT limit, and large enough that a
    // streaming or chunked-encoding mistake would show.
    let k = key("large");
    let bytes = vec![0xABu8; 5 * 1024 * 1024];
    store.put(&k, &bytes).await.expect("the large write failed");
    assert_eq!(store.get(&k).await.unwrap().len(), bytes.len());
    store.delete(&k).await.unwrap();
}

#[tokio::test]
async fn a_wrong_secret_is_refused_by_the_server() {
    // The property that makes hand-writing SigV4 defensible: a bad signature
    // fails closed, at the server, rather than authorising anything.
    let Some(mut config) = S3Config::from_env() else {
        return;
    };
    config.secret_key = "not the right secret at all".into();
    let store = S3BlobStore::new(config).unwrap();

    let result = store.put(&key("should-not-exist"), b"x").await;
    assert!(result.is_err(), "a wrong secret was accepted");
}

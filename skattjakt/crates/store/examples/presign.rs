//! Prints a presigned PUT and GET URL, for the integration script to exercise
//! with curl and no credential.

fn main() {
    let Some(config) = skattjakt_store::s3::S3Config::from_env() else {
        eprintln!("SKATTJAKT_S3_ENDPOINT is not set");
        return;
    };
    let store = skattjakt_store::s3::S3BlobStore::new(config).expect("the client builds");
    let key = "companies/alfa/uploads/presigned-object";
    println!("{}", store.presign("PUT", key, 900).expect("presign PUT"));
    println!("{}", store.presign("GET", key, 900).expect("presign GET"));
}

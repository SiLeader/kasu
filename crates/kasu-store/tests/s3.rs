#![cfg(feature = "s3")]
use kasu_core::*;
use kasu_store::*;
use object_store::{ObjectStoreExt, aws::AmazonS3Builder, path::Path};
use std::{
    io::Write,
    sync::{Arc, Barrier},
};
fn options(prefix: &str) -> S3Options {
    let mut options =
        S3Options::new(std::env::var("KASU_TEST_S3_BUCKET").expect("KASU_TEST_S3_BUCKET"));
    options.endpoint = Some(std::env::var("KASU_TEST_S3_ENDPOINT").expect("KASU_TEST_S3_ENDPOINT"));
    options.region = Some("us-east-1".into());
    options.allow_plaintext = true;
    options.path_style = true;
    options.prefix = prefix.into();
    options
}
fn prefix(label: &str) -> String {
    format!(
        "tests/{label}/{}/{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
fn variant(raw: &[u8]) -> Vec<u8> {
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 9).unwrap();
    encoder.include_checksum(true).unwrap();
    encoder.write_all(raw).unwrap();
    encoder.finish().unwrap()
}
#[test]
#[ignore = "requires KASU_TEST_S3_BUCKET/ENDPOINT and AWS credentials"]
fn s3_preserves_first_frame_across_reopen_and_conditional_put() {
    let prefix = prefix("frames");
    let store = S3BlobStore::connect(options(&prefix)).unwrap();
    let raw = b"same raw object\0\xff";
    let id = ObjectId::hash(raw);
    let first = compress(raw).unwrap();
    let second = variant(raw);
    assert_ne!(first, second);
    assert!(matches!(store.get(id), Err(Error::NotFound(_))));
    store.put_compressed(id, &first).unwrap();
    store.put_compressed(id, &second).unwrap();
    assert_eq!(store.get_compressed(id).unwrap(), first);
    assert_eq!(store.get(id).unwrap(), raw);
    drop(store);
    let reopened = S3BlobStore::connect(options(&prefix)).unwrap();
    assert_eq!(reopened.get_compressed(id).unwrap(), first);
    let missing = S3BlobStore::connect(options(&format!("{prefix}/other-scope"))).unwrap();
    assert!(matches!(missing.get(id), Err(Error::NotFound(_))));
    assert!(
        reopened
            .put_compressed(ObjectId::hash(b"wrong"), &first)
            .is_err()
    );
}
#[test]
#[ignore = "requires KASU_TEST_S3_BUCKET/ENDPOINT and AWS credentials"]
fn s3_parallel_writers_converge_without_replacing_an_existing_frame() {
    let prefix = prefix("race");
    let raw = b"concurrent shared object";
    let id = ObjectId::hash(raw);
    let frames = [compress(raw).unwrap(), variant(raw)];
    let barrier = Arc::new(Barrier::new(16));
    let workers: Vec<_> = (0..16)
        .map(|n| {
            let prefix = prefix.clone();
            let barrier = barrier.clone();
            let frame = frames[n % 2].clone();
            std::thread::spawn(move || {
                let store = S3BlobStore::connect(options(&prefix)).unwrap();
                barrier.wait();
                store.put_compressed(id, &frame).unwrap();
                store.get_compressed(id).unwrap()
            })
        })
        .collect();
    let observed: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert!(observed.iter().all(|f| f == &observed[0]));
    assert!(frames.contains(&observed[0]));
    let store = S3BlobStore::connect(options(&prefix)).unwrap();
    for frame in frames {
        store.put_compressed(id, &frame).unwrap();
    }
    assert_eq!(store.get_compressed(id).unwrap(), observed[0]);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires KASU_TEST_S3_BUCKET/ENDPOINT and AWS credentials"]
async fn s3_corruption_fails_closed_and_cannot_be_replaced_by_put_if_absent() {
    let prefix = prefix("corrupt");
    let store = S3BlobStore::connect(options(&prefix)).unwrap();
    let raw = b"correct";
    let id = ObjectId::hash(raw);
    let remote = AmazonS3Builder::from_env()
        .with_bucket_name(std::env::var("KASU_TEST_S3_BUCKET").unwrap())
        .with_config(
            object_store::aws::AmazonS3ConfigKey::S3Endpoint,
            std::env::var("KASU_TEST_S3_ENDPOINT").unwrap(),
        )
        .with_allow_http(true)
        .with_region("us-east-1")
        .build()
        .unwrap();
    let hex = id.to_string();
    let path = Path::from(format!("{prefix}/{}/{}", &hex[..2], &hex[2..]));
    let corrupt = compress(b"corrupt").unwrap();
    remote.put(&path, corrupt.clone().into()).await.unwrap();
    assert!(store.get_compressed(id).is_err());
    assert!(store.put(raw).is_err());
    assert_eq!(
        remote
            .get(&path)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        corrupt
    );
    drop(store);
}
#[test]
fn s3_configuration_requires_canonical_prefix_and_explicit_plaintext() {
    let mut options = S3Options::new("test".into());
    options.endpoint = Some("http://127.0.0.1:1".into());
    assert!(S3BlobStore::connect(options).is_err());
    for bad in ["", "/absolute", "a/../b", "a//b", "trailing/"] {
        let mut options = S3Options::new("test".into());
        options.prefix = bad.into();
        assert!(S3BlobStore::connect(options).is_err(), "{bad}");
    }
}

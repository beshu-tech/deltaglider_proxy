//! S3 backend parity tests
//!
//! Runs the same core operations as s3_api_test but against TestServer::s3()
//! to verify the S3 storage backend works identically to filesystem.
//! All tests gated with skip_unless_minio!() — skips gracefully without MinIO.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use common::{generate_binary, mutate_binary, TestServer};
use std::sync::atomic::{AtomicU64, Ordering};

/// Counter for unique test prefixes
static PREFIX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique prefix to isolate each test's data in the shared MinIO bucket
fn unique_prefix() -> String {
    let counter = PREFIX_COUNTER.fetch_add(1, Ordering::SeqCst);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("test-{}-{}", timestamp, counter)
}

#[tokio::test]
async fn test_s3_put_get_roundtrip() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    let data = b"Hello via S3 backend!";
    let key = format!("{}/hello.txt", prefix);

    client
        .put_object()
        .bucket(server.bucket())
        .key(&key)
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();
    let body = client
        .get_object()
        .bucket(server.bucket())
        .key(&key)
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), data);
}

#[tokio::test]
async fn test_s3_put_get_delete_lifecycle() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    let key = format!("{}/lifecycle.txt", prefix);
    client
        .put_object()
        .bucket(server.bucket())
        .key(&key)
        .body(ByteStream::from(b"data".to_vec()))
        .send()
        .await
        .unwrap();
    client
        .delete_object()
        .bucket(server.bucket())
        .key(&key)
        .send()
        .await
        .unwrap();
    let get = client
        .get_object()
        .bucket(server.bucket())
        .key(&key)
        .send()
        .await;
    assert!(get.is_err(), "GET after DELETE should fail");
}

#[tokio::test]
async fn test_s3_put_overwrite() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    let key = format!("{}/overwrite.txt", prefix);
    client
        .put_object()
        .bucket(server.bucket())
        .key(&key)
        .body(ByteStream::from(b"v1".to_vec()))
        .send()
        .await
        .unwrap();
    client
        .put_object()
        .bucket(server.bucket())
        .key(&key)
        .body(ByteStream::from(b"v2".to_vec()))
        .send()
        .await
        .unwrap();

    let body = client
        .get_object()
        .bucket(server.bucket())
        .key(&key)
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), b"v2");
}

#[tokio::test]
async fn test_s3_list_objects_with_prefix() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    for i in 0..3 {
        client
            .put_object()
            .bucket(server.bucket())
            .key(format!("{}/file{}.txt", prefix, i))
            .body(ByteStream::from(format!("{}", i).into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix(format!("{}/", prefix))
        .send()
        .await
        .unwrap();

    assert_eq!(list.contents().len(), 3);
}

#[tokio::test]
async fn test_s3_list_objects_pagination() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    for i in 0..5 {
        client
            .put_object()
            .bucket(server.bucket())
            .key(format!("{}/p{}.txt", prefix, i))
            .body(ByteStream::from(format!("{}", i).into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let page1 = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix(format!("{}/", prefix))
        .max_keys(2)
        .send()
        .await
        .unwrap();

    assert_eq!(page1.contents().len(), 2);
    assert!(page1.is_truncated().unwrap_or(false));
}

#[tokio::test]
async fn test_s3_copy_object() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    let src = format!("{}/src.txt", prefix);
    let dst = format!("{}/dst.txt", prefix);

    client
        .put_object()
        .bucket(server.bucket())
        .key(&src)
        .body(ByteStream::from(b"copy me".to_vec()))
        .send()
        .await
        .unwrap();
    client
        .copy_object()
        .bucket(server.bucket())
        .key(&dst)
        .copy_source(format!("{}/{}", server.bucket(), src))
        .send()
        .await
        .unwrap();

    let body = client
        .get_object()
        .bucket(server.bucket())
        .key(&dst)
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), b"copy me");
}

#[tokio::test]
async fn test_s3_delete_objects_batch() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    for i in 0..3 {
        client
            .put_object()
            .bucket(server.bucket())
            .key(format!("{}/b{}.txt", prefix, i))
            .body(ByteStream::from(b"x".to_vec()))
            .send()
            .await
            .unwrap();
    }

    let ids: Vec<ObjectIdentifier> = (0..3)
        .map(|i| {
            ObjectIdentifier::builder()
                .key(format!("{}/b{}.txt", prefix, i))
                .build()
                .unwrap()
        })
        .collect();

    client
        .delete_objects()
        .bucket(server.bucket())
        .delete(Delete::builder().set_objects(Some(ids)).build().unwrap())
        .send()
        .await
        .unwrap();

    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix(format!("{}/", prefix))
        .send()
        .await
        .unwrap();
    assert_eq!(list.key_count(), Some(0));
}

#[tokio::test]
async fn test_s3_head_object() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    let key = format!("{}/head.txt", prefix);
    let data = b"head test";

    client
        .put_object()
        .bucket(server.bucket())
        .key(&key)
        .content_type("text/plain")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();

    let head = client
        .head_object()
        .bucket(server.bucket())
        .key(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(head.content_length(), Some(data.len() as i64));
    assert!(head.e_tag().is_some());
}

#[tokio::test]
async fn test_s3_etag_consistent() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    let data = b"etag consistency";

    client
        .put_object()
        .bucket(server.bucket())
        .key(format!("{}/e1.txt", prefix))
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();
    client
        .put_object()
        .bucket(server.bucket())
        .key(format!("{}/e2.txt", prefix))
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();

    let e1 = client
        .head_object()
        .bucket(server.bucket())
        .key(format!("{}/e1.txt", prefix))
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string();
    let e2 = client
        .head_object()
        .bucket(server.bucket())
        .key(format!("{}/e2.txt", prefix))
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string();

    assert_eq!(e1, e2, "Same data should produce same ETag on S3 backend");
}

#[tokio::test]
async fn test_s3_unicode_key() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    let key = format!("{}/données.txt", prefix);
    let data = b"unicode key data";

    client
        .put_object()
        .bucket(server.bucket())
        .key(&key)
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();
    let body = client
        .get_object()
        .bucket(server.bucket())
        .key(&key)
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), data);
}

#[tokio::test]
async fn test_s3_delta_similar_files() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let base = generate_binary(100_000, 42);
    let variant = mutate_binary(&base, 0.01);

    // PUT base
    let url1 = format!(
        "{}/{}/{}/base.zip",
        server.endpoint(),
        server.bucket(),
        prefix
    );
    let resp1 = http
        .put(&url1)
        .header("content-type", "application/zip")
        .body(base.clone())
        .send()
        .await
        .unwrap();
    assert!(resp1.status().is_success());

    // PUT variant
    let url2 = format!(
        "{}/{}/{}/v1.zip",
        server.endpoint(),
        server.bucket(),
        prefix
    );
    let resp2 = http
        .put(&url2)
        .header("content-type", "application/zip")
        .body(variant.clone())
        .send()
        .await
        .unwrap();
    assert!(resp2.status().is_success());

    // Verify both retrievable
    let got_base = http.get(&url1).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(got_base.as_ref(), base.as_slice());

    let got_v1 = http.get(&url2).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(got_v1.as_ref(), variant.as_slice());
}

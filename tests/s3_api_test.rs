//! S3 API compliance tests using filesystem backend
//!
//! These tests verify S3 protocol compliance through the AWS SDK.
//! All use TestServer::filesystem() — no Docker needed.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use common::{generate_binary, TestServer};

// ============================================================================
// CRUD lifecycle
// ============================================================================

#[tokio::test]
async fn test_put_get_roundtrip() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let data = b"Hello, DeltaGlider Proxy!";

    client
        .put_object()
        .bucket(server.bucket())
        .key("test.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("PUT should succeed");

    let get_result = client
        .get_object()
        .bucket(server.bucket())
        .key("test.txt")
        .send()
        .await
        .expect("GET should succeed");

    let body = get_result.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), data, "Content should match");
}

#[tokio::test]
async fn test_put_get_binary() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let data = generate_binary(10_000, 42);

    client
        .put_object()
        .bucket(server.bucket())
        .key("binary.bin")
        .body(ByteStream::from(data.clone()))
        .send()
        .await
        .expect("PUT should succeed");

    let get_result = client
        .get_object()
        .bucket(server.bucket())
        .key("binary.bin")
        .send()
        .await
        .expect("GET should succeed");

    let body = get_result.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), data.as_slice());
}

#[tokio::test]
async fn test_put_get_delete_lifecycle() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let data = b"To be deleted";

    client
        .put_object()
        .bucket(server.bucket())
        .key("deleteme.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("PUT should succeed");

    let body = client
        .get_object()
        .bucket(server.bucket())
        .key("deleteme.txt")
        .send()
        .await
        .expect("GET should succeed")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), data);

    client
        .delete_object()
        .bucket(server.bucket())
        .key("deleteme.txt")
        .send()
        .await
        .expect("DELETE should succeed");

    let get_after = client
        .get_object()
        .bucket(server.bucket())
        .key("deleteme.txt")
        .send()
        .await;
    assert!(get_after.is_err(), "GET after DELETE should fail");
}

#[tokio::test]
async fn test_put_overwrite_same_key() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("overwrite.txt")
        .body(ByteStream::from(b"version 1".to_vec()))
        .send()
        .await
        .unwrap();

    client
        .put_object()
        .bucket(server.bucket())
        .key("overwrite.txt")
        .body(ByteStream::from(b"version 2".to_vec()))
        .send()
        .await
        .unwrap();

    let body = client
        .get_object()
        .bucket(server.bucket())
        .key("overwrite.txt")
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), b"version 2", "Should return latest version");
}

#[tokio::test]
async fn test_put_empty_body() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("empty.txt")
        .body(ByteStream::from(Vec::<u8>::new()))
        .send()
        .await
        .expect("PUT empty body should succeed");

    let get_result = client
        .get_object()
        .bucket(server.bucket())
        .key("empty.txt")
        .send()
        .await
        .expect("GET should succeed");

    let body = get_result.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.len(), 0, "Body should be empty");
}

#[tokio::test]
async fn test_put_large_1mb() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let data = generate_binary(1_000_000, 123);

    client
        .put_object()
        .bucket(server.bucket())
        .key("large.bin")
        .body(ByteStream::from(data.clone()))
        .send()
        .await
        .expect("PUT 1MB should succeed");

    let body = client
        .get_object()
        .bucket(server.bucket())
        .key("large.bin")
        .send()
        .await
        .expect("GET should succeed")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.len(), data.len());
    assert_eq!(body.as_ref(), data.as_slice());
}

#[tokio::test]
async fn test_put_large_10mb() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let data = generate_binary(10_000_000, 456);

    client
        .put_object()
        .bucket(server.bucket())
        .key("huge.bin")
        .body(ByteStream::from(data.clone()))
        .send()
        .await
        .expect("PUT 10MB should succeed");

    let body = client
        .get_object()
        .bucket(server.bucket())
        .key("huge.bin")
        .send()
        .await
        .expect("GET should succeed")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.len(), data.len());
    assert_eq!(body.as_ref(), data.as_slice());
}

// ============================================================================
// HeadObject
// ============================================================================

#[tokio::test]
async fn test_head_object_metadata() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let data = b"Test content for HEAD request";

    client
        .put_object()
        .bucket(server.bucket())
        .key("headtest.txt")
        .content_type("text/plain")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();

    let head = client
        .head_object()
        .bucket(server.bucket())
        .key("headtest.txt")
        .send()
        .await
        .expect("HEAD should succeed");

    assert_eq!(head.content_length(), Some(data.len() as i64));
    assert_eq!(head.content_type(), Some("text/plain"));
    assert!(head.e_tag().is_some(), "ETag should be present");
}

#[tokio::test]
async fn test_head_nonexistent() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let result = client
        .head_object()
        .bucket(server.bucket())
        .key("nope.txt")
        .send()
        .await;
    assert!(result.is_err());
}

// ============================================================================
// Content-Type
// ============================================================================

#[tokio::test]
async fn test_content_type_preserved() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("data.json")
        .content_type("application/json")
        .body(ByteStream::from(b"{\"key\":\"value\"}".to_vec()))
        .send()
        .await
        .unwrap();

    let get = client
        .get_object()
        .bucket(server.bucket())
        .key("data.json")
        .send()
        .await
        .unwrap();
    assert_eq!(get.content_type(), Some("application/json"));
}

#[tokio::test]
async fn test_content_type_default() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // PUT without Content-Type
    client
        .put_object()
        .bucket(server.bucket())
        .key("noct.bin")
        .body(ByteStream::from(b"data".to_vec()))
        .send()
        .await
        .unwrap();

    let get = client
        .get_object()
        .bucket(server.bucket())
        .key("noct.bin")
        .send()
        .await
        .unwrap();

    // Should have some content type (application/octet-stream is typical default)
    assert!(get.content_type().is_some(), "Should have a Content-Type");
}

// ============================================================================
// ETag
// ============================================================================

#[tokio::test]
async fn test_etag_format() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("etag.txt")
        .body(ByteStream::from(b"etag test data".to_vec()))
        .send()
        .await
        .unwrap();

    let head = client
        .head_object()
        .bucket(server.bucket())
        .key("etag.txt")
        .send()
        .await
        .unwrap();

    let etag = head.e_tag().expect("ETag should be present");
    // S3 ETags are quoted hex strings: "abcdef0123456789..."
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "ETag should be quoted: {}",
        etag
    );
    let inner = &etag[1..etag.len() - 1];
    assert!(
        inner.chars().all(|c| c.is_ascii_hexdigit()),
        "ETag inner should be hex: {}",
        inner
    );
}

#[tokio::test]
async fn test_etag_consistent_same_data() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let data = b"consistent etag data";

    client
        .put_object()
        .bucket(server.bucket())
        .key("etag1.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();

    client
        .put_object()
        .bucket(server.bucket())
        .key("etag2.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();

    let e1 = client
        .head_object()
        .bucket(server.bucket())
        .key("etag1.txt")
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string();

    let e2 = client
        .head_object()
        .bucket(server.bucket())
        .key("etag2.txt")
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string();

    assert_eq!(e1, e2, "Same data should produce same ETag");
}

#[tokio::test]
async fn test_etag_differs_different_data() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("diff1.txt")
        .body(ByteStream::from(b"data A".to_vec()))
        .send()
        .await
        .unwrap();

    client
        .put_object()
        .bucket(server.bucket())
        .key("diff2.txt")
        .body(ByteStream::from(b"data B".to_vec()))
        .send()
        .await
        .unwrap();

    let e1 = client
        .head_object()
        .bucket(server.bucket())
        .key("diff1.txt")
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string();
    let e2 = client
        .head_object()
        .bucket(server.bucket())
        .key("diff2.txt")
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string();

    assert_ne!(e1, e2, "Different data should produce different ETags");
}

// ============================================================================
// ListObjectsV2
// ============================================================================

#[tokio::test]
async fn test_list_objects_basic() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    for i in 0..3 {
        client
            .put_object()
            .bucket(server.bucket())
            .key(format!("prefix/file{}.txt", i))
            .body(ByteStream::from(format!("Content {}", i).into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("prefix/")
        .send()
        .await
        .unwrap();

    let keys: Vec<String> = list
        .contents()
        .iter()
        .filter_map(|o| o.key().map(String::from))
        .collect();
    assert_eq!(keys.len(), 3);
    assert!(keys.contains(&"prefix/file0.txt".to_string()));
    assert!(keys.contains(&"prefix/file1.txt".to_string()));
    assert!(keys.contains(&"prefix/file2.txt".to_string()));
}

#[tokio::test]
async fn test_list_objects_with_prefix() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("a/1.txt")
        .body(ByteStream::from(b"a".to_vec()))
        .send()
        .await
        .unwrap();
    client
        .put_object()
        .bucket(server.bucket())
        .key("b/1.txt")
        .body(ByteStream::from(b"b".to_vec()))
        .send()
        .await
        .unwrap();

    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("a/")
        .send()
        .await
        .unwrap();
    let keys: Vec<String> = list
        .contents()
        .iter()
        .filter_map(|o| o.key().map(String::from))
        .collect();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0], "a/1.txt");
}

#[tokio::test]
async fn test_list_objects_with_delimiter() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Create objects under different "folders"
    client
        .put_object()
        .bucket(server.bucket())
        .key("dir/sub1/a.txt")
        .body(ByteStream::from(b"a".to_vec()))
        .send()
        .await
        .unwrap();
    client
        .put_object()
        .bucket(server.bucket())
        .key("dir/sub2/b.txt")
        .body(ByteStream::from(b"b".to_vec()))
        .send()
        .await
        .unwrap();
    client
        .put_object()
        .bucket(server.bucket())
        .key("dir/top.txt")
        .body(ByteStream::from(b"c".to_vec()))
        .send()
        .await
        .unwrap();

    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("dir/")
        .delimiter("/")
        .send()
        .await
        .unwrap();

    // top.txt should be in contents, sub1/ and sub2/ in common prefixes
    let keys: Vec<String> = list
        .contents()
        .iter()
        .filter_map(|o| o.key().map(String::from))
        .collect();
    assert!(
        keys.contains(&"dir/top.txt".to_string()),
        "Should contain top.txt, got {:?}",
        keys
    );

    let prefixes: Vec<String> = list
        .common_prefixes()
        .iter()
        .filter_map(|p| p.prefix().map(String::from))
        .collect();
    assert!(
        prefixes.contains(&"dir/sub1/".to_string()),
        "Should contain dir/sub1/, got {:?}",
        prefixes
    );
    assert!(
        prefixes.contains(&"dir/sub2/".to_string()),
        "Should contain dir/sub2/, got {:?}",
        prefixes
    );
}

#[tokio::test]
async fn test_list_objects_pagination() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    for i in 0..5 {
        client
            .put_object()
            .bucket(server.bucket())
            .key(format!("page/f{}.txt", i))
            .body(ByteStream::from(format!("{}", i).into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let page1 = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("page/")
        .max_keys(2)
        .send()
        .await
        .unwrap();

    assert_eq!(page1.contents().len(), 2);
    assert!(page1.is_truncated().unwrap_or(false));
    assert!(page1.next_continuation_token().is_some());

    let page2 = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("page/")
        .max_keys(2)
        .continuation_token(page1.next_continuation_token().unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(page2.contents().len(), 2);

    // Collect all keys across pages
    let mut all_keys: Vec<String> = page1
        .contents()
        .iter()
        .chain(page2.contents().iter())
        .filter_map(|o| o.key().map(String::from))
        .collect();
    all_keys.sort();

    // Should have 4 unique keys across two pages (continuation is exclusive)
    assert!(
        all_keys.len() >= 4,
        "Should have at least 4 keys across pages, got {:?}",
        all_keys
    );
}

#[tokio::test]
async fn test_list_objects_empty_bucket() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .send()
        .await
        .unwrap();

    assert_eq!(list.key_count(), Some(0));
    assert!(!list.is_truncated().unwrap_or(false));
}

#[tokio::test]
async fn test_list_objects_prefix_not_found() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("exists/f.txt")
        .body(ByteStream::from(b"x".to_vec()))
        .send()
        .await
        .unwrap();

    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("nonexistent/")
        .send()
        .await
        .unwrap();

    assert_eq!(list.key_count(), Some(0));
}

// ============================================================================
// CopyObject
// ============================================================================

#[tokio::test]
async fn test_copy_object_same_bucket() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;
    let bucket = server.bucket();

    let data = b"Original content to copy";

    client
        .put_object()
        .bucket(bucket)
        .key("source.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();

    client
        .copy_object()
        .bucket(bucket)
        .key("dest.txt")
        .copy_source(format!("{}/source.txt", bucket))
        .send()
        .await
        .unwrap();

    let body = client
        .get_object()
        .bucket(bucket)
        .key("dest.txt")
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
async fn test_copy_preserves_content_type() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;
    let bucket = server.bucket();

    client
        .put_object()
        .bucket(bucket)
        .key("typed.json")
        .content_type("application/json")
        .body(ByteStream::from(b"{}".to_vec()))
        .send()
        .await
        .unwrap();

    client
        .copy_object()
        .bucket(bucket)
        .key("typed_copy.json")
        .copy_source(format!("{}/typed.json", bucket))
        .send()
        .await
        .unwrap();

    let get = client
        .get_object()
        .bucket(bucket)
        .key("typed_copy.json")
        .send()
        .await
        .unwrap();
    assert_eq!(get.content_type(), Some("application/json"));
}

#[tokio::test]
async fn test_copy_nonexistent_source() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;
    let bucket = server.bucket();

    let result = client
        .copy_object()
        .bucket(bucket)
        .key("dest.txt")
        .copy_source(format!("{}/nonexistent.txt", bucket))
        .send()
        .await;
    assert!(result.is_err(), "Copy from nonexistent source should fail");
}

// ============================================================================
// DeleteObject
// ============================================================================

#[tokio::test]
async fn test_delete_existing() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("del.txt")
        .body(ByteStream::from(b"x".to_vec()))
        .send()
        .await
        .unwrap();
    client
        .delete_object()
        .bucket(server.bucket())
        .key("del.txt")
        .send()
        .await
        .expect("DELETE should succeed");

    let get = client
        .get_object()
        .bucket(server.bucket())
        .key("del.txt")
        .send()
        .await;
    assert!(get.is_err());
}

#[tokio::test]
async fn test_delete_nonexistent_idempotent() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // S3 spec: DELETE on nonexistent key returns 204 (success)
    let result = client
        .delete_object()
        .bucket(server.bucket())
        .key("never_existed.txt")
        .send()
        .await;
    assert!(
        result.is_ok(),
        "DELETE nonexistent should succeed (S3 spec: idempotent)"
    );
}

#[tokio::test]
async fn test_delete_then_get_returns_404() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("gone.txt")
        .body(ByteStream::from(b"bye".to_vec()))
        .send()
        .await
        .unwrap();
    client
        .delete_object()
        .bucket(server.bucket())
        .key("gone.txt")
        .send()
        .await
        .unwrap();

    let get = client
        .get_object()
        .bucket(server.bucket())
        .key("gone.txt")
        .send()
        .await;
    assert!(get.is_err(), "GET after DELETE should fail");
}

// ============================================================================
// DeleteObjects (batch)
// ============================================================================

#[tokio::test]
async fn test_delete_objects_batch() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    for i in 0..5 {
        client
            .put_object()
            .bucket(server.bucket())
            .key(format!("batch/file{}.txt", i))
            .body(ByteStream::from(format!("Content {}", i).into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let ids: Vec<ObjectIdentifier> = (0..5)
        .map(|i| {
            ObjectIdentifier::builder()
                .key(format!("batch/file{}.txt", i))
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
        .expect("Batch delete should succeed");

    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("batch/")
        .send()
        .await
        .unwrap();
    assert_eq!(list.key_count(), Some(0), "All should be deleted");
}

#[tokio::test]
async fn test_delete_objects_quiet_mode() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("quiet/a.txt")
        .body(ByteStream::from(b"a".to_vec()))
        .send()
        .await
        .unwrap();

    let ids = vec![ObjectIdentifier::builder()
        .key("quiet/a.txt")
        .build()
        .unwrap()];
    let delete = Delete::builder()
        .quiet(true)
        .set_objects(Some(ids))
        .build()
        .unwrap();

    let result = client
        .delete_objects()
        .bucket(server.bucket())
        .delete(delete)
        .send()
        .await
        .expect("Quiet delete should succeed");

    // In quiet mode, only errors are returned
    assert!(result.errors().is_empty(), "Should have no errors");
}

#[tokio::test]
async fn test_delete_objects_nonexistent_keys() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // S3 spec: batch delete treats nonexistent keys as success
    let ids = vec![
        ObjectIdentifier::builder()
            .key("nope1.txt")
            .build()
            .unwrap(),
        ObjectIdentifier::builder()
            .key("nope2.txt")
            .build()
            .unwrap(),
    ];
    let delete = Delete::builder().set_objects(Some(ids)).build().unwrap();

    let result = client
        .delete_objects()
        .bucket(server.bucket())
        .delete(delete)
        .send()
        .await;
    assert!(result.is_ok(), "Batch delete of nonexistent should succeed");
}

#[tokio::test]
async fn test_delete_objects_mixed() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("mixed/exists.txt")
        .body(ByteStream::from(b"x".to_vec()))
        .send()
        .await
        .unwrap();

    let ids = vec![
        ObjectIdentifier::builder()
            .key("mixed/exists.txt")
            .build()
            .unwrap(),
        ObjectIdentifier::builder()
            .key("mixed/nope.txt")
            .build()
            .unwrap(),
    ];
    let delete = Delete::builder().set_objects(Some(ids)).build().unwrap();

    let result = client
        .delete_objects()
        .bucket(server.bucket())
        .delete(delete)
        .send()
        .await
        .expect("Mixed delete should succeed");
    assert!(
        result.errors().is_empty(),
        "Both existing and nonexistent should succeed"
    );
}

// ============================================================================
// Bucket operations
// ============================================================================

#[tokio::test]
async fn test_list_buckets() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let result = client
        .list_buckets()
        .send()
        .await
        .expect("LIST buckets should succeed");
    let buckets = result.buckets();
    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets[0].name(), Some(server.bucket()));
}

#[tokio::test]
async fn test_head_bucket() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let result = client.head_bucket().bucket(server.bucket()).send().await;
    assert!(result.is_ok(), "HEAD default bucket should succeed");
}

#[tokio::test]
async fn test_head_bucket_nonexistent() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let result = client
        .head_bucket()
        .bucket("nonexistent-bucket")
        .send()
        .await;
    assert!(result.is_err(), "HEAD nonexistent bucket should fail");
}

#[tokio::test]
async fn test_create_bucket_default() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let result = client.create_bucket().bucket(server.bucket()).send().await;
    assert!(result.is_ok(), "CREATE default bucket should succeed");
}

#[tokio::test]
async fn test_create_bucket_any_name() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Multi-bucket mode: creating any bucket should succeed
    let result = client.create_bucket().bucket("custom-bucket").send().await;
    assert!(
        result.is_ok(),
        "CREATE any bucket should succeed in multi-bucket mode"
    );

    // Verify the new bucket appears in list_buckets
    let buckets = client.list_buckets().send().await.unwrap();
    let names: Vec<&str> = buckets
        .buckets()
        .iter()
        .map(|b| b.name().unwrap_or(""))
        .collect();
    assert!(
        names.contains(&"custom-bucket"),
        "New bucket should appear in list: {:?}",
        names
    );
}

#[tokio::test]
async fn test_delete_empty_bucket() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let result = client.delete_bucket().bucket(server.bucket()).send().await;
    assert!(result.is_ok(), "DELETE empty bucket should succeed");
}

#[tokio::test]
async fn test_delete_nonempty_bucket() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("blocker.txt")
        .body(ByteStream::from(b"x".to_vec()))
        .send()
        .await
        .unwrap();

    let result = client.delete_bucket().bucket(server.bucket()).send().await;
    assert!(result.is_err(), "DELETE non-empty bucket should fail");
}

// ============================================================================
// Special characters in keys
// ============================================================================

#[tokio::test]
async fn test_unicode_key() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let data = b"unicode content";
    client
        .put_object()
        .bucket(server.bucket())
        .key("files/日本語.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("PUT with unicode key should succeed");

    let body = client
        .get_object()
        .bucket(server.bucket())
        .key("files/日本語.txt")
        .send()
        .await
        .expect("GET with unicode key should succeed")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), data);
}

#[tokio::test]
async fn test_key_with_spaces() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let data = b"spaces in key";
    client
        .put_object()
        .bucket(server.bucket())
        .key("my folder/my file.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("PUT with spaces should succeed");

    let body = client
        .get_object()
        .bucket(server.bucket())
        .key("my folder/my file.txt")
        .send()
        .await
        .expect("GET with spaces should succeed")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), data);
}

#[tokio::test]
async fn test_key_with_special_chars() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let data = b"special chars";
    client
        .put_object()
        .bucket(server.bucket())
        .key("special/file-v1.0+build.123.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("PUT with special chars should succeed");

    let body = client
        .get_object()
        .bucket(server.bucket())
        .key("special/file-v1.0+build.123.txt")
        .send()
        .await
        .expect("GET with special chars should succeed")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), data);
}

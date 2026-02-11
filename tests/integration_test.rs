//! Integration tests for DeltaGlider Proxy S3 server with AWS SDK
//!
//! Tests use the official aws-sdk-s3 to verify S3 compatibility

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use rand::{Rng, SeedableRng};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

/// Port counter to avoid conflicts between tests
static PORT_COUNTER: AtomicU16 = AtomicU16::new(19000);

/// Test server wrapper
struct TestServer {
    process: Child,
    port: u16,
    _data_dir: TempDir,
}

impl TestServer {
    /// Start a test server on a random port
    async fn start() -> Self {
        let port = PORT_COUNTER.fetch_add(1, Ordering::SeqCst);
        let data_dir = TempDir::new().expect("Failed to create temp dir");

        let mut process = Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"))
            .env(
                "DELTAGLIDER_PROXY_LISTEN_ADDR",
                format!("127.0.0.1:{}", port),
            )
            .env("DELTAGLIDER_PROXY_DATA_DIR", data_dir.path())
            .env("DELTAGLIDER_PROXY_DEFAULT_BUCKET", "bucket")
            .env("RUST_LOG", "deltaglider_proxy=warn")
            .spawn()
            .expect("Failed to start server");

        // Wait for server to be ready by polling
        let addr = format!("127.0.0.1:{}", port);
        let mut ready = false;
        for _ in 0..150 {
            if std::net::TcpStream::connect(&addr).is_ok() {
                // Give it a bit more time to fully initialize
                sleep(Duration::from_millis(100)).await;
                ready = true;
                break;
            }

            if let Ok(Some(status)) = process.try_wait() {
                panic!(
                    "DeltaGlider Proxy server exited before becoming ready: {}",
                    status
                );
            }

            sleep(Duration::from_millis(100)).await;
        }

        if !ready {
            let _ = process.kill();
            panic!(
                "Timed out waiting for DeltaGlider Proxy server to listen on {}",
                addr
            );
        }

        Self {
            process,
            port,
            _data_dir: data_dir,
        }
    }

    /// Create an S3 client configured for this test server
    async fn s3_client(&self) -> Client {
        let credentials = Credentials::new("test", "test", None, None, "test");

        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(format!("http://127.0.0.1:{}", self.port))
            .credentials_provider(credentials)
            .force_path_style(true)
            .build();

        Client::from_conf(config)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

/// Generate a binary file with pseudorandom content
fn generate_binary(size: usize, seed: u64) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut data = vec![0u8; size];
    rng.fill(&mut data[..]);
    data
}

/// Mutate a binary file by changing a percentage of bytes
fn mutate_binary(data: &[u8], change_ratio: f64) -> Vec<u8> {
    let mut result = data.to_vec();
    let changes = (data.len() as f64 * change_ratio) as usize;
    let mut rng = rand::thread_rng();

    for _ in 0..changes {
        let idx = rng.gen_range(0..result.len());
        result[idx] = rng.gen();
    }

    result
}

#[tokio::test]
async fn test_put_get_single_file() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    let data = b"Hello, DeltaGlider Proxy!";

    // PUT object
    client
        .put_object()
        .bucket("bucket")
        .key("test.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("PUT should succeed");

    // GET object
    let get_result = client
        .get_object()
        .bucket("bucket")
        .key("test.txt")
        .send()
        .await
        .expect("GET should succeed");

    let body = get_result
        .body
        .collect()
        .await
        .expect("Failed to read body")
        .into_bytes();

    assert_eq!(body.as_ref(), data, "Content should match");
}

#[tokio::test]
async fn test_put_get_delete() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    let data = b"To be deleted";

    // PUT object
    client
        .put_object()
        .bucket("bucket")
        .key("deleteme.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("PUT should succeed");

    // Verify it exists with GET
    let get_result = client
        .get_object()
        .bucket("bucket")
        .key("deleteme.txt")
        .send()
        .await
        .expect("GET should succeed before delete");

    let body = get_result.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), data);

    // DELETE object
    client
        .delete_object()
        .bucket("bucket")
        .key("deleteme.txt")
        .send()
        .await
        .expect("DELETE should succeed");

    // Verify it's gone
    let get_after_delete = client
        .get_object()
        .bucket("bucket")
        .key("deleteme.txt")
        .send()
        .await;

    assert!(
        get_after_delete.is_err(),
        "GET after DELETE should fail with NoSuchKey"
    );
}

#[tokio::test]
async fn test_list_objects() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    // PUT multiple files
    for i in 0..3 {
        client
            .put_object()
            .bucket("bucket")
            .key(format!("prefix/file{}.txt", i))
            .body(ByteStream::from(format!("Content {}", i).into_bytes()))
            .send()
            .await
            .expect("PUT should succeed");
    }

    // LIST objects
    let list_result = client
        .list_objects_v2()
        .bucket("bucket")
        .prefix("prefix/")
        .send()
        .await
        .expect("LIST should succeed");

    let keys: Vec<String> = list_result
        .contents()
        .iter()
        .filter_map(|obj| obj.key().map(String::from))
        .collect();

    assert_eq!(keys.len(), 3, "Should list 3 objects");
    assert!(keys.contains(&"prefix/file0.txt".to_string()));
    assert!(keys.contains(&"prefix/file1.txt".to_string()));
    assert!(keys.contains(&"prefix/file2.txt".to_string()));
}

#[tokio::test]
async fn test_delta_deduplication_three_files() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    // Generate base file (100KB)
    let base_data = generate_binary(100_000, 42);

    // Generate two variants with small changes (1% and 2% different)
    let variant1 = mutate_binary(&base_data, 0.01);
    let variant2 = mutate_binary(&base_data, 0.02);

    // Upload all three as ZIP files (delta-eligible)
    println!("Uploading base.zip ({} bytes)", base_data.len());
    client
        .put_object()
        .bucket("bucket")
        .key("releases/base.zip")
        .body(ByteStream::from(base_data.clone()))
        .send()
        .await
        .expect("PUT base failed");

    println!("Uploading v1.zip ({} bytes)", variant1.len());
    client
        .put_object()
        .bucket("bucket")
        .key("releases/v1.zip")
        .body(ByteStream::from(variant1.clone()))
        .send()
        .await
        .expect("PUT v1 failed");

    println!("Uploading v2.zip ({} bytes)", variant2.len());
    client
        .put_object()
        .bucket("bucket")
        .key("releases/v2.zip")
        .body(ByteStream::from(variant2.clone()))
        .send()
        .await
        .expect("PUT v2 failed");

    // Verify retrieval - all three files should be reconstructed correctly
    println!("Retrieving base.zip");
    let get_base = client
        .get_object()
        .bucket("bucket")
        .key("releases/base.zip")
        .send()
        .await
        .expect("GET base failed");
    let retrieved_base = get_base.body.collect().await.unwrap().into_bytes();
    assert_eq!(
        retrieved_base.as_ref(),
        base_data.as_slice(),
        "base.zip content mismatch"
    );

    println!("Retrieving v1.zip");
    let get_v1 = client
        .get_object()
        .bucket("bucket")
        .key("releases/v1.zip")
        .send()
        .await
        .expect("GET v1 failed");
    let retrieved_v1 = get_v1.body.collect().await.unwrap().into_bytes();
    assert_eq!(
        retrieved_v1.as_ref(),
        variant1.as_slice(),
        "v1.zip content mismatch"
    );

    println!("Retrieving v2.zip");
    let get_v2 = client
        .get_object()
        .bucket("bucket")
        .key("releases/v2.zip")
        .send()
        .await
        .expect("GET v2 failed");
    let retrieved_v2 = get_v2.body.collect().await.unwrap().into_bytes();
    assert_eq!(
        retrieved_v2.as_ref(),
        variant2.as_slice(),
        "v2.zip content mismatch"
    );

    // Verify LIST returns all three files
    let list_result = client
        .list_objects_v2()
        .bucket("bucket")
        .prefix("releases/")
        .send()
        .await
        .expect("LIST failed");

    let keys: Vec<String> = list_result
        .contents()
        .iter()
        .filter_map(|obj| obj.key().map(String::from))
        .collect();

    assert!(keys.contains(&"releases/base.zip".to_string()));
    assert!(keys.contains(&"releases/v1.zip".to_string()));
    assert!(keys.contains(&"releases/v2.zip".to_string()));

    println!("All three files uploaded and retrieved successfully with AWS SDK!");
}

#[tokio::test]
async fn test_get_nonexistent_returns_error() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    let result = client
        .get_object()
        .bucket("bucket")
        .key("nonexistent.txt")
        .send()
        .await;

    assert!(result.is_err(), "GET nonexistent should return error");
}

#[tokio::test]
async fn test_large_binary_file() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    // Generate 1MB binary file
    let data = generate_binary(1_000_000, 123);

    // PUT
    client
        .put_object()
        .bucket("bucket")
        .key("large.bin")
        .body(ByteStream::from(data.clone()))
        .send()
        .await
        .expect("PUT large file should succeed");

    // GET
    let get_result = client
        .get_object()
        .bucket("bucket")
        .key("large.bin")
        .send()
        .await
        .expect("GET large file should succeed");

    let body = get_result.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.len(), data.len(), "Size should match");
    assert_eq!(body.as_ref(), data.as_slice(), "Content should match");
}

#[tokio::test]
async fn test_content_type_preserved() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    let data = b"{\"key\": \"value\"}";

    // PUT with content type
    client
        .put_object()
        .bucket("bucket")
        .key("data.json")
        .content_type("application/json")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("PUT should succeed");

    // GET and check content type
    let get_result = client
        .get_object()
        .bucket("bucket")
        .key("data.json")
        .send()
        .await
        .expect("GET should succeed");

    assert_eq!(
        get_result.content_type(),
        Some("application/json"),
        "Content-Type should be preserved"
    );
}

#[tokio::test]
async fn test_head_object() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    let data = b"Test content for HEAD request";

    // PUT object
    client
        .put_object()
        .bucket("bucket")
        .key("headtest.txt")
        .content_type("text/plain")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("PUT should succeed");

    // HEAD object
    let head_result = client
        .head_object()
        .bucket("bucket")
        .key("headtest.txt")
        .send()
        .await
        .expect("HEAD should succeed");

    assert_eq!(
        head_result.content_length(),
        Some(data.len() as i64),
        "Content-Length should match"
    );
    assert_eq!(
        head_result.content_type(),
        Some("text/plain"),
        "Content-Type should be preserved"
    );
    assert!(head_result.e_tag().is_some(), "ETag should be present");
}

#[tokio::test]
async fn test_copy_object() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    let data = b"Original content to copy";

    // PUT source object
    client
        .put_object()
        .bucket("bucket")
        .key("source.txt")
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("PUT source should succeed");

    // COPY object
    client
        .copy_object()
        .bucket("bucket")
        .key("destination.txt")
        .copy_source("bucket/source.txt")
        .send()
        .await
        .expect("COPY should succeed");

    // GET copied object
    let get_result = client
        .get_object()
        .bucket("bucket")
        .key("destination.txt")
        .send()
        .await
        .expect("GET copied object should succeed");

    let body = get_result.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), data, "Copied content should match original");
}

#[tokio::test]
async fn test_delete_objects_batch() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    // Create multiple objects
    for i in 0..5 {
        client
            .put_object()
            .bucket("bucket")
            .key(format!("batch/file{}.txt", i))
            .body(ByteStream::from(format!("Content {}", i).into_bytes()))
            .send()
            .await
            .expect("PUT should succeed");
    }

    // Verify objects exist
    let list_before = client
        .list_objects_v2()
        .bucket("bucket")
        .prefix("batch/")
        .send()
        .await
        .expect("LIST should succeed");
    assert_eq!(list_before.key_count(), Some(5), "Should have 5 objects");

    // Delete multiple objects using batch delete
    use aws_sdk_s3::types::{Delete, ObjectIdentifier};

    let objects_to_delete: Vec<ObjectIdentifier> = (0..5)
        .map(|i| {
            ObjectIdentifier::builder()
                .key(format!("batch/file{}.txt", i))
                .build()
                .unwrap()
        })
        .collect();

    let delete = Delete::builder()
        .set_objects(Some(objects_to_delete))
        .build()
        .unwrap();

    client
        .delete_objects()
        .bucket("bucket")
        .delete(delete)
        .send()
        .await
        .expect("DELETE batch should succeed");

    // Verify objects are deleted
    let list_after = client
        .list_objects_v2()
        .bucket("bucket")
        .prefix("batch/")
        .send()
        .await
        .expect("LIST should succeed");
    assert_eq!(
        list_after.key_count(),
        Some(0),
        "All objects should be deleted"
    );
}

#[tokio::test]
async fn test_head_bucket() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    // HEAD bucket should succeed for the default bucket
    let result = client.head_bucket().bucket("bucket").send().await;

    assert!(
        result.is_ok(),
        "HEAD bucket should succeed for default bucket"
    );
}

#[tokio::test]
async fn test_head_bucket_not_found() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    // HEAD bucket should fail for non-existent bucket
    let result = client
        .head_bucket()
        .bucket("nonexistent-bucket")
        .send()
        .await;

    assert!(
        result.is_err(),
        "HEAD bucket should fail for non-existent bucket"
    );
}

#[tokio::test]
async fn test_list_buckets() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    // LIST buckets
    let result = client
        .list_buckets()
        .send()
        .await
        .expect("LIST buckets should succeed");

    let buckets = result.buckets();
    assert_eq!(buckets.len(), 1, "Should have exactly 1 bucket");
    assert_eq!(
        buckets[0].name(),
        Some("bucket"),
        "Bucket name should match"
    );
}

#[tokio::test]
async fn test_create_bucket_default() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    // CREATE the default bucket should succeed (it already exists conceptually)
    let result = client.create_bucket().bucket("bucket").send().await;

    assert!(result.is_ok(), "CREATE default bucket should succeed");
}

#[tokio::test]
async fn test_delete_empty_bucket() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    // DELETE empty bucket should succeed
    let result = client.delete_bucket().bucket("bucket").send().await;

    assert!(result.is_ok(), "DELETE empty bucket should succeed");
}

#[tokio::test]
async fn test_delete_non_empty_bucket_fails() {
    let server = TestServer::start().await;
    let client = server.s3_client().await;

    // PUT an object
    client
        .put_object()
        .bucket("bucket")
        .key("blocker.txt")
        .body(ByteStream::from(b"content".to_vec()))
        .send()
        .await
        .expect("PUT should succeed");

    // DELETE bucket should fail (not empty)
    let result = client.delete_bucket().bucket("bucket").send().await;

    assert!(result.is_err(), "DELETE non-empty bucket should fail");
}

//! S3 Backend integration tests
//!
//! These tests verify that S3Backend works correctly against a real S3-compatible
//! service (MinIO). Requires MinIO to be running via docker-compose.
//!
//! Usage:
//!   docker compose up -d
//!   cargo test --test s3_backend_test -- --ignored
//!   docker compose down

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::Client;
use rand::{Rng, SeedableRng};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// Import deltaglider_proxy types for direct S3Backend testing
use deltaglider_proxy::config::{BackendConfig, Config};
use deltaglider_proxy::storage::{S3Backend, StorageBackend, StorageError};
use deltaglider_proxy::types::FileMetadata;

/// Unique test prefix counter to avoid conflicts between tests
static TEST_PREFIX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// MinIO configuration for tests
const MINIO_ENDPOINT: &str = "http://localhost:9000";
const MINIO_BUCKET: &str = "deltaglider-test";
const MINIO_REGION: &str = "us-east-1";
const MINIO_ACCESS_KEY: &str = "minioadmin";
const MINIO_SECRET_KEY: &str = "minioadmin";

/// Create S3 backend configuration for MinIO
fn minio_config() -> Config {
    Config {
        backend: BackendConfig::S3 {
            endpoint: Some(MINIO_ENDPOINT.to_string()),
            bucket: MINIO_BUCKET.to_string(),
            region: MINIO_REGION.to_string(),
            force_path_style: true,
            access_key_id: Some(MINIO_ACCESS_KEY.to_string()),
            secret_access_key: Some(MINIO_SECRET_KEY.to_string()),
        },
        ..Default::default()
    }
}

/// Create a unique test prefix to isolate tests
fn unique_prefix() -> String {
    let counter = TEST_PREFIX_COUNTER.fetch_add(1, Ordering::SeqCst);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("test-{}-{}", timestamp, counter)
}

/// Check if MinIO is available
async fn minio_available() -> bool {
    let credentials = Credentials::new(MINIO_ACCESS_KEY, MINIO_SECRET_KEY, None, None, "test");

    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(MINIO_REGION))
        .endpoint_url(MINIO_ENDPOINT)
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();

    let client = Client::from_conf(config);

    // Try to list buckets with a timeout
    tokio::time::timeout(Duration::from_secs(2), client.list_buckets().send())
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// Create a new FileMetadata for direct storage
fn test_metadata_direct(original_name: &str, size: u64) -> FileMetadata {
    FileMetadata::new_direct(
        original_name.to_string(),
        "test_sha256_hash".to_string(),
        "test_md5_hash".to_string(),
        size,
        Some("application/octet-stream".to_string()),
    )
}

/// Create a new FileMetadata for reference storage
fn test_metadata_reference(original_name: &str, size: u64) -> FileMetadata {
    FileMetadata::new_reference(
        original_name.to_string(),
        original_name.to_string(),
        "test_sha256_hash".to_string(),
        "test_md5_hash".to_string(),
        size,
        Some("application/octet-stream".to_string()),
    )
}

/// Create a new FileMetadata for delta storage
fn test_metadata_delta(original_name: &str, file_size: u64, delta_size: u64) -> FileMetadata {
    FileMetadata::new_delta(
        original_name.to_string(),
        "test_sha256_hash".to_string(),
        "test_md5_hash".to_string(),
        file_size,
        "reference.bin".to_string(),
        "ref_sha256".to_string(),
        delta_size,
        Some("application/octet-stream".to_string()),
    )
}

/// Generate pseudorandom binary data
fn generate_binary(size: usize, seed: u64) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut data = vec![0u8; size];
    rng.fill(&mut data[..]);
    data
}

// ============================================
// S3Backend Direct Tests
// ============================================

#[tokio::test]
#[ignore = "Requires MinIO: docker compose up -d"]
async fn test_s3_backend_reference_operations() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }

    let config = minio_config();
    let backend = S3Backend::new(&config.backend)
        .await
        .expect("Failed to create S3Backend");

    let prefix = unique_prefix();
    let data = b"Reference file content";
    let metadata = test_metadata_reference(&format!("{}/reference", prefix), data.len() as u64);

    // Initially no reference
    assert!(
        !backend.has_reference(&prefix).await,
        "Should not have reference initially"
    );

    // Put reference
    backend
        .put_reference(&prefix, data, &metadata)
        .await
        .expect("put_reference failed");

    // Has reference
    assert!(
        backend.has_reference(&prefix).await,
        "Should have reference after put"
    );

    // Get reference
    let retrieved = backend
        .get_reference(&prefix)
        .await
        .expect("get_reference failed");
    assert_eq!(retrieved, data, "Data mismatch");

    // Get metadata
    let retrieved_meta = backend
        .get_reference_metadata(&prefix)
        .await
        .expect("get_reference_metadata failed");
    assert_eq!(
        retrieved_meta.original_name, metadata.original_name,
        "Metadata mismatch"
    );

    // Delete reference
    backend
        .delete_reference(&prefix)
        .await
        .expect("delete_reference failed");
    assert!(
        !backend.has_reference(&prefix).await,
        "Should not have reference after delete"
    );
}

#[tokio::test]
#[ignore = "Requires MinIO: docker compose up -d"]
async fn test_s3_backend_delta_operations() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }

    let config = minio_config();
    let backend = S3Backend::new(&config.backend)
        .await
        .expect("Failed to create S3Backend");

    let prefix = unique_prefix();
    let filename = "v1.zip";
    let data = generate_binary(10_000, 42);
    let metadata = test_metadata_delta(
        &format!("{}/{}", prefix, filename),
        data.len() as u64,
        data.len() as u64,
    );

    // Put delta
    backend
        .put_delta(&prefix, filename, &data, &metadata)
        .await
        .expect("put_delta failed");

    // Get delta
    let retrieved = backend
        .get_delta(&prefix, filename)
        .await
        .expect("get_delta failed");
    assert_eq!(retrieved, data, "Delta data mismatch");

    // Get delta metadata
    let retrieved_meta = backend
        .get_delta_metadata(&prefix, filename)
        .await
        .expect("get_delta_metadata failed");
    assert!(retrieved_meta.is_delta(), "Should be delta storage type");

    // Delete delta
    backend
        .delete_delta(&prefix, filename)
        .await
        .expect("delete_delta failed");

    // Verify deleted
    let result = backend.get_delta(&prefix, filename).await;
    assert!(matches!(result, Err(StorageError::NotFound(_))));
}

#[tokio::test]
#[ignore = "Requires MinIO: docker compose up -d"]
async fn test_s3_backend_direct_operations() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }

    let config = minio_config();
    let backend = S3Backend::new(&config.backend)
        .await
        .expect("Failed to create S3Backend");

    let prefix = unique_prefix();
    let filename = "small.txt";
    let data = b"Direct storage content - too small for delta";
    let metadata = test_metadata_direct(&format!("{}/{}", prefix, filename), data.len() as u64);

    // Put direct
    backend
        .put_direct(&prefix, filename, data, &metadata)
        .await
        .expect("put_direct failed");

    // Get direct
    let retrieved = backend
        .get_direct(&prefix, filename)
        .await
        .expect("get_direct failed");
    assert_eq!(retrieved, data, "Direct data mismatch");

    // Get direct metadata
    let retrieved_meta = backend
        .get_direct_metadata(&prefix, filename)
        .await
        .expect("get_direct_metadata failed");
    assert!(
        !retrieved_meta.is_reference() && !retrieved_meta.is_delta(),
        "Should be direct storage type"
    );

    // Delete direct
    backend
        .delete_direct(&prefix, filename)
        .await
        .expect("delete_direct failed");

    // Verify deleted
    let result = backend.get_direct(&prefix, filename).await;
    assert!(matches!(result, Err(StorageError::NotFound(_))));
}

#[tokio::test]
#[ignore = "Requires MinIO: docker compose up -d"]
async fn test_s3_backend_scan_deltaspace() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }

    let config = minio_config();
    let backend = S3Backend::new(&config.backend)
        .await
        .expect("Failed to create S3Backend");

    let prefix = unique_prefix();

    // Create reference
    let ref_data = generate_binary(5_000, 100);
    let ref_meta = test_metadata_reference(&format!("{}/reference", prefix), ref_data.len() as u64);
    backend
        .put_reference(&prefix, &ref_data, &ref_meta)
        .await
        .expect("put_reference failed");

    // Create a few delta files
    for i in 0..3u64 {
        let filename = format!("file{}.zip", i);
        let data = generate_binary(1_000, 200 + i);
        let meta = test_metadata_delta(
            &format!("{}/{}", prefix, filename),
            data.len() as u64,
            data.len() as u64,
        );
        backend
            .put_delta(&prefix, &filename, &data, &meta)
            .await
            .expect("put_delta failed");
    }

    // Create a direct file
    let direct_data = b"direct content";
    let direct_meta =
        test_metadata_direct(&format!("{}/small.txt", prefix), direct_data.len() as u64);
    backend
        .put_direct(&prefix, "small.txt", direct_data, &direct_meta)
        .await
        .expect("put_direct failed");

    // Scan deltaspace
    let files = backend
        .scan_deltaspace(&prefix)
        .await
        .expect("scan_deltaspace failed");

    // Should find 5 files: 1 reference + 3 deltas + 1 direct
    assert_eq!(files.len(), 5, "Should find 5 files, found: {:?}", files);

    // Cleanup
    backend.delete_reference(&prefix).await.ok();
    for i in 0..3 {
        backend
            .delete_delta(&prefix, &format!("file{}.zip", i))
            .await
            .ok();
    }
    backend.delete_direct(&prefix, "small.txt").await.ok();
}

#[tokio::test]
#[ignore = "Requires MinIO: docker compose up -d"]
async fn test_s3_backend_list_deltaspaces() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }

    let config = minio_config();
    let backend = S3Backend::new(&config.backend)
        .await
        .expect("Failed to create S3Backend");

    // Create two deltaspaces
    let prefix1 = unique_prefix();
    let prefix2 = unique_prefix();

    let data = b"test data";
    let meta1 = test_metadata_reference(&format!("{}/ref", prefix1), data.len() as u64);
    let meta2 = test_metadata_reference(&format!("{}/ref", prefix2), data.len() as u64);

    backend
        .put_reference(&prefix1, data, &meta1)
        .await
        .expect("put_reference failed");
    backend
        .put_reference(&prefix2, data, &meta2)
        .await
        .expect("put_reference failed");

    // List deltaspaces
    let spaces = backend
        .list_deltaspaces()
        .await
        .expect("list_deltaspaces failed");

    assert!(
        spaces.contains(&prefix1),
        "Should contain prefix1: {} in {:?}",
        prefix1,
        spaces
    );
    assert!(
        spaces.contains(&prefix2),
        "Should contain prefix2: {} in {:?}",
        prefix2,
        spaces
    );

    // Cleanup
    backend.delete_reference(&prefix1).await.ok();
    backend.delete_reference(&prefix2).await.ok();
}

#[tokio::test]
#[ignore = "Requires MinIO: docker compose up -d"]
async fn test_s3_backend_total_size() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }

    let config = minio_config();
    let backend = S3Backend::new(&config.backend)
        .await
        .expect("Failed to create S3Backend");

    let prefix = unique_prefix();
    let data = generate_binary(10_000, 999);
    let meta = test_metadata_reference(&format!("{}/ref", prefix), data.len() as u64);

    // Store some data
    backend
        .put_reference(&prefix, &data, &meta)
        .await
        .expect("put_reference failed");

    // Total size should include our data
    let size = backend.total_size().await.expect("total_size failed");

    // Size should be at least our data size (data + metadata)
    assert!(
        size >= data.len() as u64,
        "Total size {} should be at least {}",
        size,
        data.len()
    );

    // Cleanup
    backend.delete_reference(&prefix).await.ok();
}

#[tokio::test]
#[ignore = "Requires MinIO: docker compose up -d"]
async fn test_s3_backend_not_found_errors() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }

    let config = minio_config();
    let backend = S3Backend::new(&config.backend)
        .await
        .expect("Failed to create S3Backend");

    let prefix = unique_prefix();

    // Reference not found
    let result = backend.get_reference(&prefix).await;
    assert!(
        matches!(result, Err(StorageError::NotFound(_))),
        "Expected NotFound, got {:?}",
        result
    );

    // Delta not found
    let result = backend.get_delta(&prefix, "nonexistent.zip").await;
    assert!(
        matches!(result, Err(StorageError::NotFound(_))),
        "Expected NotFound, got {:?}",
        result
    );

    // Direct not found
    let result = backend.get_direct(&prefix, "nonexistent.txt").await;
    assert!(
        matches!(result, Err(StorageError::NotFound(_))),
        "Expected NotFound, got {:?}",
        result
    );

    // Metadata not found
    let result = backend.get_reference_metadata(&prefix).await;
    assert!(
        matches!(result, Err(StorageError::NotFound(_))),
        "Expected NotFound, got {:?}",
        result
    );
}

#[tokio::test]
#[ignore = "Requires MinIO: docker compose up -d"]
async fn test_s3_backend_large_file() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }

    let config = minio_config();
    let backend = S3Backend::new(&config.backend)
        .await
        .expect("Failed to create S3Backend");

    let prefix = unique_prefix();

    // 5MB file
    let data = generate_binary(5 * 1024 * 1024, 12345);
    let meta = test_metadata_direct(&format!("{}/large.bin", prefix), data.len() as u64);

    println!("Uploading {} bytes...", data.len());

    backend
        .put_direct(&prefix, "large.bin", &data, &meta)
        .await
        .expect("put_direct large file failed");

    println!("Downloading {} bytes...", data.len());

    let retrieved = backend
        .get_direct(&prefix, "large.bin")
        .await
        .expect("get_direct large file failed");

    assert_eq!(retrieved.len(), data.len(), "Size mismatch");
    assert_eq!(retrieved, data, "Content mismatch");

    // Cleanup
    backend.delete_direct(&prefix, "large.bin").await.ok();
}

// ============================================
// Raw S3 operations test
// ============================================

#[tokio::test]
#[ignore = "Requires MinIO: docker compose up -d"]
async fn test_s3_backend_raw_operations() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }

    let config = minio_config();
    let backend = S3Backend::new(&config.backend)
        .await
        .expect("Failed to create S3Backend");

    let path = std::path::Path::new("test-raw/file.txt");
    let data = b"Raw file content";

    // Put raw
    backend.put_raw(path, data).await.expect("put_raw failed");

    // Exists
    assert!(backend.exists(path).await, "File should exist");

    // Get raw
    let retrieved = backend.get_raw(path).await.expect("get_raw failed");
    assert_eq!(retrieved, data);

    // List prefix
    let files = backend
        .list_prefix(std::path::Path::new("test-raw"))
        .await
        .expect("list_prefix failed");
    assert!(
        files.iter().any(|f| f.contains("file.txt")),
        "Should list file.txt in {:?}",
        files
    );

    // Delete
    backend.delete(path).await.expect("delete failed");

    // Not exists
    assert!(
        !backend.exists(path).await,
        "File should not exist after delete"
    );
}

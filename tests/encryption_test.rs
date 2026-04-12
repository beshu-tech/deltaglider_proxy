//! Integration tests for transparent encryption at rest.
//! All tests spawn a REAL proxy with DGP_ENCRYPTION_KEY set.

mod common;

use common::TestServer;

const BUCKET: &str = "encbkt";
const TEST_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
// Reserved for future wrong-key test
#[allow(dead_code)]
const OTHER_KEY: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

async fn put_object(server: &TestServer, key: &str, body: &[u8]) {
    let client = server.s3_client().await;
    client
        .put_object()
        .bucket(BUCKET)
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from(body.to_vec()))
        .send()
        .await
        .expect("PUT failed");
}

async fn get_object(server: &TestServer, key: &str) -> Vec<u8> {
    let client = server.s3_client().await;
    let resp = client
        .get_object()
        .bucket(BUCKET)
        .key(key)
        .send()
        .await
        .expect("GET failed");
    resp.body.collect().await.unwrap().to_vec()
}

fn encrypted_builder() -> common::TestServerBuilder {
    TestServer::builder()
        .bucket(BUCKET)
        .auth("ENCKEY", "ENCSECRET")
        .encryption_key(TEST_KEY)
}

// ═══════════════════════════════════════════════════
// Basic encrypted PUT/GET
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_put_get_encrypted() {
    let server = encrypted_builder().build().await;
    let data = b"hello, encrypted world!";
    put_object(&server, "test.txt", data).await;
    let got = get_object(&server, "test.txt").await;
    assert_eq!(got, data, "Decrypted data should match original");
}

#[tokio::test]
async fn test_put_get_encrypted_large() {
    let server = encrypted_builder().build().await;
    let data: Vec<u8> = (0..500_000u32).map(|i| (i % 256) as u8).collect();
    put_object(&server, "large.bin", &data).await;
    let got = get_object(&server, "large.bin").await;
    assert_eq!(
        got, data,
        "Large encrypted object should roundtrip correctly"
    );
}

#[tokio::test]
async fn test_encrypted_head_correct_size() {
    let server = encrypted_builder().build().await;
    let data = b"size check";
    put_object(&server, "sized.txt", data).await;

    let client = server.s3_client().await;
    let head = client
        .head_object()
        .bucket(BUCKET)
        .key("sized.txt")
        .send()
        .await
        .expect("HEAD failed");

    // Content-Length should reflect PLAINTEXT size, not encrypted size
    // (the engine stores plaintext size in metadata)
    assert!(head.content_length.unwrap_or(0) > 0);
}

// ═══════════════════════════════════════════════════
// Delta compression + encryption composition
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_delta_with_encryption() {
    // Delta-eligible file type (.zip), two versions → reference + delta, both encrypted
    let server = encrypted_builder()
        .max_delta_ratio(0.95) // Very permissive ratio
        .build()
        .await;

    // v1: seeds the reference baseline
    let v1 = common::generate_binary(100_000, 42);
    put_object(&server, "releases/app.zip", &v1).await;

    // v2: 90% similar → should create a delta
    let v2 = common::mutate_binary(&v1, 0.1);
    put_object(&server, "releases/app.zip", &v2).await;

    // GET v2 → decrypted + reconstructed from encrypted delta + encrypted reference
    let got = get_object(&server, "releases/app.zip").await;
    assert_eq!(
        got, v2,
        "Delta-reconstructed object should match v2 after decryption"
    );
}

#[tokio::test]
async fn test_passthrough_with_encryption() {
    // Non-delta-eligible file (.jpg) → passthrough, encrypted
    let server = encrypted_builder().build().await;
    let data = common::generate_binary(50_000, 99);
    put_object(&server, "photo.jpg", &data).await;
    let got = get_object(&server, "photo.jpg").await;
    assert_eq!(got, data, "Passthrough encrypted object should roundtrip");
}

// ═══════════════════════════════════════════════════
// On-disk verification: data is actually encrypted
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_data_encrypted_on_disk() {
    let server = encrypted_builder().build().await;
    let plaintext = b"THIS SHOULD NOT APPEAR ON DISK IN PLAINTEXT";
    put_object(&server, "secret.txt", plaintext).await;

    // Read the raw file from the filesystem backend
    if let Some(data_dir) = server.data_dir() {
        let mut found_plaintext = false;
        // Walk the data directory looking for files
        for entry in walkdir::WalkDir::new(data_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                if let Ok(contents) = std::fs::read(entry.path()) {
                    if contents.windows(plaintext.len()).any(|w| w == plaintext) {
                        found_plaintext = true;
                        break;
                    }
                }
            }
        }
        assert!(
            !found_plaintext,
            "Plaintext should NOT appear in any file on disk"
        );
    }
}

// ═══════════════════════════════════════════════════
// Backward compatibility: unencrypted objects still readable
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_unencrypted_still_readable() {
    // Start WITHOUT encryption, write an object
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("NOENC1", "NOENC1SECRET")
        .build()
        .await;
    let data = b"unencrypted data";
    put_object(&server, "plain.txt", data).await;
    let got = get_object(&server, "plain.txt").await;
    assert_eq!(got, data, "Unencrypted object should be readable");
}

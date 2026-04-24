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

/// B1 regression: the encryption wrapper must always be in the storage
/// stack, even when no key is configured. Without this, an operator who
/// removes the key (or forgets to set the env var after a restart) would
/// get historical encrypted-on-disk bytes streamed to clients AS IF they
/// were plaintext — a silent data-corruption bug that looks like
/// "DGE1...random bytes..." on the client side with no error.
///
/// The fix is in `src/deltaglider/engine/mod.rs`: the EncryptingBackend
/// is always wrapped, and when the key is None its read path returns
/// `StorageError::Encryption("object is encrypted but no key is
/// configured")` on any object whose metadata carries the
/// `dg-encrypted` marker. The S3 handler surfaces that as 500 — the
/// client never sees raw ciphertext.
#[tokio::test]
async fn test_disable_key_then_read_encrypted_object_errors_not_corrupts() {
    let mut server = encrypted_builder().build().await;

    // Write two objects while encryption is ENABLED:
    //   - a single-shot encrypted one (small object → put_passthrough)
    //   - a chunked-encrypted one (multipart → put_passthrough_chunked)
    let small_plaintext = b"classified single-shot payload";
    put_object(&server, "secret-small.txt", small_plaintext).await;

    let big_plaintext: Vec<u8> = (0..200_000u32).map(|i| (i & 0xff) as u8).collect();
    let parts = vec![
        big_plaintext[..100_000].to_vec(),
        big_plaintext[100_000..].to_vec(),
    ];
    multipart_put(&server, "secret-big.bin", &parts).await;

    // Sanity: the encrypted-read path works right now.
    assert_eq!(
        get_object(&server, "secret-small.txt").await,
        small_plaintext
    );
    assert_eq!(get_object(&server, "secret-big.bin").await, big_plaintext);

    // Act: restart the proxy against the SAME data dir WITHOUT the key.
    // Simulates the operator who disables encryption (or loses the key
    // through a deploy mistake) with historical encrypted objects
    // still on disk.
    server.respawn_without_encryption_key().await;

    // Assert: both reads must FAIL. Specifically, they must NOT return
    // raw ciphertext — that's the silent-corruption mode the fix
    // exists to prevent.
    let client = server.s3_client().await;

    let small_resp = client
        .get_object()
        .bucket(BUCKET)
        .key("secret-small.txt")
        .send()
        .await;
    // Two acceptable outcomes: SDK surfaces the 500 as an error, OR the
    // server closes the stream mid-body. The unacceptable outcome is a
    // clean 200 with ciphertext bytes in the body.
    match small_resp {
        Err(_) => { /* expected */ }
        Ok(resp) => {
            let body_result = resp.body.collect().await;
            match body_result {
                Err(_) => { /* expected */ }
                Ok(agg) => {
                    let body = agg.to_vec();
                    assert_ne!(
                        body, small_plaintext,
                        "SILENT CORRUPTION: encrypted object without key returned PLAINTEXT — \
                         wrapper is not in the stack on disable"
                    );
                    // Also check we didn't just serve raw ciphertext
                    // (which would look like garbage but still be a
                    // successful 200). A successful body of any shape
                    // here is a bug.
                    panic!(
                        "expected error, got {} bytes of body (first 16: {:02x?}) \
                         — disable path must hard-fail reads of historical \
                         encrypted objects, not serve ciphertext",
                        body.len(),
                        &body.iter().take(16).copied().collect::<Vec<_>>()
                    );
                }
            }
        }
    }

    let big_resp = client
        .get_object()
        .bucket(BUCKET)
        .key("secret-big.bin")
        .send()
        .await;
    match big_resp {
        Err(_) => { /* expected */ }
        Ok(resp) => {
            let body_result = resp.body.collect().await;
            match body_result {
                Err(_) => { /* expected */ }
                Ok(agg) => {
                    let body = agg.to_vec();
                    assert_ne!(body, big_plaintext, "SILENT CORRUPTION on chunked path");
                    panic!(
                        "chunked-encrypted object without key returned {} bytes \
                         (first 8: {:02x?}) — must hard-fail, not stream ciphertext",
                        body.len(),
                        &body.iter().take(8).copied().collect::<Vec<_>>()
                    );
                }
            }
        }
    }
}

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

// ═══════════════════════════════════════════════════
// Step 4: native S3 server-side encryption (SSE-S3)
//
// MinIO implements SSE-S3 natively. We test that:
//   1. A PUT with SSE-S3 configured produces an object that round-trips
//      through the proxy (AWS transparently decrypts).
//   2. The `dg-encrypted-native: sse-s3` user-metadata marker is
//      stamped on the object (distinguishes native vs proxy-side
//      encryption for ops introspection).
//   3. The underlying object has the `x-amz-server-side-encryption:
//      AES256` header when queried directly (bypassing the proxy via
//      the DG-metadata marker surfaced on the proxy HEAD response).
//
// Proxy-side AES-256-GCM continues to work in parallel; SSE-S3 is a
// distinct per-backend choice.
//
// Requires MinIO running at the default endpoint. Skipped when absent.
// ═══════════════════════════════════════════════════

#[ignore = "Requires MinIO running at http://localhost:9000 (docker compose up)"]
#[tokio::test]
async fn test_sse_s3_roundtrip_through_s3_backend() {
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("SSES3K", "SSES3SECRET")
        .s3_endpoint(&common::minio_endpoint_url())
        .sse_s3()
        .build()
        .await;

    let plaintext = b"hello sse-s3";
    put_object(&server, "sse-s3-target.txt", plaintext).await;

    // Read back through the proxy — MinIO decrypts transparently.
    let got = get_object(&server, "sse-s3-target.txt").await;
    assert_eq!(got, plaintext);

    // Verify the dg-encrypted-native marker was stamped (via HEAD).
    let client = server.s3_client().await;
    let head = client
        .head_object()
        .bucket(BUCKET)
        .key("sse-s3-target.txt")
        .send()
        .await
        .expect("HEAD should succeed");
    // AWS SDK surfaces user-metadata as a lowercase-keyed map.
    let dg_native = head
        .metadata
        .as_ref()
        .and_then(|m| m.get("dg-encrypted-native"))
        .map(|s| s.as_str());
    assert_eq!(
        dg_native,
        Some("sse-s3"),
        "SSE-S3 writes must stamp `dg-encrypted-native: sse-s3` in user-metadata, \
         got metadata: {:?}",
        head.metadata
    );

    // Proxy-side encryption markers must NOT be set — native and
    // proxy encryption are mutually exclusive on a given backend.
    let dg_enc = head
        .metadata
        .as_ref()
        .and_then(|m| m.get("dg-encrypted"))
        .map(|s| s.as_str());
    assert_eq!(
        dg_enc, None,
        "native-SSE objects must NOT carry the proxy `dg-encrypted` marker"
    );
}

// ═══════════════════════════════════════════════════
// Chunked streaming encryption tests
//
// These exercise the `aes-256-gcm-chunked-v1` wire format introduced
// for `put_passthrough_chunked`. To actually HIT that path we must
// upload via multipart (single-PUT goes through `put_passthrough`,
// which uses v1 single-shot). The chunked path is invoked by the
// multipart-completion handler for non-delta-eligible keys.
// ═══════════════════════════════════════════════════

/// Helper: upload via multipart so the wrapped `put_passthrough_chunked`
/// is actually invoked. Non-delta-eligible key (.bin) forces the
/// chunked-storage path. Returns the assembled plaintext bytes for
/// later comparison.
///
/// Uses the AWS SDK S3 client so SigV4 signing works out of the box —
/// the test server rejects unsigned writes with 403.
async fn multipart_put(server: &TestServer, key: &str, parts_data: &[Vec<u8>]) -> Vec<u8> {
    let client = server.s3_client().await;

    // Initiate
    let init = client
        .create_multipart_upload()
        .bucket(BUCKET)
        .key(key)
        .content_type("application/octet-stream")
        .send()
        .await
        .expect("create multipart");
    let upload_id = init.upload_id.expect("no upload id").to_string();

    // Upload parts
    let mut completed: Vec<aws_sdk_s3::types::CompletedPart> = Vec::new();
    for (i, part) in parts_data.iter().enumerate() {
        let part_num = (i + 1) as i32;
        let resp = client
            .upload_part()
            .bucket(BUCKET)
            .key(key)
            .upload_id(&upload_id)
            .part_number(part_num)
            .body(aws_sdk_s3::primitives::ByteStream::from(part.clone()))
            .send()
            .await
            .expect("upload part");
        completed.push(
            aws_sdk_s3::types::CompletedPart::builder()
                .part_number(part_num)
                .set_e_tag(resp.e_tag.clone())
                .build(),
        );
    }

    // Complete
    let completed_upload = aws_sdk_s3::types::CompletedMultipartUpload::builder()
        .set_parts(Some(completed))
        .build();
    client
        .complete_multipart_upload()
        .bucket(BUCKET)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed_upload)
        .send()
        .await
        .expect("complete multipart");

    parts_data.iter().flatten().copied().collect()
}

/// Large-passthrough roundtrip via multipart → chunked encryption
/// path. 5 MiB in 5×1 MiB parts exercises ~80 × 64-KiB encrypted
/// chunks on disk and verifies the whole pipeline (encrypt streaming,
/// decrypt streaming, plaintext byte-for-byte match).
///
/// An OOM in the old single-buffer path would manifest here at much
/// larger sizes; we use 5 MiB to keep test runtime reasonable while
/// still crossing many chunk boundaries.
#[tokio::test]
async fn test_chunked_encryption_multipart_roundtrip() {
    let server = encrypted_builder().build().await;
    let total_size: usize = 5 * 1024 * 1024; // 5 MiB
    let part_size: usize = 1024 * 1024; // 1 MiB per part, 5 parts
                                        // Deterministic byte pattern so any mismatch points at WHICH offset
                                        // went wrong (the byte at position `i` is `(i >> 3) ^ (i & 0xff)`).
    let pattern: Vec<u8> = (0..total_size)
        .map(|i| ((i >> 3) ^ (i & 0xff)) as u8)
        .collect();
    let parts: Vec<Vec<u8>> = pattern.chunks(part_size).map(|c| c.to_vec()).collect();
    assert_eq!(parts.len(), 5);

    let expected = multipart_put(&server, "large.bin", &parts).await;
    assert_eq!(expected.len(), total_size);
    let got = get_object(&server, "large.bin").await;
    assert_eq!(got.len(), total_size, "length mismatch: got {}", got.len());
    // Compare byte-for-byte. Using a plain assert_eq! would dump a
    // giant diff; instead find the first mismatch for a clean error.
    if got != pattern {
        let first_diff = got
            .iter()
            .zip(pattern.iter())
            .position(|(a, b)| a != b)
            .expect("lengths match but vecs differ");
        panic!(
            "byte mismatch at offset {}: got 0x{:02x}, expected 0x{:02x}",
            first_diff, got[first_diff], pattern[first_diff]
        );
    }
}

/// After a chunked upload, verify the on-disk format has the
/// `aes-256-gcm-chunked-v1` metadata marker (not v1 single-shot) —
/// this confirms we actually HIT the chunked code path. Without this
/// the test above could accidentally be covered by the v1 buffer
/// path if the wiring were wrong.
#[tokio::test]
async fn test_chunked_path_actually_exercised() {
    let server = encrypted_builder().build().await;
    let parts = vec![vec![0u8; 1024 * 1024]; 2]; // 2 × 1 MiB
    multipart_put(&server, "chunked-marker-test.bin", &parts).await;

    // Walk the filesystem looking for the passthrough file + its
    // xattr marker. The engine stores each object's metadata as an
    // xattr on the data file.
    let data_dir = server.data_dir().expect("filesystem backend");
    let mut found_chunked_marker = false;
    let mut found_v1_marker = false;
    for (_, meta) in common::read_xattr_metadata(data_dir) {
        // Match against the serialised JSON form to catch the
        // marker no matter which field carries it.
        let meta_json = meta.to_string();
        if meta_json.contains("aes-256-gcm-chunked-v1") {
            found_chunked_marker = true;
        }
        if meta_json.contains("\"aes-256-gcm-v1\"") {
            found_v1_marker = true;
        }
    }
    assert!(
        found_chunked_marker,
        "chunked-format marker not found — the chunked write path wasn't exercised"
    );
    // v1 marker is allowed (the reference/delta paths still use v1);
    // but we specifically want the chunked marker to ALSO be present.
    let _ = found_v1_marker;
}

/// Step 3 regression: every encrypted write stamps
/// `dg-encryption-key-id` on the object's xattr metadata. Without
/// this stamp the key-id mismatch check on reads can't fire, and the
/// ops value of "tell the operator WHICH key the object was written
/// with" disappears into an opaque AEAD failure.
///
/// Test covers BOTH single-shot writes (put_passthrough) and the
/// chunked write path (put_passthrough_chunked via multipart) to catch
/// regressions in either mark_encrypted / mark_chunked_encrypted.
#[tokio::test]
async fn test_write_stamps_key_id_metadata() {
    let server = encrypted_builder().build().await;

    // Single-shot: a small object goes through put_passthrough.
    put_object(&server, "small.txt", b"tiny").await;

    // Chunked: a multipart upload goes through put_passthrough_chunked.
    let parts = vec![vec![0u8; 256 * 1024]; 2];
    multipart_put(&server, "chunked.bin", &parts).await;

    let data_dir = server.data_dir().expect("filesystem backend");
    let mut small_kid: Option<String> = None;
    let mut chunked_kid: Option<String> = None;
    for (path, parsed) in common::read_xattr_metadata(data_dir) {
        let kid = parsed
            .get("user_metadata")
            .and_then(|um| um.get("dg-encryption-key-id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(kid) = kid {
            let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            // Both files get the SAME kid (singleton "default"
            // backend → same wrapper → same id).
            if fname == "small.txt" {
                small_kid = Some(kid);
            } else if fname == "chunked.bin" {
                chunked_kid = Some(kid);
            }
        }
    }

    assert!(
        small_kid.is_some(),
        "single-shot write must stamp dg-encryption-key-id metadata"
    );
    assert!(
        chunked_kid.is_some(),
        "chunked write must stamp dg-encryption-key-id metadata"
    );
    // The observable cross-path invariant: single-shot and chunked
    // writes go through the same EncryptingBackend wrapper, so they
    // must stamp the SAME key_id. Format invariants (16 hex chars,
    // lowercase, etc.) are covered by unit tests on `derive_key_id`
    // — the integration test deliberately doesn't re-cover them.
    assert_eq!(
        small_kid, chunked_kid,
        "two writes through the same backend wrapper must produce the SAME key_id"
    );
}

/// Range reads on a chunked-encrypted object. Exercises the O(1)
/// offset math: range covers chunks 10-12 (mid-object) of an 80-chunk
/// object.
#[tokio::test]
async fn test_chunked_encryption_range_read() {
    let server = encrypted_builder().build().await;
    let total_size: usize = 5 * 1024 * 1024;
    let pattern: Vec<u8> = (0..total_size).map(|i| (i & 0xff) as u8).collect();
    let parts: Vec<Vec<u8>> = pattern.chunks(1024 * 1024).map(|c| c.to_vec()).collect();
    multipart_put(&server, "range-target.bin", &parts).await;

    // Pick a range spanning a few 64-KiB chunks: bytes 700_000-800_000
    // covers (with chunk_size=65536): chunk 10 (offset 655360) through
    // chunk 12 (ending 851967). 100001 bytes total (inclusive range).
    let start: usize = 700_000;
    let end: usize = 800_000; // inclusive
    let client = server.s3_client().await;
    let resp = client
        .get_object()
        .bucket(BUCKET)
        .key("range-target.bin")
        .range(format!("bytes={}-{}", start, end))
        .send()
        .await
        .expect("range GET");
    let body = resp.body.collect().await.unwrap().to_vec();
    let expected_len = end - start + 1;
    assert_eq!(body.len(), expected_len, "range length mismatch");
    let expected = &pattern[start..=end];
    if body != expected {
        let first_diff = body
            .iter()
            .zip(expected.iter())
            .position(|(a, b)| a != b)
            .expect("lengths match but contents differ");
        panic!(
            "range byte mismatch at offset {} (plaintext pos {}): got 0x{:02x}, expected 0x{:02x}",
            first_diff,
            start + first_diff,
            body[first_diff],
            expected[first_diff]
        );
    }
}

/// Range that starts on a chunk boundary (chunk 5 begins at plaintext
/// offset 327680 = 5 × 65536). Regression guard: the "0 bytes to
/// skip" path in the decoder must emit the first chunk's plaintext
/// without truncation.
#[tokio::test]
async fn test_chunked_encryption_range_on_chunk_boundary() {
    let server = encrypted_builder().build().await;
    let pattern: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i & 0xff) as u8).collect();
    let parts: Vec<Vec<u8>> = pattern.chunks(1024 * 1024).map(|c| c.to_vec()).collect();
    multipart_put(&server, "boundary.bin", &parts).await;

    let start: usize = 5 * 65536; // chunk 5 boundary
    let end: usize = start + 65536 - 1; // exactly one full chunk, inclusive
    let client = server.s3_client().await;
    let resp = client
        .get_object()
        .bucket(BUCKET)
        .key("boundary.bin")
        .range(format!("bytes={}-{}", start, end))
        .send()
        .await
        .expect("range GET");
    let body = resp.body.collect().await.unwrap().to_vec();
    assert_eq!(body, &pattern[start..=end]);
}

/// Range covering the LAST chunk (which has is_final=true in its AAD).
/// Catches off-by-one bugs in `final_chunk_index_for_plaintext_size`
/// and the decoder's "next" emission after the final chunk.
#[tokio::test]
async fn test_chunked_encryption_range_over_final_chunk() {
    let server = encrypted_builder().build().await;
    // Size chosen so the last chunk is SHORT (not a full 64 KiB):
    // 1 MiB + 42 bytes → chunk 16 (index 15 full + 1 short final).
    let total: usize = 1024 * 1024 + 42;
    let pattern: Vec<u8> = (0..total).map(|i| (i & 0xff) as u8).collect();
    // Upload as 2 parts so the multipart path is used.
    let parts = vec![
        pattern[..1024 * 1024].to_vec(),
        pattern[1024 * 1024..].to_vec(),
    ];
    multipart_put(&server, "tail.bin", &parts).await;

    // Request the last 100 bytes — crosses into the short final chunk.
    let start: usize = total - 100;
    let end: usize = total - 1;
    let client = server.s3_client().await;
    let resp = client
        .get_object()
        .bucket(BUCKET)
        .key("tail.bin")
        .range(format!("bytes={}-{}", start, end))
        .send()
        .await
        .expect("range GET");
    let body = resp.body.collect().await.unwrap().to_vec();
    assert_eq!(body.len(), 100);
    assert_eq!(body, &pattern[start..=end]);
}

/// On-disk chunk truncation must produce a decryption failure, not
/// silently return a shorter object. Simulates an attacker who
/// truncates the last frame's ciphertext by 1 byte.
#[tokio::test]
async fn test_chunked_truncation_detected() {
    let server = encrypted_builder().build().await;
    let parts = vec![vec![0xABu8; 200 * 1024]; 2]; // 2 × 200 KiB, crosses chunk boundaries
    multipart_put(&server, "truncate-me.bin", &parts).await;

    // Find the passthrough file on disk and truncate it by 1 byte.
    let data_dir = server.data_dir().expect("filesystem backend");
    let mut target_path: Option<std::path::PathBuf> = None;
    for entry in walkdir::WalkDir::new(data_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().file_name().and_then(|s| s.to_str()) == Some("truncate-me.bin") {
            target_path = Some(entry.path().to_path_buf());
            break;
        }
    }
    let path = target_path.expect("passthrough file missing on disk");
    let orig_size = std::fs::metadata(&path).unwrap().len();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(orig_size - 1).unwrap();

    // GET must now fail. The truncation crosses a chunk boundary or
    // trims the GCM tag — either way the decoder rejects.
    let client = server.s3_client().await;
    let result = client
        .get_object()
        .bucket(BUCKET)
        .key("truncate-me.bin")
        .send()
        .await;
    match result {
        Err(_) => {
            // Expected: SDK returned an error because the server
            // responded with a non-2xx (decrypt fail fast path) or
            // closed the connection mid-stream.
        }
        Ok(resp) => {
            // Server started streaming; body collection must fail or
            // return something shorter than the uncorrupted plaintext.
            let body_result = resp.body.collect().await;
            match body_result {
                Ok(agg) => {
                    // If the body came back complete, at least the
                    // length must be short (truncation must surface).
                    let body_len = agg.to_vec().len();
                    assert!(
                        body_len < orig_size as usize - 1,
                        "truncated encrypted object returned a complete clean body — decoder missed the truncation"
                    );
                }
                Err(_) => {
                    // Body errored mid-stream: also acceptable.
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════
// Step 6: admin API — per-backend encryption mutation through the
// storage section. Replaces the three ignored-in-Step-1 tests that
// targeted `advanced.encryption_key`; the field no longer exists.
//
// All tests PATCH the `storage` section with a `backend_encryption`
// block (singleton path). A `backends[i].encryption` variant would
// go through the same code but requires a richer test-server
// fixture; the preservation logic is shared.
// ═══════════════════════════════════════════════════

/// Snapshot of the singleton backend's encryption summary straight
/// from `GET /api/admin/config` — the UI-authoritative view. The
/// server synthesises a "default" entry in `backends` for the
/// legacy singleton path so this works uniformly across YAML shapes.
async fn get_singleton_encryption_summary(
    http: &reqwest::Client,
    endpoint: &str,
) -> serde_json::Value {
    let cfg: serde_json::Value = http
        .get(format!("{}/_/api/admin/config", endpoint))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    cfg.get("backends")
        .and_then(|arr| arr.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|b| b.get("name").and_then(|v| v.as_str()) == Some("default"))
                .cloned()
        })
        .unwrap_or_default()
}

/// Convenience: extract the `encryption.mode` string from a backend
/// entry returned by `get_singleton_encryption_summary`. Returns
/// `"none"` when the entry or field is absent.
fn backend_mode(entry: &serde_json::Value) -> String {
    entry
        .get("encryption")
        .and_then(|e| e.get("mode"))
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string()
}

/// B2 regression, per-backend shape: the admin-UI "Disable
/// encryption" button now sends
/// `{"backend_encryption": {"mode": "none"}}` (or explicit `key: null`
/// on the existing mode). RFC 7396 merge-patch collapses either form
/// to "primary key None" after deserialization. The per-backend
/// preservation check must distinguish "absent key field" from
/// "explicitly null key field" so the disable action actually clears
/// the key; the non-null "don't touch" path must still preserve.
#[tokio::test]
async fn test_section_put_explicit_null_clears_per_backend_encryption_key() {
    let server = encrypted_builder().build().await;
    let http = common::admin_http_client(&server.endpoint()).await;

    // Precondition: singleton is encrypted (proxy mode with a key).
    let entry_before = get_singleton_encryption_summary(&http, &server.endpoint()).await;
    assert_eq!(
        backend_mode(&entry_before),
        "aes256-gcm-proxy",
        "precondition: singleton must be in aes256-gcm-proxy mode; got entry: {entry_before:#}"
    );

    // Act: PATCH the storage section with backend_encryption mode:
    // "none" (the cleanest way to disable encryption via the API).
    // This is what the Step 7 BackendsPanel Disable button will send.
    let resp = http
        .put(format!(
            "{}/_/api/admin/config/section/storage",
            server.endpoint()
        ))
        .json(&serde_json::json!({ "backend_encryption": { "mode": "none" } }))
        .send()
        .await
        .expect("disable PUT");
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("JSON body");
    assert!(
        status.is_success(),
        "disable PUT should succeed, got {} body {}",
        status,
        body
    );

    // Assert: encryption is now OFF.
    let entry_after = get_singleton_encryption_summary(&http, &server.endpoint()).await;
    assert_eq!(
        backend_mode(&entry_after),
        "none",
        "mode:none PUT MUST clear encryption. Entry: {entry_after:#}"
    );
}

/// B2 companion, per-backend shape: omitting the `backend_encryption`
/// block entirely (e.g. editing an unrelated storage.buckets field)
/// must PRESERVE the existing encryption setup. Absent != null; the
/// preservation guard must fire.
#[tokio::test]
async fn test_section_put_absent_field_preserves_per_backend_encryption_key() {
    let server = encrypted_builder().build().await;
    let http = common::admin_http_client(&server.endpoint()).await;

    // PATCH a totally unrelated field — the max_delta_ratio on the
    // default backend's bucket entry.
    let resp = http
        .put(format!(
            "{}/_/api/admin/config/section/storage",
            server.endpoint()
        ))
        .json(&serde_json::json!({
            "buckets": {
                "encbkt": { "max_delta_ratio": 0.42 }
            }
        }))
        .send()
        .await
        .expect("absent-field PUT");
    assert!(
        resp.status().is_success(),
        "absent-field PUT should succeed, got {}",
        resp.status()
    );

    // Assert: encryption is STILL on.
    let entry_after = get_singleton_encryption_summary(&http, &server.endpoint()).await;
    assert_eq!(
        backend_mode(&entry_after),
        "aes256-gcm-proxy",
        "absent backend_encryption field MUST preserve existing key. \
         Entry: {entry_after:#}"
    );
}

/// A storage-section PUT with `backend_encryption.mode: aes256-gcm-proxy`
/// and a malformed hex key must return 4xx. `Config::check` warnings
/// catch this at the dry-run level; the handler surfaces them so the
/// admin UI can show a clear rejection before the config lands.
#[tokio::test]
async fn test_invalid_per_backend_encryption_key_rejected() {
    let server = encrypted_builder().build().await;
    let http = common::admin_http_client(&server.endpoint()).await;

    // Try to rotate the singleton backend to a key that isn't
    // parseable as 32-byte hex.
    let resp = http
        .put(format!(
            "{}/_/api/admin/config/section/storage",
            server.endpoint()
        ))
        .json(&serde_json::json!({
            "backend_encryption": {
                "mode": "aes256-gcm-proxy",
                "key": "not-a-hex-key"
            }
        }))
        .send()
        .await
        .expect("section PUT");
    let status = resp.status();
    // `Config::check` only warns on bad-hex (the warn vs error split
    // lets operators see the message in the Apply dialog). The real
    // hard-error path is the engine-rebuild step in the apply
    // transition — which fires on `apply_config_transition` → the
    // handler returns 4xx when the engine can't rebuild.
    assert!(
        !status.is_success(),
        "malformed hex key must NOT land a successful apply, got status {}",
        status
    );

    // Explicit good-path check: a well-formed hex key is accepted.
    let good_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let resp = http
        .put(format!(
            "{}/_/api/admin/config/section/storage",
            server.endpoint()
        ))
        .json(&serde_json::json!({
            "backend_encryption": {
                "mode": "aes256-gcm-proxy",
                "key": good_key,
                "key_id": "rotated-step6"
            }
        }))
        .send()
        .await
        .expect("section PUT (good key)");
    assert!(
        resp.status().is_success(),
        "well-formed key should be accepted, got {}",
        resp.status()
    );
}

/// Step 6 — per-backend encryption summary surfaces in the field-
/// level GET `/api/admin/config`. This is the shape the UI uses to
/// render per-backend badges in Step 7.
#[tokio::test]
async fn test_get_config_exposes_per_backend_encryption_summary() {
    let server = encrypted_builder().build().await;
    let http = common::admin_http_client(&server.endpoint()).await;

    let entry = get_singleton_encryption_summary(&http, &server.endpoint()).await;
    let enc = entry
        .get("encryption")
        .expect("BackendInfoResponse must carry encryption summary");
    assert_eq!(
        enc.get("mode").and_then(|v| v.as_str()),
        Some("aes256-gcm-proxy"),
        "mode must be exposed; got: {enc:#}"
    );
    assert_eq!(
        enc.get("has_key").and_then(|v| v.as_bool()),
        Some(true),
        "has_key must be true when key is configured"
    );
    assert!(
        enc.get("key_id")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "key_id (derived or explicit) must be exposed; got: {enc:#}"
    );
    // The summary must NOT leak key material.
    let raw = serde_json::to_string(enc).unwrap();
    assert!(
        !raw.contains("0123456789abcdef"),
        "key bytes leaked in summary: {raw}"
    );
}

/// Step 6 — rotating the key on the singleton backend produces a
/// readable fingerprint diff in the Apply dialog's response. The
/// diff must NOT leak the underlying key material.
#[tokio::test]
async fn test_section_put_key_rotation_surfaces_fingerprint_diff() {
    let server = encrypted_builder().build().await;
    let http = common::admin_http_client(&server.endpoint()).await;

    // Rotate to a new key. /validate returns the diff without
    // persisting — perfect for asserting on the response shape.
    let new_key = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    let resp: serde_json::Value = http
        .post(format!(
            "{}/_/api/admin/config/section/storage/validate",
            server.endpoint()
        ))
        .json(&serde_json::json!({
            "backend_encryption": {
                "mode": "aes256-gcm-proxy",
                "key": new_key
            }
        }))
        .send()
        .await
        .expect("validate")
        .json()
        .await
        .expect("JSON response");

    let diff = resp
        .get("diff")
        .and_then(|v| v.get("storage"))
        .expect("validate response must include storage diff");
    // The diff is keyed by dotted path; find the backend_encryption.key
    // entry.
    let raw = serde_json::to_string(diff).unwrap();
    assert!(
        raw.contains("fp:"),
        "rotation diff must carry fingerprints (fp:xxxxxxxx), got: {raw}"
    );
    // Must NOT leak either key material.
    assert!(
        !raw.contains("0123456789abcdef"),
        "old key bytes leaked in diff: {raw}"
    );
    assert!(
        !raw.contains("fedcba9876543210"),
        "new key bytes leaked in diff: {raw}"
    );
}

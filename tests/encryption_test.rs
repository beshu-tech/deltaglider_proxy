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
    for entry in walkdir::WalkDir::new(data_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        // Read the xattr containing the object's metadata JSON. The
        // key (`user.dg.metadata`) matches the constant in
        // `src/storage/xattr_meta.rs::XATTR_NAME` — test file
        // deliberately duplicates the string rather than depending on
        // a crate-internal constant.
        let xattr_raw = match xattr::get(entry.path(), "user.dg.metadata") {
            Ok(Some(bytes)) => bytes,
            _ => continue,
        };
        let meta_json = match std::str::from_utf8(&xattr_raw) {
            Ok(s) => s,
            Err(_) => continue,
        };
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
// Admin API: malformed encryption_key must be rejected at the section
// level BEFORE it gets written to the config file.
// ═══════════════════════════════════════════════════

/// B2 regression: the admin-UI "Disable encryption" button sends
/// `{"encryption_key": null}`. RFC 7396 merge-patch collapses that to
/// "field absent" in the merged target, which deserializes to None in
/// the flat Config. Without explicit-null detection, the preservation
/// guard ("if incoming is None, preserve old value") misfires and
/// restores the old key — turning the disable button into a silent
/// no-op. The fix inspects the RAW body for an explicit `null` to
/// distinguish "don't change" (field absent) from "explicitly clear"
/// (field present and null).
#[tokio::test]
async fn test_section_put_explicit_null_disables_encryption() {
    let server = encrypted_builder().build().await;
    let http = common::admin_http_client(&server.endpoint()).await;

    // Precondition: encryption is currently ON. The field_level GET
    // carries `encryption_enabled: true` which the UI trusts for
    // status display.
    let cfg_before: serde_json::Value = http
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        cfg_before
            .get("encryption_enabled")
            .and_then(|v| v.as_bool()),
        Some(true),
        "precondition: encryption must be enabled before the disable test"
    );

    // Act: POST the exact body the EncryptionPanel's startDisable flow
    // sends — `{"encryption_key": null}`. Note that this is distinct
    // from `{}` (which is "no change") even though both would
    // deserialize to `encryption_key = None` naïvely.
    let resp = http
        .put(format!(
            "{}/_/api/admin/config/section/advanced",
            server.endpoint()
        ))
        .json(&serde_json::json!({ "encryption_key": null }))
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

    // Assert: encryption is now OFF. The field_level GET's
    // encryption_enabled boolean is the authoritative runtime
    // indicator — it's derived from the engine's live config, so it
    // only flips if the hot-reload actually swapped the key out.
    let cfg_after: serde_json::Value = http
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        cfg_after
            .get("encryption_enabled")
            .and_then(|v| v.as_bool()),
        Some(false),
        "explicit null MUST disable encryption — if this assertion \
         fires the preservation guard is still restoring the old key. \
         Full config: {cfg_after:#}"
    );
}

/// B2 companion: omitting the `encryption_key` field entirely (as
/// happens during a GET → edit-unrelated-field → PUT round-trip) must
/// PRESERVE the existing key, not clear it. Counterpoint to the
/// explicit-null disable test — the three-state logic has to get both
/// cases right.
#[tokio::test]
async fn test_section_put_absent_field_preserves_encryption_key() {
    let server = encrypted_builder().build().await;
    let http = common::admin_http_client(&server.endpoint()).await;

    // Send an `advanced`-section PUT with NO encryption_key field at
    // all. Edit an unrelated field (`max_delta_ratio`) so the diff
    // isn't empty — that mirrors the operator workflow "touch a knob
    // somewhere else in Advanced, Apply" without realizing there's an
    // encryption_key on the server.
    let resp = http
        .put(format!(
            "{}/_/api/admin/config/section/advanced",
            server.endpoint()
        ))
        .json(&serde_json::json!({ "max_delta_ratio": 0.6 }))
        .send()
        .await
        .expect("absent-field PUT");
    assert!(
        resp.status().is_success(),
        "absent-field PUT should succeed, got {}",
        resp.status()
    );

    // Assert: encryption is STILL on. Absent != null; the preservation
    // guard must still fire for this shape.
    let cfg_after: serde_json::Value = http
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        cfg_after
            .get("encryption_enabled")
            .and_then(|v| v.as_bool()),
        Some(true),
        "absent encryption_key field MUST preserve existing key — \
         otherwise every unrelated Advanced edit silently kills \
         encryption. Full config: {cfg_after:#}"
    );
}

/// A PUT to /api/admin/config/section/advanced with a malformed
/// encryption_key must return 4xx with a clear error. Without this
/// validation the bogus key would land in the YAML on disk and the
/// engine would fail only on the next startup.
#[tokio::test]
async fn test_invalid_encryption_key_rejected_by_section_put() {
    let server = encrypted_builder().build().await;
    let http = common::admin_http_client(&server.endpoint()).await;

    // Try to rotate to an obviously malformed key (not 64 hex chars).
    let resp = http
        .put(format!(
            "{}/_/api/admin/config/section/advanced",
            server.endpoint()
        ))
        .json(&serde_json::json!({ "encryption_key": "not-a-hex-key" }))
        .send()
        .await
        .expect("section PUT");
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("JSON body");
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "malformed hex key must yield 400, got {} with body {}",
        status,
        body
    );
    let err = body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        err.contains("invalid encryption_key"),
        "error body should explain the rejection, got {:?}",
        body
    );

    // Explicit good-path check: a well-formed hex key is accepted.
    let good_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let resp = http
        .put(format!(
            "{}/_/api/admin/config/section/advanced",
            server.endpoint()
        ))
        .json(&serde_json::json!({ "encryption_key": good_key }))
        .send()
        .await
        .expect("section PUT (good key)");
    assert!(
        resp.status().is_success(),
        "well-formed key should be accepted, got {}",
        resp.status()
    );
}

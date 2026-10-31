// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for transparent encryption at rest.
//! All tests spawn a REAL proxy with DGP_ENCRYPTION_KEY set.

use crate::common;

use common::TestServer;

const BUCKET: &str = "encbkt";
const TEST_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// A second key: an object written under `TEST_KEY` does not open with it.
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

/// The key lives in the YAML file, not in `DGP_ENCRYPTION_KEY`. Tests that
/// rotate the key through the admin API need this: an env-provided key keeps
/// winning at runtime, so a GUI rotation would never take effect.
fn yaml_keyed_builder() -> common::TestServerBuilder {
    TestServer::builder()
        .bucket(BUCKET)
        .auth("ENCKEY", "ENCSECRET")
        .extra_yaml_root(&format!(
            "backend_encryption:\n  mode: aes256-gcm-proxy\n  key: \"{TEST_KEY}\"\n"
        ))
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

    // Content-Length is the PLAINTEXT size (from the sealed facts), not
    // the stored size (DGE2 header + one GCM tag per frame).
    assert_eq!(head.content_length, Some(data.len() as i64));
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
/// stack, even when no key is configured. Without it, an operator who
/// removes the key (or forgets to set the env var after a restart) would
/// get historical encrypted-on-disk bytes streamed to clients AS IF they
/// were plaintext: "DGE2...random bytes..." with no error.
///
/// `src/deltaglider/engine/construction.rs` always wraps the raw backend.
/// With no key the wrapper's keyring is empty, so `seal::open` cannot find
/// the key that the object's `dg-sealed` fingerprint names and returns
/// `StorageError::Encryption` ("... was encrypted with a key that this
/// proxy does not have ..."). The S3 handler surfaces that as 400
/// InvalidRequest: the client never sees raw ciphertext.
#[tokio::test]
async fn test_disable_key_then_read_encrypted_object_errors_not_corrupts() {
    let mut server = encrypted_builder().build().await;

    // Write two objects while encryption is ENABLED:
    //   - a single PUT (put_passthrough)
    //   - a multipart upload (put_passthrough_chunked)
    let small_plaintext = b"classified single-put payload";
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
                        body.iter().take(16).copied().collect::<Vec<_>>()
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
                        body.iter().take(8).copied().collect::<Vec<_>>()
                    );
                }
            }
        }
    }

    // The objects that do not open still list (one of them must not fail
    // the listing of the folder), and a DELETE removes them: an object
    // that cannot be read can still be deleted.
    let listed = client
        .list_objects_v2()
        .bucket(BUCKET)
        .send()
        .await
        .expect("a listing survives objects whose seal does not open");
    let mut keys: Vec<String> = listed
        .contents()
        .iter()
        .filter_map(|o| o.key().map(str::to_string))
        .collect();
    keys.sort();
    assert_eq!(keys, ["secret-big.bin", "secret-small.txt"]);
    for key in &keys {
        client
            .delete_object()
            .bucket(BUCKET)
            .key(key)
            .send()
            .await
            .unwrap_or_else(|e| panic!("DELETE {key} of an object that does not open: {e:?}"));
    }
    let after = client
        .list_objects_v2()
        .bucket(BUCKET)
        .send()
        .await
        .unwrap();
    assert!(after.contents().is_empty(), "{:?}", after.contents());
}

/// Two proxies over one bucket (the report behind the key sentinel): one
/// encrypts it, the other has no key. The keyless proxy must not write the
/// bucket, because the encrypting proxy refuses every object that it does
/// not seal. A restart without the encryption block is that second proxy.
/// Its PUT answers 400 InvalidRequest that names the key fingerprint and an
/// object; nothing is written, and a DELETE still works.
async fn keyless_proxy_refuses_to_write(builder: common::TestServerBuilder, bucket: &str) {
    let mut server = builder.encryption_key(TEST_KEY).build().await;
    let http = server.http();
    let url = |key: &str| format!("{}/{bucket}/{key}", server.endpoint());
    let sealed = http
        .put(url("reports/sealed.html"))
        .body("written encrypted")
        .send()
        .await
        .unwrap();
    assert!(sealed.status().is_success(), "{}", sealed.status());
    // The second proxy: no encryption block at all (a proxy whose mode is
    // aes256-gcm-proxy without a key refuses every write anyway).
    let yaml = std::fs::read_to_string(server.config_path()).unwrap();
    let keyless = yaml.replace("backend_encryption:\n  mode: aes256-gcm-proxy\n", "");
    assert_ne!(yaml, keyless, "the generated config changed its shape");
    std::fs::write(server.config_path(), keyless).unwrap();
    server.respawn_without_encryption_key().await;

    let http = server.http();
    let url = |key: &str| format!("{}/{bucket}/{key}", server.endpoint());
    let put = http
        .put(url("reports/plain.html"))
        .body("would land unencrypted")
        .send()
        .await
        .unwrap();
    let status = put.status();
    let body = put.text().await.unwrap();
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("<Code>InvalidRequest</Code>"), "{body}");
    assert!(
        body.contains(common::key_fp_label(TEST_KEY).trim_start_matches("fp-")),
        "{body}"
    );
    assert!(
        !body.contains("reports/sealed.html"),
        "the client learns the key label only, not other objects: {body}"
    );

    let head = http.head(url("reports/plain.html")).send().await.unwrap();
    assert_eq!(head.status(), 404, "the refused PUT wrote nothing");
    let delete = http
        .delete(url("reports/sealed.html"))
        .send()
        .await
        .unwrap();
    assert!(
        delete.status().is_success(),
        "a DELETE is not refused: {}",
        delete.status()
    );
}

/// On S3 the restarted proxy finds the owner's key claim (and the `!!2`
/// listing token of the sealed object). The wrapper-level cases (sightings,
/// rotation, removal, refuse mode, cost) are in
/// `src/storage/encrypting/sentinel_tests.rs`.
#[tokio::test]
async fn a_proxy_without_the_key_refuses_to_write_an_encrypted_s3_bucket() {
    skip_unless_minio!();
    let bucket = common::unique_bucket("sentinel");
    let builder = TestServer::builder()
        .bucket(&bucket)
        .auth("ENCKEY", "ENCSECRET")
        .s3_endpoint(&common::minio_endpoint_url());
    keyless_proxy_refuses_to_write(builder, &bucket).await;
}

/// Two proxies RUNNING AT ONCE with separate configs over one S3 bucket:
/// the report's case (the owner encrypts, a second team's proxy has no key
/// or another key). `key` is the second proxy's key, `None` for plaintext.
async fn two_live_proxies(key: Option<&str>) -> (TestServer, TestServer, String) {
    let bucket = common::unique_bucket("two-proxies");
    let owner = TestServer::builder()
        .bucket(&bucket)
        .auth("OWNER", "OWNERSECRET")
        .s3_endpoint(&common::minio_endpoint_url())
        .encryption_key(TEST_KEY)
        .build()
        .await;
    let mut other = TestServer::builder()
        .bucket(&bucket)
        .auth("OTHER", "OTHERSECRET")
        .s3_endpoint(&common::minio_endpoint_url());
    if let Some(k) = key {
        other = other.encryption_key(k);
    }
    (owner, other.build().await, bucket)
}

async fn put_status(server: &TestServer, bucket: &str, key: &str) -> (u16, String) {
    let resp = server
        .http()
        .put(format!("{}/{bucket}/{key}", server.endpoint()))
        .body(format!("body of {key}"))
        .send()
        .await
        .unwrap();
    (resp.status().as_u16(), resp.text().await.unwrap())
}

/// The report: the second team's proxy has no key. Its PUT and its GET of
/// the owner's object both answer 400 that names the owner's key; the
/// owner never sees a plaintext object appear.
#[tokio::test]
async fn a_second_proxy_without_the_key_cannot_write_the_owners_bucket() {
    skip_unless_minio!();
    let (owner, other, bucket) = two_live_proxies(None).await;
    assert_eq!(put_status(&owner, &bucket, "integrity/a.html").await.0, 200);
    let fp = common::key_fp_label(TEST_KEY);

    let (status, body) = put_status(&other, &bucket, "integrity/b.html").await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(fp.trim_start_matches("fp-")), "{body}");

    let get = other
        .http()
        .get(format!("{}/{bucket}/integrity/a.html", other.endpoint()))
        .send()
        .await
        .unwrap();
    let get_status = get.status();
    let get_body = get.text().await.unwrap();
    assert_eq!(get_status, 400, "{get_body}");
    assert!(
        get_body.contains(fp.trim_start_matches("fp-")),
        "{get_body}"
    );

    let listed = owner
        .s3_client()
        .await
        .list_objects_v2()
        .bucket(&bucket)
        .send()
        .await
        .unwrap();
    let keys: Vec<_> = listed.contents().iter().filter_map(|o| o.key()).collect();
    assert_eq!(keys, ["integrity/a.html"]);
}

/// Two keys in one bucket: a second proxy with ANOTHER key is refused, and
/// so is the owner once the other key holds the claim. Writes from both at
/// the same moment on an empty bucket leave exactly one key.
#[tokio::test]
async fn two_proxies_with_different_keys_never_both_write_one_bucket() {
    skip_unless_minio!();
    let (owner, other, bucket) = two_live_proxies(Some(OTHER_KEY)).await;
    // Both write their first object at the same moment, on an empty bucket.
    let (a, b) = tokio::join!(
        put_status(&owner, &bucket, "first-owner.bin"),
        put_status(&other, &bucket, "first-other.bin")
    );
    assert!(
        (a.0 == 200) != (b.0 == 200),
        "exactly one key wins the bucket: owner {a:?}, other {b:?}"
    );
    let (winner, loser, winner_key) = if a.0 == 200 {
        (&owner, &other, TEST_KEY)
    } else {
        (&other, &owner, OTHER_KEY)
    };
    let fp = common::key_fp_label(winner_key);
    for key in ["later-1.bin", "integrity/later-2.bin"] {
        let (status, body) = put_status(loser, &bucket, key).await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains(fp.trim_start_matches("fp-")), "{body}");
    }
    assert_eq!(put_status(winner, &bucket, "more.bin").await.0, 200);

    // No client request reaches the claim, so none can unclaim the bucket.
    let claim = format!("{}/{bucket}/.dg/facts/!!key-claim", winner.endpoint());
    for resp in [
        winner.http().get(&claim).send().await.unwrap(),
        winner.http().delete(&claim).send().await.unwrap(),
    ] {
        assert_eq!(resp.status(), 400, "{:?}", resp.text().await);
    }

    // The claim is internal: it does not list, and it does not keep an
    // emptied bucket from being deleted.
    let client = winner.s3_client().await;
    let listed = client
        .list_objects_v2()
        .bucket(&bucket)
        .send()
        .await
        .unwrap();
    let mut keys: Vec<_> = listed
        .contents()
        .iter()
        .filter_map(|o| o.key().map(str::to_string))
        .collect();
    keys.sort();
    let first = if a.0 == 200 {
        "first-owner.bin"
    } else {
        "first-other.bin"
    };
    assert_eq!(keys, [first, "more.bin"]);
    for key in &keys {
        client
            .delete_object()
            .bucket(&bucket)
            .key(key)
            .send()
            .await
            .unwrap();
    }
    client
        .delete_bucket()
        .bucket(&bucket)
        .send()
        .await
        .expect("the key claim must not keep an empty bucket");
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

// MinIO serves SSE-S3 only with a KMS: CI and docker-compose.yml set a
// static `MINIO_KMS_SECRET_KEY`.
#[tokio::test]
async fn test_sse_s3_roundtrip_through_s3_backend() {
    skip_unless_minio!();
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

    // The proxy never shows its own `dg-*` metadata to clients, so read the
    // stored object from MinIO directly: it must carry the native marker,
    // MinIO's own SSE-S3 header, and no proxy-encryption marker.
    let minio = common::minio_client().await;
    let listed = minio
        .list_objects_v2()
        .bucket(BUCKET)
        .send()
        .await
        .expect("list the backend bucket");
    let stored_key = listed
        .contents()
        .iter()
        .filter_map(|o| o.key())
        .find(|k| k.ends_with("sse-s3-target.txt"))
        .expect("the object is stored in the backend bucket")
        .to_string();
    let head = minio
        .head_object()
        .bucket(BUCKET)
        .key(&stored_key)
        .send()
        .await
        .expect("HEAD on the backend should succeed");
    assert_eq!(
        head.server_side_encryption(),
        Some(&aws_sdk_s3::types::ServerSideEncryption::Aes256),
        "MinIO stored the object with SSE-S3"
    );
    let meta = |k: &str| head.metadata().and_then(|m| m.get(k)).map(|s| s.as_str());
    assert_eq!(
        meta("dg-encrypted-native"),
        Some("sse-s3"),
        "SSE-S3 writes must stamp `dg-encrypted-native: sse-s3`, got metadata: {:?}",
        head.metadata()
    );
    assert_eq!(
        meta("dg-encrypted"),
        None,
        "native-SSE objects must NOT carry the proxy `dg-encrypted` marker"
    );
}

// ═══════════════════════════════════════════════════
// Chunked streaming encryption tests
//
// Format v2 (issue #120) has ONE body format: a DGE2 header and 64 KiB
// GCM frames, for single PUTs and multipart uploads alike. These tests
// upload via multipart (the wrapped `put_passthrough_chunked`) and check
// the frame layout through range reads and truncation.
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
/// 5 MiB keeps the runtime short while it still crosses many frame
/// boundaries.
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

/// Every encrypted write seals its facts under the key (format v2, issue
/// #120): the stored metadata carries `dg-sealed`, whose fingerprint
/// names the key, and never the key id or the content hashes in clear.
/// Without the seal, a read could not find the key, and the operator
/// could not learn WHICH key wrote the object.
///
/// Covers a single PUT (put_passthrough) and a multipart upload
/// (put_passthrough_chunked), so that a regression in either path fails.
#[tokio::test]
async fn test_write_seals_under_the_key_fingerprint() {
    let server = encrypted_builder().build().await;

    // Single-shot: a small object goes through put_passthrough.
    put_object(&server, "small.txt", b"tiny").await;

    // Chunked: a multipart upload goes through put_passthrough_chunked.
    let parts = vec![vec![0u8; 256 * 1024]; 2];
    multipart_put(&server, "chunked.dat", &parts).await;

    let data_dir = server.data_dir().expect("filesystem backend");
    let want = common::key_fp_label(TEST_KEY);
    let mut seen = Vec::new();
    for (path, parsed) in common::read_xattr_metadata(data_dir) {
        let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if fname != "small.txt" && fname != "chunked.dat" {
            continue;
        }
        let um = &parsed["user_metadata"];
        assert_eq!(
            common::sealed_fp_label(um["dg-sealed"].as_str()).as_deref(),
            Some(want.as_str()),
            "{fname}: sealed under the configured key: {parsed}"
        );
        for clear in ["dg-encryption-key-id", "dg-md5", "dg-file-sha256"] {
            assert!(
                um.get(clear).is_none(),
                "{fname}: {clear} in clear: {parsed}"
            );
        }
        let sha = parsed["file_sha256"].as_str().unwrap_or_default();
        assert!(
            sha.starts_with("sealed:"),
            "{fname}: file_sha256 is an identity token, not a content hash: {parsed}"
        );
        seen.push(fname.to_string());
    }
    seen.sort();
    assert_eq!(seen, ["chunked.dat", "small.txt"], "both writes found");
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
    multipart_put(&server, "truncate-me.dat", &parts).await;

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
        if entry.path().file_name().and_then(|s| s.to_str()) == Some("truncate-me.dat") {
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
        .key("truncate-me.dat")
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
        // Disabling the backend's encryption is a downgrade for its buckets:
        // it needs the confirmation (issue #116 P3).
        .header("X-DGP-Confirm-Encryption-Downgrade", "backend:default")
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
    let server = yaml_keyed_builder().build().await;
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

/// Issue #116 P0.2: `key: null` on a backend that stays in proxy mode
/// never drops the key. The key moves to the legacy slot, and the result
/// (a proxy backend with no key) is refused, so the running key stays and
/// objects written under it stay readable.
#[tokio::test]
async fn test_section_put_null_key_in_proxy_mode_keeps_the_key() {
    let server = yaml_keyed_builder().build().await;
    put_object(&server, "before.txt", b"written under the key").await;
    let http = common::admin_http_client(&server.endpoint()).await;
    let resp = http
        .put(format!(
            "{}/_/api/admin/config/section/storage",
            server.endpoint()
        ))
        .json(&serde_json::json!({
            "backend_encryption": { "mode": "aes256-gcm-proxy", "key": null }
        }))
        .send()
        .await
        .expect("section PUT");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(
        status.is_client_error() && body.contains("no key"),
        "a keyless proxy backend must be refused, got {status} {body}"
    );
    let entry = get_singleton_encryption_summary(&http, &server.endpoint()).await;
    assert_eq!(
        entry["encryption"]["has_key"].as_bool(),
        Some(true),
        "the key must survive: {entry:#}"
    );
    assert_eq!(
        get_object(&server, "before.txt").await,
        b"written under the key"
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
    let server = yaml_keyed_builder().build().await;
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

/// A GET that cannot decrypt because the proxy does not have the key that
/// the object's seal fingerprint names answers 400 InvalidRequest that
/// names the key fingerprint (like S3 for an SSE-C object read without its
/// key: a retry cannot succeed), and logs the cause at ERROR (never the
/// key). It used to be a sanitised 500 InternalError, which SDKs retry and
/// which said nothing about the key.
#[tokio::test]
async fn a_key_mismatch_on_get_names_the_key_and_logs_its_cause() {
    let mut server = encrypted_builder().build().await;
    put_object(&server, "mismatch.bin", b"written under TEST_KEY").await;
    server
        .respawn_with_env(&[("DGP_ENCRYPTION_KEY", OTHER_KEY)])
        .await;

    let get = server
        .http()
        .get(format!("{}/{BUCKET}/mismatch.bin", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 400, "the GET must fail under the other key");
    let body = get.text().await.unwrap();
    assert!(body.contains("<Code>InvalidRequest</Code>"), "{body}");
    assert!(
        body.contains(common::key_fp_label(TEST_KEY).trim_start_matches("fp-")),
        "the client learns which key the object needs: {body}"
    );

    let admin = common::admin_http_client(&server.endpoint()).await;
    let logs: serde_json::Value = admin
        .get(format!(
            "{}/_/api/admin/logs?level=error&limit=200",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = logs.to_string();
    // Format v2 (#120): the object names its key by fingerprint only.
    let cause = "was encrypted with a key that this proxy does not have";
    assert!(
        text.contains(cause),
        "no ERROR line names the key mismatch: {text}"
    );
    let fp = common::key_fp_label(TEST_KEY);
    assert!(
        text.contains(fp.trim_start_matches("fp-")),
        "the ERROR line names the fingerprint of the object's key: {text}"
    );
    // One request, one ERROR line: the cause.
    let errors = logs["entries"].as_array().expect("a log list");
    assert_eq!(errors.len(), 1, "one ERROR line per refused GET: {text}");
    assert!(errors[0].to_string().contains(cause));
    assert!(
        !text.contains(TEST_KEY) && !text.contains(OTHER_KEY),
        "a key reached the log"
    );
}

/// Issue #120: a restart from a file that encrypts a bucket starts no
/// conversion (only an apply does). The boot check samples the bucket and
/// logs a warning that names a plaintext object, because every read of
/// it now fails.
#[tokio::test]
async fn boot_warns_about_plaintext_objects_in_a_bucket_that_refuses_them() {
    let mut server = encrypted_builder().build().await;
    put_object(&server, "sealed.bin", b"written encrypted").await;
    // A plaintext object that a backend writer (or an earlier, unencrypted
    // configuration) left in the bucket: no metadata, no seal.
    let dir = server
        .data_dir()
        .expect("filesystem backend")
        .join(BUCKET)
        .join("deltaspaces");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plain.txt"), b"plaintext history").unwrap();
    server
        .respawn_with_env(&[
            ("DGP_ENCRYPTION_KEY", TEST_KEY),
            ("DGP_BOOT_PLAINTEXT_CHECK", "true"),
        ])
        .await;

    let admin = common::admin_http_client(&server.endpoint()).await;
    let wanted = format!("bucket '{BUCKET}' encrypts with a proxy key, but holds objects");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let text = loop {
        let logs: serde_json::Value = admin
            .get(format!(
                "{}/_/api/admin/logs?level=warn&limit=500",
                server.endpoint()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let text = logs.to_string();
        if text.contains(&wanted) || std::time::Instant::now() > deadline {
            break text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert!(text.contains(&wanted), "no boot warning: {text}");
    assert!(
        text.contains("plain.txt"),
        "the warning names the object: {text}"
    );
    assert!(
        !text.contains("sealed.bin"),
        "a sealed object is not named: {text}"
    );
}

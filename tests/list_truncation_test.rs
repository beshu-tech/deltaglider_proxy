// SPDX-License-Identifier: GPL-3.0-only

//! ListObjectsV2 truncation regression tests
//!
//! Uploads ~65 files across deeply nested prefixes and verifies that listing
//! through the proxy never silently drops objects, regardless of pagination
//! settings or backend type.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use common::{generate_binary, mutate_binary, TestServer};
use std::collections::BTreeSet;

// ============================================================================
// Helpers
// ============================================================================

const VERSIONS: &[&str] = &["1.0.0", "1.1.0", "1.2.0"];
const PLATFORMS: &[&str] = &[
    "linux-x64",
    "linux-arm64",
    "windows-x64",
    "macos-x64",
    "macos-arm64",
];
const PLATFORM_FILES: &[&str] = &["app.zip", "lib.jar", "checksum.txt", "manifest.json"];

/// Upload a realistic build-artifact tree through the given S3 client.
/// Returns the set of all user-visible keys (relative to bucket root).
async fn upload_test_tree(client: &Client, bucket: &str, prefix: &str) -> BTreeSet<String> {
    let mut expected = BTreeSet::new();

    // Generate base binary data for delta-eligible files
    let base_zip = generate_binary(1024, 100);
    let base_jar = generate_binary(1024, 200);

    // Root-level files
    for (name, body) in [
        (
            format!("{prefix}/config.json"),
            b"{\"version\":\"test\"}".to_vec(),
        ),
        (
            format!("{prefix}/README.txt"),
            b"Test artifact tree".to_vec(),
        ),
    ] {
        client
            .put_object()
            .bucket(bucket)
            .key(&name)
            .body(ByteStream::from(body))
            .send()
            .await
            .unwrap_or_else(|e| panic!("PUT {name} failed: {e}"));
        expected.insert(name);
    }

    for (vi, version) in VERSIONS.iter().enumerate() {
        // release-notes.txt per version
        let rn_key = format!("{prefix}/build/{version}/release-notes.txt");
        client
            .put_object()
            .bucket(bucket)
            .key(&rn_key)
            .body(ByteStream::from(
                format!("Release notes for {version}").into_bytes(),
            ))
            .send()
            .await
            .unwrap_or_else(|e| panic!("PUT {rn_key} failed: {e}"));
        expected.insert(rn_key);

        for (pi, platform) in PLATFORMS.iter().enumerate() {
            for file in PLATFORM_FILES {
                let key = format!("{prefix}/build/{version}/{platform}/{file}");
                let body = match *file {
                    // Delta-eligible: mutate base so later versions produce deltas
                    "app.zip" => {
                        let ratio = (vi * PLATFORMS.len() + pi) as f64 * 0.005;
                        mutate_binary(&base_zip, ratio.min(0.15))
                    }
                    "lib.jar" => {
                        let ratio = (vi * PLATFORMS.len() + pi) as f64 * 0.005;
                        mutate_binary(&base_jar, ratio.min(0.15))
                    }
                    // Passthrough
                    _ => format!("{file} for {version}/{platform}").into_bytes(),
                };
                client
                    .put_object()
                    .bucket(bucket)
                    .key(&key)
                    .body(ByteStream::from(body))
                    .send()
                    .await
                    .unwrap_or_else(|e| panic!("PUT {key} failed: {e}"));
                expected.insert(key);
            }
        }
    }

    // Sanity-check the expected count
    // 3 versions × (5 platforms × 4 files + 1 release-notes) + 2 root = 65
    assert_eq!(expected.len(), 65, "Expected 65 user-visible files");
    expected
}

/// Paginate through all ListObjectsV2 pages and collect keys + common prefixes.
/// Returns (keys, common_prefixes).
async fn list_all_objects(
    client: &Client,
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    max_keys: i32,
) -> (Vec<String>, Vec<String>) {
    let mut all_keys = Vec::new();
    let mut all_prefixes = Vec::new();
    let mut continuation_token: Option<String> = None;

    loop {
        let mut req = client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .max_keys(max_keys);

        if let Some(d) = delimiter {
            req = req.delimiter(d);
        }
        if let Some(ref token) = continuation_token {
            req = req.continuation_token(token);
        }

        let resp = req
            .send()
            .await
            .unwrap_or_else(|e| panic!("ListObjectsV2 failed: {e}"));

        for obj in resp.contents() {
            if let Some(key) = obj.key() {
                all_keys.push(key.to_string());
            }
        }
        for cp in resp.common_prefixes() {
            if let Some(p) = cp.prefix() {
                all_prefixes.push(p.to_string());
            }
        }

        if resp.is_truncated() == Some(true) {
            continuation_token = resp.next_continuation_token().map(String::from);
            assert!(
                continuation_token.is_some(),
                "is_truncated=true but no continuation token"
            );
        } else {
            break;
        }
    }

    (all_keys, all_prefixes)
}

// ============================================================================
// Filesystem backend tests (always run — no Docker needed)
// ============================================================================

#[tokio::test]
async fn test_list_no_truncation_filesystem() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;
    let prefix = "trunc_fs";

    let expected = upload_test_tree(&client, server.bucket(), prefix).await;

    // Try multiple max_keys values to exercise pagination at different boundaries
    for max_keys in [5, 10, 20, 1000] {
        let (keys, _) = list_all_objects(
            &client,
            server.bucket(),
            &format!("{prefix}/"),
            None,
            max_keys,
        )
        .await;

        let key_set: BTreeSet<String> = keys.iter().cloned().collect();

        // No duplicates
        assert_eq!(
            keys.len(),
            key_set.len(),
            "max_keys={max_keys}: found duplicates across pages"
        );

        // Exact match with expected set
        assert_eq!(
            key_set, expected,
            "max_keys={max_keys}: key set mismatch.\n  Missing from listing: {:?}\n  Extra in listing: {:?}",
            expected.difference(&key_set).collect::<Vec<_>>(),
            key_set.difference(&expected).collect::<Vec<_>>(),
        );
    }
}

#[tokio::test]
async fn test_list_with_delimiter_filesystem() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;
    let prefix = "delim_fs";

    upload_test_tree(&client, server.bucket(), prefix).await;

    // List at root with delimiter — should see build/ prefix + 2 root files
    let (keys, prefixes) = list_all_objects(
        &client,
        server.bucket(),
        &format!("{prefix}/"),
        Some("/"),
        1000,
    )
    .await;

    let key_set: BTreeSet<String> = keys.into_iter().collect();
    let prefix_set: BTreeSet<String> = prefixes.into_iter().collect();

    assert!(
        key_set.contains(&format!("{prefix}/config.json")),
        "Should contain config.json, got keys: {key_set:?}"
    );
    assert!(
        key_set.contains(&format!("{prefix}/README.txt")),
        "Should contain README.txt, got keys: {key_set:?}"
    );
    assert_eq!(key_set.len(), 2, "Root should have exactly 2 files");
    assert!(
        prefix_set.contains(&format!("{prefix}/build/")),
        "Should contain build/ prefix, got: {prefix_set:?}"
    );

    // Drill into build/ — should see 3 version prefixes
    let (keys, prefixes) = list_all_objects(
        &client,
        server.bucket(),
        &format!("{prefix}/build/"),
        Some("/"),
        1000,
    )
    .await;

    let prefix_set: BTreeSet<String> = prefixes.into_iter().collect();
    assert!(
        keys.is_empty(),
        "build/ should have no direct files at this level, got: {keys:?}"
    );
    for version in VERSIONS {
        assert!(
            prefix_set.contains(&format!("{prefix}/build/{version}/")),
            "Missing version prefix {version}/, got: {prefix_set:?}"
        );
    }
    assert_eq!(
        prefix_set.len(),
        3,
        "Should have exactly 3 version prefixes"
    );

    // Drill into build/1.0.0/ — should see 5 platform prefixes + release-notes.txt
    let (keys, prefixes) = list_all_objects(
        &client,
        server.bucket(),
        &format!("{prefix}/build/1.0.0/"),
        Some("/"),
        1000,
    )
    .await;

    let key_set: BTreeSet<String> = keys.into_iter().collect();
    let prefix_set: BTreeSet<String> = prefixes.into_iter().collect();

    assert!(
        key_set.contains(&format!("{prefix}/build/1.0.0/release-notes.txt")),
        "Should contain release-notes.txt"
    );
    assert_eq!(key_set.len(), 1, "Should have 1 file at version level");
    assert_eq!(
        prefix_set.len(),
        5,
        "Should have 5 platform prefixes, got: {prefix_set:?}"
    );
    for platform in PLATFORMS {
        assert!(
            prefix_set.contains(&format!("{prefix}/build/1.0.0/{platform}/")),
            "Missing platform prefix {platform}/"
        );
    }
}

// ============================================================================
// S3 backend tests (require MinIO on localhost:9000)
// ============================================================================

#[tokio::test]
async fn test_list_no_truncation_s3() {
    skip_unless_minio!();

    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = format!("trunc_s3_{}", std::process::id());

    let expected = upload_test_tree(&client, server.bucket(), &prefix).await;

    for max_keys in [5, 10, 20, 1000] {
        let (keys, _) = list_all_objects(
            &client,
            server.bucket(),
            &format!("{prefix}/"),
            None,
            max_keys,
        )
        .await;

        let key_set: BTreeSet<String> = keys.iter().cloned().collect();

        assert_eq!(
            keys.len(),
            key_set.len(),
            "max_keys={max_keys}: found duplicates across pages"
        );

        assert_eq!(
            key_set,
            expected,
            "max_keys={max_keys}: key set mismatch.\n  Missing: {:?}\n  Extra: {:?}",
            expected.difference(&key_set).collect::<Vec<_>>(),
            key_set.difference(&expected).collect::<Vec<_>>(),
        );
    }
}

#[tokio::test]
async fn test_list_proxy_vs_direct_minio() {
    skip_unless_minio!();

    let server = TestServer::s3().await;
    let proxy_client = server.s3_client().await;
    let direct_client = common::minio_client().await;
    let prefix = format!("proxy_vs_direct_{}", std::process::id());

    let expected = upload_test_tree(&proxy_client, server.bucket(), &prefix).await;

    // List through proxy — should see exactly the 65 user-visible files
    let (proxy_keys, _) = list_all_objects(
        &proxy_client,
        server.bucket(),
        &format!("{prefix}/"),
        None,
        1000,
    )
    .await;
    let proxy_set: BTreeSet<String> = proxy_keys.into_iter().collect();
    assert_eq!(
        proxy_set, expected,
        "Proxy listing should match expected uploads"
    );

    // List directly from MinIO — should see MORE keys (internal .dg files)
    let (direct_keys, _) = list_all_objects(
        &direct_client,
        server.bucket(),
        &format!("{prefix}/"),
        None,
        1000,
    )
    .await;

    assert!(
        direct_keys.len() > expected.len(),
        "Direct MinIO should have more keys than proxy ({} vs {}), \
         proving internal files (reference.bin, .delta) exist and are filtered",
        direct_keys.len(),
        expected.len(),
    );

    // Verify internal files are present in direct listing (reference.bin and .delta files)
    let direct_set: BTreeSet<String> = direct_keys.into_iter().collect();
    let has_reference = direct_set.iter().any(|k| k.ends_with("/reference.bin"));
    let has_delta = direct_set.iter().any(|k| k.ends_with(".delta"));
    assert!(
        has_reference,
        "Direct MinIO listing should contain reference.bin files"
    );
    assert!(
        has_delta,
        "Direct MinIO listing should contain .delta files"
    );
}

#[tokio::test]
async fn test_list_with_delimiter_s3() {
    skip_unless_minio!();

    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = format!("delim_s3_{}", std::process::id());

    upload_test_tree(&client, server.bucket(), &prefix).await;

    // List at root with delimiter
    let (keys, prefixes) = list_all_objects(
        &client,
        server.bucket(),
        &format!("{prefix}/"),
        Some("/"),
        1000,
    )
    .await;

    let key_set: BTreeSet<String> = keys.into_iter().collect();
    let prefix_set: BTreeSet<String> = prefixes.into_iter().collect();

    assert_eq!(
        key_set.len(),
        2,
        "Root should have 2 files, got: {key_set:?}"
    );
    assert!(
        prefix_set.contains(&format!("{prefix}/build/")),
        "Should contain build/ prefix"
    );

    // Drill into build/
    let (keys, prefixes) = list_all_objects(
        &client,
        server.bucket(),
        &format!("{prefix}/build/"),
        Some("/"),
        1000,
    )
    .await;

    let prefix_set: BTreeSet<String> = prefixes.into_iter().collect();
    // On S3 backend the .dg/ internal directory must be filtered out
    assert!(
        keys.is_empty(),
        "build/ should have no direct files, got: {keys:?}"
    );
    for version in VERSIONS {
        assert!(
            prefix_set.contains(&format!("{prefix}/build/{version}/")),
            "Missing version prefix {version}/"
        );
    }
    assert_eq!(
        prefix_set.len(),
        3,
        "Should have exactly 3 version prefixes, got: {prefix_set:?}"
    );

    // Drill into build/1.0.0/
    let (keys, prefixes) = list_all_objects(
        &client,
        server.bucket(),
        &format!("{prefix}/build/1.0.0/"),
        Some("/"),
        1000,
    )
    .await;

    let key_set: BTreeSet<String> = keys.into_iter().collect();
    let prefix_set: BTreeSet<String> = prefixes.into_iter().collect();

    assert!(
        key_set.contains(&format!("{prefix}/build/1.0.0/release-notes.txt")),
        "Should contain release-notes.txt"
    );
    assert_eq!(key_set.len(), 1, "Should have 1 file at version level");
    assert_eq!(
        prefix_set.len(),
        5,
        "Should have 5 platform prefixes, got: {prefix_set:?}"
    );
}

#[tokio::test]
async fn test_list_small_pages_s3() {
    skip_unless_minio!();

    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = format!("smallpg_s3_{}", std::process::id());

    let expected = upload_test_tree(&client, server.bucket(), &prefix).await;

    // max_keys=3 forces many pagination rounds (~22 pages for 65 files)
    let (keys, _) =
        list_all_objects(&client, server.bucket(), &format!("{prefix}/"), None, 3).await;

    let key_set: BTreeSet<String> = keys.iter().cloned().collect();

    // No duplicates
    assert_eq!(
        keys.len(),
        key_set.len(),
        "Small pages: found {} duplicates",
        keys.len() - key_set.len()
    );

    // Exact match
    assert_eq!(
        key_set,
        expected,
        "Small pages: key set mismatch.\n  Missing: {:?}\n  Extra: {:?}",
        expected.difference(&key_set).collect::<Vec<_>>(),
        key_set.difference(&expected).collect::<Vec<_>>(),
    );
}

// ════════════════════════════════════════════════════════════════════
// ListObjects V1 paranoia tests
//
// `GET /<bucket>` WITHOUT `?list-type=2` dispatches to ListObjects V1.
// AWS SDKs default to V2, but legacy SDKs, hand-rolled SigV4 clients,
// and the AWS CLI's older paths still use V1. The s3s adapter
// shipped only V2; today's commit added a V1 shim. These tests pin
// the V1-specific behavior — marker/next-marker shape, encoding-type,
// empty/missing bucket, etc.
// ════════════════════════════════════════════════════════════════════

/// Helper: send a raw `GET /<bucket>` with no `list-type` query →
/// dispatches to V1.
async fn list_objects_v1_raw(endpoint: &str, bucket: &str, qs: Option<&str>) -> reqwest::Response {
    let qs = qs.map(|q| format!("?{}", q)).unwrap_or_default();
    let url = format!("{}/{}{}", endpoint, bucket, qs);
    reqwest::Client::new().get(&url).send().await.unwrap()
}

/// V1 against a unique empty PREFIX — must return 200 + empty
/// Contents under that prefix, `IsTruncated=false`. We use a
/// unique prefix (UUID-based) rather than relying on an empty
/// bucket, because the shared MinIO `deltaglider-test` bucket
/// is polluted by other tests running in parallel. The semantic
/// being pinned is the same: "no objects matching the request
/// produces an empty Contents-less response."
#[tokio::test]
async fn test_v1_list_objects_empty_prefix() {
    let server = TestServer::s3().await;
    let unique = format!(
        "v1-empty-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let resp = list_objects_v1_raw(
        &server.endpoint(),
        server.bucket(),
        Some(&format!("prefix={}/", unique)),
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "V1 list with unmatched prefix → 200"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<ListBucketResult"),
        "must include the V1 XML root"
    );
    assert!(
        body.contains("<IsTruncated>false</IsTruncated>"),
        "unmatched-prefix listing must mark IsTruncated=false; got {}",
        body
    );
    assert!(
        !body.contains("<Contents>"),
        "unmatched-prefix listing must have no Contents elements; got {}",
        body
    );
}

/// V1 against a non-existent bucket — must return 404 NoSuchBucket.
/// Catches the "shim didn't propagate NoSuchBucket" regression.
#[tokio::test]
async fn test_v1_list_objects_nonexistent_bucket_returns_404() {
    let server = TestServer::s3().await;
    let resp = list_objects_v1_raw(
        &server.endpoint(),
        "this-bucket-deliberately-does-not-exist-paranoid-test",
        None,
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        404,
        "V1 list of non-existent bucket must return 404, got {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("NoSuchBucket"),
        "404 body must cite NoSuchBucket, got: {}",
        body
    );
}

/// V1 with `max-keys=1` against a populated unique-prefix slice —
/// must return exactly 1 key + `IsTruncated=true` + a pageable
/// marker. The shared MinIO bucket is polluted by parallel tests,
/// so we scope to a unique prefix and use `prefix=` filter on both
/// pages to isolate from cross-test contamination.
#[tokio::test]
async fn test_v1_list_objects_paginates_via_marker() {
    let server = TestServer::s3().await;
    let s3 = server.s3_client().await;
    let prefix = format!(
        "v1-page-{}/",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    // Seed 3 keys WITHIN the unique prefix; alphabetical order.
    for k in ["aaa.bin", "bbb.bin", "ccc.bin"] {
        s3.put_object()
            .bucket(server.bucket())
            .key(format!("{}{}", prefix, k))
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"x"))
            .send()
            .await
            .unwrap();
    }

    // First page: max-keys=1 within our prefix.
    let resp = list_objects_v1_raw(
        &server.endpoint(),
        server.bucket(),
        Some(&format!("max-keys=1&prefix={}", prefix)),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<IsTruncated>true</IsTruncated>"),
        "page-1 must be truncated when max-keys=1 and N=3 under unique prefix"
    );

    // V1 NextMarker is OPTIONAL per the S3 spec. Handle both
    // present (s3s shim) and absent (axum, plain max-keys truncation).
    let next_marker: String = if let Some(start) = body.find("<NextMarker>") {
        let value_start = start + "<NextMarker>".len();
        let value_end = body[value_start..].find("</NextMarker>").unwrap();
        body[value_start..value_start + value_end].to_string()
    } else {
        let mut last = None;
        let mut search_from = 0;
        while let Some(idx) = body[search_from..].find("<Key>") {
            let key_start = search_from + idx + "<Key>".len();
            let key_end = body[key_start..].find("</Key>").unwrap();
            last = Some(body[key_start..key_start + key_end].to_string());
            search_from = key_start + key_end;
        }
        last.expect("page-1 must have at least one Contents entry to derive a marker")
    };

    // Second page: same prefix + the derived marker.
    let resp2 = list_objects_v1_raw(
        &server.endpoint(),
        server.bucket(),
        Some(&format!(
            "max-keys=1&prefix={}&marker={}",
            prefix, next_marker
        )),
    )
    .await;
    assert_eq!(resp2.status().as_u16(), 200);
    let body2 = resp2.text().await.unwrap();
    let first_key = format!("<Key>{}aaa.bin</Key>", prefix);
    assert!(
        !body2.contains(&first_key),
        "page-2 must skip the key from page-1; marker ignored?"
    );

    // Cleanup.
    for k in ["aaa.bin", "bbb.bin", "ccc.bin"] {
        s3.delete_object()
            .bucket(server.bucket())
            .key(format!("{}{}", prefix, k))
            .send()
            .await
            .ok();
    }
}

/// V1 with `max-keys=1500` (over the S3 limit of 1000) — server
/// must clamp to 1000.
#[tokio::test]
async fn test_v1_list_objects_max_keys_above_1000_is_clamped() {
    let server = TestServer::s3().await;
    let resp =
        list_objects_v1_raw(&server.endpoint(), server.bucket(), Some("max-keys=5000")).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    // Find MaxKeys element. Should be clamped to 1000 (S3 spec).
    assert!(
        body.contains("<MaxKeys>1000</MaxKeys>"),
        "max-keys=5000 must be clamped to 1000 in response, got: {}",
        body
    );
}

/// V1 with `delimiter=/` — must produce CommonPrefixes for the
/// directory shapes we created under a UNIQUE PREFIX (to isolate
/// from parallel-test pollution of the shared bucket).
#[tokio::test]
async fn test_v1_list_objects_with_delimiter_produces_common_prefixes() {
    let server = TestServer::s3().await;
    let s3 = server.s3_client().await;
    let prefix = format!(
        "v1-delim-{}/",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    // Seed nested keys WITHIN the unique prefix.
    for k in ["dir-a/x.bin", "dir-b/y.bin", "top.bin"] {
        s3.put_object()
            .bucket(server.bucket())
            .key(format!("{}{}", prefix, k))
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"x"))
            .send()
            .await
            .unwrap();
    }

    let resp = list_objects_v1_raw(
        &server.endpoint(),
        server.bucket(),
        Some(&format!("delimiter=%2F&prefix={}", prefix)),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<CommonPrefixes>"),
        "delimiter=/ must produce CommonPrefixes, got: {}",
        body
    );
    let dir_a_prefix = format!("{}dir-a/", prefix);
    assert!(
        body.contains(&dir_a_prefix),
        "CommonPrefixes must include {}, got: {}",
        dir_a_prefix,
        body
    );
    let top_key = format!("<Key>{}top.bin</Key>", prefix);
    assert!(
        body.contains(&top_key),
        "top-level objects must appear as Contents (not CommonPrefixes), got: {}",
        body
    );

    // Cleanup.
    for k in ["dir-a/x.bin", "dir-b/y.bin", "top.bin"] {
        s3.delete_object()
            .bucket(server.bucket())
            .key(format!("{}{}", prefix, k))
            .send()
            .await
            .ok();
    }
}

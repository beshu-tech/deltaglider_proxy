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

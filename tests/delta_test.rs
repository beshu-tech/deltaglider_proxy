//! Delta compression behavior tests
//!
//! Verifies delta compression through the S3 API using TestServer::filesystem().
//! Checks the `x-amz-storage-type` response header to verify storage decisions.

mod common;

use common::{generate_binary, mutate_binary, TestServer};

/// Helper to PUT via reqwest and return the x-amz-storage-type header
async fn put_and_get_storage_type(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
    content_type: &str,
) -> String {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client
        .put(&url)
        .header("content-type", content_type)
        .body(data)
        .send()
        .await
        .expect("PUT failed");
    assert!(
        resp.status().is_success(),
        "PUT {} failed: {}",
        key,
        resp.status()
    );
    resp.headers()
        .get("x-amz-storage-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// Helper to GET via reqwest and return bytes
async fn get_bytes(client: &reqwest::Client, endpoint: &str, bucket: &str, key: &str) -> Vec<u8> {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client.get(&url).send().await.expect("GET failed");
    assert!(
        resp.status().is_success(),
        "GET {} failed: {}",
        key,
        resp.status()
    );
    resp.bytes().await.unwrap().to_vec()
}

#[tokio::test]
async fn test_similar_files_stored_as_delta() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let base = generate_binary(100_000, 42);
    let variant = mutate_binary(&base, 0.01);

    let st1 = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "releases/base.zip",
        base,
        "application/zip",
    )
    .await;
    // First zip → reference (stored as delta with identity)
    assert!(
        st1 == "reference" || st1 == "delta",
        "First .zip should be reference or delta, got: {}",
        st1
    );

    let st2 = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "releases/v1.zip",
        variant,
        "application/zip",
    )
    .await;
    assert_eq!(st2, "delta", "Similar file should be stored as delta");
}

#[tokio::test]
async fn test_three_versions_all_retrievable() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let base = generate_binary(100_000, 42);
    let v1 = mutate_binary(&base, 0.01);
    let v2 = mutate_binary(&base, 0.02);

    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "ver/base.zip",
        base.clone(),
        "application/zip",
    )
    .await;
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "ver/v1.zip",
        v1.clone(),
        "application/zip",
    )
    .await;
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "ver/v2.zip",
        v2.clone(),
        "application/zip",
    )
    .await;

    assert_eq!(
        get_bytes(&http, &server.endpoint(), server.bucket(), "ver/base.zip").await,
        base
    );
    assert_eq!(
        get_bytes(&http, &server.endpoint(), server.bucket(), "ver/v1.zip").await,
        v1
    );
    assert_eq!(
        get_bytes(&http, &server.endpoint(), server.bucket(), "ver/v2.zip").await,
        v2
    );
}

#[tokio::test]
async fn test_txt_file_stored_passthrough() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "docs/readme.txt",
        b"This is a text file".to_vec(),
        "text/plain",
    )
    .await;

    assert_eq!(
        st, "passthrough",
        ".txt files should be stored as passthrough"
    );
}

#[tokio::test]
async fn test_mixed_types_same_prefix() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let zip_data = generate_binary(50_000, 100);

    let st_zip = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "mix/app.zip",
        zip_data,
        "application/zip",
    )
    .await;
    let st_txt = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "mix/readme.txt",
        b"readme".to_vec(),
        "text/plain",
    )
    .await;

    assert!(
        st_zip == "reference" || st_zip == "delta",
        "zip should be reference or delta"
    );
    assert_eq!(st_txt, "passthrough", "txt should be passthrough");
}

#[tokio::test]
async fn test_delete_last_delta_cleans_reference() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let data = generate_binary(50_000, 200);
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "clean/app.zip",
        data,
        "application/zip",
    )
    .await;

    // Delete the file
    let url = format!("{}/{}/clean/app.zip", server.endpoint(), server.bucket());
    let resp = http.delete(&url).send().await.unwrap();
    assert!(resp.status().is_success() || resp.status().as_u16() == 204);

    // PUT a new zip — should still work (new reference created)
    let new_data = generate_binary(50_000, 300);
    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "clean/v2.zip",
        new_data.clone(),
        "application/zip",
    )
    .await;
    assert!(
        st == "reference" || st == "delta",
        "New zip after cleanup should work"
    );

    assert_eq!(
        get_bytes(&http, &server.endpoint(), server.bucket(), "clean/v2.zip").await,
        new_data
    );
}

#[tokio::test]
async fn test_delete_one_of_many_deltas() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let base = generate_binary(50_000, 400);
    let v1 = mutate_binary(&base, 0.01);

    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "multi/base.zip",
        base.clone(),
        "application/zip",
    )
    .await;
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "multi/v1.zip",
        v1.clone(),
        "application/zip",
    )
    .await;

    // Delete base
    let url = format!("{}/{}/multi/base.zip", server.endpoint(), server.bucket());
    http.delete(&url).send().await.unwrap();

    // v1 should still be retrievable
    assert_eq!(
        get_bytes(&http, &server.endpoint(), server.bucket(), "multi/v1.zip").await,
        v1
    );
}

#[tokio::test]
async fn test_dissimilar_files_still_delta() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // First file creates reference
    let base = generate_binary(50_000, 500);
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "dissim/base.zip",
        base,
        "application/zip",
    )
    .await;

    // Completely different file — once reference exists, still stored as delta
    let different = generate_binary(50_000, 999);
    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "dissim/other.zip",
        different.clone(),
        "application/zip",
    )
    .await;
    assert_eq!(
        st, "delta",
        "Once ref exists, all zips are delta even if dissimilar"
    );

    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            "dissim/other.zip"
        )
        .await,
        different
    );
}

#[tokio::test]
async fn test_first_zip_creates_reference() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let data = generate_binary(50_000, 600);
    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "first/app.zip",
        data,
        "application/zip",
    )
    .await;

    // First zip in a deltaspace creates a reference baseline
    assert!(
        st == "reference" || st == "delta",
        "First zip should establish reference, got: {}",
        st
    );
}

// ============================================================================
// Listing & Pagination
// ============================================================================

/// Helper to make a raw ListObjectsV2 request and return the XML body
async fn list_objects_raw(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    params: &str,
) -> String {
    let url = format!("{}/{}?list-type=2&{}", endpoint, bucket, params);
    let resp = client.get(&url).send().await.unwrap();
    assert!(
        resp.status().is_success(),
        "ListObjects failed: {}",
        resp.status()
    );
    resp.text().await.unwrap()
}

#[tokio::test]
async fn test_list_objects_reports_original_sizes() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload a base zip (reference)
    let base = generate_binary(1024, 42);
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "sizes_test/base.zip",
        base.clone(),
        "application/zip",
    )
    .await;

    // Upload a similar variant (should be stored as delta, much smaller on disk)
    let variant = mutate_binary(&base, 0.01);
    let variant_len = variant.len();
    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "sizes_test/v1.zip",
        variant,
        "application/zip",
    )
    .await;
    assert_eq!(st, "delta", "Variant should be stored as delta");

    // List and check sizes
    let xml = list_objects_raw(
        &http,
        &server.endpoint(),
        server.bucket(),
        "prefix=sizes_test/",
    )
    .await;

    // Extract all <Size> values
    let sizes: Vec<u64> = xml
        .match_indices("<Size>")
        .map(|(start, _)| {
            let rest = &xml[start + 6..];
            let end = rest.find("</Size>").unwrap();
            rest[..end].parse::<u64>().unwrap()
        })
        .collect();

    assert_eq!(sizes.len(), 2, "Should list 2 objects, got: {:?}", sizes);
    // Both sizes should be the original file sizes, not delta sizes
    for size in &sizes {
        assert!(
            *size >= 1000,
            "Listed size {} should be original size (~1024), not delta size",
            size
        );
    }
    // The variant's listed size should match its original length
    // Find the size for v1.zip specifically
    let v1_pos = xml.find("<Key>sizes_test/v1.zip</Key>").unwrap();
    let size_after_v1 = &xml[v1_pos..];
    let size_start = size_after_v1.find("<Size>").unwrap() + 6;
    let size_end = size_after_v1[size_start..].find("</Size>").unwrap() + size_start;
    let v1_listed_size: u64 = size_after_v1[size_start..size_end].parse().unwrap();
    assert_eq!(
        v1_listed_size, variant_len as u64,
        "v1.zip listed size should be original size {}, got {}",
        variant_len, v1_listed_size
    );
}

#[tokio::test]
async fn test_list_objects_delimiter_common_prefixes() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload objects under different sub-prefixes
    for key in &[
        "delim/a/file1.zip",
        "delim/a/file2.zip",
        "delim/b/file1.zip",
    ] {
        put_and_get_storage_type(
            &http,
            &server.endpoint(),
            server.bucket(),
            key,
            generate_binary(1024, 42),
            "application/zip",
        )
        .await;
    }

    // List with delimiter — should collapse into CommonPrefixes
    let xml = list_objects_raw(
        &http,
        &server.endpoint(),
        server.bucket(),
        "prefix=delim/&delimiter=/",
    )
    .await;

    // Should have CommonPrefixes for delim/a/ and delim/b/
    assert!(
        xml.contains("<Prefix>delim/a/</Prefix>"),
        "Should contain CommonPrefix delim/a/, got:\n{}",
        xml
    );
    assert!(
        xml.contains("<Prefix>delim/b/</Prefix>"),
        "Should contain CommonPrefix delim/b/, got:\n{}",
        xml
    );

    // Should have no <Contents> since all objects are behind sub-prefixes
    assert!(
        !xml.contains("<Key>"),
        "Should have no direct <Key> entries with delimiter, got:\n{}",
        xml
    );
}

#[tokio::test]
async fn test_list_objects_pagination() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload 4 files
    for i in 1..=4 {
        put_and_get_storage_type(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("page_test/file{}.zip", i),
            generate_binary(1024, i as u64),
            "application/zip",
        )
        .await;
    }

    // First page: max-keys=2
    let xml1 = list_objects_raw(
        &http,
        &server.endpoint(),
        server.bucket(),
        "prefix=page_test/&max-keys=2",
    )
    .await;

    assert!(
        xml1.contains("<IsTruncated>true</IsTruncated>"),
        "First page should be truncated, got:\n{}",
        xml1
    );
    assert!(
        xml1.contains("<KeyCount>2</KeyCount>"),
        "First page should have KeyCount=2, got:\n{}",
        xml1
    );

    // Extract NextContinuationToken
    let token_start = xml1.find("<NextContinuationToken>").unwrap() + 23;
    let token_end = xml1[token_start..]
        .find("</NextContinuationToken>")
        .unwrap()
        + token_start;
    let token = &xml1[token_start..token_end];

    // Second page with continuation token
    let xml2 = list_objects_raw(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("prefix=page_test/&max-keys=2&continuation-token={}", token),
    )
    .await;

    assert!(
        xml2.contains("<IsTruncated>false</IsTruncated>"),
        "Second page should not be truncated, got:\n{}",
        xml2
    );
    assert!(
        xml2.contains("<KeyCount>2</KeyCount>"),
        "Second page should have KeyCount=2, got:\n{}",
        xml2
    );

    // Collect all keys across both pages
    let all_xml = format!("{}{}", xml1, xml2);
    let mut keys: Vec<&str> = Vec::new();
    let mut search_from = 0;
    while let Some(pos) = all_xml[search_from..].find("<Key>") {
        let abs_pos = search_from + pos + 5;
        let end = all_xml[abs_pos..].find("</Key>").unwrap() + abs_pos;
        keys.push(&all_xml[abs_pos..end]);
        search_from = end;
    }
    assert_eq!(
        keys.len(),
        4,
        "Should have 4 keys total across both pages: {:?}",
        keys
    );
}

#[tokio::test]
async fn test_first_file_bad_delta_ratio_passthrough() {
    // Use a very low max_delta_ratio so the identity delta (first file against itself)
    // exceeds the threshold and triggers the passthrough fallback
    let server = TestServer::filesystem_with_max_delta_ratio(0.001).await;
    let http = reqwest::Client::new();

    let data = generate_binary(1024, 99999);

    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "bad_ratio/random.zip",
        data.clone(),
        "application/zip",
    )
    .await;
    assert_eq!(
        st, "passthrough",
        "First file with delta ratio exceeding threshold should be passthrough, got: {}",
        st
    );

    // Verify the data round-trips correctly
    let retrieved = get_bytes(
        &http,
        &server.endpoint(),
        server.bucket(),
        "bad_ratio/random.zip",
    )
    .await;
    assert_eq!(
        retrieved, data,
        "Passthrough file should round-trip correctly"
    );
}

//! Delta compression behavior tests
//!
//! Verifies delta compression through the S3 API using TestServer::filesystem().
//! Checks the `x-amz-storage-type` response header to verify storage decisions.

mod common;

use common::{
    generate_binary, get_bytes, head_headers, list_objects_raw, mutate_binary,
    put_and_get_storage_type, put_object, TestServer,
};

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
async fn test_xdelta_does_not_recompress_magic_compressed_payloads() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let mut base = b"\xFD7zXZ\x00".to_vec();
    base.extend((0..512_000).map(|i| (i % 251) as u8));

    let mut variant = base.clone();
    variant.extend_from_slice(b"-variant-release");
    for i in (8192..variant.len()).step_by(4096) {
        variant[i] = variant[i].wrapping_add(7);
    }

    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "compressed-magic/base.zip",
        base.clone(),
        "application/zip",
    )
    .await;
    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "compressed-magic/v1.zip",
        variant.clone(),
        "application/zip",
    )
    .await;

    assert_eq!(st, "delta", "variant should exercise delta reconstruction");
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            "compressed-magic/v1.zip",
        )
        .await,
        variant,
        "delta reconstruction must preserve exact compressed bytes"
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

    // Sanity: reference.bin must exist on the filesystem backend after the
    // first PUT (the proxy creates it implicitly as the deltaspace baseline).
    let data_dir = server
        .data_dir()
        .expect("filesystem server must expose data_dir");
    let ref_path = data_dir
        .join(server.bucket())
        .join("deltaspaces")
        .join("clean")
        .join("reference.bin");
    assert!(
        ref_path.exists(),
        "reference.bin should exist after first PUT, looked at {:?}",
        ref_path,
    );

    // Delete the file
    let url = format!("{}/{}/clean/app.zip", server.endpoint(), server.bucket());
    let resp = http.delete(&url).send().await.unwrap();
    assert!(resp.status().is_success() || resp.status().as_u16() == 204);

    // The contract: deleting the LAST delta in a deltaspace MUST also
    // delete the reference.bin baseline. Otherwise we leak storage and,
    // worse, the next PUT under this prefix is forced to delta against a
    // stale reference (the prod 0.01% efficiency disaster on
    // s3://beshu/ror/builds/1.70.0-pre5/ — first upload was a Kibana
    // ZIP, every subsequent ES plugin deltaed against it for 0% savings,
    // and the orphan reference would have stayed had the last sibling
    // been deleted in between).
    assert!(
        !ref_path.exists(),
        "reference.bin must be removed after the last delta is deleted (storage leak), still found at {:?}",
        ref_path,
    );

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

/// Same contract, exercised through the **batch DeleteObjects** path
/// (POST `?delete` XML), which is what `aws s3 rm --recursive` and
/// the embedded UI's "delete selected" use. The cleanup loop in
/// `engine::delete()` runs once per object, so the LAST item in a
/// batch is what triggers the reference removal — verify it does.
#[tokio::test]
async fn test_batch_delete_cleans_reference_on_last_delta() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Two siblings under the same deltaspace.
    let base = generate_binary(50_000, 1234);
    let v1 = mutate_binary(&base, 0.01);
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "rmrf/base.zip",
        base,
        "application/zip",
    )
    .await;
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "rmrf/v1.zip",
        v1,
        "application/zip",
    )
    .await;

    let data_dir = server
        .data_dir()
        .expect("filesystem server must expose data_dir");
    let ref_path = data_dir
        .join(server.bucket())
        .join("deltaspaces")
        .join("rmrf")
        .join("reference.bin");
    assert!(
        ref_path.exists(),
        "reference.bin should exist after the two PUTs"
    );

    // Batch DeleteObjects of BOTH siblings in one call. After this, the
    // deltaspace has zero deltas → the reference must go.
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Delete>
  <Object><Key>rmrf/base.zip</Key></Object>
  <Object><Key>rmrf/v1.zip</Key></Object>
  <Quiet>true</Quiet>
</Delete>"#;
    let url = format!("{}/{}/?delete", server.endpoint(), server.bucket());
    // Content-MD5 is required for DeleteObjects in AWS, but our proxy
    // accepts the XML without it (validate_content_md5 only checks if
    // the header is present). Send without it for simplicity.
    let resp = http
        .post(&url)
        .header("Content-Type", "application/xml")
        .body(xml)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "DeleteObjects batch should succeed, got {}: {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    assert!(
        !ref_path.exists(),
        "reference.bin must be removed after batch delete empties the deltaspace, still found at {:?}",
        ref_path,
    );
}

/// Recursive prefix delete (DELETE `/{bucket}/{prefix}/`) is a
/// proxy-specific convenience; `aws s3 rm --recursive` actually issues
/// per-object DELETEs, but the embedded UI's "delete folder" calls
/// this. Same invariant: when the prefix is empty, no orphan
/// `reference.bin` should remain.
#[tokio::test]
async fn test_recursive_delete_cleans_reference() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let base = generate_binary(50_000, 9000);
    let v1 = mutate_binary(&base, 0.01);
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "purge/a.zip",
        base,
        "application/zip",
    )
    .await;
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "purge/b.zip",
        v1,
        "application/zip",
    )
    .await;

    let data_dir = server
        .data_dir()
        .expect("filesystem server must expose data_dir");
    let ref_path = data_dir
        .join(server.bucket())
        .join("deltaspaces")
        .join("purge")
        .join("reference.bin");
    assert!(
        ref_path.exists(),
        "reference.bin should exist after the two PUTs"
    );

    // Recursive delete of the prefix.
    let url = format!("{}/{}/purge/", server.endpoint(), server.bucket());
    let resp = http.delete(&url).send().await.unwrap();
    assert!(
        resp.status().is_success(),
        "recursive delete should succeed, got {}",
        resp.status()
    );

    assert!(
        !ref_path.exists(),
        "reference.bin must be removed after recursive prefix delete, still found at {:?}",
        ref_path,
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
/// S-P1-1 regression: when a dissimilar file produces a delta worse
/// than `max_delta_ratio`, it must be stored as PASSTHROUGH even when
/// a reference already exists in the deltaspace. Pre-fix the
/// threshold gate was `!has_existing_reference && ratio >= threshold`,
/// so once any file pinned the reference, every later file was forced
/// into delta storage regardless of cost. A dissimilar follow-up to
/// a tiny anchor produced a delta as big as the source file plus
/// xdelta3 framing — strictly worse than passthrough.
///
/// Post-fix the ratio gate fires unconditionally. The reference is
/// kept (other delta siblings might need it); only this single file
/// is stored passthrough.
async fn test_dissimilar_files_fall_back_to_passthrough() {
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

    // Completely different file — must NOT be forced into delta.
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
        st, "passthrough",
        "S-P1-1: dissimilar follow-up must store passthrough, not waste bytes on a useless delta"
    );

    // Bytes still recoverable.
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
    // Filesystem backend: LIST returns original file sizes (xattr has full metadata).
    // Both the reference and delta-compressed file report their original sizes.
    for size in &sizes {
        assert!(
            *size >= 1000,
            "Listed size {} should be original size (~1024)",
            size
        );
    }
    // HEAD also returns original size
    let headers = head_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        "sizes_test/v1.zip",
    )
    .await;
    let head_size: u64 = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert_eq!(
        head_size, variant_len as u64,
        "HEAD Content-Length should be original size {}, got {}",
        variant_len, head_size
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

// ============================================================================
// Range request on delta-reconstructed file
// ============================================================================

/// Range GET on a delta-stored file must return the correct byte slice
/// of the reconstructed content (not the raw delta bytes).
#[tokio::test]
async fn test_range_request_on_delta_file() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload base + variant to create a delta
    let base = generate_binary(100_000, 42);
    let variant = mutate_binary(&base, 0.01);

    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range_delta/base.zip",
        base,
        "application/zip",
    )
    .await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range_delta/v1.zip",
        variant.clone(),
        "application/zip",
    )
    .await;

    // Full GET to get expected content
    let full_body = get_bytes(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range_delta/v1.zip",
    )
    .await;
    assert_eq!(full_body.len(), variant.len(), "full GET size mismatch");

    // Range GET: first 100 bytes
    let url = format!(
        "{}/{}/range_delta/v1.zip",
        server.endpoint(),
        server.bucket()
    );
    let resp = http
        .get(&url)
        .header("Range", "bytes=0-99")
        .send()
        .await
        .expect("range GET failed");

    assert_eq!(
        resp.status().as_u16(),
        206,
        "expected 206 Partial Content, got {}",
        resp.status()
    );

    let content_range = resp
        .headers()
        .get("content-range")
        .expect("missing Content-Range header")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_range.starts_with("bytes 0-99/"),
        "Content-Range should start with 'bytes 0-99/', got: {}",
        content_range
    );

    let range_body = resp.bytes().await.unwrap();
    assert_eq!(range_body.len(), 100, "range body should be 100 bytes");
    assert_eq!(
        &range_body[..],
        &full_body[..100],
        "range bytes must match first 100 bytes of full GET"
    );

    // Range GET: last 50 bytes
    let resp = http
        .get(&url)
        .header("Range", "bytes=-50")
        .send()
        .await
        .expect("range GET (suffix) failed");
    assert_eq!(resp.status().as_u16(), 206);
    let range_body = resp.bytes().await.unwrap();
    assert_eq!(range_body.len(), 50);
    assert_eq!(
        &range_body[..],
        &full_body[full_body.len() - 50..],
        "suffix range bytes must match last 50 bytes of full GET"
    );
}

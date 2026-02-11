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
async fn test_txt_file_stored_direct() {
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

    assert_eq!(st, "direct", ".txt files should be stored directly");
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
    assert_eq!(st_txt, "direct", "txt should be direct");
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

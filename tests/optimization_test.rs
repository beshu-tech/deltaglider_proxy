//! Optimization verification tests
//!
//! Proves each optimization works correctly end-to-end through the real proxy:
//! piped codec, moka cache, DashMap prefix locks, zero-copy streams, Bytes boundaries,
//! body_to_utf8 zero-copy, itoa header formatting, and bounded codec concurrency.

mod common;

use common::{
    delete_object, generate_binary, get_bytes, head_headers, mutate_binary, put_object, TestServer,
};
use std::time::Duration;

// ─── C1: Large delta roundtrip (1 MB) ───

#[tokio::test]
async fn test_large_delta_roundtrip_1mb() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let base = generate_binary(1_000_000, 42);
    let variant = mutate_binary(&base, 0.05);

    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "large/base.zip",
        base.clone(),
        "application/zip",
    )
    .await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "large/v1.zip",
        variant.clone(),
        "application/zip",
    )
    .await;

    let got_base = get_bytes(&http, &server.endpoint(), server.bucket(), "large/base.zip").await;
    let got_v1 = get_bytes(&http, &server.endpoint(), server.bucket(), "large/v1.zip").await;

    assert_eq!(got_base, base, "Base roundtrip failed");
    assert_eq!(got_v1, variant, "Variant roundtrip failed");
}

// ─── C2: Twenty versions all correct ───

#[tokio::test]
async fn test_twenty_versions_all_correct() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let base = generate_binary(50_000, 100);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "versions/base.zip",
        base.clone(),
        "application/zip",
    )
    .await;

    let mut variants = vec![base.clone()];
    for i in 1..=20 {
        let v = mutate_binary(&base, 0.01 * i as f64 / 20.0);
        put_object(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("versions/v{}.zip", i),
            v.clone(),
            "application/zip",
        )
        .await;
        variants.push(v);
    }

    // GET all 21 back and verify
    let got_base = get_bytes(
        &http,
        &server.endpoint(),
        server.bucket(),
        "versions/base.zip",
    )
    .await;
    assert_eq!(got_base, variants[0], "Base data mismatch");

    for (i, expected) in variants.iter().enumerate().skip(1) {
        let got = get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("versions/v{}.zip", i),
        )
        .await;
        assert_eq!(&got, expected, "Version {} data mismatch", i);
    }
}

// ─── C3: Concurrent delta PUTs to same prefix ───

#[tokio::test]
async fn test_concurrent_delta_puts_same_prefix() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload a reference first
    let base = generate_binary(50_000, 42);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "conc_same/base.zip",
        base.clone(),
        "application/zip",
    )
    .await;

    // 10 concurrent PUTs to same prefix
    let mut handles = Vec::new();
    let mut expected = Vec::new();
    for i in 0..10 {
        let data = mutate_binary(&base, 0.02 + 0.01 * i as f64);
        expected.push(data.clone());
        let client = http.clone();
        let endpoint = server.endpoint();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            put_object(
                &client,
                &endpoint,
                &bucket,
                &format!("conc_same/file{}.zip", i),
                data,
                "application/zip",
            )
            .await;
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // Verify all 10
    for (i, exp) in expected.iter().enumerate() {
        let got = get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("conc_same/file{}.zip", i),
        )
        .await;
        assert_eq!(&got, exp, "Concurrent PUT {} data mismatch", i);
    }
}

// ─── C4: Concurrent delta PUTs to different prefixes ───

#[tokio::test]
async fn test_concurrent_delta_puts_different_prefixes() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let mut handles = Vec::new();
    let mut all_expected: Vec<(String, Vec<u8>)> = Vec::new();

    for prefix_idx in 0..10 {
        let base = generate_binary(30_000, prefix_idx as u64 * 100);
        let prefix = format!("diffpfx_{}", prefix_idx);

        // Upload base first (sequentially to establish references)
        put_object(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/base.zip", prefix),
            base.clone(),
            "application/zip",
        )
        .await;
        all_expected.push((format!("{}/base.zip", prefix), base.clone()));

        // Then spawn concurrent variant uploads
        for file_idx in 1..=2 {
            let data = mutate_binary(&base, 0.03);
            let key = format!("{}/v{}.zip", prefix, file_idx);
            all_expected.push((key.clone(), data.clone()));
            let client = http.clone();
            let endpoint = server.endpoint();
            let bucket = server.bucket().to_string();
            handles.push(tokio::spawn(async move {
                put_object(&client, &endpoint, &bucket, &key, data, "application/zip").await;
            }));
        }
    }

    for h in handles {
        h.await.unwrap();
    }

    // Verify all 30 objects
    for (key, expected) in &all_expected {
        let got = get_bytes(&http, &server.endpoint(), server.bucket(), key).await;
        assert_eq!(&got, expected, "Data mismatch for {}", key);
    }
}

// ─── C5: Concurrent GETs of same delta ───

#[tokio::test]
async fn test_concurrent_gets_same_delta() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let base = generate_binary(50_000, 42);
    let variant = mutate_binary(&base, 0.02);

    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "conc_get/base.zip",
        base,
        "application/zip",
    )
    .await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "conc_get/v1.zip",
        variant.clone(),
        "application/zip",
    )
    .await;

    // 20 concurrent GETs
    let mut handles = Vec::new();
    for _ in 0..20 {
        let client = http.clone();
        let endpoint = server.endpoint();
        let bucket = server.bucket().to_string();
        let expected = variant.clone();
        handles.push(tokio::spawn(async move {
            let got = get_bytes(&client, &endpoint, &bucket, "conc_get/v1.zip").await;
            assert_eq!(got, expected, "Concurrent GET returned wrong data");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

// ─── C6: Cache coherence — PUT then immediate GET ───

#[tokio::test]
async fn test_cache_coherence_put_then_get() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let base = generate_binary(50_000, 42);
    let variant = mutate_binary(&base, 0.01);

    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "coherence/base.zip",
        base,
        "application/zip",
    )
    .await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "coherence/v1.zip",
        variant.clone(),
        "application/zip",
    )
    .await;

    // Immediately GET — reference should be cached from PUT path
    let got = get_bytes(
        &http,
        &server.endpoint(),
        server.bucket(),
        "coherence/v1.zip",
    )
    .await;
    assert_eq!(
        got, variant,
        "Immediate GET after PUT should return correct data"
    );
}

// ─── C7: Cache invalidation after delete ───

#[tokio::test]
async fn test_cache_invalidation_after_delete() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // First generation
    let base1 = generate_binary(50_000, 42);
    let variant1 = mutate_binary(&base1, 0.02);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "inval/base.zip",
        base1,
        "application/zip",
    )
    .await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "inval/v1.zip",
        variant1,
        "application/zip",
    )
    .await;

    // Delete both
    delete_object(&http, &server.endpoint(), server.bucket(), "inval/v1.zip").await;
    delete_object(&http, &server.endpoint(), server.bucket(), "inval/base.zip").await;

    // Second generation — completely different data
    let base2 = generate_binary(50_000, 999);
    let variant2 = mutate_binary(&base2, 0.02);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "inval/base.zip",
        base2,
        "application/zip",
    )
    .await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "inval/v1.zip",
        variant2.clone(),
        "application/zip",
    )
    .await;

    // GET must return new data, not stale cached data
    let got = get_bytes(&http, &server.endpoint(), server.bucket(), "inval/v1.zip").await;
    assert_eq!(
        got, variant2,
        "After delete+recreate, GET must return new data"
    );
}

// ─── C8: Delete all and recreate same prefix ───

#[tokio::test]
async fn test_delete_all_recreate_same_prefix() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // First generation: 3 files
    let base1 = generate_binary(30_000, 1);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "recreate/a.zip",
        base1.clone(),
        "application/zip",
    )
    .await;
    let v1 = mutate_binary(&base1, 0.02);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "recreate/b.zip",
        v1,
        "application/zip",
    )
    .await;
    let v2 = mutate_binary(&base1, 0.04);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "recreate/c.zip",
        v2,
        "application/zip",
    )
    .await;

    // Delete all 3
    delete_object(&http, &server.endpoint(), server.bucket(), "recreate/a.zip").await;
    delete_object(&http, &server.endpoint(), server.bucket(), "recreate/b.zip").await;
    delete_object(&http, &server.endpoint(), server.bucket(), "recreate/c.zip").await;

    // Second generation: 3 new files with different data
    let base2 = generate_binary(30_000, 500);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "recreate/a.zip",
        base2.clone(),
        "application/zip",
    )
    .await;
    let v3 = mutate_binary(&base2, 0.02);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "recreate/b.zip",
        v3.clone(),
        "application/zip",
    )
    .await;
    let v4 = mutate_binary(&base2, 0.04);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "recreate/c.zip",
        v4.clone(),
        "application/zip",
    )
    .await;

    // GET all 3 new files
    assert_eq!(
        get_bytes(&http, &server.endpoint(), server.bucket(), "recreate/a.zip").await,
        base2
    );
    assert_eq!(
        get_bytes(&http, &server.endpoint(), server.bucket(), "recreate/b.zip").await,
        v3
    );
    assert_eq!(
        get_bytes(&http, &server.endpoint(), server.bucket(), "recreate/c.zip").await,
        v4
    );
}

// ─── C9: Multi-delete with large XML body ───

#[tokio::test]
async fn test_multi_delete_large_xml_body() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload 50 objects (use .txt for passthrough — faster, no delta encoding needed)
    let mut keys = Vec::new();
    for i in 0..50 {
        let key = format!("multidel/file_{:03}.txt", i);
        put_object(
            &http,
            &server.endpoint(),
            server.bucket(),
            &key,
            format!("data-{}", i).into_bytes(),
            "text/plain",
        )
        .await;
        keys.push(key);
    }

    // Build multi-delete XML
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Delete>");
    for key in &keys {
        xml.push_str(&format!("<Object><Key>{}</Key></Object>", key));
    }
    xml.push_str("</Delete>");

    // POST /{bucket}?delete
    let url = format!("{}/{}?delete", server.endpoint(), server.bucket());
    let resp = http
        .post(&url)
        .header("content-type", "application/xml")
        .body(xml)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "Multi-delete failed: {}",
        resp.status()
    );

    let response_body = resp.text().await.unwrap();
    // Response should mention deletions
    assert!(
        response_body.contains("<Deleted>") || response_body.contains("<DeleteResult"),
        "Response should contain delete results: {}",
        response_body
    );

    // Verify all deleted
    for key in &keys {
        let url = format!("{}/{}/{}", server.endpoint(), server.bucket(), key);
        let resp = http.get(&url).send().await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            404,
            "Object {} should be deleted",
            key
        );
    }
}

// ─── C10: Response headers numeric correctness ───

#[tokio::test]
async fn test_response_headers_numeric_correctness() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let sizes: Vec<usize> = vec![1, 999, 1000, 999_999, 1_048_576];

    for (i, &size) in sizes.iter().enumerate() {
        let data = vec![0x42u8; size];
        let key = format!("headers/file_{}.txt", i);
        put_object(
            &http,
            &server.endpoint(),
            server.bucket(),
            &key,
            data,
            "text/plain",
        )
        .await;
    }

    for (i, &size) in sizes.iter().enumerate() {
        let key = format!("headers/file_{}.txt", i);
        let headers = head_headers(&http, &server.endpoint(), server.bucket(), &key).await;

        let content_length = headers.get("content-length").unwrap().to_str().unwrap();
        // Verify it's a clean integer with no leading zeros, spaces, or trailing garbage
        assert_eq!(
            content_length,
            size.to_string(),
            "Content-Length mismatch for size {}",
            size
        );
        assert_eq!(
            content_length.trim(),
            content_length,
            "Content-Length has whitespace for size {}",
            size
        );
        assert!(
            content_length.parse::<u64>().is_ok(),
            "Content-Length is not a valid integer for size {}",
            size
        );

        // Check DG file size header
        let file_size = headers
            .get("x-amz-meta-dg-file-size")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            file_size,
            size.to_string(),
            "x-amz-meta-dg-file-size mismatch for size {}",
            size
        );
    }
}

// ─── C11: Large passthrough roundtrip (5 MB) ───

#[tokio::test]
async fn test_large_passthrough_roundtrip() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let size = 5 * 1024 * 1024; // 5MB
    let data = generate_binary(size, 42);

    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "bigfile/data.txt",
        data.clone(),
        "text/plain",
    )
    .await;

    let got = get_bytes(
        &http,
        &server.endpoint(),
        server.bucket(),
        "bigfile/data.txt",
    )
    .await;
    assert_eq!(
        got.len(),
        data.len(),
        "Length mismatch: got {} expected {}",
        got.len(),
        data.len()
    );
    assert_eq!(got, data, "5MB passthrough roundtrip failed");

    // Verify Content-Length header
    let headers = head_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        "bigfile/data.txt",
    )
    .await;
    let cl = headers.get("content-length").unwrap().to_str().unwrap();
    assert_eq!(cl, size.to_string());
}

// ─── C12: Codec concurrency = 1 (serialized) ───

#[tokio::test]
async fn test_codec_concurrency_one() {
    let server = TestServer::filesystem_with_codec_concurrency(1).await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

    let base = generate_binary(50_000, 42);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "serial/base.zip",
        base.clone(),
        "application/zip",
    )
    .await;

    // 5 concurrent delta PUTs with concurrency=1 — they queue behind the semaphore
    let mut handles = Vec::new();
    let mut expected = Vec::new();
    for i in 0..5 {
        let data = mutate_binary(&base, 0.02 + 0.01 * i as f64);
        expected.push(data.clone());
        let client = http.clone();
        let endpoint = server.endpoint();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            put_object(
                &client,
                &endpoint,
                &bucket,
                &format!("serial/v{}.zip", i),
                data,
                "application/zip",
            )
            .await;
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // Verify all
    for (i, exp) in expected.iter().enumerate() {
        let got = get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("serial/v{}.zip", i),
        )
        .await;
        assert_eq!(&got, exp, "Concurrency-1 file {} mismatch", i);
    }
}

// ─── C13: Special characters in keys ───

#[tokio::test]
async fn test_special_characters_in_keys() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let keys_and_data: Vec<(&str, Vec<u8>)> = vec![
        ("special/file-with-dashes.txt", b"dashes data".to_vec()),
        ("special/file_underscores.txt", b"underscore data".to_vec()),
        ("special/file.multiple.dots.txt", b"dots data".to_vec()),
        ("special/UPPERCASE.txt", b"upper data".to_vec()),
        ("special/MiXeD-CaSe_123.txt", b"mixed data".to_vec()),
        ("deep/nested/path/to/file.txt", b"deep data".to_vec()),
    ];

    for (key, data) in &keys_and_data {
        put_object(
            &http,
            &server.endpoint(),
            server.bucket(),
            key,
            data.clone(),
            "text/plain",
        )
        .await;
    }

    for (key, data) in &keys_and_data {
        let got = get_bytes(&http, &server.endpoint(), server.bucket(), key).await;
        assert_eq!(&got, data, "Key '{}' roundtrip failed", key);
    }

    // Multi-delete all of them
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Delete>");
    for (key, _) in &keys_and_data {
        xml.push_str(&format!("<Object><Key>{}</Key></Object>", key));
    }
    xml.push_str("</Delete>");

    let url = format!("{}/{}?delete", server.endpoint(), server.bucket());
    let resp = http
        .post(&url)
        .header("content-type", "application/xml")
        .body(xml)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "Multi-delete failed");

    // Verify all deleted
    for (key, _) in &keys_and_data {
        let url = format!("{}/{}/{}", server.endpoint(), server.bucket(), key);
        let resp = http.get(&url).send().await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            404,
            "Object {} should be deleted",
            key
        );
    }
}

// ─── C14: Hundred prefixes cache thrash ───

#[tokio::test]
async fn test_hundred_prefixes_cache_thrash() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload 1 file to each of 100 different prefixes
    let mut first_prefix_data = Vec::new();
    for i in 0..100 {
        let data = generate_binary(10_000, i as u64);
        let key = format!("thrash_{}/file.zip", i);
        put_object(
            &http,
            &server.endpoint(),
            server.bucket(),
            &key,
            data.clone(),
            "application/zip",
        )
        .await;
        if i == 0 {
            first_prefix_data = data;
        }
    }

    // GET from prefix #0 — verify correct despite cache pressure
    let got = get_bytes(
        &http,
        &server.endpoint(),
        server.bucket(),
        "thrash_0/file.zip",
    )
    .await;
    assert_eq!(
        got, first_prefix_data,
        "First prefix data should be correct after cache thrash"
    );
}

// ─── C15: Zero-byte object ───

#[tokio::test]
async fn test_zero_byte_object() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // PUT zero-byte .txt (passthrough)
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "empty/zero.txt",
        vec![],
        "text/plain",
    )
    .await;

    // GET — should return empty body
    let got = get_bytes(&http, &server.endpoint(), server.bucket(), "empty/zero.txt").await;
    assert!(
        got.is_empty(),
        "Zero-byte object should return empty body, got {} bytes",
        got.len()
    );

    // HEAD — Content-Length should be 0
    let headers = head_headers(&http, &server.endpoint(), server.bucket(), "empty/zero.txt").await;
    let cl = headers.get("content-length").unwrap().to_str().unwrap();
    assert_eq!(cl, "0", "Content-Length should be 0 for empty object");
}

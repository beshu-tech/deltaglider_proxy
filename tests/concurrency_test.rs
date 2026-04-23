//! Parallel access safety tests
//!
//! Verifies that concurrent operations don't cause corruption or panics.
//! Uses TestServer::filesystem() with multiple tokio tasks.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{generate_binary, TestServer};

#[tokio::test]
async fn test_parallel_puts_same_prefix() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let mut handles = Vec::new();
    for i in 0..10 {
        let c = client.clone();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            let data = format!("data-{}", i);
            c.put_object()
                .bucket(&bucket)
                .key(format!("concurrent/file{}.txt", i))
                .body(ByteStream::from(data.into_bytes()))
                .send()
                .await
                .expect("Concurrent PUT should succeed");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // Verify all stored
    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("concurrent/")
        .send()
        .await
        .unwrap();
    assert_eq!(list.contents().len(), 10, "All 10 objects should be stored");
}

#[tokio::test]
async fn test_parallel_put_and_get() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Pre-populate some objects
    for i in 0..5 {
        client
            .put_object()
            .bucket(server.bucket())
            .key(format!("rw/file{}.txt", i))
            .body(ByteStream::from(format!("initial-{}", i).into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let mut handles = Vec::new();

    // Concurrent writers
    for i in 5..10 {
        let c = client.clone();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            c.put_object()
                .bucket(&bucket)
                .key(format!("rw/file{}.txt", i))
                .body(ByteStream::from(format!("new-{}", i).into_bytes()))
                .send()
                .await
                .expect("Concurrent write should succeed");
        }));
    }

    // Concurrent readers
    for i in 0..5 {
        let c = client.clone();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            let result = c
                .get_object()
                .bucket(&bucket)
                .key(format!("rw/file{}.txt", i))
                .send()
                .await
                .expect("Concurrent read should succeed");
            let body = result.body.collect().await.unwrap().into_bytes();
            assert!(!body.is_empty(), "Body should not be empty");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

#[tokio::test]
async fn test_parallel_delete_and_get() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Pre-populate
    for i in 0..10 {
        client
            .put_object()
            .bucket(server.bucket())
            .key(format!("delget/file{}.txt", i))
            .body(ByteStream::from(format!("data-{}", i).into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let mut handles = Vec::new();

    // Delete even-numbered files
    for i in (0..10).step_by(2) {
        let c = client.clone();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            let _ = c
                .delete_object()
                .bucket(&bucket)
                .key(format!("delget/file{}.txt", i))
                .send()
                .await;
        }));
    }

    // Read odd-numbered files
    for i in (1..10).step_by(2) {
        let c = client.clone();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            // May or may not succeed depending on timing, but should not panic
            let _ = c
                .get_object()
                .bucket(&bucket)
                .key(format!("delget/file{}.txt", i))
                .send()
                .await;
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // No panics = success
}

#[tokio::test]
async fn test_parallel_puts_different_prefixes() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let mut handles = Vec::new();

    for prefix_idx in 0..5 {
        for file_idx in 0..4 {
            let c = client.clone();
            let bucket = server.bucket().to_string();
            let data = generate_binary(1000, (prefix_idx * 10 + file_idx) as u64);
            handles.push(tokio::spawn(async move {
                c.put_object()
                    .bucket(&bucket)
                    .key(format!("iso{}/file{}.txt", prefix_idx, file_idx))
                    .body(ByteStream::from(data))
                    .send()
                    .await
                    .expect("PUT should succeed");
            }));
        }
    }

    for h in handles {
        h.await.unwrap();
    }

    // Verify prefix isolation
    for prefix_idx in 0..5 {
        let list = client
            .list_objects_v2()
            .bucket(server.bucket())
            .prefix(format!("iso{}/", prefix_idx))
            .send()
            .await
            .unwrap();
        assert_eq!(
            list.contents().len(),
            4,
            "Prefix iso{}/ should have 4 objects",
            prefix_idx
        );
    }
}

// ─── QA finding #8: concurrency-edge tests ─────────────────────────────
//
// The 4 tests above cover the obvious cases (parallel PUTs across
// different keys, PUT/GET races on different keys). The three below
// exercise the HARDER invariants where a naïve implementation would
// silently corrupt state: same-key mutations, same-upload-ID completion
// races, and the read-after-delete-with-NEW-write sequence that
// produces split-brain if the metadata cache isn't invalidated
// correctly.

/// Double-Complete on the SAME multipart upload ID: only one must
/// succeed. The proxy guards this at the MultipartStore layer with a
/// `completed` boolean flipped under the uploads RwLock — but that
/// invariant needs to survive the full HTTP round-trip (handler →
/// extractor → store → XML response), which is what we're asserting
/// here. A regression where the flag were dropped would surface as
/// two 200 OKs (the second clobbers the first's assembled state).
#[tokio::test]
async fn test_concurrent_complete_same_upload_id_one_wins() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    // Step 1: initiate multipart
    let init = http
        .post(format!("{endpoint}/{bucket}/dup-complete.bin?uploads"))
        .send()
        .await
        .unwrap();
    assert!(init.status().is_success(), "initiate failed");
    let body = init.text().await.unwrap();
    let upload_id = body
        .split("<UploadId>")
        .nth(1)
        .unwrap()
        .split("</UploadId>")
        .next()
        .unwrap()
        .to_string();

    // Step 2: upload a single part
    let part_data = vec![0x5A; 4096];
    let put = http
        .put(format!(
            "{endpoint}/{bucket}/dup-complete.bin?partNumber=1&uploadId={upload_id}"
        ))
        .body(part_data)
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success(), "upload_part failed");
    let etag = put
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .expect("upload_part must return ETag")
        .to_string();

    // Step 3: fire two Complete requests at the same upload ID
    // concurrently. Only ONE must land a 200 OK; the other must
    // be rejected. What "rejected" looks like is documented by
    // `MultipartStore::complete`: the NoSuchUpload variant when the
    // upload is already marked completed. Over HTTP that surfaces
    // as 4xx, not 200.
    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag}</ETag></Part></CompleteMultipartUpload>"#
    );
    let url = format!("{endpoint}/{bucket}/dup-complete.bin?uploadId={upload_id}");

    let client_a = http.clone();
    let url_a = url.clone();
    let xml_a = complete_xml.clone();
    let client_b = http.clone();
    let url_b = url.clone();
    let xml_b = complete_xml.clone();

    let a = tokio::spawn(async move {
        client_a
            .post(&url_a)
            .header("content-type", "application/xml")
            .body(xml_a)
            .send()
            .await
    });
    let b = tokio::spawn(async move {
        client_b
            .post(&url_b)
            .header("content-type", "application/xml")
            .body(xml_b)
            .send()
            .await
    });

    let (ra, rb) = (a.await.unwrap().unwrap(), b.await.unwrap().unwrap());
    let statuses = [ra.status().as_u16(), rb.status().as_u16()];
    let successes = statuses.iter().filter(|s| **s < 400).count();
    let failures = statuses.iter().filter(|s| **s >= 400).count();

    // EXACTLY one success, EXACTLY one failure. Two successes would be
    // the silent-corruption regression; two failures would be a worse
    // regression where Complete is broken.
    assert_eq!(
        successes, 1,
        "expected exactly 1 Complete to succeed, got statuses {statuses:?}"
    );
    assert_eq!(
        failures, 1,
        "expected exactly 1 Complete to fail, got statuses {statuses:?}"
    );

    // And the object exists and is readable — whichever Complete won
    // actually produced a final object.
    let got = http
        .get(format!("{endpoint}/{bucket}/dup-complete.bin"))
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200, "final object must be readable");
    let bytes = got.bytes().await.unwrap();
    assert_eq!(
        bytes.len(),
        4096,
        "final object must be the assembled part (4096 bytes)"
    );
}

/// Two parallel multipart uploads for the SAME key with different
/// upload IDs. Both can legitimately complete — this is the S3
/// last-write-wins model. But the proxy's MultipartStore keys state
/// by upload_id (not by key), so cross-contamination between the two
/// streams would manifest as the "wrong" bytes landing for one of
/// them.
///
/// We seed the two uploads with deterministic, DIFFERENT payloads,
/// then complete them concurrently. Each Complete call must return
/// the bytes it uploaded — not the bytes the sibling uploaded.
#[tokio::test]
async fn test_concurrent_multipart_different_upload_ids_same_key_isolated() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();
    let key = "shared-key.bin";

    // Initiate two uploads.
    async fn init_upload(
        http: &reqwest::Client,
        endpoint: &str,
        bucket: &str,
        key: &str,
    ) -> String {
        let r = http
            .post(format!("{endpoint}/{bucket}/{key}?uploads"))
            .send()
            .await
            .unwrap();
        let body = r.text().await.unwrap();
        body.split("<UploadId>")
            .nth(1)
            .unwrap()
            .split("</UploadId>")
            .next()
            .unwrap()
            .to_string()
    }

    let upload_a = init_upload(&http, &endpoint, bucket, key).await;
    let upload_b = init_upload(&http, &endpoint, bucket, key).await;
    assert_ne!(upload_a, upload_b, "upload IDs must be distinct");

    // Distinguishable payloads — same size, different content.
    let payload_a = vec![0xAAu8; 5000];
    let payload_b = vec![0xBBu8; 5000];

    async fn upload_part(
        http: &reqwest::Client,
        endpoint: &str,
        bucket: &str,
        key: &str,
        upload_id: &str,
        data: Vec<u8>,
    ) -> String {
        let r = http
            .put(format!(
                "{endpoint}/{bucket}/{key}?partNumber=1&uploadId={upload_id}"
            ))
            .body(data)
            .send()
            .await
            .unwrap();
        r.headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .expect("etag")
            .to_string()
    }

    let etag_a = upload_part(&http, &endpoint, bucket, key, &upload_a, payload_a.clone()).await;
    let etag_b = upload_part(&http, &endpoint, bucket, key, &upload_b, payload_b.clone()).await;

    // Concurrently complete both. Both MUST succeed. Final bytes will
    // be one of the two — doesn't matter which, we're testing the
    // HANDLER not corrupting cross-upload state.
    let xml_a = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag_a}</ETag></Part></CompleteMultipartUpload>"#
    );
    let xml_b = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag_b}</ETag></Part></CompleteMultipartUpload>"#
    );

    let url_a = format!("{endpoint}/{bucket}/{key}?uploadId={upload_a}");
    let url_b = format!("{endpoint}/{bucket}/{key}?uploadId={upload_b}");
    let client_a = http.clone();
    let client_b = http.clone();
    let h_a = tokio::spawn(async move {
        client_a
            .post(&url_a)
            .header("content-type", "application/xml")
            .body(xml_a)
            .send()
            .await
    });
    let h_b = tokio::spawn(async move {
        client_b
            .post(&url_b)
            .header("content-type", "application/xml")
            .body(xml_b)
            .send()
            .await
    });
    let ra = h_a.await.unwrap().unwrap();
    let rb = h_b.await.unwrap().unwrap();
    assert_eq!(
        ra.status(),
        200,
        "upload A's Complete must succeed (cross-upload corruption guard)"
    );
    assert_eq!(
        rb.status(),
        200,
        "upload B's Complete must succeed (cross-upload corruption guard)"
    );

    // Final object: whichever completed last. The invariant we care
    // about is that it's EXACTLY one of the two payloads — not a
    // mix, not truncated, not byte-interleaved.
    let got = http
        .get(format!("{endpoint}/{bucket}/{key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let bytes = got.bytes().await.unwrap();
    assert!(
        bytes.as_ref() == payload_a.as_slice() || bytes.as_ref() == payload_b.as_slice(),
        "final object must equal one of the two payloads, not a mix — \
         got {} bytes with first byte 0x{:02X}",
        bytes.len(),
        bytes.first().copied().unwrap_or(0)
    );
}

/// PUT racing with DELETE on the same key. The invariant: after both
/// operations settle, HEAD and GET must agree. A split-brain (HEAD
/// 200 + GET 404, or HEAD 404 + GET 200) indicates a stale cache
/// entry surviving the delete — which is exactly the metadata-cache
/// invalidation bug class the QA audit flagged.
///
/// We don't care WHICH outcome we observe (object present or absent)
/// — the race doesn't have a deterministic winner. We care that HEAD
/// and GET agree on whichever outcome they saw.
#[tokio::test]
async fn test_concurrent_put_and_delete_leaves_consistent_state() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;
    let bucket = server.bucket().to_string();

    // Seed so DELETE has a real target.
    client
        .put_object()
        .bucket(&bucket)
        .key("race/key.bin")
        .body(ByteStream::from(b"seed".to_vec()))
        .send()
        .await
        .unwrap();

    // Race PUT and DELETE 20x to shake out any timing-window bug.
    // Each iteration also reads the state after and asserts
    // consistency — so a split-brain at ANY iteration fails fast.
    for iter in 0..20 {
        let c1 = client.clone();
        let c2 = client.clone();
        let b1 = bucket.clone();
        let b2 = bucket.clone();
        let put = tokio::spawn(async move {
            c1.put_object()
                .bucket(&b1)
                .key("race/key.bin")
                .body(ByteStream::from(format!("iter-{iter}").into_bytes()))
                .send()
                .await
        });
        let del = tokio::spawn(async move {
            c2.delete_object()
                .bucket(&b2)
                .key("race/key.bin")
                .send()
                .await
        });
        let _ = put.await.unwrap();
        let _ = del.await.unwrap();

        // HEAD and GET must agree. 404/404 or 200/200 — never 200/404.
        let head = client
            .head_object()
            .bucket(&bucket)
            .key("race/key.bin")
            .send()
            .await;
        let get = client
            .get_object()
            .bucket(&bucket)
            .key("race/key.bin")
            .send()
            .await;
        let head_present = head.is_ok();
        let get_present = get.is_ok();
        assert_eq!(
            head_present, get_present,
            "iter {iter}: HEAD/GET disagree — head_present={head_present}, \
             get_present={get_present} (metadata-cache invalidation bug)"
        );
    }
}

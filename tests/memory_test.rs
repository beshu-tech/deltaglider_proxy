//! Memory-bounded multipart upload integration test
//!
//! Verifies that the server process's peak RSS stays bounded during large
//! multipart uploads of non-delta-eligible files. The streaming pass-through
//! path (`store_direct_chunked`) avoids assembling all parts into a contiguous
//! buffer, keeping the memory spike proportional to a single part rather than
//! the entire object.
//!
//! Uses `peak_rss_bytes` from the `/health` endpoint (backed by
//! `getrusage(RUSAGE_SELF).ru_maxrss`) which captures even microsecond-lived
//! allocations — no polling or timing sensitivity.

mod common;

use common::{generate_binary, TestServer};
use sha2::{Digest, Sha256};

const MB: u64 = 1024 * 1024;

/// GET /health and extract `peak_rss_bytes` from JSON response
async fn get_peak_rss(client: &reqwest::Client, endpoint: &str) -> u64 {
    let url = format!("{}/health", endpoint);
    let resp = client.get(&url).send().await.expect("GET /health failed");
    assert!(resp.status().is_success(), "GET /health returned {}", resp.status());
    let body: serde_json::Value = resp.json().await.expect("Failed to parse /health JSON");
    body["peak_rss_bytes"]
        .as_u64()
        .expect("peak_rss_bytes missing or not u64")
}

/// Initiate a multipart upload, return upload_id
async fn create_multipart_upload(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> String {
    let url = format!("{}/{}/{}?uploads", endpoint, bucket, key);
    let resp = client
        .post(&url)
        .header("content-type", "application/octet-stream")
        .send()
        .await
        .expect("CreateMultipartUpload failed");
    assert!(
        resp.status().is_success(),
        "CreateMultipartUpload failed: {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    let start = body.find("<UploadId>").expect("No UploadId in response") + 10;
    let end = body[start..]
        .find("</UploadId>")
        .expect("No closing UploadId")
        + start;
    body[start..end].to_string()
}

/// Upload a part, return ETag
async fn upload_part(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    upload_id: &str,
    part_number: u32,
    data: Vec<u8>,
) -> String {
    let url = format!(
        "{}/{}/{}?partNumber={}&uploadId={}",
        endpoint, bucket, key, part_number, upload_id
    );
    let resp = client
        .put(&url)
        .body(data)
        .send()
        .await
        .expect("UploadPart failed");
    assert!(
        resp.status().is_success(),
        "UploadPart {} failed: {}",
        part_number,
        resp.status()
    );
    resp.headers()
        .get("etag")
        .expect("No ETag in UploadPart response")
        .to_str()
        .unwrap()
        .to_string()
}

/// Complete a multipart upload
async fn complete_multipart_upload(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    upload_id: &str,
    parts: &[(u32, &str)],
) -> reqwest::Response {
    let url = format!("{}/{}/{}?uploadId={}", endpoint, bucket, key, upload_id);
    let mut xml = String::from("<CompleteMultipartUpload>");
    for (num, etag) in parts {
        xml.push_str(&format!(
            "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
            num, etag
        ));
    }
    xml.push_str("</CompleteMultipartUpload>");

    client
        .post(&url)
        .header("content-type", "application/xml")
        .body(xml)
        .send()
        .await
        .expect("CompleteMultipartUpload failed")
}

/// GET an object and return the bytes
async fn get_bytes(client: &reqwest::Client, endpoint: &str, bucket: &str, key: &str) -> Vec<u8> {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client.get(&url).send().await.expect("GET failed");
    assert!(resp.status().is_success(), "GET {} failed: {}", key, resp.status());
    resp.bytes().await.unwrap().to_vec()
}

/// Verify that peak RSS does not spike excessively during a large multipart
/// upload of a non-delta-eligible `.bin` file.
///
/// Threshold rationale (35 MB above pre-complete baseline):
///   - Parts buffered in MultipartStore: 30 MB (unavoidable, present before complete)
///   - Old path: +30 MB contiguous BytesMut assembly -> ~60 MB spike
///   - New path: sequential chunk writes -> ~5 MB streaming overhead
///   - 35 MB threshold catches the old 60 MB spike while tolerating OS jitter
#[tokio::test]
async fn test_multipart_memory_bounded() {
    // 50 MB max object size to allow our 30 MB upload
    let server = TestServer::filesystem_with_max_object_size(50 * MB).await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();
    let key = "memory-test/large.bin"; // .bin is non-delta-eligible

    // Warm up: hit /health once to stabilize RSS after server startup
    let _ = get_peak_rss(&http, &endpoint).await;

    // Record peak RSS before the multipart upload (after parts are buffered
    // but before CompleteMultipartUpload triggers assembly)
    let part_size = 5 * MB as usize;
    let num_parts: u32 = 6;
    let total_size = part_size * num_parts as usize;

    // Generate all part data and compute expected SHA256
    let mut all_data = Vec::with_capacity(total_size);
    let mut part_data_vec: Vec<Vec<u8>> = Vec::new();
    for i in 0..num_parts {
        let data = generate_binary(part_size, 1000 + i as u64);
        all_data.extend_from_slice(&data);
        part_data_vec.push(data);
    }
    let expected_sha256 = hex::encode(Sha256::digest(&all_data));

    // Initiate multipart upload
    let upload_id = create_multipart_upload(&http, &endpoint, bucket, key).await;

    // Upload all parts
    let mut etags: Vec<String> = Vec::new();
    for (i, data) in part_data_vec.into_iter().enumerate() {
        let etag = upload_part(
            &http,
            &endpoint,
            bucket,
            key,
            &upload_id,
            (i + 1) as u32,
            data,
        )
        .await;
        etags.push(etag);
    }

    // Snapshot peak RSS before CompleteMultipartUpload
    // At this point all 30 MB of parts are buffered in the server's MultipartStore
    let peak_before = get_peak_rss(&http, &endpoint).await;
    eprintln!(
        "Peak RSS before CompleteMultipartUpload: {:.1} MB",
        peak_before as f64 / MB as f64
    );

    // Complete the multipart upload
    let parts_for_complete: Vec<(u32, &str)> = etags
        .iter()
        .enumerate()
        .map(|(i, etag)| ((i + 1) as u32, etag.as_str()))
        .collect();
    let resp = complete_multipart_upload(&http, &endpoint, bucket, key, &upload_id, &parts_for_complete).await;
    assert!(
        resp.status().is_success(),
        "CompleteMultipartUpload failed: {}",
        resp.status()
    );

    // Snapshot peak RSS after CompleteMultipartUpload
    let peak_after = get_peak_rss(&http, &endpoint).await;
    eprintln!(
        "Peak RSS after CompleteMultipartUpload: {:.1} MB",
        peak_after as f64 / MB as f64
    );

    let spike = peak_after.saturating_sub(peak_before);
    eprintln!(
        "RSS spike during complete: {:.1} MB",
        spike as f64 / MB as f64
    );

    // The streaming path should keep the spike well under 35 MB.
    // The old assembly path would spike ~30 MB (full contiguous copy).
    assert!(
        spike < 35 * MB,
        "Memory spike during CompleteMultipartUpload was {:.1} MB, expected < 35 MB. \
         This suggests parts are being assembled into a contiguous buffer.",
        spike as f64 / MB as f64
    );

    // Verify data integrity: GET the object back and check SHA256
    let retrieved = get_bytes(&http, &endpoint, bucket, key).await;
    let actual_sha256 = hex::encode(Sha256::digest(&retrieved));
    assert_eq!(
        actual_sha256, expected_sha256,
        "SHA256 mismatch: uploaded data does not match retrieved data"
    );
    assert_eq!(
        retrieved.len(),
        total_size,
        "Size mismatch: expected {} bytes, got {}",
        total_size,
        retrieved.len()
    );
}

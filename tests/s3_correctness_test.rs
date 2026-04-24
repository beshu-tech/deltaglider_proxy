//! Regression tests for the second-wave correctness findings
//! (H2 / M1 / M2 / M3 / M4 / L1). Each finding has its own
//! `test_*` function; see the CHANGELOG for the full context.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::TestServer;

// ────────────────────────────────────────────────────────────────────────
// H2 — DeleteBucket rejects when multipart uploads exist
// ────────────────────────────────────────────────────────────────────────

/// Pre-fix, DeleteBucket returned 204 while MultipartStore still held
/// uploads for that bucket. Now it must reject with a clear error.
#[tokio::test]
async fn test_delete_bucket_refuses_with_active_multipart_uploads() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Seed a bucket with no objects but an in-progress MPU.
    let bucket = "h2-bucket";
    client
        .create_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("create bucket");
    let create = client
        .create_multipart_upload()
        .bucket(bucket)
        .key("pending.bin")
        .send()
        .await
        .expect("initiate mpu");
    let upload_id = create.upload_id().unwrap().to_string();

    // DeleteBucket must now fail (409 BucketNotEmpty). The SDK-level
    // error message is opaque, so check via raw HTTP that the XML
    // body actually cites MPU.
    let del = client.delete_bucket().bucket(bucket).send().await;
    assert!(del.is_err(), "delete_bucket with active MPU must fail");

    let http = reqwest::Client::new();
    let raw = http
        .delete(format!("{}/{}", server.endpoint(), bucket))
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status().as_u16(), 409);
    let body = raw.text().await.unwrap();
    assert!(
        body.contains("BucketNotEmpty") && body.contains("multipart"),
        "expected BucketNotEmpty + multipart in body, got: {}",
        body
    );

    // Abort the upload and retry — now delete succeeds.
    client
        .abort_multipart_upload()
        .bucket(bucket)
        .key("pending.bin")
        .upload_id(&upload_id)
        .send()
        .await
        .expect("abort");
    client
        .delete_bucket()
        .bucket(bucket)
        .send()
        .await
        .expect("delete now ok");
}

// ────────────────────────────────────────────────────────────────────────
// M1 — UploadPart and UploadPartCopy check destination bucket existence
// ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_upload_part_to_deleted_bucket_returns_nosuchbucket() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Initiate on a real bucket, force-delete the bucket dir under the
    // proxy's feet, then UploadPart — which must 404 rather than
    // silently accepting bytes.
    let bucket = server.bucket();
    let create = client
        .create_multipart_upload()
        .bucket(bucket)
        .key("part.bin")
        .send()
        .await
        .expect("initiate");
    let upload_id = create.upload_id().unwrap().to_string();

    // Remove the bucket directory directly (simulating a racey admin).
    let data_dir = server.data_dir().expect("fs data dir");
    let _ = std::fs::remove_dir_all(data_dir.join(bucket));

    let part_body = vec![0u8; 5 * 1024 * 1024];
    let res = client
        .upload_part()
        .bucket(bucket)
        .key("part.bin")
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from(part_body))
        .send()
        .await;

    assert!(res.is_err(), "UploadPart must fail on missing bucket");
    let err_msg = format!("{:?}", res.unwrap_err());
    assert!(
        err_msg.contains("NoSuchBucket") || err_msg.contains("404"),
        "expected NoSuchBucket / 404, got {}",
        err_msg
    );
}

// ────────────────────────────────────────────────────────────────────────
// M2 — CopyObject honours x-amz-copy-source-if-* preconditions
// ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_copy_source_if_match_rejects_wrong_etag() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Seed source.
    client
        .put_object()
        .bucket(server.bucket())
        .key("src.bin")
        .body(ByteStream::from(b"hello world".to_vec()))
        .send()
        .await
        .unwrap();

    // Copy with an intentionally-wrong If-Match.
    let res = client
        .copy_object()
        .bucket(server.bucket())
        .key("dst.bin")
        .copy_source(format!("{}/{}", server.bucket(), "src.bin"))
        .copy_source_if_match("\"definitely-not-the-real-etag\"")
        .send()
        .await;
    assert!(res.is_err(), "CopyObject must honor If-Match");
    let msg = format!("{:?}", res.unwrap_err());
    assert!(
        msg.contains("PreconditionFailed") || msg.contains("412"),
        "expected 412, got {}",
        msg
    );
}

#[tokio::test]
async fn test_copy_source_if_none_match_star_rejects() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("src.bin")
        .body(ByteStream::from(b"x".to_vec()))
        .send()
        .await
        .unwrap();

    let res = client
        .copy_object()
        .bucket(server.bucket())
        .key("dst.bin")
        .copy_source(format!("{}/{}", server.bucket(), "src.bin"))
        .copy_source_if_none_match("*")
        .send()
        .await;
    assert!(
        res.is_err(),
        "CopyObject with If-None-Match: * must fail when source exists"
    );
}

// ────────────────────────────────────────────────────────────────────────
// M3 — invalid x-amz-metadata-directive is rejected, not silently COPY'd
// ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_invalid_metadata_directive_rejected() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Seed source.
    let put_url = format!("{}/{}/src.bin", server.endpoint(), server.bucket());
    http.put(&put_url)
        .body(b"payload".to_vec())
        .send()
        .await
        .unwrap();

    // Copy with an invalid directive via raw HTTP (SDK enforces enum on client side).
    let copy_url = format!("{}/{}/dst.bin", server.endpoint(), server.bucket());
    let resp = http
        .put(&copy_url)
        .header(
            "x-amz-copy-source",
            format!("/{}/src.bin", server.bucket()),
        )
        .header("x-amz-metadata-directive", "REPLAC") // typo
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "invalid metadata-directive must 400"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("InvalidArgument") && body.contains("REPLAC"),
        "response should cite InvalidArgument + the bad value, got: {}",
        body
    );
}

// ────────────────────────────────────────────────────────────────────────
// M4 — tagging stubs return 501 NotImplemented (not fake 200 OK)
// ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_object_tagging_get_returns_501() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Seed the object first so the handler reaches the tagging branch.
    let put_url = format!("{}/{}/t.bin", server.endpoint(), server.bucket());
    http.put(&put_url)
        .body(b"x".to_vec())
        .send()
        .await
        .unwrap();

    let get_url = format!("{}/{}/t.bin?tagging", server.endpoint(), server.bucket());
    let resp = http.get(&get_url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 501);
    let body = resp.text().await.unwrap();
    assert!(body.contains("NotImplemented"), "{}", body);
}

#[tokio::test]
async fn test_object_tagging_put_returns_501() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();
    let put_url = format!("{}/{}/u.bin", server.endpoint(), server.bucket());
    http.put(&put_url)
        .body(b"x".to_vec())
        .send()
        .await
        .unwrap();

    let tag_url = format!("{}/{}/u.bin?tagging", server.endpoint(), server.bucket());
    let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<Tagging><TagSet><Tag><Key>k</Key><Value>v</Value></Tag></TagSet></Tagging>"#;
    let resp = http.put(&tag_url).body(body).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 501);
}

#[tokio::test]
async fn test_bucket_tagging_returns_501() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let url = format!("{}/{}?tagging", server.endpoint(), server.bucket());
    let resp = http.get(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 501);
}

// ────────────────────────────────────────────────────────────────────────
// L1 — ListParts and ListMultipartUploads honour pagination params
// ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_list_parts_honours_max_parts_and_marker() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Initiate multipart + upload 4 parts.
    let bucket = server.bucket();
    let create = client
        .create_multipart_upload()
        .bucket(bucket)
        .key("multi.bin")
        .send()
        .await
        .unwrap();
    let upload_id = create.upload_id().unwrap().to_string();

    // Each part must be >= 5 MiB except the last (S3 multipart rules).
    for n in 1..=4u32 {
        let body = vec![n as u8; 5 * 1024 * 1024];
        client
            .upload_part()
            .bucket(bucket)
            .key("multi.bin")
            .upload_id(&upload_id)
            .part_number(n as i32)
            .body(ByteStream::from(body))
            .send()
            .await
            .unwrap();
    }

    // Request a page of 2.
    let page1 = client
        .list_parts()
        .bucket(bucket)
        .key("multi.bin")
        .upload_id(&upload_id)
        .max_parts(2)
        .send()
        .await
        .unwrap();
    assert!(page1.is_truncated().unwrap_or(false), "page 1 truncated");
    assert_eq!(page1.parts().len(), 2);
    assert_eq!(page1.max_parts(), Some(2));
    let next = page1
        .next_part_number_marker()
        .expect("next marker")
        .to_string();
    assert_eq!(next.parse::<u32>().unwrap(), 2);

    // Request page 2 with that marker.
    let page2 = client
        .list_parts()
        .bucket(bucket)
        .key("multi.bin")
        .upload_id(&upload_id)
        .max_parts(2)
        .part_number_marker(&next)
        .send()
        .await
        .unwrap();
    assert!(!page2.is_truncated().unwrap_or(true), "page 2 complete");
    assert_eq!(page2.parts().len(), 2);
    let nums: Vec<i32> = page2
        .parts()
        .iter()
        .filter_map(|p| p.part_number())
        .collect();
    assert_eq!(nums, vec![3, 4]);

    // Cleanup
    client
        .abort_multipart_upload()
        .bucket(bucket)
        .key("multi.bin")
        .upload_id(&upload_id)
        .send()
        .await
        .ok();
}

#[tokio::test]
async fn test_list_multipart_uploads_honours_max_uploads_and_markers() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let bucket = server.bucket();
    // Initiate 3 uploads on different keys so the tuple-cursor pagination has work to do.
    let mut ids = Vec::new();
    for name in ["a.bin", "b.bin", "c.bin"] {
        let c = client
            .create_multipart_upload()
            .bucket(bucket)
            .key(name)
            .send()
            .await
            .unwrap();
        ids.push((name, c.upload_id().unwrap().to_string()));
    }

    // First page: 2 uploads.
    let page1 = client
        .list_multipart_uploads()
        .bucket(bucket)
        .max_uploads(2)
        .send()
        .await
        .unwrap();
    assert!(page1.is_truncated().unwrap_or(false));
    assert_eq!(page1.uploads().len(), 2);
    let next_key = page1.next_key_marker().unwrap_or_default().to_string();
    let next_id = page1.next_upload_id_marker().unwrap_or_default().to_string();
    assert!(!next_key.is_empty(), "next key marker should be populated");

    // Second page with marker.
    let page2 = client
        .list_multipart_uploads()
        .bucket(bucket)
        .max_uploads(2)
        .key_marker(&next_key)
        .upload_id_marker(&next_id)
        .send()
        .await
        .unwrap();
    assert!(!page2.is_truncated().unwrap_or(true));
    assert_eq!(page2.uploads().len(), 1, "third upload should land alone");

    // Cleanup
    for (name, id) in ids {
        client
            .abort_multipart_upload()
            .bucket(bucket)
            .key(name)
            .upload_id(&id)
            .send()
            .await
            .ok();
    }
}

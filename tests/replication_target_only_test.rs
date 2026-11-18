// SPDX-License-Identifier: BUSL-1.1

//! Guard A integration tests: a `replication_target_only` bucket refuses
//! every CLIENT write shape with 403 AccessDenied, while replication (which
//! calls the engine directly, below the gated layer) writes into it fine and
//! reads stay open. This is the enforced single-writer property that makes a
//! non-CAS backend (e.g. Backblaze B2) safe as a replication destination.

use crate::common;

use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, sign_raw, wait_for_run_after, TestServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: rto-rule
      enabled: true
      source:
        bucket: rto-src
        prefix: \"\"
      destination:
        bucket: rto-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
";

/// Assert an SdkError is a service-level AccessDenied.
fn assert_access_denied<E, R>(err: &aws_sdk_s3::error::SdkError<E, R>, what: &str)
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    let code = err.meta().code().unwrap_or("");
    assert_eq!(
        code, "AccessDenied",
        "{what}: expected AccessDenied, got {err:?}"
    );
    assert!(
        err.meta()
            .message()
            .unwrap_or("")
            .contains("replication targets only"),
        "{what}: message must name the marker, got {:?}",
        err.meta().message()
    );
}

#[tokio::test]
async fn test_marked_bucket_rejects_client_writes_replication_still_works() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(RULE_YAML)
        .bucket_policy("rto-dst", "replication_target_only: true")
        .build()
        .await;
    let client = server.s3_client().await;

    for b in ["rto-src", "rto-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    // Unmarked source bucket accepts client writes (control).
    client
        .put_object()
        .bucket("rto-src")
        .key("a.txt")
        .body(ByteStream::from_static(b"alpha"))
        .send()
        .await
        .expect("PUT to unmarked bucket must succeed");

    // 1. PUT → 403.
    let err = client
        .put_object()
        .bucket("rto-dst")
        .key("direct.txt")
        .body(ByteStream::from_static(b"nope"))
        .send()
        .await
        .expect_err("PUT to marked bucket must fail");
    assert_access_denied(&err, "PutObject");

    // 2. DeleteObject → 403 (a delete mutates the deltaspace too).
    let err = client
        .delete_object()
        .bucket("rto-dst")
        .key("whatever.txt")
        .send()
        .await
        .expect_err("DeleteObject on marked bucket must fail");
    assert_access_denied(&err, "DeleteObject");

    // 3. DeleteObjects (batch) → 403.
    let err = client
        .delete_objects()
        .bucket("rto-dst")
        .delete(
            aws_sdk_s3::types::Delete::builder()
                .objects(
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key("x.txt")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect_err("DeleteObjects on marked bucket must fail");
    assert_access_denied(&err, "DeleteObjects");

    // 4. CreateMultipartUpload → 403 (earliest reject).
    let err = client
        .create_multipart_upload()
        .bucket("rto-dst")
        .key("big.bin")
        .send()
        .await
        .expect_err("CreateMultipartUpload on marked bucket must fail");
    assert_access_denied(&err, "CreateMultipartUpload");

    // 5. CopyObject with marked DEST → 403.
    let err = client
        .copy_object()
        .bucket("rto-dst")
        .key("copied.txt")
        .copy_source("rto-src/a.txt")
        .send()
        .await
        .expect_err("CopyObject into marked bucket must fail");
    assert_access_denied(&err, "CopyObject");

    // 6. Replication INTO the marked bucket succeeds (engine-direct path).
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:rto-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 202, "run-now must be accepted");
    let run = wait_for_run_after(&admin, &server.endpoint(), "rto-rule", -1).await;
    assert_eq!(
        run["status"].as_str(),
        Some("succeeded"),
        "replication into the marked bucket must succeed, got {run}"
    );
    assert_eq!(run["errors"].as_i64(), Some(0), "no copy errors: {run}");

    // 7. Reads on the marked bucket stay open: the replicated object is
    //    GETtable and listable by a normal client.
    let got = client
        .get_object()
        .bucket("rto-dst")
        .key("a.txt")
        .send()
        .await
        .expect("GET from marked bucket must succeed");
    let body = got.body.collect().await.unwrap().into_bytes();
    assert_eq!(&body[..], b"alpha");
    let listed = client
        .list_objects_v2()
        .bucket("rto-dst")
        .send()
        .await
        .expect("LIST on marked bucket must succeed");
    assert_eq!(listed.key_count(), Some(1));

    // 8. Copy FROM the marked bucket into an unmarked one is a read of the
    //    marked bucket — allowed.
    client
        .copy_object()
        .bucket("rto-src")
        .key("copied-back.txt")
        .copy_source("rto-dst/a.txt")
        .send()
        .await
        .expect("copy OUT of marked bucket must succeed");

    // 9. DeleteBucket on the marked bucket → 403 (destroying a replication
    //    destination is a client write). CreateBucket, by contrast, is allowed
    //    — an empty destination must be bootstrappable.
    let err = client
        .delete_bucket()
        .bucket("rto-dst")
        .send()
        .await
        .expect_err("DeleteBucket on marked bucket must fail");
    assert_access_denied(&err, "DeleteBucket");
    // The setup at the top of this test sent the same CreateBucket. If both
    // land in the same clock second they carry the same SigV4 signature, and
    // the proxy's replay guard rightly rejects the second PUT. A signed
    // nonce header makes this request distinct without changing its meaning.
    client
        .create_bucket()
        .bucket("rto-dst")
        .customize()
        .mutate_request(|req| {
            req.headers_mut()
                .insert("x-amz-meta-dgp-test-nonce", "recreate");
        })
        .send()
        .await
        .expect("CreateBucket on an existing marked bucket must be allowed");
}

/// The gate fires BEFORE the upload is looked up (403 before 404), so a
/// bogus upload id still answers `AccessDenied` on a marked bucket. A removed
/// gate answers `NoSuchUpload` instead, which is the failure this pins for
/// `UploadPartCopy` and `CompleteMultipartUpload` (CreateMultipartUpload is
/// covered above; a marked bucket never has an upload of its own).
#[tokio::test]
async fn test_marked_bucket_rejects_part_copy_and_complete() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .bucket_policy("rto-dst", "replication_target_only: true")
        .build()
        .await;
    let client = server.s3_client().await;
    client.create_bucket().bucket("rto-src").send().await.ok();
    client.create_bucket().bucket("rto-dst").send().await.ok();
    client
        .put_object()
        .bucket("rto-src")
        .key("a.txt")
        .body(ByteStream::from_static(b"alpha"))
        .send()
        .await
        .expect("PUT to the unmarked bucket must succeed");

    let err = client
        .upload_part_copy()
        .bucket("rto-dst")
        .key("big.bin")
        .upload_id("no-such-upload")
        .part_number(1)
        .copy_source("rto-src/a.txt")
        .send()
        .await
        .expect_err("UploadPartCopy into a marked bucket must fail");
    assert_access_denied(&err, "UploadPartCopy");

    let err = client
        .complete_multipart_upload()
        .bucket("rto-dst")
        .key("big.bin")
        .upload_id("no-such-upload")
        .multipart_upload(
            aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .parts(
                    aws_sdk_s3::types::CompletedPart::builder()
                        .part_number(1)
                        .e_tag("\"x\"")
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .expect_err("CompleteMultipartUpload on a marked bucket must fail");
    assert_access_denied(&err, "CompleteMultipartUpload");
}

/// A signed browser form POST (PostObject) into a marked bucket answers 403
/// with the marker message, and the same form into an unmarked bucket stores
/// (204). The form handler calls the client-write gate itself (it does not
/// go through the s3s adapter), so a removed call there lets the form
/// through; this pins the call. The control row makes sure the 403 comes
/// from the marker and not from the form.
#[tokio::test]
async fn test_marked_bucket_refuses_form_post() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .bucket_policy("rto-dst", "replication_target_only: true")
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["rto-dst", "rto-plain"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    let creds = ("bootstrap_key", "bootstrap_secret");

    let r = common::signed_form_post(&server.endpoint(), "rto-dst", creds)
        .send()
        .await
        .unwrap();
    let (status, text) = (r.status().as_u16(), r.text().await.unwrap());
    assert_eq!(status, 403, "{text}");
    assert!(text.contains("replication targets only"), "{text}");
    assert!(
        client
            .head_object()
            .bucket("rto-dst")
            .key("post/x.txt")
            .send()
            .await
            .is_err(),
        "the refused form stored nothing"
    );

    let r = common::signed_form_post(&server.endpoint(), "rto-plain", creds)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        204,
        "an unmarked bucket takes the form"
    );
}

/// The alias hole: a marked bucket declared with an `alias` is protected under
/// its VIRTUAL name, but a client PUT to the UNCONFIGURED real (alias) name
/// resolves — via the cross-backend probe — onto the same protected storage.
/// That write must be blocked too; an unrelated unconfigured name still writes.
#[tokio::test]
async fn test_marked_bucket_alias_name_is_also_blocked() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        // Marked virtual name `mirror` aliases real storage `real-mirror`.
        .bucket_policy(
            "mirror",
            "replication_target_only: true\nalias: real-mirror",
        )
        .build()
        .await;
    let client = server.s3_client().await;
    client.create_bucket().bucket("mirror").send().await.ok();

    // PUT to the UNCONFIGURED alias name → 403 with the collision message.
    let err = client
        .put_object()
        .bucket("real-mirror")
        .key("sneaky.txt")
        .body(ByteStream::from_static(b"nope"))
        .send()
        .await
        .expect_err("write to the alias real-name must be blocked");
    assert_eq!(err.meta().code().unwrap_or(""), "AccessDenied", "{err:?}");
    assert!(
        err.meta()
            .message()
            .unwrap_or("")
            .contains("maps to the storage"),
        "must be the alias-collision message, got {:?}",
        err.meta().message()
    );

    // An unrelated unconfigured bucket accepts writes (control).
    client.create_bucket().bucket("unrelated").send().await.ok();
    client
        .put_object()
        .bucket("unrelated")
        .key("ok.txt")
        .body(ByteStream::from_static(b"fine"))
        .send()
        .await
        .expect("unrelated unconfigured bucket must accept writes");
}

#[tokio::test]
async fn test_admin_bulk_ops_respect_marker() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .bucket_policy("adm-dst", "replication_target_only: true")
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["adm-src", "adm-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    client
        .put_object()
        .bucket("adm-src")
        .key("f.txt")
        .body(ByteStream::from_static(b"data"))
        .send()
        .await
        .unwrap();

    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();

    // Bulk copy INTO the marked bucket → 403 with the doc-linked message.
    let resp = admin
        .post(format!("{ep}/_/api/admin/objects/copy"))
        .json(&serde_json::json!({
            "source_bucket": "adm-src",
            "dest_bucket": "adm-dst",
            "dest_prefix": "",
            "items": [{"source_key": "f.txt", "relative": "f.txt"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403, "bulk copy into marked bucket");
    assert!(resp
        .text()
        .await
        .unwrap()
        .contains("replication targets only"));

    // Bulk move with marked SOURCE → 403 (the post-copy source deletes).
    let resp = admin
        .post(format!("{ep}/_/api/admin/objects/move"))
        .json(&serde_json::json!({
            "source_bucket": "adm-dst",
            "dest_bucket": "adm-src",
            "dest_prefix": "",
            "items": [{"source_key": "f.txt", "relative": "f.txt"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "bulk move out of marked bucket"
    );

    // Bulk delete ON the marked bucket → 403.
    let resp = admin
        .post(format!("{ep}/_/api/admin/objects/delete"))
        .json(&serde_json::json!({
            "bucket": "adm-dst",
            "keys": ["f.txt"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403, "bulk delete on marked bucket");

    // Control: the same bulk delete on the UNMARKED bucket succeeds.
    let resp = admin
        .post(format!("{ep}/_/api/admin/objects/delete"))
        .json(&serde_json::json!({
            "bucket": "adm-src",
            "keys": ["f.txt"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "bulk delete on unmarked bucket"
    );
}

/// #187 item 4: a refused PUT with a body larger than the socket buffers
/// must reach the client as 403, not as a closed connection. The gate
/// refuses before the body is read.
#[tokio::test]
async fn a_refused_large_put_answers_403_not_a_closed_connection() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .bucket_policy("rto-dst", "replication_target_only: true")
        .build()
        .await;
    let client = server.s3_client().await;
    client.create_bucket().bucket("rto-dst").send().await.ok();
    let err = client
        .put_object()
        .bucket("rto-dst")
        .key("big.bin")
        .body(ByteStream::from(vec![7u8; 32 * 1024 * 1024]))
        .send()
        .await
        .expect_err("PUT to marked bucket must fail");
    assert_access_denied(&err, "large PutObject");
}

const RAW_BODY: usize = 32 * 1024 * 1024;

/// A PUT head for `/rto-dst/big.bin` with a 32 MiB body, signed or not.
fn raw_put_head(ep: &str, signed: bool) -> String {
    let url = format!("{ep}/rto-dst/big.bin");
    let host = ep.trim_start_matches("http://");
    let mut head =
        format!("PUT /rto-dst/big.bin HTTP/1.1\r\nHost: {host}\r\nContent-Length: {RAW_BODY}\r\n");
    if signed {
        for (k, v) in sign_raw(
            reqwest::Method::PUT,
            &url,
            "bootstrap_key",
            "bootstrap_secret",
        ) {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
    } else {
        head.push_str("x-amz-content-sha256: UNSIGNED-PAYLOAD\r\n");
    }
    head.push_str("\r\n");
    head
}

/// Read one HTTP response (status line, headers, Content-Length body).
async fn read_raw_response(s: &mut tokio::net::TcpStream) -> Option<(u16, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            while buf.len() < end + 4 + len {
                let n = s.read(&mut tmp).await.ok()?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let status = head.split_whitespace().nth(1)?.parse().ok()?;
            return Some((status, head));
        }
        let n = tokio::time::timeout(std::time::Duration::from_secs(20), s.read(&mut tmp))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

async fn rto_server() -> TestServer {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .bucket_policy("rto-dst", "replication_target_only: true")
        .build()
        .await;
    let client = server.s3_client().await;
    client.create_bucket().bucket("rto-dst").send().await.ok();
    server
}

/// #187 item 4: the body of a refused SIGNED request is read, so the
/// connection survives for the next request (the proxy before a reverse
/// proxy sees an answer, not a reset).
#[tokio::test]
async fn a_refused_signed_put_body_is_read_and_the_connection_stays_open() {
    let server = rto_server().await;
    let ep = server.endpoint();
    let mut s = tokio::net::TcpStream::connect(ep.trim_start_matches("http://"))
        .await
        .unwrap();
    s.write_all(raw_put_head(&ep, true).as_bytes())
        .await
        .unwrap();
    let chunk = vec![7u8; 1024 * 1024];
    for _ in 0..RAW_BODY / chunk.len() {
        s.write_all(&chunk).await.expect("the whole body is read");
    }
    let (status, _) = read_raw_response(&mut s).await.expect("first response");
    assert_eq!(status, 403);
    // Keep-alive: a second signed request on the same socket gets an answer.
    let head = sign_raw(
        reqwest::Method::GET,
        &format!("{ep}/"),
        "bootstrap_key",
        "bootstrap_secret",
    );
    let mut req = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\n",
        ep.trim_start_matches("http://")
    );
    for (k, v) in head {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let (status, _) = read_raw_response(&mut s).await.expect("second response");
    assert_eq!(status, 200);
}

/// The DoS guard: an anonymous refused request is not drained, so its
/// connection closes.
#[tokio::test]
async fn a_refused_anonymous_put_body_is_not_read() {
    let server = rto_server().await;
    let ep = server.endpoint();
    let mut s = tokio::net::TcpStream::connect(ep.trim_start_matches("http://"))
        .await
        .unwrap();
    s.write_all(raw_put_head(&ep, false).as_bytes())
        .await
        .unwrap();
    let chunk = vec![7u8; 1024 * 1024];
    let mut written = 0usize;
    for _ in 0..RAW_BODY / chunk.len() {
        if s.write_all(&chunk).await.is_err() {
            break;
        }
        written += chunk.len();
    }
    // The server stops reading: the socket closes before all bytes are taken
    // or right after the answer, and a second request gets no reply.
    let first = read_raw_response(&mut s).await;
    let second = if first.is_some() {
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await.ok();
        read_raw_response(&mut s).await
    } else {
        None
    };
    assert!(
        second.is_none(),
        "anonymous connection must close (written {written}, first {first:?})"
    );
}

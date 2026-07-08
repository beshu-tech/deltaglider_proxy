// SPDX-License-Identifier: GPL-3.0-only

//! Guard A integration tests: a `replication_target_only` bucket refuses
//! every CLIENT write shape with 403 AccessDenied, while replication (which
//! calls the engine directly, below the gated layer) writes into it fine and
//! reads stay open. This is the enforced single-writer property that makes a
//! non-CAS backend (e.g. Backblaze B2) safe as a replication destination.

mod common;

use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, wait_for_run_after, TestServer};

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
            .contains("replication_target_only"),
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
    client
        .create_bucket()
        .bucket("rto-dst")
        .send()
        .await
        .expect("CreateBucket on an existing marked bucket must be allowed");
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
        .contains("replication_target_only"));

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

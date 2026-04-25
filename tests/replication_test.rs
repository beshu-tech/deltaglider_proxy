//! End-to-end integration tests for lazy replication.
//!
//! Exercises the worker via the admin API's `run-now` endpoint so the
//! full stack (config → DB → engine → worker → state store) is tested
//! together. Skeleton: seed a rule in YAML, seed source objects, trigger
//! run-now, verify destination + status + history + counters.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, TestServer};
use serde_json::Value;

const RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: repl-a-to-b
      enabled: true
      source:
        bucket: repl-src
        prefix: \"\"
      destination:
        bucket: repl-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
";

const PAUSED_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: paused-rule
      enabled: true
      source:
        bucket: p-src
        prefix: \"\"
      destination:
        bucket: p-dst
        prefix: \"\"
      interval: \"1h\"
";

const MULTIPAGE_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: multipage-rule
      enabled: true
      source:
        bucket: mp-src
        prefix: \"\"
      destination:
        bucket: mp-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 5
";

const DELETE_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: delete-rule
      enabled: true
      source:
        bucket: del-src
        prefix: \"\"
      destination:
        bucket: del-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
      replicate_deletes: true
";

/// Spin up a proxy with two buckets and a replication rule wired
/// up in the YAML. A single run-now copies all objects from source
/// to destination.
#[tokio::test]
async fn test_replication_run_now_copies_missing_objects() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(RULE_YAML)
        .build()
        .await;

    let client = server.s3_client().await;

    // Create both buckets and seed source with 3 objects.
    for b in ["repl-src", "repl-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for (key, body) in [
        ("a.txt", &b"alpha"[..]),
        ("b.txt", &b"bravo"[..]),
        ("nested/c.txt", &b"charlie"[..]),
    ] {
        client
            .put_object()
            .bucket("repl-src")
            .key(key)
            .body(ByteStream::from(body.to_vec()))
            .send()
            .await
            .expect("seed");
    }

    // Trigger the replication run-now via the admin API.
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/repl-a-to-b/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request");
    assert_eq!(resp.status().as_u16(), 200, "run-now should succeed");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"].as_str(), Some("succeeded"), "run status");
    assert_eq!(
        body["objects_copied"].as_i64().unwrap_or(-1),
        3,
        "copied count in run-now response: {}",
        body
    );

    // Verify the destination now has all three objects.
    for key in ["a.txt", "b.txt", "nested/c.txt"] {
        let got = client
            .get_object()
            .bucket("repl-dst")
            .key(key)
            .send()
            .await
            .expect("dest object present")
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes();
        assert!(!got.is_empty(), "dest key {} has content", key);
    }

    // History endpoint: 1 run, status=succeeded, objects_copied=3.
    let hist: Value = admin
        .get(format!(
            "{}/_/api/admin/replication/rules/repl-a-to-b/history",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let runs = hist["runs"].as_array().expect("history runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["status"].as_str(), Some("succeeded"));
    assert_eq!(runs[0]["objects_copied"].as_i64(), Some(3));
}

/// A paused rule must return 409 on run-now until resumed.
#[tokio::test]
async fn test_replication_paused_rule_blocks_run_now() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(PAUSED_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["p-src", "p-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    let admin = admin_http_client(&server.endpoint()).await;

    // Pause the rule.
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/paused-rule/pause",
            server.endpoint()
        ))
        .send()
        .await
        .expect("pause");
    assert_eq!(resp.status().as_u16(), 204);

    // Run-now must 409.
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/paused-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 409);

    // Resume + verify run-now now accepts the call (with zero work).
    admin
        .post(format!(
            "{}/_/api/admin/replication/rules/paused-rule/resume",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/paused-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

/// H1 fix regression: a single run-now must replicate ALL objects
/// across multiple pages, not just the first batch_size keys. With
/// batch_size=5 and 17 objects, we expect 17 copied (= 4 pages).
#[tokio::test]
async fn test_replication_paginates_until_complete() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(MULTIPAGE_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;

    for b in ["mp-src", "mp-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // Seed 17 objects (3 full pages of 5 + a 4th of 2).
    for i in 0..17u32 {
        let key = format!("file-{:03}.bin", i);
        client
            .put_object()
            .bucket("mp-src")
            .key(&key)
            .body(ByteStream::from(vec![i as u8; 16]))
            .send()
            .await
            .expect("seed");
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/multipage-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    // Pre-fix: copied was capped at batch_size=5. Post-fix: 17.
    assert_eq!(
        body["objects_copied"].as_i64().unwrap_or(-1),
        17,
        "H1 REGRESSION: should copy all 17 objects across pages, got {}",
        body
    );
    assert_eq!(body["status"].as_str(), Some("succeeded"));

    // Verify destination has all 17.
    let listed = client
        .list_objects_v2()
        .bucket("mp-dst")
        .send()
        .await
        .unwrap();
    let count = listed.contents().len();
    assert_eq!(count, 17);

    // Continuation token should be cleared after a clean complete pass.
    // (Implicitly: a second run-now copies nothing because all keys exist
    // and conflict=newer-wins skips equal-or-older destinations.)
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/multipage-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["objects_copied"].as_i64().unwrap_or(-1),
        0,
        "second run should be a no-op when source==dest, got {}",
        body
    );
}

/// H2 fix regression: replicate_deletes=true must remove destination
/// keys that no longer exist on source.
#[tokio::test]
async fn test_replication_replicate_deletes_removes_orphans() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(DELETE_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;

    for b in ["del-src", "del-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // Seed both source and destination with 3 objects.
    for key in ["a.txt", "b.txt", "c.txt"] {
        client
            .put_object()
            .bucket("del-src")
            .key(key)
            .body(ByteStream::from(b"x".to_vec()))
            .send()
            .await
            .unwrap();
        client
            .put_object()
            .bucket("del-dst")
            .key(key)
            .body(ByteStream::from(b"x".to_vec()))
            .send()
            .await
            .unwrap();
    }
    // Add an extra orphan on destination only.
    client
        .put_object()
        .bucket("del-dst")
        .key("orphan.txt")
        .body(ByteStream::from(b"orphan".to_vec()))
        .send()
        .await
        .unwrap();

    // Trigger replication. Forward pass copies nothing new (all in sync),
    // delete pass should drop "orphan.txt".
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/delete-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Verify the orphan is gone.
    let head_orphan = client
        .head_object()
        .bucket("del-dst")
        .key("orphan.txt")
        .send()
        .await;
    assert!(
        head_orphan.is_err(),
        "H2 REGRESSION: orphan.txt should have been deleted from destination"
    );

    // Verify the legit keys are still there.
    for key in ["a.txt", "b.txt", "c.txt"] {
        client
            .head_object()
            .bucket("del-dst")
            .key(key)
            .send()
            .await
            .expect("legit dest key remains");
    }
}

/// H3 fix regression: source's multipart ETag must propagate through
/// replication. After replication, dest HEAD ETag == source HEAD ETag,
/// preserving the "abc-N" multipart format.
#[tokio::test]
async fn test_replication_preserves_multipart_etag() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(
            "
replication:
  enabled: true
  rules:
    - name: mp-etag-rule
      enabled: true
      source: { bucket: e-src, prefix: \"\" }
      destination: { bucket: e-dst, prefix: \"\" }
      interval: \"1h\"
      batch_size: 100
",
        )
        .build()
        .await;

    let client = server.s3_client().await;
    for b in ["e-src", "e-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // Create a multipart upload on the SOURCE bucket so the source
    // object carries a multipart_etag.
    let key = "big.bin";
    let create = client
        .create_multipart_upload()
        .bucket("e-src")
        .key(key)
        .send()
        .await
        .unwrap();
    let upload_id = create.upload_id().unwrap().to_string();

    let part1 = vec![0xAAu8; 5 * 1024 * 1024];
    let part2 = vec![0xBBu8; 1024];
    let etag1 = client
        .upload_part()
        .bucket("e-src")
        .key(key)
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from(part1))
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string();
    let etag2 = client
        .upload_part()
        .bucket("e-src")
        .key(key)
        .upload_id(&upload_id)
        .part_number(2)
        .body(ByteStream::from(part2))
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string();
    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
    let completed = CompletedMultipartUpload::builder()
        .parts(
            CompletedPart::builder()
                .part_number(1)
                .e_tag(&etag1)
                .build(),
        )
        .parts(
            CompletedPart::builder()
                .part_number(2)
                .e_tag(&etag2)
                .build(),
        )
        .build();
    let complete = client
        .complete_multipart_upload()
        .bucket("e-src")
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .unwrap();
    let source_etag = complete.e_tag().unwrap().to_string();
    assert!(
        source_etag.contains("-2"),
        "source should have multipart ETag, got {}",
        source_etag
    );

    // Trigger replication.
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/mp-etag-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // HEAD destination — must return the SAME multipart ETag.
    let dest_head = client
        .head_object()
        .bucket("e-dst")
        .key(key)
        .send()
        .await
        .unwrap();
    let dest_etag = dest_head.e_tag().unwrap().to_string();
    assert_eq!(
        dest_etag, source_etag,
        "H3 REGRESSION: destination ETag {} differs from source ETag {} after replication",
        dest_etag, source_etag
    );
}

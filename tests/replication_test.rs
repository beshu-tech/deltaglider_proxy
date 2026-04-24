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

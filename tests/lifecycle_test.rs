// SPDX-License-Identifier: GPL-3.0-only

//! End-to-end tests for delete-only lifecycle rules.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, TestServer};
use serde_json::Value;

const LIFECYCLE_YAML: &str = r#"
lifecycle:
  enabled: true
  tick_interval: "1h"
  rules:
    - name: expire-old-prefix
      enabled: true
      bucket: life-bucket
      prefix: ""
      expire_after: "1ms"
      batch_size: 100
      include_globs: ["old/**", ".deltaglider/**"]
      exclude_globs: []
"#;

const LIFECYCLE_TRANSITION_KEEP_SOURCE_YAML: &str = r#"
lifecycle:
  enabled: true
  tick_interval: "1h"
  rules:
    - name: archive-old
      enabled: true
      bucket: life-src
      prefix: "old"
      action:
        type: transition
        destination:
          bucket: life-archive
          prefix: "cold"
        delete_source_after_success: false
      expire_after: "1ms"
      batch_size: 100
      include_globs: ["old/**"]
      exclude_globs: []
"#;

const LIFECYCLE_TRANSITION_DELETE_SOURCE_YAML: &str = r#"
lifecycle:
  enabled: true
  tick_interval: "1h"
  rules:
    - name: move-old
      enabled: true
      bucket: move-src
      prefix: "old"
      action:
        type: transition
        destination:
          bucket: move-archive
          prefix: "cold"
        delete_source_after_success: true
      expire_after: "1ms"
      batch_size: 100
      include_globs: ["old/**"]
      exclude_globs: []
"#;

const LIFECYCLE_TRANSITION_MISSING_DEST_YAML: &str = r#"
lifecycle:
  enabled: true
  tick_interval: "1h"
  rules:
    - name: failed-move
      enabled: true
      bucket: fail-src
      prefix: "old"
      action:
        type: transition
        destination:
          bucket: missing-destination
          prefix: "cold"
        delete_source_after_success: true
      expire_after: "1ms"
      batch_size: 100
      include_globs: ["old/**"]
      exclude_globs: []
"#;

#[tokio::test]
async fn test_lifecycle_run_now_deletes_visible_expired_and_preserves_skipped_keys() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(LIFECYCLE_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    client
        .create_bucket()
        .bucket("life-bucket")
        .send()
        .await
        .ok();

    for (key, body) in [
        ("old/delete-me.txt", b"expired".as_slice()),
        ("keep/not-matched.txt", b"keep".as_slice()),
        (".deltaglider/config.db", b"internal".as_slice()),
    ] {
        client
            .put_object()
            .bucket("life-bucket")
            .key(key)
            .body(ByteStream::from(body.to_vec()))
            .send()
            .await
            .expect("seed lifecycle object");
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let preview: Value = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:expire-old-prefix/preview",
            server.endpoint()
        ))
        .send()
        .await
        .expect("preview request")
        .json()
        .await
        .unwrap();
    assert_eq!(preview["status"].as_str(), Some("preview"));
    assert_eq!(preview["objects_affected"].as_i64(), Some(1), "{preview}");

    let history_before: Value = admin
        .get(format!(
            "{}/_/api/admin/jobs/lifecycle:expire-old-prefix/runs",
            server.endpoint()
        ))
        .send()
        .await
        .expect("history request")
        .json()
        .await
        .unwrap();
    assert_eq!(
        history_before["runs"].as_array().map(Vec::len),
        Some(0),
        "preview must stay read-only and not create lifecycle history: {history_before}"
    );

    let run: Value = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:expire-old-prefix/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request")
        .json()
        .await
        .unwrap();
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    assert_eq!(run["objects_affected"].as_i64(), Some(1), "{run}");
    let run_id = run["run_id"]
        .as_i64()
        .expect("run-now should return run_id");

    let history_after: Value = admin
        .get(format!(
            "{}/_/api/admin/jobs/lifecycle:expire-old-prefix/runs",
            server.endpoint()
        ))
        .send()
        .await
        .expect("history request after run")
        .json()
        .await
        .unwrap();
    assert_eq!(history_after["runs"][0]["id"].as_i64(), Some(run_id));
    assert_eq!(
        history_after["runs"][0]["triggered_by"].as_str(),
        Some("run-now")
    );
    assert_eq!(
        history_after["runs"][0]["objects_processed"].as_i64(),
        Some(1)
    );

    let failures: Value = admin
        .get(format!(
            "{}/_/api/admin/jobs/lifecycle:expire-old-prefix/failures",
            server.endpoint()
        ))
        .send()
        .await
        .expect("failures request")
        .json()
        .await
        .unwrap();
    assert_eq!(failures["failures"].as_array().map(Vec::len), Some(0));

    let deleted = client
        .get_object()
        .bucket("life-bucket")
        .key("old/delete-me.txt")
        .send()
        .await;
    assert!(deleted.is_err(), "expired object should be gone");

    for key in ["keep/not-matched.txt", ".deltaglider/config.db"] {
        let got = client
            .get_object()
            .bucket("life-bucket")
            .key(key)
            .send()
            .await
            .expect("preserved object")
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes();
        assert!(!got.is_empty(), "key {key} should be preserved");
    }
}

#[tokio::test]
async fn test_lifecycle_transition_copies_expired_object_and_preserves_source() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(LIFECYCLE_TRANSITION_KEEP_SOURCE_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    for bucket in ["life-src", "life-archive"] {
        client.create_bucket().bucket(bucket).send().await.ok();
    }
    client
        .put_object()
        .bucket("life-src")
        .key("old/app.zip")
        .body(ByteStream::from(b"archive me".to_vec()))
        .send()
        .await
        .expect("seed transition source");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let admin = admin_http_client(&server.endpoint()).await;
    let preview: Value = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:archive-old/preview",
            server.endpoint()
        ))
        .send()
        .await
        .expect("preview request")
        .json()
        .await
        .unwrap();
    assert_eq!(preview["objects_affected"].as_i64(), Some(1), "{preview}");
    assert_eq!(
        preview["candidates"][0]["action"].as_str(),
        Some("transition")
    );
    assert_eq!(
        preview["candidates"][0]["destination_bucket"].as_str(),
        Some("life-archive")
    );
    assert_eq!(
        preview["candidates"][0]["destination_key"].as_str(),
        Some("cold/app.zip")
    );

    let run: Value = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:archive-old/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request")
        .json()
        .await
        .unwrap();
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    assert_eq!(run["objects_affected"].as_i64(), Some(1), "{run}");

    let archived = client
        .get_object()
        .bucket("life-archive")
        .key("cold/app.zip")
        .send()
        .await
        .expect("archived object")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(&archived[..], b"archive me");

    client
        .head_object()
        .bucket("life-src")
        .key("old/app.zip")
        .send()
        .await
        .expect("source should be preserved when delete_source_after_success=false");
}

#[tokio::test]
async fn test_lifecycle_transition_delete_source_after_success() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(LIFECYCLE_TRANSITION_DELETE_SOURCE_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    for bucket in ["move-src", "move-archive"] {
        client.create_bucket().bucket(bucket).send().await.ok();
    }
    client
        .put_object()
        .bucket("move-src")
        .key("old/app.zip")
        .body(ByteStream::from(b"move me".to_vec()))
        .send()
        .await
        .expect("seed move source");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let admin = admin_http_client(&server.endpoint()).await;
    let run: Value = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:move-old/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request")
        .json()
        .await
        .unwrap();
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");

    client
        .head_object()
        .bucket("move-archive")
        .key("cold/app.zip")
        .send()
        .await
        .expect("destination should exist after move");
    assert!(
        client
            .head_object()
            .bucket("move-src")
            .key("old/app.zip")
            .send()
            .await
            .is_err(),
        "source should be deleted only after successful copy"
    );
}

#[tokio::test]
async fn test_lifecycle_transition_copy_failure_does_not_delete_source() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(LIFECYCLE_TRANSITION_MISSING_DEST_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    client.create_bucket().bucket("fail-src").send().await.ok();
    client
        .put_object()
        .bucket("fail-src")
        .key("old/app.zip")
        .body(ByteStream::from(b"keep me".to_vec()))
        .send()
        .await
        .expect("seed failing transition source");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let admin = admin_http_client(&server.endpoint()).await;
    let run: Value = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:failed-move/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request")
        .json()
        .await
        .unwrap();
    assert_eq!(run["status"].as_str(), Some("failed"), "{run}");
    assert_eq!(run["objects_affected"].as_i64(), Some(0), "{run}");
    assert_eq!(run["errors"].as_i64(), Some(1), "{run}");

    client
        .head_object()
        .bucket("fail-src")
        .key("old/app.zip")
        .send()
        .await
        .expect("source must survive failed transition copy");
}

/// Pause/resume parity (new in the unified jobs API): a paused lifecycle
/// rule 409s run-now until resumed, and the jobs overview reports the flag.
#[tokio::test]
async fn test_lifecycle_pause_blocks_run_now() {
    let server = TestServer::builder()
        .bucket("life-pause")
        .extra_yaml_storage_section(
            r#"
lifecycle:
  enabled: true
  tick_interval: "1h"
  rules:
    - name: pause-me
      enabled: true
      bucket: life-pause
      prefix: ""
      expire_after: "1ms"
      batch_size: 100
      include_globs: ["**"]
      exclude_globs: []
"#,
        )
        .build()
        .await;
    let admin = common::admin_http_client(&server.endpoint()).await;

    let pause = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:pause-me/pause",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert!(pause.status().is_success(), "pause: {}", pause.status());

    let run = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:pause-me/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(run.status(), 409, "paused rule must refuse run-now");

    // Overview reports paused=true with the unified row shape.
    let body: Value = admin
        .get(format!("{}/_/api/admin/jobs", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = body["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == "lifecycle:pause-me")
        .expect("rule visible in jobs overview")
        .clone();
    assert_eq!(row["kind"], "lifecycle");
    assert_eq!(row["trigger"], "scheduled");
    assert_eq!(row["paused"], true, "{row}");

    let resume = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:pause-me/resume",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert!(resume.status().is_success());
    let run = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:pause-me/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert!(
        run.status().is_success(),
        "resumed rule runs: {}",
        run.status()
    );

    // Unsupported action on a rule kind → 400 with the supported list.
    let bad = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:pause-me/cancel",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400, "cancel unsupported for lifecycle rules");
}

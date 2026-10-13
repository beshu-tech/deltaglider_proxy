// SPDX-License-Identifier: BUSL-1.1

//! End-to-end tests for delete-only lifecycle rules.

use crate::common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, lifecycle_run_now_and_wait, TestServer};
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

    // GET on the same path polls a running preview walk; none runs now.
    let walk: Value = admin
        .get(format!(
            "{}/_/api/admin/jobs/lifecycle:expire-old-prefix/preview",
            server.endpoint()
        ))
        .send()
        .await
        .expect("preview status request")
        .json()
        .await
        .unwrap();
    assert_eq!(walk["running"].as_bool(), Some(false), "{walk}");

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

    let run = lifecycle_run_now_and_wait(&admin, &server.endpoint(), "expire-old-prefix").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    assert_eq!(run["objects_processed"].as_i64(), Some(1), "{run}");
    let run_id = run["id"].as_i64().expect("the settled run has an id");

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

    let run = lifecycle_run_now_and_wait(&admin, &server.endpoint(), "archive-old").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    assert_eq!(run["objects_processed"].as_i64(), Some(1), "{run}");

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

/// A transition copy keeps the source object's created-at, so the archive
/// shows the original time and ages on it count from the object's creation.
#[tokio::test]
async fn test_lifecycle_transition_keeps_source_created_at() {
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
    // A listing carries milliseconds; HEAD has one-second resolution.
    let listed = |bucket: &'static str| {
        let client = client.clone();
        async move {
            let out = client
                .list_objects_v2()
                .bucket(bucket)
                .send()
                .await
                .unwrap();
            *out.contents()[0].last_modified().unwrap()
        }
    };
    let created = listed("life-src").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let admin = admin_http_client(&server.endpoint()).await;
    let run = lifecycle_run_now_and_wait(&admin, &server.endpoint(), "archive-old").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    assert_eq!(listed("life-archive").await, created);
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
    let run = lifecycle_run_now_and_wait(&admin, &server.endpoint(), "move-old").await;
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
    let run = lifecycle_run_now_and_wait(&admin, &server.endpoint(), "failed-move").await;
    assert_eq!(run["status"].as_str(), Some("failed"), "{run}");
    assert_eq!(run["objects_processed"].as_i64(), Some(0), "{run}");
    assert_eq!(run["errors"].as_i64(), Some(1), "{run}");

    client
        .head_object()
        .bucket("fail-src")
        .key("old/app.zip")
        .send()
        .await
        .expect("source must survive failed transition copy");

    // #12: the row says why it failed, naming the destination key, and the
    // failures list says how many the ring keeps.
    let row = lifecycle_row(&admin, &server.endpoint(), "failed-move").await;
    let last_error = row["last_error"].as_str().unwrap_or_default();
    assert!(
        last_error.contains("missing-destination/cold/app.zip"),
        "{row}"
    );
    assert_eq!(row["failed_total"].as_i64(), Some(1), "{row}");
    let failures: Value = admin
        .get(format!(
            "{}/_/api/admin/jobs/lifecycle:failed-move/failures",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(failures["total"].as_i64(), Some(1), "{failures}");
}

/// The `GET /jobs` row of lifecycle rule `rule`.
async fn lifecycle_row(admin: &reqwest::Client, endpoint: &str, rule: &str) -> Value {
    let body: Value = admin
        .get(format!("{endpoint}/_/api/admin/jobs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == format!("lifecycle:{rule}"))
        .unwrap_or_else(|| panic!("no row for {rule}: {body}"))
        .clone()
}

/// Poll the row of `rule` until `done(row)` holds (10 s).
async fn wait_for_row(
    admin: &reqwest::Client,
    endpoint: &str,
    rule: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let row = lifecycle_row(admin, endpoint, rule).await;
        if done(&row) {
            return row;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "row never got there: {row}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn job_post(admin: &reqwest::Client, endpoint: &str, rule: &str, action: &str) -> u16 {
    admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/lifecycle:{rule}/{action}"
        ))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

const SLOW_LIFECYCLE_YAML: &str = r#"
lifecycle:
  enabled: true
  tick_interval: "1h"
  rules:
    - name: slow-expire
      enabled: true
      bucket: life-slow
      prefix: "old/"
      expire_after: "1ms"
      batch_size: 4
"#;

/// #2 + #10: a running lifecycle run shows LIVE progress in `GET /jobs`
/// and in its run entry; Pause stops it at the next object (status
/// `paused`, the rest of the objects stay), and Kill stops a run too
/// (status `cancelled`). Before, Pause only flagged the rule and the run
/// went on to the end, and the row showed zeros until the run ended.
#[tokio::test]
async fn test_lifecycle_pause_and_kill_stop_a_running_run_with_live_progress() {
    let server = TestServer::builder()
        .bucket("life-slow")
        .env("DGP_TEST_LIFECYCLE_OBJECT_STALL_MS", "60")
        .extra_yaml_storage_section(SLOW_LIFECYCLE_YAML)
        .build()
        .await;
    let endpoint = server.endpoint();
    let client = server.s3_client().await;
    const N: usize = 40;
    for i in 0..N {
        client
            .put_object()
            .bucket("life-slow")
            .key(format!("old/{i:03}.txt"))
            .body(ByteStream::from(b"expired".to_vec()))
            .send()
            .await
            .unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let admin = admin_http_client(&endpoint).await;

    // ── Pause ──
    let resp: Value = admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/lifecycle:slow-expire/run-now"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run_id = resp["run_id"].as_i64().expect("run id");
    let row = wait_for_row(&admin, &endpoint, "slow-expire", |r| {
        r["status"] == "running" && r["progress"]["deleted"].as_i64().unwrap_or(0) >= 4
    })
    .await;
    assert!(
        row["detail"]["live"]["objects_scanned"].as_i64().unwrap() >= 4,
        "{row}"
    );
    let runs: Value = admin
        .get(format!(
            "{endpoint}/_/api/admin/jobs/lifecycle:slow-expire/runs"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(runs["runs"][0]["status"], "running", "{runs}");
    assert!(
        runs["runs"][0]["objects_deleted"].as_i64().unwrap_or(0) >= 4,
        "the running entry is live: {runs}"
    );
    assert_eq!(
        job_post(&admin, &endpoint, "slow-expire", "pause").await,
        204
    );
    let run = common::wait_for_job_run(&admin, &endpoint, "lifecycle:slow-expire", run_id).await;
    assert_eq!(run["status"], "paused", "{run}");
    let deleted = run["objects_deleted"].as_i64().unwrap();
    assert!(deleted >= 4 && deleted < N as i64, "{run}");
    let row = lifecycle_row(&admin, &endpoint, "slow-expire").await;
    assert_eq!(row["status"], "paused", "{row}");
    assert_eq!(row["paused"], true, "{row}");
    let left = client
        .list_objects_v2()
        .bucket("life-slow")
        .prefix("old/")
        .send()
        .await
        .unwrap()
        .key_count()
        .unwrap_or(0);
    assert_eq!(
        left as i64,
        N as i64 - deleted,
        "the paused run deleted nothing more"
    );

    // ── Kill ──
    assert_eq!(
        job_post(&admin, &endpoint, "slow-expire", "resume").await,
        204
    );
    let resp: Value = admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/lifecycle:slow-expire/run-now"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run_id = resp["run_id"].as_i64().expect("run id");
    wait_for_row(&admin, &endpoint, "slow-expire", |r| {
        r["status"] == "running" && r["progress"]["deleted"].as_i64().unwrap_or(0) >= 1
    })
    .await;
    assert_eq!(
        job_post(&admin, &endpoint, "slow-expire", "kill").await,
        202
    );
    let run = common::wait_for_job_run(&admin, &endpoint, "lifecycle:slow-expire", run_id).await;
    assert_eq!(run["status"], "cancelled", "{run}");
    assert_eq!(
        job_post(&admin, &endpoint, "slow-expire", "kill").await,
        409,
        "no run left to kill"
    );
}

/// #6: a new enabled rule is first due one tick after the proxy sees it,
/// and the row says when. Before, it ran (and deleted) on the next tick.
#[tokio::test]
async fn test_lifecycle_new_rule_is_first_due_one_tick_later() {
    let server = TestServer::builder()
        .bucket("life-pause")
        .extra_yaml_storage_section(
            SLOW_LIFECYCLE_YAML
                .replace("life-slow", "life-pause")
                .as_str(),
        )
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let row = lifecycle_row(&admin, &server.endpoint(), "slow-expire").await;
    let due = row["next_due_at"]
        .as_i64()
        .expect("a new rule has a first run");
    assert!(
        due >= now + 3600 - 120 && due <= now + 3600 + 5,
        "first due one tick after boot: due={due} now={now} {row}"
    );
    assert_eq!(row["detail"]["tick_interval"], "1h", "{row}");
}

/// #13: run-now refuses a rule whose bucket a maintenance job gates, and
/// says which bucket.
#[tokio::test]
async fn test_lifecycle_run_now_refuses_while_a_maintenance_job_gates_the_bucket() {
    let server = TestServer::builder()
        .bucket("life-gated")
        .env("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "400")
        .extra_yaml_storage_section(
            SLOW_LIFECYCLE_YAML
                .replace("life-slow", "life-gated")
                .as_str(),
        )
        .build()
        .await;
    let endpoint = server.endpoint();
    let client = server.s3_client().await;
    for i in 0..10 {
        client
            .put_object()
            .bucket("life-gated")
            .key(format!("old/{i}.txt"))
            .body(ByteStream::from(b"x".to_vec()))
            .send()
            .await
            .unwrap();
    }
    let admin = admin_http_client(&endpoint).await;
    let started = admin
        .post(format!("{endpoint}/_/api/admin/jobs/backfill-metadata"))
        .json(&serde_json::json!({ "buckets": ["life-gated"] }))
        .send()
        .await
        .unwrap();
    assert!(started.status().is_success(), "{}", started.status());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let resp = admin
            .post(format!(
                "{endpoint}/_/api/admin/jobs/lifecycle:slow-expire/run-now"
            ))
            .send()
            .await
            .unwrap();
        let code = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if code == 409 {
            assert!(
                text.contains("bucket 'life-gated' has an active maintenance job"),
                "{text}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run-now never refused under the gate: {code} {text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
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
        .header("user-agent", "dgp-audit-probe/1")
        .send()
        .await
        .unwrap();
    assert!(pause.status().is_success(), "pause: {}", pause.status());

    // The audit entry carries the REAL request's client info, not the empty
    // headers the job action used to pass.
    let audit: Value = admin
        .get(format!("{}/_/api/admin/audit?limit=50", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "lifecycle_pause")
        .expect("pause is audited")
        .clone();
    assert_eq!(entry["ua"], "dgp-audit-probe/1", "{entry}");

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

const LIFECYCLE_RETAIN_NEWEST_YAML: &str = r#"
lifecycle:
  enabled: true
  tick_interval: "1h"
  rules:
    - name: keep-last-two
      enabled: true
      bucket: retain-bucket
      prefix: "nightly/"
      action:
        type: retain-newest
        count: 2
        qualify:
          min_size_bytes: 1048576
      batch_size: 100
"#;

/// The headline use case, end to end: keep the newest 2 real backups, delete
/// older ones, and a stray 0-byte file must be IGNORED (never counted, never
/// deleted) so it can't push a real backup out of the keep set.
#[tokio::test]
async fn test_lifecycle_retain_newest_keeps_reals_and_ignores_junk() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(LIFECYCLE_RETAIN_NEWEST_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    client
        .create_bucket()
        .bucket("retain-bucket")
        .send()
        .await
        .ok();

    // A >=1 MiB body qualifies; the empty file does not. Seed OLDEST first so
    // upload order == age order (created_at is monotonic per PUT).
    let big = vec![b'x'; 2 * 1024 * 1024];
    for key in [
        "nightly/dump-1.bin",
        "nightly/dump-2.bin",
        "nightly/dump-3.bin",
    ] {
        client
            .put_object()
            .bucket("retain-bucket")
            .key(key)
            .body(ByteStream::from(big.clone()))
            .send()
            .await
            .expect("seed dump");
    }
    // The stray junk file, newest of all — a naive "keep newest 2" would keep
    // THIS plus dump-3 and delete a real backup. min_size must exclude it.
    client
        .put_object()
        .bucket("retain-bucket")
        .key("nightly/README")
        .body(ByteStream::from(Vec::new()))
        .send()
        .await
        .expect("seed junk");

    let admin = admin_http_client(&server.endpoint()).await;

    // Preview: 1 deletion (dump-1), README ignored.
    let preview: Value = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:keep-last-two/preview",
            server.endpoint()
        ))
        .send()
        .await
        .expect("preview")
        .json()
        .await
        .unwrap();
    assert_eq!(preview["status"].as_str(), Some("preview"));
    assert_eq!(
        preview["objects_affected"].as_i64(),
        Some(1),
        "exactly dump-1 should delete: {preview}"
    );
    assert_eq!(
        preview["objects_ignored"].as_i64(),
        Some(1),
        "README must be ignored, not counted: {preview}"
    );

    // Run it.
    let run = lifecycle_run_now_and_wait(&admin, &server.endpoint(), "keep-last-two").await;
    assert_eq!(run["objects_processed"].as_i64(), Some(1), "{run}");
    assert_eq!(run["errors"].as_i64(), Some(0), "{run}");

    // dump-1 (oldest real) is gone.
    assert!(
        client
            .get_object()
            .bucket("retain-bucket")
            .key("nightly/dump-1.bin")
            .send()
            .await
            .is_err(),
        "oldest real backup should be deleted"
    );
    // The two newest reals AND the junk file all survive.
    for key in ["nightly/dump-2.bin", "nightly/dump-3.bin", "nightly/README"] {
        client
            .get_object()
            .bucket("retain-bucket")
            .key(key)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{key} must be preserved: {e:?}"));
    }
}

// ═════════════════════════════════════════════════════════════════════
// Regression: a delete-action rule missing `expire_after` is a CONFIG
// validation error, not an internal server error. Preview/run-now must
// return 400 BAD_REQUEST, not 500. Previously the handler mapped every
// Err(String) from run_or_preview to INTERNAL_SERVER_ERROR.
// See lifecycle::classify_lifecycle_run_error.
// ═════════════════════════════════════════════════════════════════════

const LIFECYCLE_DELETE_MISSING_EXPIRE_AFTER_YAML: &str = r#"
lifecycle:
  enabled: true
  tick_interval: "1h"
  rules:
    - name: expire-old
      enabled: true
      bucket: life-bucket
      prefix: ""
      # No expire_after — a delete (expire) action REQUIRES it. The run/preview
      # path returns Err("lifecycle rule 'expire-old' delete action requires
      # expire_after"); the admin handler must classify that as 400, not 500.
      batch_size: 100
      include_globs: ["old/**"]
      exclude_globs: []
"#;

#[tokio::test]
async fn test_lifecycle_preview_delete_without_expire_after_is_400_not_500() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(LIFECYCLE_DELETE_MISSING_EXPIRE_AFTER_YAML)
        .build()
        .await;

    let admin = admin_http_client(&server.endpoint()).await;

    // Preview: validation error → 400 BAD_REQUEST (was 500 before the fix).
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:expire-old/preview",
            server.endpoint()
        ))
        .send()
        .await
        .expect("preview request");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(
        status.as_u16(),
        400,
        "preview of delete rule missing expire_after must be 400 (validation), got {}: {body}",
        status
    );
    assert!(
        body.contains("requires expire_after"),
        "400 body should name the missing field, got: {body}"
    );

    // run-now: same validation error → 400 BAD_REQUEST (was 500 before the fix).
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:expire-old/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(
        status.as_u16(),
        400,
        "run-now of delete rule missing expire_after must be 400 (validation), got {}: {body}",
        status
    );
    assert!(
        body.contains("requires expire_after"),
        "400 body should name the missing field, got: {body}"
    );

    // C6: the rejected run-now must NOT have recorded a spurious FAILED run row
    // (the pre-gate now 400s BEFORE acquiring a lease / opening a run). The Jobs
    // Runs view must stay empty for this rule.
    let runs: Value = admin
        .get(format!(
            "{}/_/api/admin/jobs/lifecycle:expire-old/runs",
            server.endpoint()
        ))
        .send()
        .await
        .expect("runs history")
        .json()
        .await
        .expect("runs json");
    let history = runs["runs"].as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(
        history, 0,
        "a fatal rule's rejected run-now must create no run row, got {runs}"
    );
}

// ── review second pass (failing tests for findings) ──────────────────────

/// Review-2 (5333c4bc): the re-HEAD before delete compares `created_at`
/// from the LIST snapshot with `created_at` from a fresh HEAD. On S3 a
/// passthrough key with a cold metadata cache keeps the lite LIST entry
/// (`LastModified`, ms precision), while HEAD returns `dg-created-at` (µs).
/// They never match, so every such object is "Changed" and never expires.
/// MinIO only (self-skips locally); the filesystem backend reads one xattr
/// for both and cannot show it.
#[tokio::test]
async fn review2_lifecycle_s3_deletes_passthrough_object_with_cold_cache() {
    crate::skip_unless_minio!();
    let prefix = format!(
        "lc-cold-{}/",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let yaml = format!(
        r#"
lifecycle:
  enabled: true
  tick_interval: "1h"
  rules:
    - name: expire-logs
      enabled: true
      bucket: deltaglider-test
      prefix: "{prefix}"
      expire_after: "1ms"
      batch_size: 100
"#
    );
    let mut server = TestServer::builder()
        .s3_endpoint(&common::minio_endpoint_url())
        .bucket("deltaglider-test")
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(&yaml)
        .build()
        .await;
    let key = format!("{prefix}app.log");
    server
        .s3_client()
        .await
        .put_object()
        .bucket("deltaglider-test")
        .key(&key)
        .body(ByteStream::from(b"x".to_vec()))
        .send()
        .await
        .unwrap();
    server.respawn_with_env(&[]).await; // cold metadata cache (= next hourly tick)
    let admin = admin_http_client(&server.endpoint()).await;
    let run = lifecycle_run_now_and_wait(&admin, &server.endpoint(), "expire-logs").await;
    assert_eq!(run["objects_processed"].as_i64(), Some(1), "{run}");
}

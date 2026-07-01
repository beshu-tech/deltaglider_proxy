// SPDX-License-Identifier: GPL-3.0-only

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

/// Poll a rule's run-history until its latest run reaches a terminal status,
/// then return that latest run object. run-now is fire-and-forget (202 + a
/// background task) — a large sync can't block the HTTP response — so tests
/// assert on the settled run-history row, not the run-now response body.
async fn wait_for_latest_run(admin: &reqwest::Client, endpoint: &str, rule: &str) -> Value {
    wait_for_run_after(admin, endpoint, rule, -1).await
}

/// The run row with the highest `id` in a rule's history (the history endpoint
/// orders by `started_at DESC`, but ties within the same second are arbitrary,
/// so we pick by `id` rather than array position).
fn newest_run(h: &Value) -> Option<&Value> {
    h["runs"]
        .as_array()?
        .iter()
        .max_by_key(|run| run["id"].as_i64().unwrap_or(i64::MIN))
}

fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "succeeded" | "failed" | "completed_with_errors" | "cancelled" | "stopped"
    )
}

/// Fire run-now and return the settled NEW run row. Baselines the latest run
/// id first, tolerates the brief 409 "already running" window while a prior
/// background run releases its lease, then waits for a genuinely new terminal
/// run. This is THE helper for tests that fire run-now more than once.
async fn fire_run_now(admin: &reqwest::Client, endpoint: &str, rule: &str) -> Value {
    let before = latest_run_id(admin, endpoint, rule).await;
    let url = format!("{endpoint}/_/api/admin/jobs/replication:{rule}/run-now");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let code = admin
            .post(&url)
            .send()
            .await
            .expect("run-now")
            .status()
            .as_u16();
        if code == 202 {
            break;
        }
        assert_eq!(
            code, 409,
            "run-now: unexpected status {code} for rule '{rule}'"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "run-now for rule '{rule}' kept returning 409 (lease never released)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    wait_for_run_after(admin, endpoint, rule, before).await
}

/// `id` of the latest run in a rule's history, or 0 if none yet. Used to
/// baseline a NEW run before firing a subsequent run-now (the history row for
/// the new run only appears once its background task starts, so the plain
/// `wait_for_latest_run` would otherwise return the PREVIOUS run's terminal row).
async fn latest_run_id(admin: &reqwest::Client, endpoint: &str, rule: &str) -> i64 {
    let url = format!("{endpoint}/_/api/admin/jobs/replication:{rule}/runs");
    let h: Value = admin.get(&url).send().await.unwrap().json().await.unwrap();
    newest_run(&h)
        .and_then(|run| run["id"].as_i64())
        .unwrap_or(0)
}

/// Like `wait_for_latest_run`, but waits for a run whose `id` is strictly
/// greater than `after_id` (i.e. a genuinely NEW run) to reach a terminal
/// status. Essential for tests that fire run-now multiple times and read each
/// run's totals in turn.
async fn wait_for_run_after(
    admin: &reqwest::Client,
    endpoint: &str,
    rule: &str,
    after_id: i64,
) -> Value {
    let url = format!("{endpoint}/_/api/admin/jobs/replication:{rule}/runs");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let h: Value = admin.get(&url).send().await.unwrap().json().await.unwrap();
        if let Some(run) = newest_run(&h) {
            let id = run["id"].as_i64().unwrap_or(0);
            let st = run["status"].as_str().unwrap_or("");
            if id > after_id && is_terminal(st) {
                return run.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "new run (id > {after_id}) for rule '{rule}' did not settle in 10s; last history: {h}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

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

// Fast tick so the event-driven consumer (and reconcile scheduler) wake
// quickly in tests. The consumer drains the outbox each tick.
const EVENT_DRIVEN_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"5s\"
  rules:
    - name: ev-a-to-b
      enabled: true
      source:
        bucket: ev-src
        prefix: \"\"
      destination:
        bucket: ev-dst
        prefix: \"\"
      interval: \"24h\"
      batch_size: 100
      replicate_deletes: true
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

const SCHEDULER_EMPTY_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"5s\"
  rules: []
";

const PREFIX_NORMALIZATION_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: prefix-normalization-rule
      enabled: true
      source:
        bucket: norm-src
        prefix: \"source\"
      destination:
        bucket: norm-dst
        prefix: \"dest\"
      interval: \"1h\"
      batch_size: 100
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
            "{}/_/api/admin/jobs/replication:repl-a-to-b/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request");
    // run-now is fire-and-forget: 202 Accepted, run continues in the background.
    assert_eq!(resp.status().as_u16(), 202, "run-now should be accepted");
    // Wait for the background run to settle, then assert the outcome.
    let run = wait_for_latest_run(&admin, &server.endpoint(), "repl-a-to-b").await;
    assert_eq!(
        run["status"].as_str(),
        Some("succeeded"),
        "run status: {run}"
    );
    assert_eq!(run["objects_processed"].as_i64(), Some(3), "copied: {run}");

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
            "{}/_/api/admin/jobs/replication:repl-a-to-b/runs",
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
    assert_eq!(runs[0]["triggered_by"].as_str(), Some("run-now"));
    assert_eq!(runs[0]["objects_processed"].as_i64(), Some(3));
}

const SPARSE_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: sparse-rule
      enabled: true
      source:
        bucket: sparse-src
        prefix: \"\"
      destination:
        bucket: sparse-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
      conflict: content-diff
";

/// Prefix-tree dest oracle: a SPARSE destination (one subtree already mirrored,
/// another entirely absent) converges in one run. The absent subtree is
/// bulk-copied without per-object dest HEADs (proven absent from a common-prefix
/// probe); the present subtree is descended and compared. Exercises
/// build_dest_oracle's descend/absent classification end-to-end.
#[tokio::test]
async fn test_replication_sparse_dest_prefix_tree() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(SPARSE_RULE_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    for b in ["sparse-src", "sparse-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // Source: two subtrees + a root object.
    for (key, body) in [
        ("present/a.txt", &b"alpha"[..]),
        ("present/b.txt", &b"bravo"[..]),
        ("absent/x.txt", &b"xray"[..]),
        ("absent/deep/y.txt", &b"yankee"[..]),
        ("root.txt", &b"rootbytes"[..]),
    ] {
        client
            .put_object()
            .bucket("sparse-src")
            .key(key)
            .body(ByteStream::from(body.to_vec()))
            .send()
            .await
            .expect("seed src");
    }
    // Dest: pre-seed ONLY the `present/` subtree, byte-identical (content-diff
    // should SKIP these), so the oracle descends present/ and bulk-copies the
    // rest (absent/ subtree + root.txt).
    for (key, body) in [
        ("present/a.txt", &b"alpha"[..]),
        ("present/b.txt", &b"bravo"[..]),
    ] {
        client
            .put_object()
            .bucket("sparse-dst")
            .key(key)
            .body(ByteStream::from(body.to_vec()))
            .send()
            .await
            .expect("seed dst");
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:sparse-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 202);
    let run = wait_for_latest_run(&admin, &server.endpoint(), "sparse-rule").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    // The 2 byte-identical present/ objects are SKIPPED (content-diff); the 3
    // absent objects (absent/x, absent/deep/y, root.txt) are copied.
    assert_eq!(
        run["objects_processed"].as_i64(),
        Some(3),
        "only the absent subtree + root copy; present/ skipped: {run}"
    );

    // Every source object now exists on dest (full convergence).
    for key in [
        "present/a.txt",
        "present/b.txt",
        "absent/x.txt",
        "absent/deep/y.txt",
        "root.txt",
    ] {
        client
            .get_object()
            .bucket("sparse-dst")
            .key(key)
            .send()
            .await
            .unwrap_or_else(|_| panic!("dest missing {key}"));
    }
}

/// Scheduler regression: a rule added via the storage section should run
/// automatically when due, without calling the run-now endpoint.
#[tokio::test]
async fn test_replication_scheduler_copies_due_rule() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(SCHEDULER_EMPTY_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    for b in ["sched-src", "sched-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    client
        .put_object()
        .bucket("sched-src")
        .key("hello.txt")
        .body(ByteStream::from(b"hello from scheduler".to_vec()))
        .send()
        .await
        .expect("seed scheduler source");

    let admin = admin_http_client(&server.endpoint()).await;
    let apply = admin
        .put(format!(
            "{}/_/api/admin/config/section/storage",
            server.endpoint()
        ))
        .json(&serde_json::json!({
            "replication": {
                "enabled": true,
                "tick_interval": "5s",
                "rules": [{
                    "name": "scheduler-rule",
                    "enabled": true,
                    "source": { "bucket": "sched-src", "prefix": "" },
                    "destination": { "bucket": "sched-dst", "prefix": "" },
                    "interval": "30s",
                    "batch_size": 100
                }]
            }
        }))
        .send()
        .await
        .expect("apply storage replication section");
    assert_eq!(apply.status().as_u16(), 200, "apply response: {:?}", apply);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if client
            .head_object()
            .bucket("sched-dst")
            .key("hello.txt")
            .send()
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "scheduled replication did not copy sched-dst/hello.txt before timeout"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    // The file appearing on the destination only proves the COPY landed.
    // The run-history row is updated AFTER the copy: workflow is
    //   list → copy → set status=succeeded.
    // On a slow CI runner (sccache cold, tokio runtime contention) the
    // assertion below can race ahead of the status flip and observe
    // "running". Poll for a terminal status (succeeded|failed) before
    // asserting — same shape as the file-arrival poll above. 5 s is
    // plenty since the file already arrived; if status never settles
    // we have a real bug worth surfacing as the timeout.
    let history_url = format!(
        "{}/_/api/admin/jobs/replication:scheduler-rule/runs",
        server.endpoint()
    );
    let status_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let hist: Value = loop {
        let h: Value = admin
            .get(&history_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let runs = h["runs"].as_array().expect("history runs");
        if let Some(first) = runs.first() {
            let status = first["status"].as_str().unwrap_or("");
            if status == "succeeded" || status == "failed" {
                break h;
            }
        }
        assert!(
            std::time::Instant::now() < status_deadline,
            "scheduler run status did not reach a terminal state in 5s; last history: {h}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };

    let runs = hist["runs"].as_array().expect("history runs");
    assert_eq!(runs.len(), 1, "expected exactly one scheduler run: {hist}");
    assert_eq!(runs[0]["status"].as_str(), Some("succeeded"));
    assert_eq!(runs[0]["triggered_by"].as_str(), Some("scheduler"));
    assert_eq!(runs[0]["objects_processed"].as_i64(), Some(1));
}

/// Prefix normalization regression: direct YAML may use `prefix: "source"`
/// without a trailing slash. The worker must list `source/`, not raw
/// `source`, otherwise a sibling key like `source-other/file.txt` is listed
/// and then rejected by the normalized planner as outside the source prefix.
#[tokio::test]
async fn test_replication_normalizes_prefixes_at_worker_boundaries() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(PREFIX_NORMALIZATION_RULE_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    for b in ["norm-src", "norm-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    client
        .put_object()
        .bucket("norm-src")
        .key("source/file.txt")
        .body(ByteStream::from(b"copy me".to_vec()))
        .send()
        .await
        .expect("seed normalized source key");
    client
        .put_object()
        .bucket("norm-src")
        .key("source-other/poison.txt")
        .body(ByteStream::from(b"must not be listed".to_vec()))
        .send()
        .await
        .expect("seed sibling prefix key");

    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:prefix-normalization-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 202);
    let run = wait_for_latest_run(&admin, &server.endpoint(), "prefix-normalization-rule").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    assert_eq!(run["objects_processed"].as_i64(), Some(1), "{run}");

    client
        .head_object()
        .bucket("norm-dst")
        .key("dest/file.txt")
        .send()
        .await
        .expect("normalized destination key exists");
    assert!(
        client
            .head_object()
            .bucket("norm-dst")
            .key("dest-other/poison.txt")
            .send()
            .await
            .is_err(),
        "sibling prefix key must not be replicated"
    );
}

const MIDRUN_PAUSE_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: midpause-rule
      enabled: true
      source:
        bucket: mpz-src
        prefix: \"\"
      destination:
        bucket: mpz-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 1
";

/// A pause issued WHILE a run is in flight must stop the run promptly — not let
/// it run to completion. (The bug: pause only blocked the scheduler from
/// STARTING a run; the worker never re-checked the flag mid-sweep, so a paused
/// run kept going and lingered as "running".) The worker now re-reads the DB
/// `paused` flag at every page boundary, settles as a terminal (cancelled)
/// status, and preserves the cursor for resume.
///
/// Deterministic enough: `batch_size: 1` makes each object its own page, so the
/// page-boundary check fires between every object; with 60 objects and a pause
/// landing early, the run copies strictly fewer than all 60 and ends terminal.
#[tokio::test]
async fn test_replication_pause_mid_run_stops_promptly_and_preserves_cursor() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(MIDRUN_PAUSE_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["mpz-src", "mpz-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    // 60 objects, one per page (batch_size=1).
    for i in 0..60 {
        client
            .put_object()
            .bucket("mpz-src")
            .key(format!("obj-{i:03}.txt"))
            .body(ByteStream::from(format!("payload-{i}").into_bytes()))
            .send()
            .await
            .expect("seed");
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();

    // Fire run-now (fire-and-forget, 202), then pause almost immediately so the
    // pause lands while the background worker is still paginating.
    let resp = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:midpause-rule/run-now"
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 202, "run-now accepted");

    // Let a few pages copy, then pause.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let p = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:midpause-rule/pause"
        ))
        .send()
        .await
        .expect("pause");
    assert_eq!(p.status().as_u16(), 204, "pause accepted");

    // Wait for the background run to settle in the run-history.
    let body = wait_for_latest_run(&admin, &ep, "midpause-rule").await;

    // The run is terminal (NOT running) and stopped EARLY — fewer than all 60
    // copied. (If the pause raced in after the last page, copied could be 60 and
    // status succeeded; assert the core invariant that it's terminal + that the
    // status is one we expect, and — when it did stop early — that it's marked
    // stopped/cancelled with the cursor kept.)
    let copied = body["objects_processed"].as_i64().unwrap_or(-1);
    let status = body["status"].as_str().unwrap_or("");
    // A mid-run pause settles as "stopped" (normalized to `cancelled` in the
    // jobs LIST view). If the pause raced in only after the final page, the run
    // may have finished "succeeded".
    assert!(
        matches!(
            status,
            "stopped" | "cancelled" | "succeeded" | "completed_with_errors"
        ),
        "run must end terminal, got status={status} body={body}"
    );
    if matches!(status, "stopped" | "cancelled") {
        assert!(
            copied < 60,
            "a stopped run must NOT have copied all 60: {body}"
        );
    }
    assert!(copied <= 60, "never copies more than seeded: {body}");

    // The dest must NOT have all 60 if we stopped early; and a resume (run-now
    // after resume) must finish the rest from the preserved cursor.
    admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:midpause-rule/resume"
        ))
        .send()
        .await
        .expect("resume");
    // Drain to completion: run-now until the dest holds all 60. run-now is
    // fire-and-forget; a prior background run may still be releasing its lease
    // (409 "already running"), so tolerate that and retry.
    let mut guard = 0;
    let mut r = Value::Null;
    loop {
        let resp = admin
            .post(format!(
                "{ep}/_/api/admin/jobs/replication:midpause-rule/run-now"
            ))
            .send()
            .await
            .expect("run-now drain");
        let code = resp.status().as_u16();
        assert!(
            code == 202 || code == 409,
            "run-now drain: unexpected status {code}"
        );
        if code == 202 {
            r = wait_for_latest_run(&admin, &ep, "midpause-rule").await;
        }
        let listed = client
            .list_objects_v2()
            .bucket("mpz-dst")
            .send()
            .await
            .unwrap();
        if listed.contents().len() >= 60 {
            break;
        }
        guard += 1;
        assert!(guard < 20, "resume failed to drain all objects: last={r}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let final_dst = client
        .list_objects_v2()
        .bucket("mpz-dst")
        .send()
        .await
        .unwrap();
    assert_eq!(
        final_dst.contents().len(),
        60,
        "all objects eventually replicate after resume"
    );
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
            "{}/_/api/admin/jobs/replication:paused-rule/pause",
            server.endpoint()
        ))
        .send()
        .await
        .expect("pause");
    assert_eq!(resp.status().as_u16(), 204);

    // run-now is a deliberate ONE-OFF: it runs even a paused rule once (202),
    // without un-pausing it.
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:paused-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(
        resp.status().as_u16(),
        202,
        "paused rule still runs one-off"
    );
    let run = wait_for_latest_run(&admin, &server.endpoint(), "paused-rule").await;
    // The one-off is accepted (202) and starts, but the background worker now
    // honors the paused flag mid-run and stops promptly (settles stopped/
    // cancelled). Either way it did NOT error and copied nothing (empty source).
    assert!(
        matches!(
            run["status"].as_str(),
            Some("succeeded") | Some("stopped") | Some("cancelled")
        ),
        "paused one-off must settle terminal without error: {run}"
    );

    // The rule is STILL paused after the one-off (run-now doesn't resume it).
    let jobs: Value = admin
        .get(format!("{}/_/api/admin/jobs", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let paused = jobs["jobs"]
        .as_array()
        .and_then(|a| a.iter().find(|j| j["id"] == "replication:paused-rule"))
        .and_then(|j| j["paused"].as_bool());
    assert_eq!(
        paused,
        Some(true),
        "one-off must not un-pause the rule: {jobs}"
    );
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
            "{}/_/api/admin/jobs/replication:multipage-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 202);
    let body = wait_for_latest_run(&admin, &server.endpoint(), "multipage-rule").await;
    // Pre-fix: copied was capped at batch_size=5. Post-fix: 17.
    assert_eq!(
        body["objects_processed"].as_i64().unwrap_or(-1),
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
    let body = fire_run_now(&admin, &server.endpoint(), "multipage-rule").await;
    assert_eq!(
        body["objects_processed"].as_i64().unwrap_or(-1),
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
    // Seed source with 4 keys (a/b/c/d).
    for key in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        client
            .put_object()
            .bucket("del-src")
            .key(key)
            .body(ByteStream::from(b"x".to_vec()))
            .send()
            .await
            .unwrap();
    }
    // H2 fix verification: an unrelated object on the destination
    // bucket (not written by replication) MUST NOT be deleted. Pre-fix
    // any dest key whose name didn't appear on source got nuked.
    client
        .put_object()
        .bucket("del-dst")
        .key("manual.txt")
        .body(ByteStream::from(b"hand-placed by an operator".to_vec()))
        .send()
        .await
        .unwrap();

    let admin = admin_http_client(&server.endpoint()).await;

    // First run: forward-copy 4 keys onto dst (with provenance markers).
    // Delete pass: nothing to delete (each replicated key still on src).
    // `manual.txt` is preserved because it has no provenance marker.
    fire_run_now(&admin, &server.endpoint(), "delete-rule").await;

    for key in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        client
            .head_object()
            .bucket("del-dst")
            .key(key)
            .send()
            .await
            .expect("replicated key on dst");
    }
    client
        .head_object()
        .bucket("del-dst")
        .key("manual.txt")
        .send()
        .await
        .expect("H2: manual.txt (no provenance marker) must survive first run");

    // Now delete d.txt from source. Next replication run should delete
    // d.txt from destination (it carries the provenance marker), but
    // leave manual.txt alone.
    client
        .delete_object()
        .bucket("del-src")
        .key("d.txt")
        .send()
        .await
        .unwrap();

    fire_run_now(&admin, &server.endpoint(), "delete-rule").await;

    // d.txt should be GONE from dst (replicated delete).
    let head_d = client
        .head_object()
        .bucket("del-dst")
        .key("d.txt")
        .send()
        .await;
    assert!(
        head_d.is_err(),
        "replicated d.txt must be deleted from destination after source delete"
    );

    // manual.txt MUST still be there — no provenance marker, not ours.
    client
        .head_object()
        .bucket("del-dst")
        .key("manual.txt")
        .send()
        .await
        .expect("H2 REGRESSION: manual.txt without provenance marker was deleted");

    // Other replicated keys should still be there.
    for key in ["a.txt", "b.txt", "c.txt"] {
        client
            .head_object()
            .bucket("del-dst")
            .key(key)
            .send()
            .await
            .expect("legit replicated key remains");
    }
}

/// M1 fix: pause/resume on a non-existent rule must 404 WITHOUT
/// creating a ghost DB row. Pre-fix the handler called
/// replication_ensure_state before checking config, leaving an
/// orphan row even though the response was 404.
#[tokio::test]
async fn test_pause_resume_ghost_rule_returns_404_without_inserting_row() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(RULE_YAML)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Pause + resume on a non-existent rule.
    for action in ["pause", "resume"] {
        let resp = admin
            .post(format!(
                "{}/_/api/admin/jobs/replication:ghost-rule/{}",
                server.endpoint(),
                action
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            404,
            "M1: {} on a non-existent rule must 404",
            action
        );
    }

    // Verify the overview doesn't list the ghost rule (no orphan row).
    let resp = admin
        .get(format!("{}/_/api/admin/jobs", server.endpoint()))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let names: Vec<&str> = body["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !names.contains(&"ghost-rule"),
        "M1 REGRESSION: ghost-rule appeared in overview after 404, names={:?}",
        names
    );
}

/// A disabled rule still runs a deliberate ONE-OFF via run-now (202) — it does
/// not flip `enabled`. The scheduler stays off; only the manual trigger runs.
#[tokio::test]
async fn test_run_now_disabled_rule_runs_one_off() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(
            "
replication:
  enabled: true
  rules:
    - name: disabled-rule
      enabled: false
      source: { bucket: dis-src, prefix: \"\" }
      destination: { bucket: dis-dst, prefix: \"\" }
      interval: \"1h\"
",
        )
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["dis-src", "dis-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    client
        .put_object()
        .bucket("dis-src")
        .key("a.txt")
        .body(ByteStream::from(b"alpha".to_vec()))
        .send()
        .await
        .expect("seed");
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:disabled-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        202,
        "disabled rule still runs a one-off"
    );
    let run = wait_for_latest_run(&admin, &server.endpoint(), "disabled-rule").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    assert_eq!(run["objects_processed"].as_i64(), Some(1), "{run}");
    // The object landed on the dest even though the rule is disabled.
    client
        .get_object()
        .bucket("dis-dst")
        .key("a.txt")
        .send()
        .await
        .expect("one-off copied the object to dest");
}

#[tokio::test]
async fn test_run_now_rejects_globally_disabled_replication() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(
            "
replication:
  enabled: false
  rules:
    - name: orphan
      enabled: true
      source: { bucket: x, prefix: \"\" }
      destination: { bucket: y, prefix: \"\" }
      interval: \"1h\"
",
        )
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:orphan/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "M2: run-now must reject when replication is globally disabled"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("globally disabled"), "got: {}", body);
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
            "{}/_/api/admin/jobs/replication:mp-etag-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    wait_for_latest_run(&admin, &server.endpoint(), "mp-etag-rule").await;

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

// ════════════════════════════════════════════════════════════════════
// H2 (fourth-wave) — replication delete-pass provenance edge cases
// ════════════════════════════════════════════════════════════════════
//
// The fourth-wave H2 fix gates `run_delete_pass` on a per-rule
// provenance marker (`x-amz-meta-dg-replication-rule = <rule.name>`)
// stamped at copy time. The basic "operator placed an unrelated
// object" path is already covered by `test_replication_replicate_
// deletes_removes_orphans` (manual.txt without any marker survives).
//
// What was NOT covered before this batch:
//
//   **Sibling-rule marker mismatch**: an object on dest bearing
//   a different rule's marker (`dg-replication-rule = sibling-b`)
//   must NOT be deleted by THIS rule's delete pass — even when
//   its source-side counterpart is missing.
//
// Pre-fix the run_delete_pass had no provenance check at all, so any
// dest key whose source counterpart was missing was deleted.
// Post-fix the marker must equal the running rule's `name` exactly.
//
// Note on test mechanics: clients cannot spoof `dg-*` metadata via
// the S3 PUT path — `extract_user_metadata` in `src/api/handlers/
// mod.rs` filters them out as a hardening measure. To plant a foreign
// marker we therefore configure TWO rules (`sibling-a`, `sibling-b`)
// pointing at overlapping destination prefixes; each rule's `copy_one`
// stamps its own name. Then we run rule A, run rule B, and verify that
// rule A's delete pass does not touch the keys rule B planted.

const TWO_SIBLING_RULES_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: sibling-a
      enabled: true
      source:
        bucket: a-src
        prefix: \"\"
      destination:
        bucket: shared-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
      replicate_deletes: true
    - name: sibling-b
      enabled: true
      source:
        bucket: b-src
        prefix: \"\"
      destination:
        bucket: shared-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
      replicate_deletes: true
";

/// H2 (fourth-wave) regression: when two rules write to the same
/// destination bucket, each rule's delete pass must only consider
/// keys that carry ITS OWN provenance marker.
///
/// Pre-fix the run_delete_pass had no provenance check at all, so
/// rule A's delete pass would gleefully delete keys rule B had just
/// replicated (because A's source bucket has no key matching B's
/// destination key, and the marker check was missing).
///
/// Setup: two rules, two source buckets, one shared destination. Both
/// rules run, both stamp their own markers. Then we delete a key from
/// rule A's source and run rule A. Rule A's delete pass MUST delete
/// the matching dest key (its own provenance), but MUST leave rule
/// B's keys alone — even though, from rule A's source's perspective,
/// they have no source counterpart.
#[tokio::test]
async fn test_replication_delete_pass_skips_sibling_rule_keys() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(TWO_SIBLING_RULES_YAML)
        .build()
        .await;
    let s3 = server.s3_client().await;
    for b in ["a-src", "b-src", "shared-dst"] {
        s3.create_bucket().bucket(b).send().await.ok();
    }

    // Rule A's source content.
    for key in ["a-only-1.bin", "a-only-2.bin"] {
        s3.put_object()
            .bucket("a-src")
            .key(key)
            .body(ByteStream::from(b"from-a".to_vec()))
            .send()
            .await
            .unwrap();
    }
    // Rule B's source content (different keys to avoid prefix collision).
    for key in ["b-only-1.bin", "b-only-2.bin"] {
        s3.put_object()
            .bucket("b-src")
            .key(key)
            .body(ByteStream::from(b"from-b".to_vec()))
            .send()
            .await
            .unwrap();
    }

    let admin = admin_http_client(&server.endpoint()).await;

    // Trigger both rules so each stamps its own marker on dest.
    for rule_name in ["sibling-a", "sibling-b"] {
        let resp = admin
            .post(format!(
                "{}/_/api/admin/jobs/replication:{}/run-now",
                server.endpoint(),
                rule_name
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202, "run {} failed", rule_name);
        wait_for_latest_run(&admin, &server.endpoint(), rule_name).await;
    }

    // Sanity: all four keys present on dst.
    for key in [
        "a-only-1.bin",
        "a-only-2.bin",
        "b-only-1.bin",
        "b-only-2.bin",
    ] {
        s3.head_object()
            .bucket("shared-dst")
            .key(key)
            .send()
            .await
            .unwrap_or_else(|_| panic!("expected {} on shared-dst after both rules ran", key));
    }

    // Now delete a-only-1 from rule A's source. Rule A's NEXT run
    // will see the orphan on dst, match its own provenance marker,
    // and delete it.
    //
    // CRUCIAL: rule A's delete pass also sees b-only-1 / b-only-2
    // on dst. From A's perspective, neither key exists in `a-src`.
    // Pre-fix it would delete them. Post-fix it sees the marker is
    // `sibling-b`, not `sibling-a`, and skips.
    s3.delete_object()
        .bucket("a-src")
        .key("a-only-1.bin")
        .send()
        .await
        .unwrap();

    let body = fire_run_now(&admin, &server.endpoint(), "sibling-a").await;
    assert_eq!(
        body["status"].as_str(),
        Some("succeeded"),
        "rule A run should succeed: {}",
        body
    );

    // a-only-1 should be GONE from dst (rule A's own deletion).
    let head_a1 = s3
        .head_object()
        .bucket("shared-dst")
        .key("a-only-1.bin")
        .send()
        .await;
    assert!(
        head_a1.is_err(),
        "rule A should delete its own orphan a-only-1 from dst"
    );

    // b-only-1 and b-only-2 must STILL be there — they were written
    // by rule B, not rule A. Pre-fix these would have been deleted.
    for key in ["b-only-1.bin", "b-only-2.bin"] {
        s3.head_object()
            .bucket("shared-dst")
            .key(key)
            .send()
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "H2 REGRESSION: rule A's delete pass deleted {} (owned by rule B): {:?}",
                    key, e
                )
            });
    }

    // a-only-2 remains because its source counterpart is still in a-src.
    s3.head_object()
        .bucket("shared-dst")
        .key("a-only-2.bin")
        .send()
        .await
        .expect("a-only-2 still on dst (source counterpart present)");
}

// ════════════════════════════════════════════════════════════════════
// M1 (third-wave) — partial-failure status flip
// ════════════════════════════════════════════════════════════════════
//
// Pre-fix the run summary reported `status="succeeded"` even when SOME
// objects failed (the flip only happened when ALL copies errored).
// Post-fix any per-object failure flips status to `"failed"`.
//
// To trigger a partial failure deterministically we configure a rule
// with a NON-EXISTENT source bucket. The forward pass's
// `engine.list_objects` errors on bucket-not-found, which sets
// `hit_fatal_error = true` AND increments errors. With
// `replicate_deletes = false` and no successful work, the run is a
// pure-failure case — but it pins down the truth-table requirement:
// any error MUST produce `status="failed"`.
//
// (A genuinely "mixed" success/failure run on filesystem backend is
// hard to synthesise without race conditions; the truth-table check
// here proves the M1 logic and complements the existing happy-path
// `status="succeeded"` assertions in the other tests.)

const MISSING_DST_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: missing-dst-rule
      enabled: true
      source:
        bucket: m1-src
        prefix: \"\"
      destination:
        bucket: nonexistent-dst-bucket
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
";

/// Cause a per-object `copy_one` failure (destination bucket missing
/// → engine.store on the destination errors with NoSuchBucket) and
/// assert the run summary surfaces `status="failed"` with non-zero
/// errors. Pre-fix the wave-3 M1 fix the status was only flipped when
/// every copy errored — but the underlying truth-table bug was that
/// `had_any_error` wasn't consulted at the final-status decision; this
/// test pins that down by triggering exactly one error path
/// (the destination doesn't exist) on a populated source.
#[tokio::test]
async fn test_replication_any_error_flips_status_to_failed() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(MISSING_DST_RULE_YAML)
        .build()
        .await;
    let s3 = server.s3_client().await;
    // Create source only; destination intentionally missing.
    s3.create_bucket().bucket("m1-src").send().await.ok();
    for key in ["x.bin", "y.bin"] {
        s3.put_object()
            .bucket("m1-src")
            .key(key)
            .body(ByteStream::from(b"data".to_vec()))
            .send()
            .await
            .unwrap();
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:missing-dst-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    let body = wait_for_latest_run(&admin, &server.endpoint(), "missing-dst-rule").await;

    let errors = body["errors"].as_i64().unwrap_or(0);
    let status = body["status"].as_str().unwrap_or("");
    assert!(
        errors > 0,
        "test pre-condition: rule should record at least one error \
         when copying into a missing destination bucket. body={}",
        body
    );
    // This scenario copies into a MISSING destination bucket, so every
    // copy errors and `objects_copied == 0`. A sweep that errored and
    // copied NOTHING is a genuine failure — status must be "failed", not
    // the partial-progress "completed_with_errors" (which is reserved for
    // runs that copied SOME objects but hit a transient error on others).
    assert_eq!(
        body["objects_processed"].as_i64().unwrap_or(-1),
        0,
        "test pre-condition: nothing should copy into a missing bucket. body={body}"
    );
    assert_eq!(
        status, "failed",
        "errors={errors} copied=0 but status={status} (copied-nothing-and-errored must be 'failed'). body={body}"
    );
}

/// Event-driven replication: PUTting an object to the source bucket emits an
/// ObjectCreated event that the background consumer drains and replicates to
/// the destination — WITHOUT any run-now trigger. Then deleting it propagates
/// the delete (replicate_deletes: true). This is the core event-driven path.
#[tokio::test]
async fn test_event_driven_replication_copies_and_deletes() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(EVENT_DRIVEN_RULE_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    for b in ["ev-src", "ev-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    let endpoint = server.endpoint();
    // The jobs/*-version routes are public (like iam/version), so the barrier
    // can be polled with a plain, unauthenticated client.
    let http = reqwest::Client::new();

    // PUT a single object to the source. No run-now: the write-path emits an
    // event and the consumer (5s tick) should replicate it on its own.
    let ev_before = common::get_replication_event_version(&http, &endpoint).await;
    client
        .put_object()
        .bucket("ev-src")
        .key("evt/obj.txt")
        .body(ByteStream::from(b"event-driven".to_vec()))
        .send()
        .await
        .expect("put source object");

    // Barrier on the consumer drain (no S3 polling / sleeps), then confirm once.
    common::wait_for_replication_event(&http, &endpoint, ev_before).await;
    let out = client
        .get_object()
        .bucket("ev-dst")
        .key("evt/obj.txt")
        .send()
        .await
        .expect("object should be replicated to ev-dst by the event consumer (no run-now)");
    let body = out.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), b"event-driven", "replicated body matches");

    // Now DELETE the source object — the delete event should propagate.
    let del_before = common::get_replication_event_version(&http, &endpoint).await;
    client
        .delete_object()
        .bucket("ev-src")
        .key("evt/obj.txt")
        .send()
        .await
        .expect("delete source object");

    common::wait_for_replication_event(&http, &endpoint, del_before).await;
    let deleted = client
        .get_object()
        .bucket("ev-dst")
        .key("evt/obj.txt")
        .send()
        .await
        .is_err();
    assert!(
        deleted,
        "delete should propagate to ev-dst (replicate_deletes: true)"
    );
}

const FOREIGN_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: foreign-newerwins
      enabled: true
      source:
        bucket: fk-src
        prefix: \"\"
      destination:
        bucket: fk-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
";

/// Convergence guard for FOREIGN objects (written to the backend out-of-band,
/// carrying partial DG metadata but NO `dg-created-at`): replication NewerWins
/// must SKIP them on the 2nd run, never re-copy every tick.
///
/// IMPORTANT (multi-agent review finding): this passes BOTH before and after the
/// `created_at→LastModified` fix — for a same-backend src/dst with identity key
/// rewriting, foreign objects already converge (source LastModified < the dest's
/// copy-time created_at). So this test does NOT reproduce the prod re-copy; it is
/// a guard that the same-backend foreign case stays convergent. The prod scenario
/// (cross-backend Hetzner→filesystem, distinct buckets) is NOT yet reproduced here
/// — see docs/plan/rca-replication-recopy-2026-06-30.md (root cause still open).
///
/// Both shapes are exercised to document the scope:
///  - `artifact.zip` is delta-ELIGIBLE → LIST HEADs it → touches the fixed path.
///  - `artifact.sha1` is NON-eligible → LIST lite (no-HEAD) path, already stable.
/// Requires MinIO.
#[tokio::test]
async fn test_replication_foreign_object_missing_created_at_converges_on_second_run() {
    let server = TestServer::builder()
        .s3_endpoint(&common::minio_endpoint_url())
        .env("DGP_BACKEND_ALLOW_LOCAL", "true")
        .bucket("fk-src")
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(FOREIGN_RULE_YAML)
        .build()
        .await;

    // Buckets via the proxy (single S3 backend → identity routing, same names
    // on MinIO).
    let proxy = server.s3_client().await;
    for b in ["fk-src", "fk-dst"] {
        proxy.create_bucket().bucket(b).send().await.ok();
    }

    // RAW client straight to MinIO, bypassing the proxy, so we can plant objects
    // with PARTIAL DG metadata and NO dg-created-at (the proxy would always
    // stamp dg-created-at on a normal PUT).
    let raw = {
        let creds = aws_sdk_s3::config::Credentials::new(
            common::MINIO_ACCESS_KEY,
            common::MINIO_SECRET_KEY,
            None,
            None,
            "test",
        );
        let cfg = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .endpoint_url(common::minio_endpoint_url())
            .credentials_provider(creds)
            .force_path_style(true)
            .build();
        aws_sdk_s3::Client::from_conf(cfg)
    };

    // A valid-shaped (64-hex) sha; the read path doesn't validate its content.
    let sha = "a".repeat(64);
    for key in ["artifact.zip", "artifact.sha1"] {
        raw.put_object()
            .bucket("fk-src")
            .key(key)
            .body(ByteStream::from(format!("foreign-{key}").into_bytes()))
            .metadata("dg-tool", "foreign-uploader")
            .metadata("dg-original-name", key)
            .metadata("dg-file-sha256", &sha)
            // deliberately NO dg-created-at
            .send()
            .await
            .expect("plant foreign object");
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let run_now = || async { fire_run_now(&admin, &server.endpoint(), "foreign-newerwins").await };

    // Run 1: dest empty → both objects copied (dest=None → Copy, policy-agnostic).
    let r1 = run_now().await;
    assert_eq!(r1["status"].as_str(), Some("succeeded"), "run1: {r1}");
    assert_eq!(
        r1["objects_processed"].as_i64(),
        Some(2),
        "run1 must copy both foreign objects: {r1}"
    );

    // Run 2 — THE REGRESSION ASSERTION. Source created_at now resolves to the
    // planted objects' stable S3 LastModified (< the dest's run-1 copy time) →
    // NewerWins skips. Pre-fix, the .zip re-synthesised Utc::now() each scan →
    // always newer → re-copied → this assert fails with copied=1 (or 2).
    let r2 = run_now().await;
    assert_eq!(r2["status"].as_str(), Some("succeeded"), "run2: {r2}");
    assert_eq!(
        r2["objects_processed"].as_i64(),
        Some(0),
        "REGRESSION: foreign object(s) re-copied on 2nd run (created_at→now bug): {r2}"
    );
    assert_eq!(
        r2["objects_skipped"].as_i64(),
        Some(2),
        "both foreign objects should be skipped as not-newer on 2nd run: {r2}"
    );
}

const CONTENT_DIFF_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: cd-rule
      enabled: true
      source:
        bucket: cd-src
        prefix: \"\"
      destination:
        bucket: cd-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
      conflict: content-diff
";

/// `content-diff` is the converging replacement for the removed `source-wins`:
/// it copies an object only when its bytes differ, and SKIPS byte-identical
/// objects — so a recurring rule converges (run 2 copies nothing) yet still
/// propagates a real content change (run 3 copies exactly the changed object).
/// This is the property source-wins could never have (it re-copied everything
/// every run).
#[tokio::test]
async fn test_replication_content_diff_converges_then_copies_real_change() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(CONTENT_DIFF_RULE_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    for b in ["cd-src", "cd-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for (key, body) in [
        ("a.txt", &b"alpha"[..]),
        ("b.txt", &b"bravo"[..]),
        ("c.txt", &b"charlie"[..]),
    ] {
        client
            .put_object()
            .bucket("cd-src")
            .key(key)
            .body(ByteStream::from(body.to_vec()))
            .send()
            .await
            .expect("seed");
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let run_now = || async { fire_run_now(&admin, &server.endpoint(), "cd-rule").await };

    // Run 1: dest empty → all 3 copied.
    let r1 = run_now().await;
    assert_eq!(
        r1["objects_processed"].as_i64(),
        Some(3),
        "run1 copies all: {r1}"
    );

    // Run 2: identical content already on dest → content-diff copies NOTHING
    // (the convergence source-wins lacked).
    let r2 = run_now().await;
    assert_eq!(
        r2["objects_processed"].as_i64(),
        Some(0),
        "content-diff must converge: 2nd run copies 0, got {r2}"
    );
    assert_eq!(
        r2["objects_skipped"].as_i64(),
        Some(3),
        "all 3 skipped as identical: {r2}"
    );

    // Overwrite ONE object's content on the source.
    client
        .put_object()
        .bucket("cd-src")
        .key("b.txt")
        .body(ByteStream::from(b"bravo-CHANGED-and-longer".to_vec()))
        .send()
        .await
        .expect("overwrite b.txt");

    // Run 3: exactly the changed object copies; the other two stay skipped.
    let r3 = run_now().await;
    assert_eq!(
        r3["objects_processed"].as_i64(),
        Some(1),
        "content-diff must copy exactly the changed object: {r3}"
    );
    assert_eq!(
        r3["objects_skipped"].as_i64(),
        Some(2),
        "the two unchanged objects skip: {r3}"
    );

    // The dest now carries the new bytes.
    let got = client
        .get_object()
        .bucket("cd-dst")
        .key("b.txt")
        .send()
        .await
        .expect("dest b.txt");
    let bytes = got.body.collect().await.unwrap().into_bytes();
    assert_eq!(
        &bytes[..],
        b"bravo-CHANGED-and-longer",
        "dest reflects the new content"
    );
}

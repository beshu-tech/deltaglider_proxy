// SPDX-License-Identifier: GPL-3.0-only

//! End-to-end integration tests for lazy replication.
//!
//! Exercises the worker via the admin API's `run-now` endpoint so the
//! full stack (config → DB → engine → worker → state store) is tested
//! together. Skeleton: seed a rule in YAML, seed source objects, trigger
//! run-now, verify destination + status + history + counters.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, latest_run_id, wait_for_run_after, TestServer};
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

// `latest_run_id` and `wait_for_run_after` are the shared 60s-deadline helpers
// from `common` (the earlier 10s local shadows were too tight for stalled runs).

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

/// The walk's flagship shape: a SPARSE destination (one subtree already
/// mirrored, another entirely absent) converges in one run. The absent
/// subtree bulk-copies with ZERO dest HEADs (absence proven by the per-dir
/// merge); the present subtree is compared HEAD-free too (FS↔FS PureMirror:
/// lite listings carry logical facts). Count-gated via Prometheus — never
/// wall-clock.
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

    // Deterministic I/O accounting (fresh server ⇒ counters are this run's):
    // dirs: root, present/, absent/, absent/deep/ = 4.
    // Lists: Compare dirs (root, present/) cost 2 pages, SrcOnly dirs
    // (absent/, absent/deep/) cost 1 — batch_size 100 ⇒ one page per level.
    // HEADs: ZERO — PureMirror decides every pair from listing facts, and the
    // absent subtree needs no existence probes at all.
    let m = common::metrics_snapshot(&server.endpoint()).await;
    assert_eq!(m.head_calls_total, 0, "PureMirror run must issue no HEADs");
    assert_eq!(
        m.dirs_completed_total, 4,
        "root + present + absent + absent/deep"
    );
    assert_eq!(
        m.list_calls_total, 6,
        "2×Compare(2 dirs) + 1×SrcOnly(2 dirs)"
    );
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
  tick_interval: \"1s\"
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

/// A pause issued WHILE a SCHEDULED run is in flight must stop the run
/// promptly — not let it run to completion. (One-offs are the opposite
/// contract: run-now ignores pause and is stopped by KILL, so this test uses a
/// scheduler-started run — tick_interval 1s — as the pause target.)
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
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();

    // Pause IMMEDIATELY so the scheduler can't start a run mid-seed (the tick
    // is clamped to >=5s; seeding must not race it).
    let p = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:midpause-rule/pause"
        ))
        .send()
        .await
        .expect("boot pause");
    assert_eq!(p.status().as_u16(), 204);

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

    // Resume: the next scheduler tick starts the run against the full seed.
    let r = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:midpause-rule/resume"
        ))
        .send()
        .await
        .expect("resume");
    assert_eq!(r.status().as_u16(), 204);

    // Wait for the SCHEDULER to start a run, then pause while it paginates.
    let runs_url = format!("{ep}/_/api/admin/jobs/replication:midpause-rule/runs");
    // tick_interval is clamped to a 5s minimum — allow a couple of ticks.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let h: Value = admin
            .get(&runs_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if newest_run(&h).is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "scheduler never started a run: {h}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
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

/// A run-now on a PAUSED rule is a deliberate one-off: it must actually
/// REPLICATE (pause governs the scheduler, kill stops a running one-off) and
/// leave the rule paused. Regression for the tautology where the one-off was
/// accepted (202) but the worker's page-0 pause check silently copied nothing.
#[tokio::test]
async fn test_replication_paused_rule_one_off_actually_copies() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(PAUSED_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["p-src", "p-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for i in 0..3 {
        client
            .put_object()
            .bucket("p-src")
            .key(format!("obj-{i}.txt"))
            .body(ByteStream::from(format!("payload-{i}").into_bytes()))
            .send()
            .await
            .expect("seed");
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

    // The one-off runs to completion DESPITE the pause and copies everything.
    let run = fire_run_now(&admin, &server.endpoint(), "paused-rule").await;
    assert_eq!(
        run["status"].as_str(),
        Some("succeeded"),
        "paused one-off must complete: {run}"
    );
    assert_eq!(run["objects_processed"].as_i64(), Some(3), "{run}");
    let dst = client
        .list_objects_v2()
        .bucket("p-dst")
        .send()
        .await
        .unwrap();
    assert_eq!(dst.contents().len(), 3, "one-off copied all seeded objects");

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

    // The one-off must NOT flip the rule enabled — the scheduler stays off.
    let jobs: Value = admin
        .get(format!("{}/_/api/admin/jobs", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let enabled = jobs["jobs"]
        .as_array()
        .and_then(|a| a.iter().find(|j| j["id"] == "replication:disabled-rule"))
        .and_then(|j| j["enabled"].as_bool());
    assert_eq!(
        enabled,
        Some(false),
        "one-off must not enable the rule: {jobs}"
    );
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
    skip_unless_minio!();
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

const HEAL_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: heal-rule
      enabled: true
      source:
        bucket: heal-src
        prefix: \"\"
      destination:
        bucket: heal-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
      conflict: content-diff
";

/// End-to-end convergence over a destination whose delta metadata was stripped
/// (the prod `backup-hz` corruption). Before the fix this re-copied the whole
/// deltaspace EVERY run forever, because a stripped delta reads back with the
/// wrong (delta-stored) size → content-diff always saw a difference. Now the
/// copy heals the reference on write, so the re-stamped dest delta resolves
/// cleanly and the NEXT run skips → the run converges.
#[tokio::test]
async fn test_replication_heals_corrupt_dest_and_converges() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(HEAL_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    let data_dir = server.data_dir().expect("filesystem backend").to_path_buf();
    for b in ["heal-src", "heal-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    // Two similar delta-eligible objects in one prefix → reference + delta.
    let v1: Vec<u8> = (0..20000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut v2 = v1.clone();
    for b in v2.iter_mut().take(200) {
        *b ^= 0x5a;
    }
    for (key, body) in [("app/v1.zip", &v1), ("app/v2.zip", &v2)] {
        client
            .put_object()
            .bucket("heal-src")
            .key(key)
            .body(ByteStream::from(body.clone()))
            .send()
            .await
            .expect("seed src");
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();
    let run = || async { fire_run_now(&admin, &ep, "heal-rule").await };

    // Run 1: dest empty → both copied (builds dest reference + delta).
    let r1 = run().await;
    assert_eq!(
        r1["objects_processed"].as_i64(),
        Some(2),
        "run1 copies 2: {r1}"
    );

    // Corrupt the DEST like the prod damage: strip the reference AND the delta
    // xattrs (bytes intact). The prefix on disk is "app".
    let ds = |f: &str| {
        data_dir
            .join("heal-dst")
            .join("deltaspaces")
            .join("app")
            .join(f)
    };
    for f in ["reference.bin", "v1.zip.delta", "v2.zip.delta"] {
        let p = ds(f);
        if p.exists() {
            let _ = xattr::remove(&p, "user.dg.metadata");
        }
    }

    // Run 2: the corrupt dest reads back with wrong sizes → content-diff
    // re-copies. This is the heal-triggering pass (store re-stamps the
    // reference; the fresh deltas carry clean metadata).
    let r2 = run().await;
    assert!(
        r2["objects_processed"].as_i64().unwrap_or(0) >= 1,
        "run2 re-copies the corrupt objects (and heals): {r2}"
    );

    // Run 3: now the dest metadata resolves cleanly → content-diff SKIPS.
    // Before the fix this would re-copy forever; convergence is the fix.
    let r3 = run().await;
    assert_eq!(
        r3["objects_processed"].as_i64(),
        Some(0),
        "run3 converges: 0 re-copied after the heal: {r3}"
    );
    assert_eq!(
        r3["objects_skipped"].as_i64(),
        Some(2),
        "both objects skip once healed: {r3}"
    );
}

// ───────────────────────── kill / delete / truncation ─────────────────────────
// The kill feature shipped three fix waves with ZERO tests calling the kill
// action; these are the regression pins for the whole control surface.

/// POST kill until the worker's run is killable (202) — tolerates the window
/// before the background run row exists (409 "no running run to kill").
async fn fire_kill(admin: &reqwest::Client, endpoint: &str, rule: &str) {
    let url = format!("{endpoint}/_/api/admin/jobs/replication:{rule}/kill");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let code = admin
            .post(&url)
            .send()
            .await
            .expect("kill")
            .status()
            .as_u16();
        if code == 202 {
            return;
        }
        assert_eq!(code, 409, "kill: unexpected status {code}");
        assert!(
            std::time::Instant::now() < deadline,
            "kill for '{rule}' kept returning 409 (run never became killable)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

const KILL_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: kill-rule
      enabled: true
      source:
        bucket: kr-src
        prefix: \"\"
      destination:
        bucket: kr-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 1
";

/// Kill mid-run: the run settles `cancelled`, copying STOPS (dest count is
/// stable after settle), and a later run-now resumes and drains the rest.
#[tokio::test]
async fn test_replication_kill_mid_run_settles_cancelled_and_stops_copying() {
    // Stall every copy 50ms so 150 objects ≈ 7.5s of work — the kill
    // DETERMINISTICALLY lands mid-run (copied < 150), no wall-clock racing.
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(KILL_RULE_YAML)
        .env("DGP_TEST_COPY_STALL_MS", "50")
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["kr-src", "kr-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for i in 0..150 {
        client
            .put_object()
            .bucket("kr-src")
            .key(format!("obj-{i:03}.txt"))
            .body(ByteStream::from(format!("payload-{i}").into_bytes()))
            .send()
            .await
            .expect("seed");
    }
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();

    let resp = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:kill-rule/run-now"
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 202);

    fire_kill(&admin, &ep, "kill-rule").await;
    let run = wait_for_latest_run(&admin, &ep, "kill-rule").await;
    assert_eq!(
        run["status"].as_str(),
        Some("cancelled"),
        "killed run must settle cancelled, never succeeded: {run}"
    );
    let copied = run["objects_processed"].as_i64().unwrap_or(-1);
    // The copy-stall guarantees the kill lands mid-run — strictly incomplete.
    assert!(
        (0..150).contains(&copied),
        "killed run must stop before copying everything: {run}"
    );

    // Copying actually STOPPED. A TERMINAL 'cancelled' run means the worker
    // loop has exited — no copy can follow — so the dest count is authoritative
    // WITHOUT a wall-clock stability window. It must be strictly incomplete
    // (the stall guarantees the kill interrupted the sweep).
    let count_after_settle = client
        .list_objects_v2()
        .bucket("kr-dst")
        .send()
        .await
        .unwrap()
        .contents()
        .len();
    assert!(
        count_after_settle < 150,
        "kill left the sweep incomplete (settled cancelled): {count_after_settle}"
    );

    // A later one-off drains the tail FROM THE PRESERVED CURSOR: when the
    // kill landed after >=1 persisted page, the resumed run must scan fewer
    // than all 150 (a cleared cursor would restart from key zero).
    let killed_copied = copied.max(0);
    let mut guard = 0;
    let mut first_resume_scanned: Option<i64> = None;
    loop {
        let run = fire_run_now(&admin, &ep, "kill-rule").await;
        if first_resume_scanned.is_none() {
            first_resume_scanned = run["objects_scanned"].as_i64();
        }
        let dst = client
            .list_objects_v2()
            .bucket("kr-dst")
            .send()
            .await
            .unwrap()
            .contents()
            .len();
        if dst >= 150 {
            break;
        }
        guard += 1;
        assert!(
            guard < 10,
            "post-kill drain never completed: last run {run}"
        );
    }
    if killed_copied > 0 {
        let scanned = first_resume_scanned.unwrap_or(150);
        assert!(
            scanned < 150,
            "resume must continue from the kill's preserved cursor, not restart \
             (killed after {killed_copied} copies, resumed run scanned {scanned})"
        );
    }
}

const KILLDEL_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"1h\"
  rules:
    - name: killdel-rule
      enabled: true
      replicate_deletes: true
      source:
        bucket: kd-src
        prefix: \"\"
      destination:
        bucket: kd-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 1
";

/// Kill during the DELETE pass (the run's only destructive phase): a killed
/// run must stop deleting destination objects, not grind through the sweep
/// and then claim "cancelled".
#[tokio::test]
async fn test_replication_kill_stops_delete_pass() {
    // 50ms stall per copy AND per delete → both passes run long enough for the
    // kill to land deterministically mid-run (150 ops ≈ 7.5s each, < deadline).
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(KILLDEL_RULE_YAML)
        .env("DGP_TEST_COPY_STALL_MS", "50")
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["kd-src", "kd-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for i in 0..150 {
        client
            .put_object()
            .bucket("kd-src")
            .key(format!("obj-{i:03}.txt"))
            .body(ByteStream::from(format!("payload-{i}").into_bytes()))
            .send()
            .await
            .expect("seed");
    }
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();

    // Run 1: full replication (stamps the provenance markers the delete pass
    // requires).
    let run = fire_run_now(&admin, &ep, "killdel-rule").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");

    // Remove every source object → run 2's delete pass wants to delete all
    // 150 dest objects, one page each (batch_size 1).
    for i in 0..150 {
        client
            .delete_object()
            .bucket("kd-src")
            .key(format!("obj-{i:03}.txt"))
            .send()
            .await
            .expect("unseed");
    }

    let before = latest_run_id(&admin, &ep, "killdel-rule").await;
    let resp = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:killdel-rule/run-now"
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 202);

    fire_kill(&admin, &ep, "killdel-rule").await;
    let run = wait_for_run_after(&admin, &ep, "killdel-rule", before).await;
    assert_eq!(
        run["status"].as_str(),
        Some("cancelled"),
        "killed delete-pass run settles cancelled: {run}"
    );

    // The kill landed before the sweep finished: destination objects survive.
    // A TERMINAL 'cancelled' run means the delete loop has exited — no deletion
    // can follow — so this count is authoritative without a stability window.
    let survivors = client
        .list_objects_v2()
        .bucket("kd-dst")
        .send()
        .await
        .unwrap()
        .contents()
        .len();
    assert!(
        survivors > 0,
        "killed delete pass must leave destination objects behind (settled cancelled)"
    );
}

const DELRULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: del-rule
      enabled: true
      source:
        bucket: dr-src
        prefix: \"\"
      destination:
        bucket: dr-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 1
";

/// H2 regression: deleting a rule with a live run is refused (409) — even in
/// the acquire-to-run-row gap right after run-now's 202, because the LEASE is
/// the liveness anchor. After kill+settle the delete succeeds and purges the
/// rule everywhere.
#[tokio::test]
async fn test_replication_delete_rule_refused_under_live_run_then_succeeds() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(DELRULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["dr-src", "dr-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for i in 0..150 {
        client
            .put_object()
            .bucket("dr-src")
            .key(format!("obj-{i:03}.txt"))
            .body(ByteStream::from(format!("payload-{i}").into_bytes()))
            .send()
            .await
            .expect("seed");
    }
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();
    let delete_url = format!("{ep}/_/api/admin/jobs/replication:del-rule/delete");

    let resp = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:del-rule/run-now"
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 202);

    // IMMEDIATELY after the 202 the run row may not exist yet — but the lease
    // does, so the delete must already be refused (the exact race H2 had).
    let code = admin
        .post(&delete_url)
        .send()
        .await
        .expect("delete")
        .status()
        .as_u16();
    assert_eq!(code, 409, "delete under a live run (lease held) must 409");

    fire_kill(&admin, &ep, "del-rule").await;
    let run = wait_for_latest_run(&admin, &ep, "del-rule").await;
    assert_eq!(run["status"].as_str(), Some("cancelled"), "{run}");

    // The spawned task releases the lease just after settle — tolerate the
    // brief 409 window, then the delete must go through.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let code = admin
            .post(&delete_url)
            .send()
            .await
            .expect("delete")
            .status()
            .as_u16();
        if code == 204 {
            break;
        }
        assert_eq!(code, 409, "delete after settle: unexpected {code}");
        assert!(
            std::time::Instant::now() < deadline,
            "delete kept 409ing after the run settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Gone from the jobs list, runs endpoint, and run-now 404s.
    let jobs: Value = admin
        .get(format!("{ep}/_/api/admin/jobs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["id"] == "replication:del-rule"),
        "deleted rule must vanish from the jobs list: {jobs}"
    );
    let code = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:del-rule/run-now"
        ))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(code, 404, "run-now on a deleted rule is 404");
}

/// Finding #17: a parity VERIFY fired while a replication run of the same rule
/// is live must 409 (the dest is mid-sync — a verdict would be a false
/// 'not in sync'). Same lease-race technique as the delete test: the run lease
/// exists in the gap right after the 202, before the run row.
#[tokio::test]
async fn test_replication_verify_refused_under_live_run() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(DELRULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["dr-src", "dr-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for i in 0..150 {
        client
            .put_object()
            .bucket("dr-src")
            .key(format!("obj-{i:03}.txt"))
            .body(ByteStream::from(format!("p-{i}").into_bytes()))
            .send()
            .await
            .expect("seed");
    }
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();

    let resp = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:del-rule/run-now"
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 202);

    // The lease is held in the acquire-to-run-row gap → verify must 409.
    let code = admin
        .post(format!("{ep}/_/api/admin/jobs/replication:del-rule/verify"))
        .send()
        .await
        .expect("verify")
        .status()
        .as_u16();
    assert_eq!(code, 409, "verify under a live run (lease held) must 409");

    // After kill + settle, verify is allowed again (202/200).
    fire_kill(&admin, &ep, "del-rule").await;
    let run = wait_for_latest_run(&admin, &ep, "del-rule").await;
    assert_eq!(run["status"].as_str(), Some("cancelled"), "{run}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let code = admin
            .post(format!("{ep}/_/api/admin/jobs/replication:del-rule/verify"))
            .send()
            .await
            .expect("verify")
            .status()
            .as_u16();
        if code == 202 || code == 200 {
            break;
        }
        assert_eq!(code, 409, "verify after settle: unexpected {code}");
        assert!(
            std::time::Instant::now() < deadline,
            "verify kept 409ing after the run settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

const TRUNC_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: trunc-rule
      enabled: true
      source:
        bucket: tr-src
        prefix: \"\"
      destination:
        bucket: tr-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 1
";

/// Page-budget truncation regression: a truncated forward pass must settle
/// "stopped" (NOT "succeeded") and KEEP the cursor so later runs drain the
/// tail — the bug settled succeeded + cleared the cursor, permanently
/// orphaning every object past the budget.
#[tokio::test]
async fn test_replication_budget_truncation_keeps_cursor_and_resumes() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .env("DGP_TEST_MAX_JOB_PAGES", "3")
        .extra_yaml_storage_section(TRUNC_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["tr-src", "tr-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for i in 0..5 {
        client
            .put_object()
            .bucket("tr-src")
            .key(format!("obj-{i}.txt"))
            .body(ByteStream::from(format!("payload-{i}").into_bytes()))
            .send()
            .await
            .expect("seed");
    }
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();

    // Run 1: 3-page budget = 1 dest page (empty level) + 2 src pages under
    // the walk's uniform both-sides accounting → truncated after 2 copies.
    // Must be "stopped" — and the cursor kept.
    let run = fire_run_now(&admin, &ep, "trunc-rule").await;
    // The jobs view folds stopped→cancelled for display; the RAW status is the
    // settle contract under test.
    assert_eq!(
        run["status_raw"].as_str(),
        Some("stopped"),
        "truncated pass must NOT claim success: {run}"
    );
    assert_eq!(run["objects_processed"].as_i64(), Some(2), "{run}");

    // Run 2 RESUMES from the cursor (1 dest page skips settled ground via the
    // resume token): 2 more, no re-copying (processed==2).
    let run = fire_run_now(&admin, &ep, "trunc-rule").await;
    assert_eq!(run["objects_processed"].as_i64(), Some(2), "resumed: {run}");
    let dst = client
        .list_objects_v2()
        .bucket("tr-dst")
        .send()
        .await
        .unwrap();
    assert_eq!(dst.contents().len(), 4, "two runs drained four objects");

    // Run 3 finishes the tail cleanly.
    let run = fire_run_now(&admin, &ep, "trunc-rule").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    let dst = client
        .list_objects_v2()
        .bucket("tr-dst")
        .send()
        .await
        .unwrap();
    assert_eq!(dst.contents().len(), 5, "tail fully drained across runs");
}

const TIMEOUT_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  object_timeout: \"1s\"
  rules:
    - name: timeout-rule
      enabled: true
      source:
        bucket: to-src
        prefix: \"\"
      destination:
        bucket: to-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 10
";

/// Per-object copy timeout (Phase A) — the Elapsed arm was untested. With a 1s
/// object_timeout and a 3s per-object test barrier, every object exceeds its
/// deadline: the run records errors and copies nothing, exercising the timeout
/// branch (worker.rs copy_result Err(_elapsed)).
#[tokio::test]
async fn test_replication_object_timeout_fires_and_records_error() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(TIMEOUT_RULE_YAML)
        .env("DGP_TEST_COPY_STALL_MS", "3000") // > the 1s object_timeout
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["to-src", "to-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for i in 0..2 {
        client
            .put_object()
            .bucket("to-src")
            .key(format!("obj-{i}.bin"))
            .body(ByteStream::from(vec![i as u8; 1024]))
            .send()
            .await
            .expect("seed");
    }
    let admin = admin_http_client(&server.endpoint()).await;
    let run = fire_run_now(&admin, &server.endpoint(), "timeout-rule").await;

    // Every object timed out → nothing copied, errors recorded.
    assert_eq!(
        run["objects_processed"].as_i64(),
        Some(0),
        "timed-out objects must not count as copied: {run}"
    );
    assert!(
        run["errors"].as_i64().unwrap_or(0) >= 1,
        "the object-timeout branch must record a failure: {run}"
    );
    // Dest stays empty — no partial/torn write from a timed-out copy.
    let dst = client
        .list_objects_v2()
        .bucket("to-dst")
        .send()
        .await
        .unwrap();
    assert_eq!(
        dst.contents().len(),
        0,
        "timed-out copy must not land on dest"
    );

    // A failure row is surfaced (the timeout message).
    let failures: Value = admin
        .get(format!(
            "{}/_/api/admin/jobs/replication:timeout-rule/failures",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let has_timeout = failures["failures"]
        .as_array()
        .map(|a| {
            a.iter().any(|f| {
                f["error"]
                    .as_str()
                    .map(|m| m.contains("timed out"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    assert!(
        has_timeout,
        "a 'timed out' failure must be recorded: {failures}"
    );
}

const DISCOVERY_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: discovery-rule
      enabled: true
      source:
        bucket: disc-src
        prefix: \"\"
      destination:
        bucket: disc-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 1
";

/// THE prod-fix proof: copies start BEFORE discovery completes. With a page
/// budget far too small to even finish listing the tree, the first run still
/// copies objects — under the old architecture the dest-oracle pre-pass had
/// to walk the ENTIRE tree before the first byte moved, so a budget this
/// small could never copy anything.
#[tokio::test]
async fn test_replication_first_copy_before_full_discovery() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .env("DGP_TEST_MAX_JOB_PAGES", "5")
        .extra_yaml_storage_section(DISCOVERY_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["disc-src", "disc-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    // 10 directories × 1 object: full discovery alone needs >10 pages at
    // batch_size 1; the budget allows 5.
    for i in 0..10 {
        client
            .put_object()
            .bucket("disc-src")
            .key(format!("d{i}/obj.txt"))
            .body(ByteStream::from(format!("payload-{i}").into_bytes()))
            .send()
            .await
            .expect("seed");
    }
    let admin = admin_http_client(&server.endpoint()).await;

    let run = fire_run_now(&admin, &server.endpoint(), "discovery-rule").await;
    assert_eq!(
        run["status_raw"].as_str(),
        Some("stopped"),
        "budget-truncated, not failed: {run}"
    );
    assert!(
        run["objects_processed"].as_i64().unwrap_or(0) >= 1,
        "copies must start before discovery finishes: {run}"
    );
    let dst = client
        .list_objects_v2()
        .bucket("disc-dst")
        .send()
        .await
        .unwrap();
    assert!(!dst.contents().is_empty(), "first bytes landed on dest");
}

const PERDIR_DEL_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  dir_concurrency: 1
  rules:
    - name: perdir-rule
      enabled: true
      source:
        bucket: pd-src
        prefix: \"\"
      destination:
        bucket: pd-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 1
      replicate_deletes: true
";

/// Per-directory delete gating: an orphan in dir `a/` (which completes
/// cleanly, first in walk order with dir_concurrency=1) is deleted even
/// though the run truncates on its page budget inside dir `b/`. The old
/// all-or-nothing gate ("deletes only after a complete clean forward pass")
/// could NEVER delete anything in a truncated run.
#[tokio::test]
async fn test_replication_per_dir_delete_with_truncation_elsewhere() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .env("DGP_TEST_MAX_JOB_PAGES", "12")
        .extra_yaml_storage_section(PERDIR_DEL_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["pd-src", "pd-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // Run 1 (cheap: 3 pages) stamps the provenance marker on a/orphan.txt.
    client
        .put_object()
        .bucket("pd-src")
        .key("a/orphan.txt")
        .body(ByteStream::from(b"orphan".to_vec()))
        .send()
        .await
        .expect("seed orphan");
    let admin = admin_http_client(&server.endpoint()).await;
    let run = fire_run_now(&admin, &server.endpoint(), "perdir-rule").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");

    // Orphan the dest copy and add enough b/ work to blow the budget there.
    client
        .delete_object()
        .bucket("pd-src")
        .key("a/orphan.txt")
        .send()
        .await
        .expect("unseed orphan");
    for i in 0..10 {
        client
            .put_object()
            .bucket("pd-src")
            .key(format!("b/obj-{i}.txt"))
            .body(ByteStream::from(format!("payload-{i}").into_bytes()))
            .send()
            .await
            .expect("seed b");
    }

    // Run 2: dir a/ (dest-only) completes cleanly → its provenance-owned
    // orphan is deleted; the budget then runs out inside b/.
    let run = fire_run_now(&admin, &server.endpoint(), "perdir-rule").await;
    assert_eq!(
        run["status_raw"].as_str(),
        Some("stopped"),
        "run truncates in b/: {run}"
    );
    assert_eq!(
        run["objects_deleted"].as_i64(),
        Some(1),
        "a/ orphan deleted despite truncation elsewhere: {run}"
    );
    let orphan = client
        .get_object()
        .bucket("pd-dst")
        .key("a/orphan.txt")
        .send()
        .await;
    assert!(orphan.is_err(), "orphan must be gone from dest");
    let copied = client
        .list_objects_v2()
        .bucket("pd-dst")
        .prefix("b/")
        .send()
        .await
        .unwrap()
        .contents()
        .len();
    assert!(
        copied > 0 && copied < 10,
        "b/ was mid-copy when the budget hit (got {copied})"
    );
}

const MIRROR_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: mirror-rule
      enabled: true
      source:
        bucket: mir-src
        prefix: \"\"
      destination:
        bucket: mir-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
";

/// PureMirror I/O contract, count-gated: an initial sync AND a converged
/// re-run over an FS↔FS pair issue ZERO per-object HEADs (lite listings are
/// fact-authoritative), and listings cost exactly 2 pages per Compare dir /
/// 1 per SrcOnly dir.
#[tokio::test]
async fn test_replication_pure_mirror_zero_heads() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(MIRROR_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["mir-src", "mir-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for (key, body) in [
        ("top.txt", &b"top"[..]),
        ("sub/a.txt", &b"alpha"[..]),
        ("sub/b.txt", &b"bravo"[..]),
    ] {
        client
            .put_object()
            .bucket("mir-src")
            .key(key)
            .body(ByteStream::from(body.to_vec()))
            .send()
            .await
            .expect("seed");
    }
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();

    // Run 1: initial sync — dest empty ⇒ root Compare (2 pages), sub/ is
    // SrcOnly (1 page). 3 copies.
    let run = fire_run_now(&admin, &ep, "mirror-rule").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    assert_eq!(run["objects_processed"].as_i64(), Some(3), "{run}");
    let m = common::metrics_snapshot(&ep).await;
    assert_eq!(m.head_calls_total, 0, "initial sync: zero HEADs");
    assert_eq!(m.list_calls_total, 3, "root Compare(2) + sub SrcOnly(1)");
    assert_eq!(m.dirs_completed_total, 2);

    // Run 2: converged mirror — both dirs Compare (2 pages each), every pair
    // decided HEAD-free from lite facts (NewerWins tie ⇒ skip).
    let run = fire_run_now(&admin, &ep, "mirror-rule").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    assert_eq!(
        run["objects_processed"].as_i64(),
        Some(0),
        "all skips: {run}"
    );
    let m = common::metrics_snapshot(&ep).await;
    assert_eq!(m.head_calls_total, 0, "converged re-run: still zero HEADs");
    assert_eq!(m.list_calls_total, 3 + 4, "run 2 adds 2×Compare for 2 dirs");
    assert_eq!(m.dirs_completed_total, 4);
}

const KILLWALK_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  dir_concurrency: 1
  rules:
    - name: killwalk-rule
      enabled: true
      source:
        bucket: kw-src
        prefix: \"\"
      destination:
        bucket: kw-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 10
";

/// Kill mid-walk on a MULTI-DIRECTORY tree: the run settles cancelled with
/// the cursor mid-tree, and the resumed run copies only the remainder — the
/// walk's per-path watermark survives a kill across directory boundaries.
#[tokio::test]
async fn test_replication_kill_mid_walk_resumes_across_dirs() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(KILLWALK_RULE_YAML)
        // 150ms/object × 60 objects ÷ 4 transfers ≈ 2.3s of runway — the
        // ≤1s kill-tick latency lands with a comfortable margin.
        .env("DGP_TEST_COPY_STALL_MS", "150")
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["kw-src", "kw-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for d in ["a", "b", "c"] {
        for i in 0..20 {
            client
                .put_object()
                .bucket("kw-src")
                .key(format!("{d}/obj-{i:02}.txt"))
                .body(ByteStream::from(format!("payload-{d}-{i}").into_bytes()))
                .send()
                .await
                .expect("seed");
        }
    }
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();

    // Start the run (60 × 50ms stalls ≈ several seconds of runway), then kill.
    let before = latest_run_id(&admin, &ep, "killwalk-rule").await;
    let resp = admin
        .post(format!(
            "{ep}/_/api/admin/jobs/replication:killwalk-rule/run-now"
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 202);
    // Let the walk make real progress first — a kill that lands before the
    // first copy proves nothing about mid-tree resume.
    for _ in 0..200 {
        let n = client
            .list_objects_v2()
            .bucket("kw-dst")
            .send()
            .await
            .map(|r| r.contents().len())
            .unwrap_or(0);
        if n >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    fire_kill(&admin, &ep, "killwalk-rule").await;
    let run = wait_for_run_after(&admin, &ep, "killwalk-rule", before).await;
    assert_eq!(run["status"].as_str(), Some("cancelled"), "{run}");
    let killed_at = client
        .list_objects_v2()
        .bucket("kw-dst")
        .send()
        .await
        .unwrap()
        .contents()
        .len();
    assert!(
        killed_at < 60,
        "kill must land before the walk finishes (copied {killed_at})"
    );

    // Resume: the remainder only — never re-copying settled ground.
    let run = fire_run_now(&admin, &ep, "killwalk-rule").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    let resumed = run["objects_processed"].as_i64().unwrap_or(-1);
    assert!(
        resumed < 60,
        "resume copies only the remainder, got {resumed}"
    );
    let total = client
        .list_objects_v2()
        .bucket("kw-dst")
        .send()
        .await
        .unwrap()
        .contents()
        .len();
    assert_eq!(total, 60, "full convergence after kill + resume");
}

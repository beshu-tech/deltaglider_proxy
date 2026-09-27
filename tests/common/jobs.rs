// SPDX-License-Identifier: BUSL-1.1

//! Barriers for background work: usage scans, replication events, job runs.

use std::time::Duration;
use tokio::time::sleep;

/// Read the usage-scan refresh counter (`GET …/usage-scan-version`), bumped
/// after every completed usage-scan cache insert. Sibling of
/// [`get_iam_version`]; used by [`wait_for_usage_scan_refresh`].
pub async fn get_usage_scan_version(client: &reqwest::Client, endpoint: &str) -> u64 {
    let resp = client
        .get(format!("{endpoint}/_/api/admin/usage-scan-version"))
        .send()
        .await
        .expect("usage-scan-version GET");
    assert!(
        resp.status().is_success(),
        "usage-scan-version must return 2xx, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("usage-scan-version JSON");
    body["version"].as_u64().expect("version is u64")
}

/// Wait until the usage-scan refresh counter advances past `baseline` — i.e.
/// a background scan has completed and inserted its result into the cache.
/// Replaces the blind `sleep(500ms)×N` polling the quota tests used to use
/// (CLAUDE.md testability rule: observable counter over sleep). `deadline_secs`
/// defaults to 15 because a scan lists the prefix (heavier than an IAM rebuild)
/// and the TTL-shortened test scans re-trigger on each stale probe.
///
/// Call pattern: capture `baseline` BEFORE the action that triggers a scan
/// (a PUT's `check_quota` → `get_or_scan` enqueues one when the cache is
/// missing/stale), then `wait_for_usage_scan_refresh(&http, &endpoint, baseline)`.
pub async fn wait_for_usage_scan_refresh(client: &reqwest::Client, endpoint: &str, baseline: u64) {
    wait_for_usage_scan_refresh_within(client, endpoint, baseline, Duration::from_secs(15)).await
}

pub async fn wait_for_usage_scan_refresh_within(
    client: &reqwest::Client,
    endpoint: &str,
    baseline: u64,
    deadline_dur: Duration,
) {
    let deadline = std::time::Instant::now() + deadline_dur;
    let mut attempts = 0u32;
    loop {
        let current = get_usage_scan_version(client, endpoint).await;
        if current > baseline {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "wait_for_usage_scan_refresh timed out after {deadline_dur:?}: \
                 baseline={baseline}, current={current}, attempts={attempts} — no \
                 usage scan completed; either nothing triggered get_or_scan or \
                 the counter isn't being bumped"
            );
        }
        attempts += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

/// Bounded, NON-panicking variant of [`wait_for_usage_scan_refresh`]: waits up
/// to `max` for the counter to advance past `baseline`, returning the highest
/// version seen (== `baseline` if no scan completed in the window). For retry
/// loops that must fall through on no-advance (e.g. the first iteration before
/// any scan is triggered) instead of panicking — replaces the old blind
/// `sleep(500ms)` polls in the quota tests with a signal-driven wait that still
/// preserves their retry-until-cache-reflects-reality structure.
pub async fn wait_usage_scan_refresh_bounded(
    client: &reqwest::Client,
    endpoint: &str,
    baseline: u64,
    max: Duration,
) -> u64 {
    let deadline = std::time::Instant::now() + max;
    loop {
        let current = get_usage_scan_version(client, endpoint).await;
        if current > baseline {
            return current;
        }
        if std::time::Instant::now() >= deadline {
            return current;
        }
        sleep(Duration::from_millis(20)).await;
    }
}

/// Read the event-driven replication drain counter
/// (`GET …/jobs/replication-event-version`), bumped each time the event
/// consumer advances its cursor after handling real events.
pub async fn get_replication_event_version(client: &reqwest::Client, endpoint: &str) -> u64 {
    let resp = client
        .get(format!(
            "{endpoint}/_/api/admin/jobs/replication-event-version"
        ))
        .send()
        .await
        .expect("replication-event-version GET");
    assert!(
        resp.status().is_success(),
        "replication-event-version must return 2xx (the route is public — no auth needed), got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("event-version JSON");
    body["version"].as_u64().expect("version is u64")
}

/// Wait until the event consumer has drained at least once past `baseline`.
/// Deadline is generous (35s) because the consumer ticks on its own interval
/// (≈5s) — the barrier replaces a `for _ in 0..30 { sleep(1s); get_object }`
/// loop, so a settled drain (not S3 polling) is the observable. Panics on
/// timeout so a broken consumer fails loudly.
pub async fn wait_for_replication_event(client: &reqwest::Client, endpoint: &str, baseline: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(35);
    loop {
        if get_replication_event_version(client, endpoint).await > baseline {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "wait_for_replication_event timed out after 35s (baseline={baseline}) — \
                 the event consumer didn't drain (cursor never advanced)"
            );
        }
        sleep(Duration::from_millis(200)).await;
    }
}

/// Poll a replication rule's run history until the latest run reaches a terminal
/// status, then return that run object. `run-now` is fire-and-forget (202) — a
/// large sync can't block the HTTP response — so tests assert on the settled
/// run-history row (`objects_processed`, `status`), not the run-now response.
///
/// Firing run-now MORE THAN ONCE in a test? Baseline with [`latest_run_id`]
/// before the fire and use [`wait_for_run_after`] — the new run's history row
/// only appears once its background task starts, so this max-id variant can
/// return the PREVIOUS run's terminal row.
pub async fn wait_for_run(
    admin: &reqwest::Client,
    endpoint: &str,
    rule: &str,
) -> serde_json::Value {
    wait_for_run_after(admin, endpoint, rule, -1).await
}

/// `id` of the newest run in a rule's history, or 0 when none exists yet.
pub async fn latest_run_id(admin: &reqwest::Client, endpoint: &str, rule: &str) -> i64 {
    let url = format!("{endpoint}/_/api/admin/jobs/replication:{rule}/runs");
    let h: serde_json::Value = admin.get(&url).send().await.unwrap().json().await.unwrap();
    h["runs"]
        .as_array()
        .and_then(|r| r.iter().filter_map(|x| x["id"].as_i64()).max())
        .unwrap_or(0)
}

/// Like [`wait_for_run`] but only accepts a run with `id > after_id` — the
/// baseline that makes back-to-back run-now assertions race-free.
pub async fn wait_for_run_after(
    admin: &reqwest::Client,
    endpoint: &str,
    rule: &str,
    after_id: i64,
) -> serde_json::Value {
    let url = format!("{endpoint}/_/api/admin/jobs/replication:{rule}/runs");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let h: serde_json::Value = admin.get(&url).send().await.unwrap().json().await.unwrap();
        if let Some(run) = h["runs"].as_array().and_then(|r| {
            r.iter()
                .max_by_key(|x| x["id"].as_i64().unwrap_or(i64::MIN))
        }) {
            let id = run["id"].as_i64().unwrap_or(0);
            let st = run["status"].as_str().unwrap_or("");
            if id > after_id
                && matches!(
                    st,
                    "succeeded" | "failed" | "completed_with_errors" | "cancelled" | "stopped"
                )
            {
                return run.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "new run (id > {after_id}) for rule '{rule}' did not settle in 60s; last history: {h}"
        );
        sleep(Duration::from_millis(200)).await;
    }
}

/// Poll a job's run history until the run `run_id` reaches a terminal status,
/// then return that run-history row. `job` is the unified job id
/// (`lifecycle:<rule>`, `replication:<rule>`).
pub async fn wait_for_job_run(
    admin: &reqwest::Client,
    endpoint: &str,
    job: &str,
    run_id: i64,
) -> serde_json::Value {
    let url = format!("{endpoint}/_/api/admin/jobs/{job}/runs");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let h: serde_json::Value = admin.get(&url).send().await.unwrap().json().await.unwrap();
        if let Some(run) = h["runs"]
            .as_array()
            .and_then(|r| r.iter().find(|x| x["id"].as_i64() == Some(run_id)))
        {
            if !matches!(
                run["status"].as_str(),
                Some("running" | "queued" | "cancelling")
            ) {
                return run.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run {run_id} of {job} did not settle in 60s; last history: {h}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// Fire a lifecycle rule's run-now (async: 202 + `run_id`) and wait for that
/// run to settle. Returns the run-history row (`status`, `objects_processed`,
/// `errors`, ...).
pub async fn lifecycle_run_now_and_wait(
    admin: &reqwest::Client,
    endpoint: &str,
    rule: &str,
) -> serde_json::Value {
    let resp = admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/lifecycle:{rule}/run-now"
        ))
        .send()
        .await
        .expect("run-now request");
    let code = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    assert_eq!(code, 202, "lifecycle run-now must be accepted: {body}");
    assert_eq!(body["status"].as_str(), Some("running"), "{body}");
    let run_id = body["run_id"].as_i64().expect("run-now returns run_id");
    wait_for_job_run(admin, endpoint, &format!("lifecycle:{rule}"), run_id).await
}

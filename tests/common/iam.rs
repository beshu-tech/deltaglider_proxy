// SPDX-License-Identifier: BUSL-1.1

//! Deterministic rebuild barriers for IAM and external-auth mutations.

use std::time::Duration;
use tokio::time::sleep;

/// Fetch the current IAM rebuild counter from the proxy.
///
/// Backed by `GET /_/api/admin/iam/version`, which is incremented by
/// [`src/api/admin/users.rs::rebuild_iam_index`] after every IAM
/// mutation (user/group CRUD, OAuth provider changes, etc.). Used by
/// [`wait_for_iam_rebuild`] as the barrier primitive.
pub async fn get_iam_version(client: &reqwest::Client, endpoint: &str) -> u64 {
    let resp = client
        .get(format!("{endpoint}/_/api/admin/iam/version"))
        .send()
        .await
        .expect("iam/version GET");
    assert!(
        resp.status().is_success(),
        "iam/version must return 2xx, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("iam/version JSON");
    body["version"].as_u64().expect("version is u64")
}

/// Wait until the proxy's IAM rebuild counter advances past `baseline`.
///
/// Call pattern:
/// 1. `let v = get_iam_version(&http, &endpoint).await;` BEFORE the mutation
/// 2. Perform the IAM mutation (POST /users, PUT /groups/..., etc.)
/// 3. `wait_for_iam_rebuild(&http, &endpoint, v).await;` — returns as soon as
///    the counter has advanced, up to 5 seconds of polling at 20ms intervals.
///
/// Replaces the earlier `sleep(1s)` pattern, which was both slow (every
/// test paid 1s whether the rebuild took 5ms or 50ms) and flake-prone
/// on slower CI runners where 1s wasn't always enough.
///
/// Panics if the counter hasn't advanced within 5s — that either
/// indicates a rebuild regression or test-setup bug, both of which
/// should fail loudly.
pub async fn wait_for_iam_rebuild(client: &reqwest::Client, endpoint: &str, baseline: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut attempts = 0u32;
    loop {
        let current = get_iam_version(client, endpoint).await;
        if current > baseline {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "wait_for_iam_rebuild timed out after 5s: baseline={baseline}, \
                 current={current}, attempts={attempts} — either the IAM \
                 mutation didn't trigger rebuild_iam_index or the counter \
                 isn't being bumped"
            );
        }
        attempts += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

/// Read the proxy's external-auth (OAuth/OIDC provider) version counter.
///
/// Backed by `GET /_/api/admin/ext-auth/version`, incremented by
/// `rebuild_external_auth` (src/api/admin/external_auth.rs) AFTER the
/// rebuilt provider set is live. Sibling of [`get_iam_version`]. Used by
/// [`wait_for_ext_auth_rebuild`] as the barrier primitive for provider
/// mutations.
pub async fn get_ext_auth_version(client: &reqwest::Client, endpoint: &str) -> u64 {
    let resp = client
        .get(format!("{endpoint}/_/api/admin/ext-auth/version"))
        .send()
        .await
        .expect("ext-auth/version GET");
    assert!(
        resp.status().is_success(),
        "ext-auth/version must return 2xx, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("ext-auth/version JSON");
    body["version"].as_u64().expect("version is u64")
}

/// Wait until the proxy's external-auth rebuild counter advances past
/// `baseline`. Same call pattern as [`wait_for_iam_rebuild`] but for
/// OAuth/OIDC provider mutations (create/update/delete provider). Panics
/// after 5s — a missing counter advance means `rebuild_external_auth`
/// either didn't run or failed (and so correctly did NOT bump on error).
pub async fn wait_for_ext_auth_rebuild(client: &reqwest::Client, endpoint: &str, baseline: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut attempts = 0u32;
    loop {
        let current = get_ext_auth_version(client, endpoint).await;
        if current > baseline {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "wait_for_ext_auth_rebuild timed out after 5s: baseline={baseline}, \
                 current={current}, attempts={attempts} — provider mutation didn't \
                 trigger rebuild_external_auth or the counter isn't being bumped"
            );
        }
        attempts += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

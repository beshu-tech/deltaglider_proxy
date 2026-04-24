//! Phase 3c.3 — Integration tests for the declarative-mode IAM reconciler.
//!
//! These cover the observable seam: `PUT /_/api/admin/config/section/access`
//! with an `iam_mode: declarative` body + `iam_users` / `iam_groups`
//! lists → DB rows materialise. A second PUT with a modified body →
//! diff-and-reconcile (add/update/delete). The pure-function diff
//! matrix lives in unit tests (`src/iam/declarative.rs::tests`); this
//! file only asserts end-to-end behaviour that needs a live server.
//!
//! Each test spawns its own TestServer (filesystem backend, bootstrap
//! auth for the admin cookie).

mod common;

use common::{admin_http_client, get_iam_version, wait_for_iam_rebuild, TestServer};
use reqwest::StatusCode;
use serde_json::json;

/// Apply a section patch to `/config/section/access` and assert the
/// HTTP status is 2xx. Returns the response body for further asserts.
async fn apply_access_section(
    admin: &reqwest::Client,
    endpoint: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let resp = admin
        .put(format!("{endpoint}/_/api/admin/config/section/access"))
        .json(&body)
        .send()
        .await
        .expect("apply_access_section request");
    let status = resp.status();
    let body_json: serde_json::Value = resp.json().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "apply_access_section must return 2xx, got {status}: {body_json}"
    );
    body_json
}

/// Fetch the current users list through the admin API (`/users`).
async fn list_users(admin: &reqwest::Client, endpoint: &str) -> Vec<serde_json::Value> {
    let resp = admin
        .get(format!("{endpoint}/_/api/admin/users"))
        .send()
        .await
        .expect("list users request");
    assert!(resp.status().is_success(), "list_users must 2xx");
    let body: serde_json::Value = resp.json().await.expect("users JSON");
    body.as_array().cloned().unwrap_or_default()
}

async fn list_groups(admin: &reqwest::Client, endpoint: &str) -> Vec<serde_json::Value> {
    let resp = admin
        .get(format!("{endpoint}/_/api/admin/groups"))
        .send()
        .await
        .expect("list groups request");
    assert!(resp.status().is_success(), "list_groups must 2xx");
    let body: serde_json::Value = resp.json().await.expect("groups JSON");
    body.as_array().cloned().unwrap_or_default()
}

// ═══════════════════════════════════════════════════
// 1. gui → declarative with IAM content creates users + groups
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn reconcile_gui_to_declarative_creates_users_and_groups() {
    let server = TestServer::builder()
        .auth("BOOTKEY1", "BOOTSECRET1")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Confirm we start in GUI mode with no users in the DB.
    let users = list_users(&admin, &server.endpoint()).await;
    assert_eq!(
        users.len(),
        0,
        "pre-reconcile: no users should exist, got {}",
        users.len()
    );

    let baseline = get_iam_version(&admin, &server.endpoint()).await;

    apply_access_section(
        &admin,
        &server.endpoint(),
        json!({
            "iam_mode": "declarative",
            "iam_groups": [
                {
                    "name": "admins",
                    "description": "full access",
                    "permissions": [
                        { "effect": "Allow", "actions": ["*"], "resources": ["*"] }
                    ]
                },
                {
                    "name": "readers",
                    "description": "",
                    "permissions": [
                        { "effect": "Allow", "actions": ["read", "list"], "resources": ["releases/*"] }
                    ]
                }
            ],
            "iam_users": [
                {
                    "name": "alice",
                    "access_key_id": "AKIAALICE0001",
                    "secret_access_key": "alice-secret",
                    "enabled": true,
                    "groups": ["admins"],
                    "permissions": []
                },
                {
                    "name": "bob",
                    "access_key_id": "AKIABOB000001",
                    "secret_access_key": "bob-secret",
                    "enabled": true,
                    "groups": ["readers"],
                    "permissions": []
                }
            ]
        }),
    )
    .await;

    wait_for_iam_rebuild(&admin, &server.endpoint(), baseline).await;

    let users = list_users(&admin, &server.endpoint()).await;
    let names: Vec<&str> = users.iter().filter_map(|u| u["name"].as_str()).collect();
    assert!(
        names.contains(&"alice") && names.contains(&"bob"),
        "users list must include alice + bob, got {names:?}"
    );

    let groups = list_groups(&admin, &server.endpoint()).await;
    let group_names: Vec<&str> = groups.iter().filter_map(|g| g["name"].as_str()).collect();
    assert!(
        group_names.contains(&"admins") && group_names.contains(&"readers"),
        "groups list must include admins + readers, got {group_names:?}"
    );
}

// ═══════════════════════════════════════════════════
// 2. Second apply with modified YAML updates + deletes
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn reconcile_second_apply_updates_and_deletes() {
    let server = TestServer::builder()
        .auth("BOOTKEY2", "BOOTSECRET2")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // First apply: alice + bob.
    let v0 = get_iam_version(&admin, &server.endpoint()).await;
    apply_access_section(
        &admin,
        &server.endpoint(),
        json!({
            "iam_mode": "declarative",
            "iam_groups": [
                { "name": "admins", "description": "", "permissions": [] }
            ],
            "iam_users": [
                {
                    "name": "alice",
                    "access_key_id": "AKIAALICE0001",
                    "secret_access_key": "sk-alice",
                    "enabled": true,
                    "groups": ["admins"],
                    "permissions": []
                },
                {
                    "name": "bob",
                    "access_key_id": "AKIABOB000001",
                    "secret_access_key": "sk-bob",
                    "enabled": true,
                    "groups": [],
                    "permissions": []
                }
            ]
        }),
    )
    .await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), v0).await;

    let users_before = list_users(&admin, &server.endpoint()).await;
    let alice_id_before = users_before
        .iter()
        .find(|u| u["name"] == "alice")
        .and_then(|u| u["id"].as_i64())
        .expect("alice has id");

    // Second apply: bob removed, alice's access key rotated.
    let v1 = get_iam_version(&admin, &server.endpoint()).await;
    apply_access_section(
        &admin,
        &server.endpoint(),
        json!({
            "iam_mode": "declarative",
            "iam_groups": [
                { "name": "admins", "description": "", "permissions": [] }
            ],
            "iam_users": [
                {
                    "name": "alice",
                    "access_key_id": "AKIAALICE_NEW",
                    "secret_access_key": "sk-alice-new",
                    "enabled": true,
                    "groups": ["admins"],
                    "permissions": []
                }
            ]
        }),
    )
    .await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), v1).await;

    let users_after = list_users(&admin, &server.endpoint()).await;
    let names: Vec<&str> = users_after
        .iter()
        .filter_map(|u| u["name"].as_str())
        .collect();
    assert!(names.contains(&"alice"), "alice must still exist");
    assert!(
        !names.contains(&"bob"),
        "bob must be deleted; got {names:?}"
    );

    // ID preservation across the rotation: alice's DB id stays.
    let alice = users_after
        .iter()
        .find(|u| u["name"] == "alice")
        .expect("alice entry");
    assert_eq!(
        alice["id"].as_i64().expect("alice id"),
        alice_id_before,
        "alice's id must be preserved across access_key rotation (update, not delete+insert)"
    );
    assert_eq!(
        alice["access_key_id"].as_str(),
        Some("AKIAALICE_NEW"),
        "alice's access_key must be the rotated value"
    );
}

// ═══════════════════════════════════════════════════
// 3. Empty-YAML gate on gui→declarative flip
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn reconcile_rejects_gui_to_declarative_empty_yaml() {
    let server = TestServer::builder()
        .auth("BOOTKEY3", "BOOTSECRET3")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // First seed a user via GUI so the DB is non-empty — the empty-
    // gate is meant to prevent wiping an existing IAM state.
    let v0 = get_iam_version(&admin, &server.endpoint()).await;
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({
            "name": "pre-existing",
            "access_key_id": "AKIAPRE0001",
            "secret_access_key": "sk-pre",
            "permissions": [
                { "effect": "Allow", "actions": ["read"], "resources": ["*"] }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "GUI user create must succeed: {}",
        resp.status()
    );
    wait_for_iam_rebuild(&admin, &server.endpoint(), v0).await;

    // Confirm the user landed.
    let users = list_users(&admin, &server.endpoint()).await;
    assert!(
        users.iter().any(|u| u["name"] == "pre-existing"),
        "seed user must exist before the flip attempt"
    );

    // Attempt the flip with empty iam_users / iam_groups. Must 4xx.
    let resp = admin
        .put(format!(
            "{}/_/api/admin/config/section/access",
            server.endpoint()
        ))
        .json(&json!({
            "iam_mode": "declarative"
            // deliberately no iam_users / iam_groups
        }))
        .send()
        .await
        .unwrap();

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    assert!(
        !status.is_success(),
        "empty-YAML flip must be rejected, got {status}: {body}"
    );
    let error = body["error"].as_str().unwrap_or("");
    assert!(
        error.contains("wipe") || error.contains("empty"),
        "error must mention wipe/empty, got: {error}"
    );

    // DB state preserved — seed user still present.
    let users = list_users(&admin, &server.endpoint()).await;
    assert!(
        users.iter().any(|u| u["name"] == "pre-existing"),
        "pre-existing user must remain after rejected flip; got {users:?}"
    );
}

// ═══════════════════════════════════════════════════
// 4. Declarative → GUI preserves DB (no reconcile runs)
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn reconcile_declarative_to_gui_preserves_db() {
    let server = TestServer::builder()
        .auth("BOOTKEY4", "BOOTSECRET4")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Seed declarative state.
    let v0 = get_iam_version(&admin, &server.endpoint()).await;
    apply_access_section(
        &admin,
        &server.endpoint(),
        json!({
            "iam_mode": "declarative",
            "iam_groups": [{ "name": "admins", "description": "", "permissions": [] }],
            "iam_users": [
                {
                    "name": "alice",
                    "access_key_id": "AKIA_A",
                    "secret_access_key": "sk-a",
                    "enabled": true,
                    "groups": ["admins"],
                    "permissions": []
                }
            ]
        }),
    )
    .await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), v0).await;

    // Flip back to gui WITHOUT specifying iam_users. A reconciler
    // that fires in gui mode would delete alice; the declarative→gui
    // path must be a no-op on the DB.
    apply_access_section(
        &admin,
        &server.endpoint(),
        json!({
            "iam_mode": "gui"
            // no iam_users / iam_groups
        }),
    )
    .await;

    let users = list_users(&admin, &server.endpoint()).await;
    assert!(
        users.iter().any(|u| u["name"] == "alice"),
        "alice must survive declarative→gui flip, got {users:?}"
    );
}

// ═══════════════════════════════════════════════════
// 5. Validation error rolls back atomically (no partial writes)
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn reconcile_atomic_rollback_on_validation_error() {
    let server = TestServer::builder()
        .auth("BOOTKEY5", "BOOTSECRET5")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Seed a known-good declarative state.
    let v0 = get_iam_version(&admin, &server.endpoint()).await;
    apply_access_section(
        &admin,
        &server.endpoint(),
        json!({
            "iam_mode": "declarative",
            "iam_groups": [{ "name": "admins", "description": "", "permissions": [] }],
            "iam_users": [
                {
                    "name": "alice",
                    "access_key_id": "AKIA_A",
                    "secret_access_key": "sk-a",
                    "enabled": true,
                    "groups": ["admins"],
                    "permissions": []
                }
            ]
        }),
    )
    .await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), v0).await;

    // Second apply references an undefined group — validation must
    // reject BEFORE any DB writes. Prior DB state must persist intact.
    let resp = admin
        .put(format!(
            "{}/_/api/admin/config/section/access",
            server.endpoint()
        ))
        .json(&json!({
            "iam_mode": "declarative",
            "iam_groups": [{ "name": "admins", "description": "", "permissions": [] }],
            "iam_users": [
                {
                    "name": "alice",
                    "access_key_id": "AKIA_A",
                    "secret_access_key": "sk-a",
                    "enabled": true,
                    "groups": ["ghosts"], // unknown group
                    "permissions": []
                },
                {
                    "name": "bob",
                    "access_key_id": "AKIA_B",
                    "secret_access_key": "sk-b",
                    "enabled": true,
                    "groups": [],
                    "permissions": []
                }
            ]
        }))
        .send()
        .await
        .unwrap();

    let status = resp.status();
    assert!(
        !status.is_success(),
        "unknown-group reference must reject the whole apply, got {status}"
    );

    // DB unchanged: alice still there, bob NOT created (the same apply
    // tried to create bob — atomicity means bob stays out too).
    let users = list_users(&admin, &server.endpoint()).await;
    let names: Vec<&str> = users.iter().filter_map(|u| u["name"].as_str()).collect();
    assert!(names.contains(&"alice"), "alice must remain");
    assert!(
        !names.contains(&"bob"),
        "bob must NOT be created — atomicity broken, got {names:?}"
    );
}

// ═══════════════════════════════════════════════════
// 6. Idempotence — reapplying the same YAML is a no-op
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn reconcile_idempotent_reapply_is_noop() {
    let server = TestServer::builder()
        .auth("BOOTKEY6", "BOOTSECRET6")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let body = json!({
        "iam_mode": "declarative",
        "iam_groups": [{ "name": "admins", "description": "full", "permissions": [] }],
        "iam_users": [
            {
                "name": "alice",
                "access_key_id": "AKIA_A",
                "secret_access_key": "sk-a",
                "enabled": true,
                "groups": ["admins"],
                "permissions": []
            }
        ]
    });

    let v0 = get_iam_version(&admin, &server.endpoint()).await;
    apply_access_section(&admin, &server.endpoint(), body.clone()).await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), v0).await;

    let users_1 = list_users(&admin, &server.endpoint()).await;
    let alice_id_1 = users_1
        .iter()
        .find(|u| u["name"] == "alice")
        .and_then(|u| u["id"].as_i64())
        .expect("alice id after first apply");

    // Second identical apply — DB state must be unchanged. We do NOT
    // gate on wait_for_iam_rebuild here because an empty diff might
    // not bump the version.
    let second_resp = apply_access_section(&admin, &server.endpoint(), body).await;

    let users_2 = list_users(&admin, &server.endpoint()).await;
    let alice_id_2 = users_2
        .iter()
        .find(|u| u["name"] == "alice")
        .and_then(|u| u["id"].as_i64())
        .expect("alice id after second apply");
    assert_eq!(
        alice_id_1, alice_id_2,
        "idempotent reapply must not change alice's DB id"
    );
    assert_eq!(
        users_1.len(),
        users_2.len(),
        "idempotent reapply must not add/remove users"
    );
    // The apply-response warnings must either be empty or not cite
    // any reconcile changes (the "declarative IAM reconciled: ..."
    // line only fires when something actually changed).
    let warnings: Vec<String> = second_resp["warnings"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !warnings.iter().any(|w| w.contains("reconciled:")),
        "second identical apply must not surface a reconcile-changes warning, got {warnings:?}"
    );
}

// ═══════════════════════════════════════════════════
// 7. Admin-API IAM mutations are 403-locked in declarative mode
// ═══════════════════════════════════════════════════

// ═══════════════════════════════════════════════════
// 9. Export-as-declarative round-trip (#6)
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn export_declarative_iam_round_trips_as_noop() {
    // End-to-end contract: export the DB as declarative YAML, paste
    // the result back via section PUT, confirm the reconciler reports
    // a no-op. This is the "Workflow A" button's reason for existing.
    let server = TestServer::builder()
        .auth("BOOTKEY_EXPORT", "BOOTSECRET_EXPORT")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Seed the DB via GUI: 1 group, 1 user in that group.
    let v0 = get_iam_version(&admin, &server.endpoint()).await;
    let group_resp = admin
        .post(format!("{}/_/api/admin/groups", server.endpoint()))
        .json(&json!({
            "name": "admins",
            "description": "full access",
            "permissions": [
                { "effect": "Allow", "actions": ["*"], "resources": ["*"] }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert!(
        group_resp.status().is_success(),
        "group create must succeed"
    );
    let group_body: serde_json::Value = group_resp.json().await.unwrap();
    let group_id = group_body["id"].as_i64().expect("group id");

    let user_resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({
            "name": "alice",
            "access_key_id": "AKIA_EXP_ALICE",
            "secret_access_key": "sk-alice-exp",
            "permissions": [
                { "effect": "Allow", "actions": ["read"], "resources": ["*"] }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert!(user_resp.status().is_success(), "user create must succeed");
    let user_body: serde_json::Value = user_resp.json().await.unwrap();
    let user_id = user_body["id"].as_i64().expect("user id");

    // Membership
    let _ = admin
        .post(format!(
            "{}/_/api/admin/groups/{}/members",
            server.endpoint(),
            group_id
        ))
        .json(&json!({ "user_id": user_id }))
        .send()
        .await
        .unwrap();
    wait_for_iam_rebuild(&admin, &server.endpoint(), v0).await;

    // Export as declarative YAML.
    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/declarative-iam-export",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let yaml_text = resp.text().await.unwrap();
    assert!(
        yaml_text.contains("iam_mode: declarative"),
        "export must declare declarative mode, got:\n{yaml_text}"
    );
    assert!(
        yaml_text.contains("name: alice") && yaml_text.contains("name: admins"),
        "export must include alice + admins, got:\n{yaml_text}"
    );
    // Secret redacted — must NOT contain the plaintext.
    assert!(
        !yaml_text.contains("sk-alice-exp"),
        "export must redact user secret_access_key, got:\n{yaml_text}"
    );

    // Parse YAML → access section → re-materialise secret → PUT as
    // section apply. Expected: is_noop == true, no DB changes.
    let mut parsed: serde_yaml::Value = serde_yaml::from_str(&yaml_text).expect("valid YAML");
    // Re-inject the secret so the diff sees "same as DB."
    let access_key = serde_yaml::Value::String("access".into());
    let iam_users_key = serde_yaml::Value::String("iam_users".into());
    let name_key = serde_yaml::Value::String("name".into());
    let alice_value = serde_yaml::Value::String("alice".into());
    let access = parsed
        .as_mapping_mut()
        .and_then(|m| m.get_mut(&access_key))
        .and_then(|v| v.as_mapping_mut())
        .expect("access map");
    let users = access
        .get_mut(&iam_users_key)
        .and_then(|v| v.as_sequence_mut())
        .expect("iam_users seq");
    for u in users {
        if let Some(map) = u.as_mapping_mut() {
            if map.get(&name_key) == Some(&alice_value) {
                map.insert(
                    serde_yaml::Value::String("secret_access_key".into()),
                    serde_yaml::Value::String("sk-alice-exp".into()),
                );
            }
        }
    }

    // Convert back to JSON to PUT via section PUT.
    let access_json = serde_json::to_value(
        parsed
            .as_mapping()
            .and_then(|m| m.get(&access_key))
            .expect("access value"),
    )
    .unwrap();
    let put_resp = admin
        .put(format!(
            "{}/_/api/admin/config/section/access",
            server.endpoint()
        ))
        .json(&access_json)
        .send()
        .await
        .unwrap();
    let status = put_resp.status();
    let body: serde_json::Value = put_resp.json().await.unwrap();
    assert!(
        status.is_success(),
        "exported section must roundtrip via PUT, got {status}: {body}"
    );
    let warnings: Vec<String> = body["warnings"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !warnings
            .iter()
            .any(|w| w.starts_with("declarative IAM reconciled:")),
        "exported → PUT must be an idempotent no-op (no 'reconciled:' warning); \
         got warnings: {warnings:?}"
    );
}

// ═══════════════════════════════════════════════════
// 8. Validate / dry-run preview of the declarative reconcile
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn validate_surfaces_declarative_iam_preview_line() {
    let server = TestServer::builder()
        .auth("BOOTKEY_PREVIEW", "BOOTSECRET_PREVIEW")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Issue a DRY-RUN (validate, not PUT) of a declarative section
    // with IAM content. Must 200 + warn about the preview.
    let resp = admin
        .post(format!(
            "{}/_/api/admin/config/section/access/validate",
            server.endpoint()
        ))
        .json(&json!({
            "iam_mode": "declarative",
            "iam_groups": [{ "name": "admins", "description": "", "permissions": [] }],
            "iam_users": [
                {
                    "name": "alice",
                    "access_key_id": "AKIA_A",
                    "secret_access_key": "sk-a",
                    "enabled": true,
                    "groups": ["admins"],
                    "permissions": []
                }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let warnings: Vec<String> = body["warnings"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("declarative IAM preview:") && w.contains("users(+1")),
        "validate must surface the declarative reconcile preview, got: {warnings:?}"
    );

    // DB must be untouched — validate is dry-run.
    let users = list_users(&admin, &server.endpoint()).await;
    assert!(
        users.is_empty(),
        "validate must NOT mutate the DB, got users: {users:?}"
    );
}

#[tokio::test]
async fn validate_empty_yaml_declarative_flip_previews_refusal() {
    let server = TestServer::builder()
        .auth("BOOTKEY_PREVIEW2", "BOOTSECRET_PREVIEW2")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Seed a DB user via GUI first so the empty-flip would be
    // destructive, not a clean no-op.
    let v0 = get_iam_version(&admin, &server.endpoint()).await;
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({
            "name": "pre",
            "access_key_id": "AKIA_PRE",
            "secret_access_key": "sk-pre",
            "permissions": [{ "effect": "Allow", "actions": ["read"], "resources": ["*"] }]
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    wait_for_iam_rebuild(&admin, &server.endpoint(), v0).await;

    // Validate a flip to declarative with empty IAM. Must return
    // 200 (dry-run never 4xx's) but the preview warning must
    // explain the live apply would REFUSE.
    let resp = admin
        .post(format!(
            "{}/_/api/admin/config/section/access/validate",
            server.endpoint()
        ))
        .json(&json!({ "iam_mode": "declarative" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let warnings: Vec<String> = body["warnings"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("REFUSED") || w.contains("wipe")),
        "validate must preview the empty-flip refusal, got: {warnings:?}"
    );
}

#[tokio::test]
async fn declarative_mode_blocks_admin_api_iam_mutations() {
    let server = TestServer::builder()
        .auth("BOOTKEY7", "BOOTSECRET7")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Seed declarative state.
    let v0 = get_iam_version(&admin, &server.endpoint()).await;
    apply_access_section(
        &admin,
        &server.endpoint(),
        json!({
            "iam_mode": "declarative",
            "iam_groups": [{ "name": "admins", "description": "", "permissions": [] }],
            "iam_users": [
                {
                    "name": "alice",
                    "access_key_id": "AKIA_A",
                    "secret_access_key": "sk-a",
                    "enabled": true,
                    "groups": ["admins"],
                    "permissions": []
                }
            ]
        }),
    )
    .await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), v0).await;

    // Attempt a user create via the admin API — must 403 in declarative.
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({
            "name": "sneaky",
            "access_key_id": "AKIA_SNEAK",
            "secret_access_key": "sk-s",
            "permissions": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "declarative mode must block admin-API user creation"
    );

    // The user must NOT appear in the DB.
    let users = list_users(&admin, &server.endpoint()).await;
    assert!(
        !users.iter().any(|u| u["name"] == "sneaky"),
        "403-blocked mutation must leave DB untouched"
    );
}

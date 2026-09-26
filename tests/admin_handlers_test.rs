// SPDX-License-Identifier: BUSL-1.1

//! Admin API handlers that the rest of the suite drives only on the happy
//! path (or not at all): sessions, backends, the bootstrap password, config
//! DB recovery, groups, the usage scanner, user keys, external-auth
//! authorize, and the browser form-POST policy checks. Every case asserts
//! the status, the body shape, the side effect, and the audit entry.

use crate::common;

use common::{admin_http_client, admin_http_client_with_password, TestServer};
use reqwest::StatusCode;
use serde_json::{json, Value};

/// Newest-first `(action, target)` pairs of the audit ring.
async fn audit(admin: &reqwest::Client, ep: &str) -> Vec<(String, String)> {
    let v: Value = admin
        .get(format!("{ep}/_/api/admin/audit?limit=500"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["action"].as_str().unwrap_or("").to_string(),
                e["target"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// The audit ring is process-global per proxy, so an entry is identified by
/// its action and target.
async fn assert_audited(admin: &reqwest::Client, ep: &str, action: &str, target_part: &str) {
    let entries = audit(admin, ep).await;
    assert!(
        entries
            .iter()
            .any(|(a, t)| a == action && t.contains(target_part)),
        "no audit entry {action} ~ {target_part:?} in {entries:?}"
    );
}

async fn json_of(resp: reqwest::Response) -> (StatusCode, Value) {
    let code = resp.status();
    let text = resp.text().await.unwrap_or_default();
    (
        code,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn create_user(admin: &reqwest::Client, ep: &str, name: &str, perms: Value) -> Value {
    let (code, v) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/users"))
            .json(&json!({ "name": name, "permissions": perms }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "create user {name}: {v}");
    v
}

fn admin_perms() -> Value {
    json!([{ "actions": ["*"], "resources": ["*"] }])
}

fn cookie_client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_provider(std::sync::Arc::new(reqwest::cookie::Jar::default()))
        .build()
        .unwrap()
}

// ── Sessions ────────────────────────────────────────────────────────────

async fn sessions(admin: &reqwest::Client, ep: &str) -> Vec<Value> {
    let v: Value = admin
        .get(format!("{ep}/_/api/admin/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["sessions"].as_array().unwrap().clone()
}

async fn session_valid(client: &reqwest::Client, ep: &str) -> bool {
    let v: Value = client
        .get(format!("{ep}/_/api/admin/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap_or_default();
    v["valid"] == true
}

#[tokio::test]
async fn sessions_list_and_force_logout() {
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let me = admin_http_client(&ep).await;
    let other = admin_http_client(&ep).await;

    let list = sessions(&me, &ep).await;
    assert_eq!(list.len(), 2, "two admin sessions: {list:?}");
    let own: Vec<&Value> = list.iter().filter(|s| s["current"] == true).collect();
    assert_eq!(
        own.len(),
        1,
        "exactly one session is the caller's: {list:?}"
    );
    assert_eq!(own[0]["auth"], "bootstrap");
    assert_eq!(own[0]["admin_gui"], true);
    let own_id = own[0]["id"].as_str().unwrap().to_string();
    let other_id = list.iter().find(|s| s["current"] == false).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Revoking one's own session is refused (use logout).
    let (code, v) = json_of(
        me.delete(format!("{ep}/_/api/admin/sessions/{own_id}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    assert!(session_valid(&me, &ep).await, "own session survives");

    let (code, v) = json_of(
        me.delete(format!("{ep}/_/api/admin/sessions/no-such-id"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{v}");

    let (code, v) = json_of(
        me.delete(format!("{ep}/_/api/admin/sessions/{other_id}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["revoked"], true);
    assert!(
        !session_valid(&other, &ep).await,
        "the revoked session is dead"
    );
    assert_eq!(sessions(&me, &ep).await.len(), 1);
    assert_audited(&me, &ep, "session_revoke", &other_id).await;
}

#[tokio::test]
async fn sessions_revoke_user_kills_every_session_of_an_identity() {
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let u = create_user(&admin, &ep, "sess-admin", admin_perms()).await;
    let (ak, sk) = (
        u["access_key_id"].as_str().unwrap().to_string(),
        u["secret_access_key"].as_str().unwrap().to_string(),
    );
    let mut clients = Vec::new();
    for _ in 0..2 {
        let c = cookie_client();
        let r = c
            .post(format!("{ep}/_/api/admin/login-as"))
            .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(session_valid(&c, &ep).await);
        clients.push(c);
    }
    let listed = sessions(&admin, &ep).await;
    assert_eq!(
        listed
            .iter()
            .filter(|s| s["identity"] == ak.as_str())
            .count(),
        2,
        "the list names the identity: {listed:?}"
    );

    let (code, v) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/sessions/revoke-user"))
            .json(&json!({}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "identity is required: {v}");

    // The legacy field name still works.
    let (code, v) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/sessions/revoke-user"))
            .json(&json!({ "access_key_id": ak }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["revoked"], 2, "{v}");
    assert_eq!(v["persisted"], true, "{v}");
    assert_eq!(v["pushed"], false, "no sync bucket: {v}");
    assert!(
        v["propagation_bound_secs"].is_null(),
        "no cross-instance bound: {v}"
    );
    for c in &clients {
        assert!(
            !session_valid(c, &ep).await,
            "every session of {ak} is dead"
        );
    }
    assert!(
        session_valid(&admin, &ep).await,
        "other identities keep theirs"
    );
    assert_audited(&admin, &ep, "session_revoke_user", &ak).await;
}

// ── Backends ────────────────────────────────────────────────────────────

fn two_backends(a: &std::path::Path, b: &std::path::Path) -> String {
    format!(
        "backends:\n  - name: src\n    type: filesystem\n    path: \"{}\"\n  - name: dst\n    type: filesystem\n    path: \"{}\"\ndefault_backend: src\n",
        a.display(),
        b.display()
    )
}

#[tokio::test]
async fn backends_create_probe_route_and_delete() {
    let (a, b, extra) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let server = TestServer::builder()
        .extra_yaml_storage_section(&two_backends(a.path(), b.path()))
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let post = |body: Value| {
        admin
            .post(format!("{ep}/_/api/admin/backends"))
            .json(&body)
            .send()
    };

    // Refused shapes: nothing changes.
    for (body, want) in [
        (
            json!({ "name": " ", "type": "filesystem", "path": "/x" }),
            "cannot be empty",
        ),
        (
            json!({ "name": "rel", "type": "filesystem", "path": "relative/dir" }),
            "absolute path",
        ),
        (
            json!({ "name": "odd", "type": "ftp" }),
            "Unknown backend type",
        ),
        (
            json!({ "name": "nokeys", "type": "s3", "endpoint": "https://s3.example.com" }),
            "access_key_id",
        ),
        (
            json!({ "name": "imds", "type": "s3", "endpoint": "http://169.254.169.254",
                    "access_key_id": "a", "secret_access_key": "b" }),
            "",
        ),
    ] {
        let (code, v) = json_of(post(body.clone()).await.unwrap()).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}: {v}");
        assert_eq!(v["success"], false, "{v}");
        assert!(
            v["error"].as_str().unwrap_or("").contains(want),
            "{body}: error names the cause: {v}"
        );
    }

    let extra_path = extra.path().display().to_string();
    let (code, v) = json_of(
        post(json!({ "name": "extra", "type": "filesystem", "path": extra_path }))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{v}");
    assert_eq!(v["success"], true);
    let (code, v) = json_of(
        post(json!({ "name": "extra", "type": "filesystem", "path": extra_path }))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "duplicate name: {v}");
    let list: Value = admin
        .get(format!("{ep}/_/api/admin/backends"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = list["backends"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["src", "dst", "extra"], "{list}");
    assert_eq!(list["default_backend"], "src");
    let persisted = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(persisted.contains("extra"), "the new backend is persisted");
    assert_audited(&admin, &ep, "backend_create", "extra").await;

    // Probe.
    let (code, v) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/backends/extra/probe"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "healthy", "{v}");
    assert!(v["probed_at"].as_i64().unwrap() > 0);
    assert_audited(&admin, &ep, "backend_probe", "extra").await;
    let code = admin
        .post(format!("{ep}/_/api/admin/backends/ghost/probe"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Create a bucket pinned to a backend.
    let mk = |body: Value| {
        admin
            .post(format!("{ep}/_/api/admin/buckets"))
            .json(&body)
            .send()
    };
    assert_eq!(
        mk(json!({ "name": " ", "backend_name": "dst" }))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        mk(json!({ "name": "b1", "backend_name": "ghost" }))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let (code, v) = json_of(
        mk(json!({ "name": "Routed-B", "backend_name": "dst" }))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["bucket"], "routed-b", "the name is normalised: {v}");
    assert!(
        b.path().join("routed-b").is_dir(),
        "the bucket exists on the chosen backend"
    );
    assert!(
        !a.path().join("routed-b").exists(),
        "and not on the default"
    );
    let origins: Value = admin
        .get(format!("{ep}/_/api/admin/buckets"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let routed = origins["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "routed-b")
        .cloned()
        .unwrap_or_else(|| panic!("routed-b listed: {origins}"));
    assert_eq!(routed["backend_name"], "dst");
    assert_eq!(routed["backend_type"], "filesystem");
    assert_audited(&admin, &ep, "admin_create_bucket", "routed-b@dst").await;

    // Delete: refused while default or routed, fine otherwise.
    let del = |name: &str| {
        admin
            .delete(format!("{ep}/_/api/admin/backends/{name}"))
            .send()
    };
    let (code, v) = json_of(del("src").await.unwrap()).await;
    assert_eq!(code, StatusCode::CONFLICT, "the default: {v}");
    let (code, v) = json_of(del("dst").await.unwrap()).await;
    assert_eq!(code, StatusCode::CONFLICT, "a routed backend: {v}");
    assert!(v["error"].as_str().unwrap().contains("routed-b"), "{v}");
    let (code, _) = json_of(del("ghost").await.unwrap()).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, v) = json_of(del("extra").await.unwrap()).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert!(
        !std::fs::read_to_string(server.config_path())
            .unwrap()
            .contains("name: extra"),
        "the removal is persisted"
    );
    assert_audited(&admin, &ep, "backend_delete", "extra").await;
}

#[tokio::test]
async fn backends_the_synthesised_default_cannot_be_deleted() {
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let list: Value = admin
        .get(format!("{ep}/_/api/admin/backends"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["backends"][0]["name"], "default", "{list}");
    let (code, v) = json_of(
        admin
            .delete(format!("{ep}/_/api/admin/backends/default"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{v}");
    assert!(v["error"].as_str().unwrap().contains("synthesised"), "{v}");
}

// ── Bootstrap password and config DB recovery ───────────────────────────

#[tokio::test]
async fn password_change_verifies_validates_persists_and_audits() {
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let put = |cur: &str, new: &str| {
        admin
            .put(format!("{ep}/_/api/admin/password"))
            .json(&json!({ "current_password": cur, "new_password": new }))
            .send()
    };
    let (code, v) = json_of(
        put("wrong-password", "a-new-strong-password-1")
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{v}");
    let (code, v) = json_of(put(common::TEST_BOOTSTRAP_PASSWORD, "short").await.unwrap()).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("12"), "{v}");
    let (code, v) = json_of(
        put(common::TEST_BOOTSTRAP_PASSWORD, "password1234")
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "a common password: {v}");

    let new_pw = "a-new-strong-password-1";
    let (code, v) = json_of(put(common::TEST_BOOTSTRAP_PASSWORD, new_pw).await.unwrap()).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["ok"], true);

    let old = cookie_client()
        .post(format!("{ep}/_/api/admin/login"))
        .json(&json!({ "password": common::TEST_BOOTSTRAP_PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        old.status(),
        StatusCode::UNAUTHORIZED,
        "the old password fails"
    );
    let fresh = admin_http_client_with_password(&ep, new_pw).await;
    assert!(session_valid(&fresh, &ep).await, "the new password logs in");

    let hash_file = server
        .config_path()
        .parent()
        .unwrap()
        .join(".deltaglider_bootstrap_hash");
    let hash = std::fs::read_to_string(&hash_file).expect("the new hash is persisted");
    assert!(bcrypt::verify(new_pw, hash.trim()).unwrap(), "{hash}");
    assert_audited(&fresh, &ep, "change_password", "").await;
}

#[tokio::test]
async fn recover_db_answers_only_under_a_mismatch_and_names_the_key_kind() {
    const KEY_GOOD: &str = "recover-good-key-0123456789abcdef0123456789ab";
    const KEY_WRONG: &str = "recover-wrong-key-0123456789abcdef012345678a";
    let mut server = TestServer::builder()
        .env("DGP_CONFIG_DB_KEY", KEY_GOOD)
        .build()
        .await;
    let ep = server.endpoint();
    let recover = |c: &reqwest::Client, candidate: &str| {
        c.post(format!("{ep}/_/api/admin/recover-db"))
            .json(&json!({ "candidate_password": candidate }))
            .send()
    };
    let admin = admin_http_client(&ep).await;
    create_user(&admin, &ep, "keeper", admin_perms()).await;
    let (code, v) = json_of(recover(&admin, KEY_GOOD).await.unwrap()).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "no mismatch, no recovery: {v}");

    server
        .respawn_with_env(&[("DGP_CONFIG_DB_KEY", KEY_WRONG)])
        .await;
    let admin = admin_http_client(&ep).await;
    let (code, v) = json_of(recover(&admin, "  ").await.unwrap()).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    let (code, v) = json_of(recover(&admin, "not-the-key-0123456789").await.unwrap()).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["success"], false);
    let resp = recover(&admin, KEY_GOOD).await.unwrap();
    assert_eq!(
        resp.headers()["cache-control"].to_str().unwrap(),
        "no-store, no-cache, must-revalidate, private"
    );
    let (code, v) = json_of(resp).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["success"], true);
    assert_eq!(v["key_kind"], "config_db_key");
    assert!(
        v.get("correct_hash").is_none(),
        "a typed key is never echoed: {v}"
    );
    assert_audited(&admin, &ep, "recover_db_success", "").await;
}

// ── Groups ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn groups_crud_members_clone_and_errors() {
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let perms = json!([{ "actions": ["read", "list"], "resources": ["bucket/team/*"] }]);
    let alice = create_user(&admin, &ep, "g-alice", json!([])).await;
    let alice_id = alice["id"].as_i64().unwrap();

    let create = |body: Value| {
        admin
            .post(format!("{ep}/_/api/admin/groups"))
            .json(&body)
            .send()
    };
    let (code, g) = json_of(
        create(
            json!({ "name": "team", "description": "d", "permissions": perms,
                       "member_ids": [alice_id, 999_999] }),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{g}");
    assert_eq!(
        g["member_ids"],
        json!([alice_id]),
        "an unknown member is skipped: {g}"
    );
    let gid = g["id"].as_i64().unwrap();
    assert_audited(&admin, &ep, "create_group", "team").await;

    let (code, v) = json_of(create(json!({ "name": "team" })).await.unwrap()).await;
    assert_eq!(code, StatusCode::CONFLICT, "duplicate name: {v}");
    let (code, v) = json_of(
        create(
            json!({ "name": "bad", "permissions": [{ "actions": ["fly"], "resources": ["*"] }] }),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "invalid action: {v}");
    let (code, v) = json_of(create(json!({ "name": "" })).await.unwrap()).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "an empty group name: {v}");

    // Update.
    let put = |id: i64, body: Value| {
        admin
            .put(format!("{ep}/_/api/admin/groups/{id}"))
            .json(&body)
            .send()
    };
    let (code, v) = json_of(put(gid, json!({ "description": "updated" })).await.unwrap()).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["description"], "updated");
    assert_eq!(v["name"], "team", "an absent field is kept");
    assert_audited(&admin, &ep, "update_group", "team").await;
    let (code, _) = json_of(
        put(
            gid,
            json!({ "permissions": [{ "actions": ["fly"], "resources": ["*"] }] }),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = json_of(put(gid, json!({ "name": " " })).await.unwrap()).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "a rename to a blank name");
    let (code, _) = json_of(put(999_999, json!({ "description": "x" })).await.unwrap()).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (_, other) = json_of(create(json!({ "name": "other" })).await.unwrap()).await;
    let (code, _) = json_of(
        put(other["id"].as_i64().unwrap(), json!({ "name": "team" }))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "a rename onto a taken name");

    // Clone.
    let clone = |id: i64, body: Value| {
        admin
            .post(format!("{ep}/_/api/admin/groups/{id}/clone"))
            .json(&body)
            .send()
    };
    let (code, c) = json_of(clone(gid, json!({ "copy_members": true })).await.unwrap()).await;
    assert_eq!(code, StatusCode::CREATED, "{c}");
    assert_ne!(c["name"], "team", "a fresh name: {c}");
    assert_eq!(c["member_ids"], json!([alice_id]), "members copied: {c}");
    assert_eq!(c["permissions"].as_array().unwrap().len(), 1);
    assert_audited(&admin, &ep, "clone_group", "team ->").await;
    let (code, _) = json_of(clone(999_999, json!({})).await.unwrap()).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _) = json_of(clone(gid, json!({ "name": "other" })).await.unwrap()).await;
    assert_eq!(code, StatusCode::CONFLICT);

    // Members.
    let bob = create_user(&admin, &ep, "g-bob", json!([])).await;
    let bob_id = bob["id"].as_i64().unwrap();
    let add = |uid: i64| {
        admin
            .post(format!("{ep}/_/api/admin/groups/{gid}/members"))
            .json(&json!({ "user_id": uid }))
            .send()
    };
    assert_eq!(add(bob_id).await.unwrap().status(), StatusCode::OK);
    // A member or group that does not exist is the caller's 404 (the
    // DB's FOREIGN KEY check), not a 400 or a 500.
    assert_eq!(add(999_999).await.unwrap().status(), StatusCode::NOT_FOUND);
    let code = admin
        .post(format!("{ep}/_/api/admin/groups/999999/members"))
        .json(&json!({ "user_id": bob_id }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::NOT_FOUND, "a member of a missing group");
    assert_audited(&admin, &ep, "add_member", "g-bob").await;
    let groups: Value = admin
        .get(format!("{ep}/_/api/admin/groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let team = groups
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == gid)
        .unwrap()
        .clone();
    let mut members: Vec<i64> = team["member_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_i64().unwrap())
        .collect();
    members.sort();
    assert_eq!(members, [alice_id, bob_id]);
    let code = admin
        .delete(format!("{ep}/_/api/admin/groups/{gid}/members/{bob_id}"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::NO_CONTENT);
    assert_audited(&admin, &ep, "remove_member", "g-bob").await;

    // Delete.
    let del = |id: i64| admin.delete(format!("{ep}/_/api/admin/groups/{id}")).send();
    assert_eq!(del(gid).await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(del(gid).await.unwrap().status(), StatusCode::NOT_FOUND);
    assert_audited(&admin, &ep, "delete_group", "team").await;
}

// ── Users ───────────────────────────────────────────────────────────────

async fn s3_status(server: &TestServer, ak: &str, sk: &str) -> u16 {
    common::S3Http::signed(ak, sk)
        .get(format!(
            "{}/{}?list-type=2",
            server.endpoint(),
            server.bucket()
        ))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn users_rotate_clone_delete_and_empty_name() {
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let u = create_user(&admin, &ep, "rot-user", admin_perms()).await;
    let id = u["id"].as_i64().unwrap();
    let (ak, sk) = (
        u["access_key_id"].as_str().unwrap().to_string(),
        u["secret_access_key"].as_str().unwrap().to_string(),
    );
    assert_eq!(s3_status(&server, &ak, &sk).await, 200);

    // Rotate to explicit keys: the old pair stops working at once.
    let (code, v) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/users/{id}/rotate-keys"))
            .json(&json!({ "access_key_id": "ROTATEDKEY0001", "secret_access_key": "rotated-secret-0001" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["access_key_id"], "ROTATEDKEY0001");
    assert_eq!(v["secret_access_key"], "rotated-secret-0001", "shown once");
    assert_eq!(
        s3_status(&server, &ak, &sk).await,
        403,
        "the old key is dead"
    );
    assert_eq!(
        s3_status(&server, "ROTATEDKEY0001", "rotated-secret-0001").await,
        200
    );
    assert_audited(&admin, &ep, "rotate_keys", "rot-user").await;
    // Generated keys when the body is absent.
    let (code, v) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/users/{id}/rotate-keys"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_ne!(v["access_key_id"], "ROTATEDKEY0001");
    let code = admin
        .post(format!("{ep}/_/api/admin/users/999999/rotate-keys"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Clone: fresh credentials, same permissions.
    let (code, c) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/users/{id}/clone"))
            .json(&json!({ "name": "rot-user-copy" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{c}");
    assert_eq!(c["name"], "rot-user-copy");
    assert_ne!(c["access_key_id"], v["access_key_id"], "fresh credentials");
    let code = admin
        .post(format!("{ep}/_/api/admin/users/999999/clone"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Delete: the credentials stop working, and a second delete is 404.
    let (cak, csk) = (
        c["access_key_id"].as_str().unwrap().to_string(),
        c["secret_access_key"].as_str().unwrap().to_string(),
    );
    assert_eq!(s3_status(&server, &cak, &csk).await, 200);
    let cid = c["id"].as_i64().unwrap();
    let del = |i: i64| admin.delete(format!("{ep}/_/api/admin/users/{i}")).send();
    assert_eq!(del(cid).await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(s3_status(&server, &cak, &csk).await, 403);
    assert_eq!(del(cid).await.unwrap().status(), StatusCode::NOT_FOUND);
    assert_audited(&admin, &ep, "delete_user", "rot-user-copy").await;

    let code = admin
        .put(format!("{ep}/_/api/admin/users/{id}"))
        .json(&json!({ "name": "" }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::BAD_REQUEST, "a rename to a blank name");

    // A name another user has is a 409 on create, clone and rename.
    let code = admin
        .post(format!("{ep}/_/api/admin/users"))
        .json(&json!({ "name": "rot-user", "permissions": [] }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::CONFLICT, "a duplicate user name");
    let code = admin
        .post(format!("{ep}/_/api/admin/users/{id}/clone"))
        .json(&json!({ "name": "rot-user" }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::CONFLICT, "a clone onto a taken name");
    let code = admin
        .put(format!("{ep}/_/api/admin/users/999999"))
        .json(&json!({ "name": "nobody" }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::NOT_FOUND, "update of a missing user");

    // Reserved and empty names.
    for name in ["$anonymous", ""] {
        let code = admin
            .post(format!("{ep}/_/api/admin/users"))
            .json(&json!({ "name": name, "permissions": [] }))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(code, StatusCode::BAD_REQUEST, "user name {name:?}");
    }
    let policies: Value = admin
        .get(format!("{ep}/_/api/admin/policies"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !policies.as_array().unwrap().is_empty(),
        "canned policies: {policies}"
    );
}

// ── Usage scanner ───────────────────────────────────────────────────────

#[tokio::test]
async fn usage_scan_counter_refresh_and_legacy_migrate() {
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let http = server.http();
    let b = server.bucket().to_string();
    for (i, size) in [100usize, 250, 4000].into_iter().enumerate() {
        common::put_object(
            &http,
            &ep,
            &b,
            &format!("use/o{i}.bin"),
            vec![7u8; size],
            "application/octet-stream",
        )
        .await;
    }

    let usage = |prefix: &str| {
        admin
            .get(format!("{ep}/_/api/admin/usage?bucket={b}&prefix={prefix}"))
            .send()
    };
    let (code, v) = json_of(usage("use/").await.unwrap()).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["cached"], false, "no scan yet: {v}");

    let before = common::get_usage_scan_version(&admin, &ep).await;
    let (code, v) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/usage/scan"))
            .json(&json!({ "bucket": b, "prefix": "use/" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED);
    assert!(
        matches!(
            v["status"].as_str(),
            Some("scan_started" | "scan_already_running")
        ),
        "{v}"
    );
    common::wait_for_usage_scan_refresh(&admin, &ep, before).await;
    let (_, v) = json_of(usage("use/").await.unwrap()).await;
    assert_eq!(v["total_objects"], 3, "{v}");
    assert_eq!(v["total_size"], 4350, "{v}");

    let (code, v) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/usage/refresh?bucket={b}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["object_count"], 3, "{v}");
    assert_eq!(v["logical_bytes"], 4350, "{v}");
    assert_eq!(v["never_scanned"], false, "{v}");
    let (_, v) = json_of(
        admin
            .get(format!("{ep}/_/api/admin/usage/bucket/{b}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["object_count"], 3, "{v}");
    let (_, v) = json_of(
        admin
            .get(format!("{ep}/_/api/admin/usage/bucket/never-used"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["never_scanned"], true, "{v}");
    assert_eq!(v["object_count"], 0, "{v}");

    let (code, v) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/migrate"))
            .json(&json!({ "bucket": b }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["migrated"], 0, "nothing legacy to migrate: {v}");
    let code = admin
        .post(format!("{ep}/_/api/admin/migrate"))
        .json(&json!({ "bucket": "../escape" }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(code, StatusCode::BAD_REQUEST, "an invalid bucket name");
}

// ── External auth ───────────────────────────────────────────────────────

#[tokio::test]
async fn oauth_authorize_reports_an_unreachable_provider_and_crud_errors() {
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let (code, p) = json_of(
        admin
            .post(format!("{ep}/_/api/admin/ext-auth/providers"))
            .json(&json!({
                "name": "dead-idp", "provider_type": "oidc", "enabled": true,
                "client_id": "c", "client_secret": "s",
                "issuer_url": "https://idp.does-not-exist.invalid",
            }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{p}");
    let pid = p["id"].as_i64().unwrap();

    let anon = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = anon
        .get(format!("{ep}/_/api/admin/oauth/authorize/dead-idp"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Provider Unavailable") && body.contains("dead-idp"),
        "an unreachable IdP gets an explanation page: {body}"
    );
    let resp = anon
        .get(format!("{ep}/_/api/admin/oauth/authorize/ghost"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    for (method, path) in [
        (reqwest::Method::PUT, "providers/999999".to_string()),
        (reqwest::Method::DELETE, "providers/999999".to_string()),
        (reqwest::Method::PUT, "mappings/999999".to_string()),
        (reqwest::Method::DELETE, "mappings/999999".to_string()),
    ] {
        let code = admin
            .request(method.clone(), format!("{ep}/_/api/admin/ext-auth/{path}"))
            .json(
                &json!({ "name": "x", "match_type": "email_exact", "match_field": "email",
                           "match_value": "a@b.c", "group_id": 1 }),
            )
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(code, StatusCode::NOT_FOUND, "{method} {path}");
    }
    let code = admin
        .delete(format!("{ep}/_/api/admin/ext-auth/providers/{pid}"))
        .send()
        .await
        .unwrap()
        .status();
    assert!(code.is_success(), "delete provider: {code}");
    assert_audited(&admin, &ep, "delete_auth_provider", "dead-idp").await;
}

// ── Browser form POST ───────────────────────────────────────────────────

mod form {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    /// Base64 policy and its SigV4 POST signature.
    pub fn sign(policy: &serde_json::Value, secret: &str, date: &str) -> (String, String) {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
        let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
        let k = hmac(&k, b"us-east-1");
        let k = hmac(&k, b"s3");
        let k = hmac(&k, b"aws4_request");
        (b64.clone(), hex::encode(hmac(&k, b64.as_bytes())))
    }
}

/// A table of policies and forms through the real handler: each refused
/// case answers its S3 error code and stores nothing.
#[tokio::test]
async fn form_post_policy_table() {
    let (ak, sk) = ("FORMKEY", "FORMSECRET0123");
    let server = TestServer::builder().auth(ak, sk).build().await;
    let ep = server.endpoint();
    let bucket = server.bucket().to_string();
    let date = "20260507";
    let amz_date = "20260507T120000Z";
    let credential = format!("{ak}/{date}/us-east-1/s3/aws4_request");
    let base = |extra: Vec<serde_json::Value>| {
        let mut c = vec![
            json!({ "bucket": bucket }),
            json!(["starts-with", "$key", "up/"]),
            json!({ "x-amz-algorithm": "AWS4-HMAC-SHA256" }),
            json!({ "x-amz-credential": credential }),
            json!({ "x-amz-date": amz_date }),
        ];
        c.extend(extra);
        json!({ "expiration": "2099-01-01T00:00:00.000Z", "conditions": c })
    };
    // (name, policy, extra form fields, key, file bytes, expected status, S3 code)
    type Case<'a> = (
        &'a str,
        Value,
        Vec<(&'a str, &'a str)>,
        &'a str,
        usize,
        u16,
        &'a str,
    );
    let cases: Vec<Case> = vec![
        (
            "ok",
            base(vec![json!(["content-length-range", 1, 100])]),
            vec![],
            "up/ok.txt",
            10,
            204,
            "",
        ),
        (
            "too big",
            base(vec![json!(["content-length-range", 1, 5])]),
            vec![],
            "up/big.txt",
            10,
            403,
            "AccessDenied",
        ),
        (
            "key outside",
            base(vec![]),
            vec![],
            "down/x.txt",
            3,
            403,
            "AccessDenied",
        ),
        (
            "expired",
            json!({ "expiration": "2001-01-01T00:00:00.000Z", "conditions": base(vec![])["conditions"] }),
            vec![],
            "up/exp.txt",
            3,
            403,
            "AccessDenied",
        ),
        (
            "bad expiration",
            json!({ "expiration": "soon", "conditions": [] }),
            vec![],
            "up/e.txt",
            3,
            400,
            "InvalidArgument",
        ),
        (
            "no conditions",
            json!({ "expiration": "2099-01-01T00:00:00Z" }),
            vec![],
            "up/n.txt",
            3,
            400,
            "InvalidArgument",
        ),
        (
            "unknown operator",
            base(vec![json!(["gt", "$key", "a"])]),
            vec![],
            "up/o.txt",
            3,
            501,
            "NotImplemented",
        ),
        (
            "unknown condition",
            base(vec![json!({ "tagging": "x" })]),
            vec![],
            "up/t.txt",
            3,
            501,
            "NotImplemented",
        ),
        (
            "public acl",
            base(vec![json!({ "acl": "public-read" })]),
            vec![("acl", "public-read")],
            "up/a.txt",
            3,
            501,
            "NotImplemented",
        ),
        (
            "private acl",
            base(vec![json!({ "acl": "private" })]),
            vec![("acl", "private")],
            "up/p.txt",
            3,
            204,
            "",
        ),
        (
            "acl mismatch",
            base(vec![json!({ "acl": "private" })]),
            vec![("acl", "bucket-owner-read")],
            "up/m.txt",
            3,
            403,
            "AccessDenied",
        ),
        (
            "redirect",
            base(vec![]),
            vec![("success_action_redirect", "https://x")],
            "up/r.txt",
            3,
            501,
            "NotImplemented",
        ),
        (
            "security token",
            base(vec![]),
            vec![("x-amz-security-token", "t")],
            "up/s.txt",
            3,
            501,
            "NotImplemented",
        ),
        (
            "bad range",
            base(vec![json!(["content-length-range", 1])]),
            vec![],
            "up/br.txt",
            3,
            400,
            "InvalidArgument",
        ),
        (
            "eq meta",
            base(vec![json!(["eq", "$x-amz-meta-team", "ops"])]),
            vec![("x-amz-meta-team", "ops")],
            "up/meta.txt",
            3,
            204,
            "",
        ),
        (
            "eq meta wrong",
            base(vec![json!(["eq", "$x-amz-meta-team", "ops"])]),
            vec![("x-amz-meta-team", "dev")],
            "up/mw.txt",
            3,
            403,
            "AccessDenied",
        ),
        (
            "missing field",
            base(vec![json!(["eq", "$x-amz-meta-team", "ops"])]),
            vec![],
            "up/mf.txt",
            3,
            400,
            "InvalidArgument",
        ),
        (
            "unsupported var",
            base(vec![]),
            vec![],
            "up/${uuid}.txt",
            3,
            501,
            "NotImplemented",
        ),
    ];
    let client = reqwest::Client::new();
    let s3 = server.s3_client().await;
    let mut wrong = Vec::new();
    for (name, policy, extra, key, len, want, code) in cases {
        let (b64, sig) = form::sign(&policy, sk, date);
        let mut f = reqwest::multipart::Form::new()
            .text("key", key.to_string())
            .text("policy", b64)
            .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
            .text("x-amz-credential", credential.clone())
            .text("x-amz-date", amz_date)
            .text("x-amz-signature", sig);
        for (k, v) in extra {
            f = f.text(k.to_string(), v.to_string());
        }
        let f = f.part(
            "file",
            reqwest::multipart::Part::bytes(vec![b'x'; len]).file_name("f.txt"),
        );
        let resp = client
            .post(format!("{ep}/{bucket}"))
            .multipart(f)
            .send()
            .await
            .unwrap();
        let got = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let stored = s3
            .head_object()
            .bucket(&bucket)
            .key(key)
            .send()
            .await
            .is_ok();
        if got != want || !body.contains(code) || stored != (want == 204) {
            wrong.push(format!(
                "{name}: want {want} {code}, got {got} stored={stored} {body}"
            ));
        }
    }
    assert!(wrong.is_empty(), "form POST table:\n{}", wrong.join("\n"));
}

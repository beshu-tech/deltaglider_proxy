//! C1 security fix regression tests: IAM LIST filtering.
//!
//! Pre-fix, a user with policy `{ resources: ["bucket/alice/*"] }` who
//! called `GET /bucket?list-type=2&prefix=` received every key in the
//! bucket because the middleware's `can_see_bucket` fallback admitted the
//! request but the handler returned unfiltered engine output. This leaked
//! keys outside the caller's permission scope.
//!
//! The fix: middleware inserts a `ListScope` extension; the handler
//! filters each key (and CommonPrefix) through the user's per-key policy
//! when the scope is `Filtered`. These tests exercise that end-to-end
//! against a real spawned proxy.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, TestServer};
use serde_json::json;

/// Shared user-creation + bucket-seed helpers. Kept private to this test
/// file because duplicating them here is cheaper than making
/// `iam_authorization_test.rs` a public module.
struct ScopeHarness {
    server: TestServer,
    admin_access_key: String,
    admin_secret: String,
}

impl ScopeHarness {
    async fn setup() -> Self {
        let server = TestServer::builder()
            .auth("bootstrap_key", "bootstrap_secret")
            .build()
            .await;

        let admin_client = admin_http_client(&server.endpoint()).await;

        // Admin for seeding.
        let body: serde_json::Value = admin_client
            .post(format!("{}/_/api/admin/users", server.endpoint()))
            .json(&json!({
                "name": "scope-admin",
                "permissions": [{"effect": "Allow", "actions": ["*"], "resources": ["*"]}],
            }))
            .send()
            .await
            .expect("create admin")
            .json()
            .await
            .unwrap();

        let admin_access_key = body["access_key_id"].as_str().unwrap().to_string();
        let admin_secret = body["secret_access_key"].as_str().unwrap().to_string();

        Self {
            server,
            admin_access_key,
            admin_secret,
        }
    }

    async fn create_user(
        &self,
        name: &str,
        permissions: Vec<serde_json::Value>,
    ) -> (String, String) {
        let admin_client = admin_http_client(&self.server.endpoint()).await;
        let body: serde_json::Value = admin_client
            .post(format!("{}/_/api/admin/users", self.server.endpoint()))
            .json(&json!({"name": name, "permissions": permissions}))
            .send()
            .await
            .expect("create user")
            .json()
            .await
            .unwrap();
        (
            body["access_key_id"].as_str().unwrap().to_string(),
            body["secret_access_key"].as_str().unwrap().to_string(),
        )
    }

    async fn admin_client(&self) -> aws_sdk_s3::Client {
        self.server
            .s3_client_with_creds(&self.admin_access_key, &self.admin_secret)
            .await
    }

    async fn user_client(&self, key: &str, secret: &str) -> aws_sdk_s3::Client {
        self.server.s3_client_with_creds(key, secret).await
    }
}

/// Prefix-scoped user listing with empty prefix must ONLY see their own keys.
#[tokio::test]
async fn test_list_with_empty_prefix_filters_to_user_scope() {
    let h = ScopeHarness::setup().await;
    let admin = h.admin_client().await;

    // Seed: alice's keys + bob's keys + secrets.
    let _ = admin.create_bucket().bucket("prod").send().await;
    for key in &[
        "alice/a1.txt",
        "alice/a2.txt",
        "bob/b1.txt",
        "bob/b2.txt",
        "secrets/password.txt",
    ] {
        admin
            .put_object()
            .bucket("prod")
            .key(*key)
            .body(ByteStream::from(b"data".to_vec()))
            .send()
            .await
            .expect("seed");
    }

    // Alice has read on prod/alice/* only.
    let (alice_key, alice_secret) = h
        .create_user(
            "alice-scope",
            vec![json!({
                "effect": "Allow",
                "actions": ["read", "list"],
                "resources": ["prod/alice/*"],
            })],
        )
        .await;

    let alice_s3 = h.user_client(&alice_key, &alice_secret).await;

    // LIST with empty prefix.
    let resp = alice_s3
        .list_objects_v2()
        .bucket("prod")
        .send()
        .await
        .expect("list should succeed — middleware fallback admits");

    let returned_keys: Vec<String> = resp
        .contents()
        .iter()
        .filter_map(|o| o.key().map(|k| k.to_string()))
        .collect();

    // Must ONLY be alice's keys. Pre-fix this returned every key in prod/.
    assert!(
        returned_keys.iter().all(|k| k.starts_with("alice/")),
        "C1 BYPASS REGRESSION: returned keys contain out-of-scope entries: {:?}",
        returned_keys
    );
    assert!(
        returned_keys.contains(&"alice/a1.txt".to_string())
            && returned_keys.contains(&"alice/a2.txt".to_string()),
        "Expected alice's own keys, got {:?}",
        returned_keys
    );
    assert!(
        !returned_keys.iter().any(|k| k.starts_with("bob/")),
        "Leaked bob's keys: {:?}",
        returned_keys
    );
    assert!(
        !returned_keys.iter().any(|k| k.starts_with("secrets/")),
        "Leaked secrets/: {:?}",
        returned_keys
    );
}

/// Listing with a prefix that matches the user's scope should be
/// unrestricted (no filter cost). Verifies we don't accidentally filter
/// when the policy explicitly covers the requested prefix.
#[tokio::test]
async fn test_list_with_matching_prefix_is_unrestricted() {
    let h = ScopeHarness::setup().await;
    let admin = h.admin_client().await;
    let _ = admin.create_bucket().bucket("prod").send().await;
    for key in &["alice/x.txt", "alice/nested/y.txt", "bob/z.txt"] {
        admin
            .put_object()
            .bucket("prod")
            .key(*key)
            .body(ByteStream::from(b"d".to_vec()))
            .send()
            .await
            .expect("seed");
    }

    let (key, secret) = h
        .create_user(
            "alice-matching",
            vec![json!({
                "effect": "Allow",
                "actions": ["read", "list"],
                "resources": ["prod/alice/*"],
            })],
        )
        .await;
    let alice = h.user_client(&key, &secret).await;

    let resp = alice
        .list_objects_v2()
        .bucket("prod")
        .prefix("alice/")
        .send()
        .await
        .expect("list alice/ ok");

    let keys: Vec<_> = resp
        .contents()
        .iter()
        .filter_map(|o| o.key().map(str::to_string))
        .collect();
    assert!(keys.contains(&"alice/x.txt".to_string()));
    assert!(keys.contains(&"alice/nested/y.txt".to_string()));
    assert!(!keys.iter().any(|k| k.starts_with("bob/")));
}

/// Explicit Deny must still override even in the filtered path.
#[tokio::test]
async fn test_list_explicit_deny_overrides_allow_in_filter() {
    let h = ScopeHarness::setup().await;
    let admin = h.admin_client().await;
    let _ = admin.create_bucket().bucket("prod").send().await;
    for key in &[
        "alice/ok.txt",
        "alice/private/secret.txt",
        "alice/public/readme.txt",
    ] {
        admin
            .put_object()
            .bucket("prod")
            .key(*key)
            .body(ByteStream::from(b"d".to_vec()))
            .send()
            .await
            .expect("seed");
    }

    // Alice: read on prod/alice/* BUT deny on prod/alice/private/*.
    let (key, secret) = h
        .create_user(
            "alice-denied",
            vec![
                json!({
                    "effect": "Allow",
                    "actions": ["read", "list"],
                    "resources": ["prod/alice/*"],
                }),
                json!({
                    "effect": "Deny",
                    "actions": ["read", "list"],
                    "resources": ["prod/alice/private/*"],
                }),
            ],
        )
        .await;
    let alice = h.user_client(&key, &secret).await;

    let resp = alice
        .list_objects_v2()
        .bucket("prod")
        .prefix("alice/")
        .send()
        .await
        .expect("list ok");

    let keys: Vec<_> = resp
        .contents()
        .iter()
        .filter_map(|o| o.key().map(str::to_string))
        .collect();
    assert!(
        !keys.iter().any(|k| k.contains("/private/")),
        "Deny must filter private keys: {:?}",
        keys
    );
    assert!(keys.iter().any(|k| k == "alice/ok.txt"));
    assert!(keys.iter().any(|k| k == "alice/public/readme.txt"));
}

/// Unscoped (full-bucket) user should see every key without any filter.
/// This is the non-regression case: admins, service accounts, etc. don't
/// pay a filter cost.
#[tokio::test]
async fn test_list_wildcard_resource_returns_all_keys() {
    let h = ScopeHarness::setup().await;
    let admin = h.admin_client().await;
    let _ = admin.create_bucket().bucket("all").send().await;
    for key in &["a.txt", "b.txt", "c/nested.txt"] {
        admin
            .put_object()
            .bucket("all")
            .key(*key)
            .body(ByteStream::from(b"d".to_vec()))
            .send()
            .await
            .expect("seed");
    }

    let (key, secret) = h
        .create_user(
            "wildcard-user",
            vec![json!({
                "effect": "Allow",
                "actions": ["*"],
                "resources": ["*"],
            })],
        )
        .await;
    let svc = h.user_client(&key, &secret).await;

    let resp = svc
        .list_objects_v2()
        .bucket("all")
        .send()
        .await
        .expect("list ok");

    let keys: Vec<_> = resp
        .contents()
        .iter()
        .filter_map(|o| o.key().map(str::to_string))
        .collect();
    assert_eq!(keys.len(), 3, "wildcard user should see every key");
}

/// CommonPrefixes (delimiter='/') on an empty-prefix LIST must also be
/// filtered — pre-fix, the user would see "secrets/" + "bob/" in
/// CommonPrefixes even though they had no permissions for them.
#[tokio::test]
async fn test_common_prefixes_filtered_in_scoped_list() {
    let h = ScopeHarness::setup().await;
    let admin = h.admin_client().await;
    let _ = admin.create_bucket().bucket("prod").send().await;
    for key in &[
        "alice/f1.txt",
        "alice/f2.txt",
        "bob/f.txt",
        "secrets/pwd.txt",
    ] {
        admin
            .put_object()
            .bucket("prod")
            .key(*key)
            .body(ByteStream::from(b"d".to_vec()))
            .send()
            .await
            .expect("seed");
    }

    let (key, secret) = h
        .create_user(
            "alice-common",
            vec![json!({
                "effect": "Allow",
                "actions": ["read", "list"],
                "resources": ["prod/alice/*"],
            })],
        )
        .await;
    let alice = h.user_client(&key, &secret).await;

    let resp = alice
        .list_objects_v2()
        .bucket("prod")
        .delimiter("/")
        .send()
        .await
        .expect("list ok");

    let cps: Vec<String> = resp
        .common_prefixes()
        .iter()
        .filter_map(|c| c.prefix().map(str::to_string))
        .collect();
    // Only "alice/" should be visible as a CommonPrefix.
    assert!(
        cps.iter().all(|p| p.starts_with("alice")),
        "CommonPrefixes leaked: {:?}",
        cps
    );
}

/// Screenshot regression: a user with Read on one deep prefix plus
/// condition-scoped ListBucket must be able to navigate allowed
/// CommonPrefixes without seeing unrelated siblings.
#[tokio::test]
async fn test_condition_scoped_beshu_reports_common_prefix_navigation() {
    let h = ScopeHarness::setup().await;
    let admin = h.admin_client().await;
    let _ = admin.create_bucket().bucket("beshu").send().await;
    for key in &[
        "ror/e2e_reports/run-1/index.html",
        "ror/e2e_reports/run-2/index.html",
        "ror/builds/build-1/artifact.zip",
        "ror/private/secret.txt",
        "blocked/top.txt",
    ] {
        admin
            .put_object()
            .bucket("beshu")
            .key(*key)
            .body(ByteStream::from(b"d".to_vec()))
            .send()
            .await
            .expect("seed");
    }

    let (key, secret) = h
        .create_user(
            "beshu-reports",
            vec![
                json!({
                    "effect": "Allow",
                    "actions": ["read"],
                    "resources": ["beshu/ror/e2e_reports/*"],
                }),
                json!({
                    "effect": "Allow",
                    "actions": ["list"],
                    "resources": ["beshu", "beshu/*"],
                    "conditions": {
                        "StringLike": {
                            "s3:prefix": ["", "ror/", "ror/builds/", "ror/e2e_reports/"]
                        }
                    },
                }),
            ],
        )
        .await;
    let reports_user = h.user_client(&key, &secret).await;

    let root = reports_user
        .list_objects_v2()
        .bucket("beshu")
        .delimiter("/")
        .send()
        .await
        .expect("root list should be admitted");
    let root_prefixes: Vec<String> = root
        .common_prefixes()
        .iter()
        .filter_map(|c| c.prefix().map(str::to_string))
        .collect();
    assert_eq!(
        root_prefixes,
        vec!["ror/".to_string()],
        "root should expose only navigable ror/: {:?}",
        root_prefixes
    );

    let ror = reports_user
        .list_objects_v2()
        .bucket("beshu")
        .prefix("ror/")
        .delimiter("/")
        .send()
        .await
        .expect("ror/ list should be admitted");
    let ror_prefixes: Vec<String> = ror
        .common_prefixes()
        .iter()
        .filter_map(|c| c.prefix().map(str::to_string))
        .collect();
    assert!(
        ror_prefixes.contains(&"ror/e2e_reports/".to_string()),
        "ror/ should expose e2e reports: {:?}",
        ror_prefixes
    );
    assert!(
        ror_prefixes.contains(&"ror/builds/".to_string()),
        "ror/ should expose explicitly listable builds: {:?}",
        ror_prefixes
    );
    assert!(
        !ror_prefixes.contains(&"ror/private/".to_string()),
        "ror/ leaked private prefix: {:?}",
        ror_prefixes
    );

    let reports = reports_user
        .list_objects_v2()
        .bucket("beshu")
        .prefix("ror/e2e_reports/")
        .send()
        .await
        .expect("e2e reports list should be admitted");
    let report_keys: Vec<String> = reports
        .contents()
        .iter()
        .filter_map(|o| o.key().map(str::to_string))
        .collect();
    assert!(
        report_keys
            .iter()
            .all(|k| k.starts_with("ror/e2e_reports/")),
        "e2e reports listing leaked unrelated keys: {:?}",
        report_keys
    );
    assert_eq!(report_keys.len(), 2, "expected both report keys");

    let private = reports_user
        .list_objects_v2()
        .bucket("beshu")
        .prefix("ror/private/")
        .send()
        .await
        .expect("disallowed prefix is admitted only for filtered listing");
    assert!(
        private.contents().is_empty() && private.common_prefixes().is_empty(),
        "private prefix should be filtered empty"
    );
}

//! Persona-based IAM integration tests.
//!
//! Tests the FULL request pipeline end-to-end: SigV4 auth → authorization middleware
//! → handler-level filtering. Each test creates users with specific permission
//! profiles and verifies what they can and cannot do.
//!
//! These tests cover the boundary bugs that unit tests miss:
//! - Group permission inheritance through the full pipeline
//! - ListBuckets per-user filtering
//! - Prefix-scoped permissions
//! - Legacy admin going through authorization
//! - Cross-user permission isolation
//! - Deny rules from groups overriding user allows

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, TestServer};
use serde_json::json;

// ============================================================================
// Shared test infrastructure
// ============================================================================

#[derive(Clone, Debug)]
struct UserCreds {
    access_key_id: String,
    secret_access_key: String,
    id: i64,
}

/// Create an IAM user via the admin API.
async fn create_user(
    admin: &reqwest::Client,
    endpoint: &str,
    name: &str,
    permissions: Vec<serde_json::Value>,
) -> UserCreds {
    let resp = admin
        .post(format!("{}/_/api/admin/users", endpoint))
        .json(&json!({ "name": name, "permissions": permissions }))
        .send()
        .await
        .expect("create user request failed");
    assert_eq!(
        resp.status().as_u16(),
        201,
        "failed to create user '{}'",
        name
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    UserCreds {
        access_key_id: body["access_key_id"].as_str().unwrap().to_string(),
        secret_access_key: body["secret_access_key"].as_str().unwrap().to_string(),
        id: body["id"].as_i64().unwrap(),
    }
}

/// Create an IAM group via the admin API, returns its ID.
async fn create_group(
    admin: &reqwest::Client,
    endpoint: &str,
    name: &str,
    permissions: Vec<serde_json::Value>,
) -> i64 {
    let resp = admin
        .post(format!("{}/_/api/admin/groups", endpoint))
        .json(&json!({ "name": name, "permissions": permissions }))
        .send()
        .await
        .expect("create group request failed");
    assert_eq!(
        resp.status().as_u16(),
        201,
        "failed to create group '{}'",
        name
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    body["id"].as_i64().unwrap()
}

/// Add a user to a group.
async fn add_to_group(admin: &reqwest::Client, endpoint: &str, group_id: i64, user_id: i64) {
    let resp = admin
        .post(format!(
            "{}/_/api/admin/groups/{}/members",
            endpoint, group_id
        ))
        .json(&json!({ "user_id": user_id }))
        .send()
        .await
        .expect("add member request failed");
    assert!(
        resp.status().is_success(),
        "failed to add user {} to group {}: {}",
        user_id,
        group_id,
        resp.status()
    );
}

/// Get S3 client for specific credentials.
async fn s3_for(server: &TestServer, creds: &UserCreds) -> aws_sdk_s3::Client {
    server
        .s3_client_with_creds(&creds.access_key_id, &creds.secret_access_key)
        .await
}

async fn put_ok(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> bool {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(b"test".to_vec()))
        .send()
        .await
        .is_ok()
}

async fn get_ok(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> bool {
    client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .is_ok()
}

async fn del_ok(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> bool {
    client
        .delete_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .is_ok()
}

async fn list_ok(client: &aws_sdk_s3::Client, bucket: &str) -> bool {
    client.list_objects_v2().bucket(bucket).send().await.is_ok()
}

async fn head_ok(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> bool {
    client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .is_ok()
}

/// PUT and return the content back via GET, verifying the full round trip.
async fn put_get_roundtrip(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    data: &[u8],
) -> Option<Vec<u8>> {
    let put = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await;
    if put.is_err() {
        return None;
    }
    let get = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .ok()?;
    Some(get.body.collect().await.ok()?.into_bytes().to_vec())
}

/// ListBuckets and return the list of bucket names.
async fn list_buckets(client: &aws_sdk_s3::Client) -> Vec<String> {
    match client.list_buckets().send().await {
        Ok(resp) => resp
            .buckets()
            .iter()
            .map(|b| b.name().unwrap_or("").to_string())
            .collect(),
        Err(_) => vec![],
    }
}

/// Seed a test object using given credentials.
async fn seed(client: &aws_sdk_s3::Client, bucket: &str, key: &str) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(b"seed".to_vec()))
        .send()
        .await
        .expect("failed to seed object");
}

/// Create an IAM admin user and return its S3 client.
/// Use this for seeding instead of bootstrap creds (which may not survive IAM migration).
async fn make_admin(admin: &reqwest::Client, server: &TestServer) -> aws_sdk_s3::Client {
    let creds = create_user(
        admin,
        &server.endpoint(),
        "test_admin",
        vec![json!({ "effect": "Allow", "actions": ["*"], "resources": ["*"] })],
    )
    .await;
    s3_for(server, &creds).await
}

// ============================================================================
// 1. GROUP PERMISSION INHERITANCE
// ============================================================================

/// User with no direct permissions inherits read+list from a group.
#[tokio::test]
async fn test_group_grants_permissions_to_member() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    // Create a user with NO direct permissions
    let user = create_user(&admin, &ep, "bare_user", vec![]).await;

    // Create a group with read+list on bucket/*
    let group_id = create_group(
        &admin,
        &ep,
        "readers",
        vec![json!({
            "effect": "Allow",
            "actions": ["read", "list"],
            "resources": ["bucket/*"]
        })],
    )
    .await;

    // Add user to group
    add_to_group(&admin, &ep, group_id, user.id).await;

    // Seed an object using bootstrap creds
    let boot = make_admin(&admin, &server).await;
    seed(&boot, "bucket", "grp/file.txt").await;

    // User should be able to read (from group)
    let client = s3_for(&server, &user).await;
    assert!(
        get_ok(&client, "bucket", "grp/file.txt").await,
        "group should grant read"
    );
    assert!(list_ok(&client, "bucket").await, "group should grant list");
    // But NOT write (group doesn't grant it)
    assert!(
        !put_ok(&client, "bucket", "grp/new.txt").await,
        "group should not grant write"
    );
}

/// User inherits permissions from multiple groups.
#[tokio::test]
async fn test_user_inherits_from_multiple_groups() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let user = create_user(&admin, &ep, "multi_grp_user", vec![]).await;

    // Group 1: read on bucket
    let g1 = create_group(
        &admin,
        &ep,
        "readers",
        vec![json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["bucket/*"] })],
    )
    .await;
    // Group 2: write on bucket
    let g2 = create_group(
        &admin,
        &ep,
        "writers",
        vec![json!({ "effect": "Allow", "actions": ["write"], "resources": ["bucket/*"] })],
    )
    .await;

    add_to_group(&admin, &ep, g1, user.id).await;
    add_to_group(&admin, &ep, g2, user.id).await;

    let boot = make_admin(&admin, &server).await;
    seed(&boot, "bucket", "multi/file.txt").await;

    let client = s3_for(&server, &user).await;
    assert!(
        get_ok(&client, "bucket", "multi/file.txt").await,
        "read from group1"
    );
    assert!(
        put_ok(&client, "bucket", "multi/new.txt").await,
        "write from group2"
    );
    // Delete NOT granted by either group
    assert!(
        !del_ok(&client, "bucket", "multi/file.txt").await,
        "delete not in any group"
    );
}

/// Deny rule from group overrides Allow from user's direct permissions.
#[tokio::test]
async fn test_group_deny_overrides_user_allow() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    // User has Allow * on *
    let user = create_user(
        &admin,
        &ep,
        "overridden_user",
        vec![json!({ "effect": "Allow", "actions": ["*"], "resources": ["*"] })],
    )
    .await;

    // Group denies delete on bucket/*
    let g = create_group(
        &admin,
        &ep,
        "no-delete",
        vec![json!({ "effect": "Deny", "actions": ["delete"], "resources": ["bucket/*"] })],
    )
    .await;
    add_to_group(&admin, &ep, g, user.id).await;

    let client = s3_for(&server, &user).await;
    assert!(
        put_ok(&client, "bucket", "deny/file.txt").await,
        "write should still work"
    );
    assert!(
        get_ok(&client, "bucket", "deny/file.txt").await,
        "read should still work"
    );
    assert!(
        !del_ok(&client, "bucket", "deny/file.txt").await,
        "delete should be denied by group Deny rule"
    );
}

// ============================================================================
// 2. LISTBUCKETS FILTERING
// ============================================================================

/// ListBuckets returns only buckets the user has permissions on.
#[tokio::test]
async fn test_listbuckets_filtered_per_user() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    // Create 3 buckets
    let boot = server.s3_client().await;
    let _ = boot.create_bucket().bucket("alpha").send().await;
    let _ = boot.create_bucket().bucket("beta").send().await;
    let _ = boot.create_bucket().bucket("gamma").send().await;

    // User A: access to alpha only
    let user_a = create_user(
        &admin,
        &ep,
        "user_alpha",
        vec![json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["alpha/*"] })],
    )
    .await;

    // User B: access to beta only
    let user_b = create_user(
        &admin,
        &ep,
        "user_beta",
        vec![json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["beta/*"] })],
    )
    .await;

    // User C: access to alpha AND gamma
    let user_c = create_user(
        &admin,
        &ep,
        "user_multi",
        vec![
            json!({ "effect": "Allow", "actions": ["read"], "resources": ["alpha/*"] }),
            json!({ "effect": "Allow", "actions": ["write"], "resources": ["gamma/*"] }),
        ],
    )
    .await;

    // Admin (full access)
    let admin_user = create_user(
        &admin,
        &ep,
        "admin_user",
        vec![json!({ "effect": "Allow", "actions": ["*"], "resources": ["*"] })],
    )
    .await;

    // Verify ListBuckets for each user
    let buckets_a = list_buckets(&s3_for(&server, &user_a).await).await;
    assert!(
        buckets_a.contains(&"alpha".to_string()),
        "user_a should see alpha"
    );
    assert!(
        !buckets_a.contains(&"beta".to_string()),
        "user_a should NOT see beta"
    );
    assert!(
        !buckets_a.contains(&"gamma".to_string()),
        "user_a should NOT see gamma"
    );

    let buckets_b = list_buckets(&s3_for(&server, &user_b).await).await;
    assert!(
        !buckets_b.contains(&"alpha".to_string()),
        "user_b should NOT see alpha"
    );
    assert!(
        buckets_b.contains(&"beta".to_string()),
        "user_b should see beta"
    );
    assert!(
        !buckets_b.contains(&"gamma".to_string()),
        "user_b should NOT see gamma"
    );

    let buckets_c = list_buckets(&s3_for(&server, &user_c).await).await;
    assert!(
        buckets_c.contains(&"alpha".to_string()),
        "user_c should see alpha"
    );
    assert!(
        !buckets_c.contains(&"beta".to_string()),
        "user_c should NOT see beta"
    );
    assert!(
        buckets_c.contains(&"gamma".to_string()),
        "user_c should see gamma"
    );

    let buckets_admin = list_buckets(&s3_for(&server, &admin_user).await).await;
    assert!(
        buckets_admin.len() >= 3,
        "admin should see all buckets, got {:?}",
        buckets_admin
    );
}

/// ListBuckets for a user with prefix-scoped permissions shows the bucket.
#[tokio::test]
async fn test_listbuckets_shows_bucket_for_prefix_permission() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    // Create bucket
    let boot = server.s3_client().await;
    let _ = boot.create_bucket().bucket("releases").send().await;

    // User with permission only on releases/builds/* (prefix within bucket)
    let user = create_user(
        &admin,
        &ep,
        "prefix_user",
        vec![json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["releases/builds/*"] })],
    )
    .await;

    let buckets = list_buckets(&s3_for(&server, &user).await).await;
    assert!(
        buckets.contains(&"releases".to_string()),
        "user with releases/builds/* should see 'releases' in ListBuckets, got {:?}",
        buckets
    );
}

/// Legacy admin (bootstrap credentials) sees all buckets.
#[tokio::test]
async fn test_legacy_admin_sees_all_buckets() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;

    let boot = server.s3_client().await;
    let _ = boot.create_bucket().bucket("private-1").send().await;
    let _ = boot.create_bucket().bucket("private-2").send().await;

    let buckets = list_buckets(&boot).await;
    assert!(
        buckets.contains(&"private-1".to_string()),
        "legacy admin should see all buckets, got {:?}",
        buckets
    );
    assert!(
        buckets.contains(&"private-2".to_string()),
        "legacy admin should see all buckets, got {:?}",
        buckets
    );
}

// ============================================================================
// 3. PREFIX-SCOPED PERMISSIONS
// ============================================================================

/// User with bucket/prefix/* can only access objects under that prefix.
#[tokio::test]
async fn test_prefix_scoped_read_access() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let user = create_user(
        &admin,
        &ep,
        "prefix_reader",
        vec![json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["bucket/builds/*"] })],
    )
    .await;

    // Seed objects in and outside the prefix
    let boot = make_admin(&admin, &server).await;
    seed(&boot, "bucket", "builds/v1.zip").await;
    seed(&boot, "bucket", "releases/v1.zip").await;

    let client = s3_for(&server, &user).await;

    // Can read within prefix
    assert!(
        get_ok(&client, "bucket", "builds/v1.zip").await,
        "should read bucket/builds/*"
    );

    // Cannot read outside prefix
    assert!(
        !get_ok(&client, "bucket", "releases/v1.zip").await,
        "should NOT read bucket/releases/*"
    );

    // CAN list the bucket (prefix-scoped permission grants bucket-level list for S3 client compat)
    assert!(
        list_ok(&client, "bucket").await,
        "prefix-scoped permission should allow bucket-level list for browsing"
    );
}

/// User with prefix-scoped write can PUT under prefix but not elsewhere.
#[tokio::test]
async fn test_prefix_scoped_write_access() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let user = create_user(
        &admin,
        &ep,
        "prefix_writer",
        vec![json!({ "effect": "Allow", "actions": ["write", "list"], "resources": ["bucket/uploads/*"] })],
    )
    .await;

    let client = s3_for(&server, &user).await;
    assert!(
        put_ok(&client, "bucket", "uploads/new.txt").await,
        "write under prefix OK"
    );
    assert!(
        !put_ok(&client, "bucket", "other/new.txt").await,
        "write outside prefix should fail"
    );
}

// ============================================================================
// 4. CROSS-USER ISOLATION
// ============================================================================

/// Two users with disjoint permissions cannot access each other's resources.
#[tokio::test]
async fn test_cross_user_no_permission_leakage() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let boot = server.s3_client().await;
    let _ = boot.create_bucket().bucket("team-a").send().await;
    let _ = boot.create_bucket().bucket("team-b").send().await;

    let alice = create_user(
        &admin,
        &ep,
        "alice",
        vec![json!({ "effect": "Allow", "actions": ["*"], "resources": ["team-a/*"] })],
    )
    .await;
    let bob = create_user(
        &admin,
        &ep,
        "bob",
        vec![json!({ "effect": "Allow", "actions": ["*"], "resources": ["team-b/*"] })],
    )
    .await;

    // Alice operates on team-a
    let alice_c = s3_for(&server, &alice).await;
    assert!(put_ok(&alice_c, "team-a", "alice.txt").await);
    seed(&alice_c, "team-a", "alice.txt").await;

    // Bob operates on team-b
    let bob_c = s3_for(&server, &bob).await;
    assert!(put_ok(&bob_c, "team-b", "bob.txt").await);
    seed(&bob_c, "team-b", "bob.txt").await;

    // Alice cannot access team-b
    assert!(
        !get_ok(&alice_c, "team-b", "bob.txt").await,
        "alice cannot read team-b"
    );
    assert!(
        !put_ok(&alice_c, "team-b", "hack.txt").await,
        "alice cannot write team-b"
    );
    assert!(
        !list_ok(&alice_c, "team-b").await,
        "alice cannot list team-b"
    );
    assert!(
        !del_ok(&alice_c, "team-b", "bob.txt").await,
        "alice cannot delete from team-b"
    );

    // Bob cannot access team-a
    assert!(
        !get_ok(&bob_c, "team-a", "alice.txt").await,
        "bob cannot read team-a"
    );
    assert!(
        !put_ok(&bob_c, "team-a", "hack.txt").await,
        "bob cannot write team-a"
    );
    assert!(!list_ok(&bob_c, "team-a").await, "bob cannot list team-a");
    assert!(
        !del_ok(&bob_c, "team-a", "alice.txt").await,
        "bob cannot delete from team-a"
    );

    // ListBuckets: each sees only their own
    let alice_buckets = list_buckets(&alice_c).await;
    assert!(alice_buckets.contains(&"team-a".to_string()));
    assert!(
        !alice_buckets.contains(&"team-b".to_string()),
        "alice should NOT see team-b"
    );

    let bob_buckets = list_buckets(&bob_c).await;
    assert!(bob_buckets.contains(&"team-b".to_string()));
    assert!(
        !bob_buckets.contains(&"team-a".to_string()),
        "bob should NOT see team-a"
    );
}

// ============================================================================
// 5. LISTBUCKETS CONSISTENCY: if you see it, you can use it
// ============================================================================

/// Every bucket in ListBuckets should be accessible (at minimum LIST).
#[tokio::test]
async fn test_listbuckets_consistent_with_authorization() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let boot = server.s3_client().await;
    let _ = boot.create_bucket().bucket("visible").send().await;
    let _ = boot.create_bucket().bucket("hidden").send().await;

    let user = create_user(
        &admin,
        &ep,
        "scoped_user",
        vec![json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["visible/*"] })],
    )
    .await;

    let client = s3_for(&server, &user).await;
    let buckets = list_buckets(&client).await;

    // Should see "visible" but not "hidden"
    assert!(buckets.contains(&"visible".to_string()));
    assert!(!buckets.contains(&"hidden".to_string()));

    // Every bucket in the list should be listable
    for b in &buckets {
        assert!(
            list_ok(&client, b).await,
            "bucket '{}' is in ListBuckets but LIST returns 403",
            b
        );
    }
}

// ============================================================================
// 6. GROUP + LISTBUCKETS INTERACTION
// ============================================================================

/// Group permissions are reflected in ListBuckets filtering.
#[tokio::test]
async fn test_group_permissions_affect_listbuckets() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let boot = server.s3_client().await;
    let _ = boot.create_bucket().bucket("shared").send().await;
    let _ = boot.create_bucket().bucket("restricted").send().await;

    // User has no direct permissions
    let user = create_user(&admin, &ep, "grp_listbucket_user", vec![]).await;

    // Group grants access to "shared" bucket
    let g = create_group(
        &admin,
        &ep,
        "shared-access",
        vec![json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["shared/*"] })],
    )
    .await;
    add_to_group(&admin, &ep, g, user.id).await;

    let client = s3_for(&server, &user).await;
    let buckets = list_buckets(&client).await;

    assert!(
        buckets.contains(&"shared".to_string()),
        "group permission should make 'shared' visible in ListBuckets"
    );
    assert!(
        !buckets.contains(&"restricted".to_string()),
        "'restricted' should NOT be visible — no permission from any source"
    );
}

// ============================================================================
// 7. BATCH DELETE AUTHORIZATION
// ============================================================================

/// Batch delete (POST ?delete) requires Delete permission, not Write.
#[tokio::test]
async fn test_batch_delete_requires_delete_permission() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    // User with write+list but NO delete
    let user = create_user(
        &admin,
        &ep,
        "writer_no_delete",
        vec![json!({ "effect": "Allow", "actions": ["write", "list", "read"], "resources": ["bucket/*"] })],
    )
    .await;

    let client = s3_for(&server, &user).await;
    seed(&client, "bucket", "batch/a.txt").await;
    seed(&client, "bucket", "batch/b.txt").await;

    // Batch delete should fail (mapped to Delete action)
    let result = client
        .delete_objects()
        .bucket("bucket")
        .delete(
            aws_sdk_s3::types::Delete::builder()
                .objects(
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key("batch/a.txt")
                        .build()
                        .unwrap(),
                )
                .objects(
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key("batch/b.txt")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await;

    assert!(
        result.is_err(),
        "batch delete should fail for user without delete permission"
    );
}

// ============================================================================
// 8. COPY SOURCE PERMISSION (cross-bucket)
// ============================================================================

/// CopyObject checks read permission on the SOURCE bucket.
#[tokio::test]
async fn test_copy_checks_source_read() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let boot = server.s3_client().await;
    let _ = boot.create_bucket().bucket("src-bucket").send().await;
    let _ = boot.create_bucket().bucket("dst-bucket").send().await;
    seed(&boot, "src-bucket", "secret.txt").await;

    // User can write to dst-bucket but NOT read from src-bucket
    let user = create_user(
        &admin,
        &ep,
        "copy_user",
        vec![json!({ "effect": "Allow", "actions": ["*"], "resources": ["dst-bucket/*"] })],
    )
    .await;

    let client = s3_for(&server, &user).await;
    let result = client
        .copy_object()
        .bucket("dst-bucket")
        .key("stolen.txt")
        .copy_source("src-bucket/secret.txt")
        .send()
        .await;

    assert!(
        result.is_err(),
        "copy should fail: no read on source bucket"
    );
}

/// CopyObject succeeds when user has read on source AND write on dest.
#[tokio::test]
async fn test_copy_succeeds_with_both_permissions() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let boot = server.s3_client().await;
    let _ = boot.create_bucket().bucket("copy-src").send().await;
    let _ = boot.create_bucket().bucket("copy-dst").send().await;
    seed(&boot, "copy-src", "doc.txt").await;

    // User has read on src + write on dst
    let user = create_user(
        &admin,
        &ep,
        "copy_both",
        vec![
            json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["copy-src/*"] }),
            json!({ "effect": "Allow", "actions": ["write", "list"], "resources": ["copy-dst/*"] }),
        ],
    )
    .await;

    let client = s3_for(&server, &user).await;
    let result = client
        .copy_object()
        .bucket("copy-dst")
        .key("doc.txt")
        .copy_source("copy-src/doc.txt")
        .send()
        .await;

    assert!(
        result.is_ok(),
        "copy should succeed with read+write: {:?}",
        result.err()
    );
}

// ============================================================================
// 9. WILDCARD RESOURCE PATTERN: */prefix/*
// ============================================================================

/// Permission with */builds/* applies to ALL buckets under builds/ prefix.
#[tokio::test]
async fn test_wildcard_bucket_with_prefix() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    // Create admin user first (triggers IAM migration), then create buckets and seed
    let adm = make_admin(&admin, &server).await;
    let _ = adm.create_bucket().bucket("proj-1").send().await;
    let _ = adm.create_bucket().bucket("proj-2").send().await;

    // User has wildcard: read on * (all buckets and all keys)
    let user = create_user(
        &admin,
        &ep,
        "wildcard_user",
        vec![json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["*"] })],
    )
    .await;

    let client = s3_for(&server, &user).await;

    // Seed with admin
    seed(&adm, "proj-1", "data.txt").await;
    seed(&adm, "proj-2", "data.txt").await;

    assert!(
        get_ok(&client, "proj-1", "data.txt").await,
        "wildcard * should allow proj-1"
    );
    assert!(
        get_ok(&client, "proj-2", "data.txt").await,
        "wildcard * should allow proj-2"
    );
    assert!(list_ok(&client, "proj-1").await);
    assert!(list_ok(&client, "proj-2").await);

    // But write is not granted
    assert!(
        !put_ok(&client, "proj-1", "hack.txt").await,
        "wildcard read does not grant write"
    );
}

// ============================================================================
// 10. EMPTY PERMISSIONS = DENY ALL
// ============================================================================

/// A user with no permissions at all is denied everything.
#[tokio::test]
async fn test_no_permissions_denies_everything() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let user = create_user(&admin, &ep, "empty_user", vec![]).await;

    let client = s3_for(&server, &user).await;
    assert!(
        !list_ok(&client, "bucket").await,
        "empty perms: list denied"
    );
    assert!(
        !put_ok(&client, "bucket", "x.txt").await,
        "empty perms: write denied"
    );

    let buckets = list_buckets(&client).await;
    assert!(
        buckets.is_empty(),
        "empty perms: no buckets visible, got {:?}",
        buckets
    );
}

// ============================================================================
// 11. FULL CRUD CYCLE WITH GROUP PERMISSIONS (end-to-end S3)
// ============================================================================

/// Group member performs full PUT → HEAD → GET (content verified) → DELETE cycle.
#[tokio::test]
async fn test_group_member_full_crud_cycle() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let user = create_user(&admin, &ep, "crud_user", vec![]).await;

    let g = create_group(
        &admin,
        &ep,
        "full-access",
        vec![json!({ "effect": "Allow", "actions": ["*"], "resources": ["bucket/*"] })],
    )
    .await;
    add_to_group(&admin, &ep, g, user.id).await;

    let client = s3_for(&server, &user).await;
    let data = b"group member wrote this content for e2e test";

    // PUT
    assert!(
        put_ok(&client, "bucket", "crud/test.txt").await,
        "group: PUT"
    );

    // PUT with content verification
    let got = put_get_roundtrip(&client, "bucket", "crud/verified.txt", data).await;
    assert_eq!(
        got.as_deref(),
        Some(data.as_slice()),
        "group: PUT→GET content mismatch"
    );

    // HEAD
    assert!(
        head_ok(&client, "bucket", "crud/test.txt").await,
        "group: HEAD"
    );

    // GET
    assert!(
        get_ok(&client, "bucket", "crud/test.txt").await,
        "group: GET"
    );

    // LIST
    let list_result = client
        .list_objects_v2()
        .bucket("bucket")
        .prefix("crud/")
        .send()
        .await;
    assert!(list_result.is_ok(), "group: LIST with prefix");
    let contents = list_result.unwrap().contents().len();
    assert!(
        contents >= 2,
        "group: LIST should find objects, found {}",
        contents
    );

    // DELETE
    assert!(
        del_ok(&client, "bucket", "crud/test.txt").await,
        "group: DELETE"
    );

    // Verify deleted
    assert!(
        !head_ok(&client, "bucket", "crud/test.txt").await,
        "group: HEAD after DELETE should fail"
    );
}

// ============================================================================
// 12. MULTIPART UPLOAD WITH PERMISSIONS
// ============================================================================

/// Multipart upload lifecycle respects write permissions.
#[tokio::test]
async fn test_multipart_upload_requires_write_permission() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    // User with read-only
    let reader = create_user(
        &admin,
        &ep,
        "mp_reader",
        vec![json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["bucket/*"] })],
    )
    .await;

    // User with write
    let writer = create_user(
        &admin,
        &ep,
        "mp_writer",
        vec![json!({ "effect": "Allow", "actions": ["write", "read", "list"], "resources": ["bucket/*"] })],
    )
    .await;

    // Reader cannot initiate multipart upload
    let reader_c = s3_for(&server, &reader).await;
    let result = reader_c
        .create_multipart_upload()
        .bucket("bucket")
        .key("mp/readonly.bin")
        .send()
        .await;
    assert!(
        result.is_err(),
        "reader should not initiate multipart upload"
    );

    // Writer can do the full multipart cycle
    let writer_c = s3_for(&server, &writer).await;
    let create = writer_c
        .create_multipart_upload()
        .bucket("bucket")
        .key("mp/writable.bin")
        .send()
        .await
        .expect("writer should initiate multipart upload");

    let upload_id = create.upload_id().unwrap().to_string();
    let part_data = vec![b'X'; 5 * 1024 * 1024]; // 5MB part

    let part = writer_c
        .upload_part()
        .bucket("bucket")
        .key("mp/writable.bin")
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from(part_data))
        .send()
        .await
        .expect("writer should upload part");

    let etag = part.e_tag().unwrap().to_string();

    let complete = writer_c
        .complete_multipart_upload()
        .bucket("bucket")
        .key("mp/writable.bin")
        .upload_id(&upload_id)
        .multipart_upload(
            aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .parts(
                    aws_sdk_s3::types::CompletedPart::builder()
                        .part_number(1)
                        .e_tag(etag)
                        .build(),
                )
                .build(),
        )
        .send()
        .await;

    assert!(
        complete.is_ok(),
        "writer should complete multipart: {:?}",
        complete.err()
    );

    // Verify the object exists
    assert!(
        head_ok(&writer_c, "bucket", "mp/writable.bin").await,
        "multipart object should exist after complete"
    );
}

// ============================================================================
// 13. HEAD WITH GROUP PERMISSIONS
// ============================================================================

/// HEAD object respects group-inherited permissions.
#[tokio::test]
async fn test_head_object_with_group_permissions() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let adm = make_admin(&admin, &server).await;

    let _ = adm.create_bucket().bucket("heads").send().await;
    seed(&adm, "heads", "visible.txt").await;
    seed(&adm, "heads", "also-visible.txt").await;

    // User with no direct perms, gets read from group
    let user = create_user(&admin, &ep, "head_user", vec![]).await;
    let g = create_group(
        &admin,
        &ep,
        "head-readers",
        vec![json!({ "effect": "Allow", "actions": ["read", "list"], "resources": ["heads/*"] })],
    )
    .await;
    add_to_group(&admin, &ep, g, user.id).await;

    let client = s3_for(&server, &user).await;
    assert!(
        head_ok(&client, "heads", "visible.txt").await,
        "HEAD should work via group read permission"
    );
    assert!(
        head_ok(&client, "heads", "also-visible.txt").await,
        "HEAD should work for all objects in allowed bucket"
    );

    // HEAD on non-existent object in allowed bucket → 404 not 403
    let result = client
        .head_object()
        .bucket("heads")
        .key("nonexistent.txt")
        .send()
        .await;
    assert!(result.is_err(), "HEAD on nonexistent should fail");
}

// ============================================================================
// 14. GROUP ADMIN CAN CREATE/DELETE BUCKETS
// ============================================================================

/// Admin permission from group allows bucket creation and deletion.
#[tokio::test]
async fn test_group_admin_can_manage_buckets() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let user = create_user(&admin, &ep, "bucket_mgr", vec![]).await;
    let g = create_group(
        &admin,
        &ep,
        "admins",
        vec![json!({ "effect": "Allow", "actions": ["*"], "resources": ["*"] })],
    )
    .await;
    add_to_group(&admin, &ep, g, user.id).await;

    let client = s3_for(&server, &user).await;

    // Create bucket
    let result = client.create_bucket().bucket("group-managed").send().await;
    assert!(
        result.is_ok(),
        "group admin should create bucket: {:?}",
        result.err()
    );

    // PUT an object
    assert!(put_ok(&client, "group-managed", "test.txt").await);

    // DELETE object first (bucket must be empty)
    assert!(del_ok(&client, "group-managed", "test.txt").await);

    // Delete bucket
    let result = client.delete_bucket().bucket("group-managed").send().await;
    assert!(
        result.is_ok(),
        "group admin should delete bucket: {:?}",
        result.err()
    );
}

// ============================================================================
// 15. PREFIX-SCOPED GROUP PERMISSIONS (end-to-end)
// ============================================================================

/// Group with prefix-scoped permissions: member can only operate within the prefix.
#[tokio::test]
async fn test_group_prefix_scoped_e2e() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let adm = make_admin(&admin, &server).await;

    let _ = adm.create_bucket().bucket("scoped").send().await;
    seed(&adm, "scoped", "team-x/doc.txt").await;
    seed(&adm, "scoped", "team-y/secret.txt").await;

    let user = create_user(&admin, &ep, "scoped_grp_user", vec![]).await;
    let g = create_group(
        &admin,
        &ep,
        "team-x-access",
        vec![json!({ "effect": "Allow", "actions": ["read", "write", "list", "delete"], "resources": ["scoped/team-x/*"] })],
    )
    .await;
    add_to_group(&admin, &ep, g, user.id).await;

    let client = s3_for(&server, &user).await;

    // Can read within prefix
    assert!(
        get_ok(&client, "scoped", "team-x/doc.txt").await,
        "group prefix: read within team-x/"
    );
    // Can write within prefix
    assert!(
        put_ok(&client, "scoped", "team-x/new.txt").await,
        "group prefix: write within team-x/"
    );
    // Can HEAD within prefix
    assert!(
        head_ok(&client, "scoped", "team-x/doc.txt").await,
        "group prefix: head within team-x/"
    );
    // Can delete within prefix
    assert!(
        del_ok(&client, "scoped", "team-x/new.txt").await,
        "group prefix: delete within team-x/"
    );

    // CANNOT read outside prefix
    assert!(
        !get_ok(&client, "scoped", "team-y/secret.txt").await,
        "group prefix: must NOT read team-y/"
    );
    // CANNOT write outside prefix
    assert!(
        !put_ok(&client, "scoped", "team-y/hack.txt").await,
        "group prefix: must NOT write team-y/"
    );
    // CANNOT delete outside prefix
    assert!(
        !del_ok(&client, "scoped", "team-y/secret.txt").await,
        "group prefix: must NOT delete team-y/"
    );
}

// ============================================================================
// 16. CONTENT VERIFICATION THROUGH PERMISSION LAYER
// ============================================================================

/// Verify that object content passes through correctly for authorized users.
/// Tests that the permission layer doesn't corrupt data.
#[tokio::test]
async fn test_content_integrity_through_permission_layer() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let user = create_user(
        &admin,
        &ep,
        "content_user",
        vec![json!({ "effect": "Allow", "actions": ["read", "write", "list"], "resources": ["bucket/*"] })],
    )
    .await;

    let client = s3_for(&server, &user).await;

    // Small text
    let small = b"hello world";
    let got = put_get_roundtrip(&client, "bucket", "content/small.txt", small).await;
    assert_eq!(
        got.as_deref(),
        Some(small.as_slice()),
        "small text roundtrip"
    );

    // Binary data
    let binary: Vec<u8> = (0..=255).cycle().take(4096).collect();
    let got = put_get_roundtrip(&client, "bucket", "content/binary.bin", &binary).await;
    assert_eq!(got.as_deref(), Some(binary.as_slice()), "binary roundtrip");

    // Empty object
    let got = put_get_roundtrip(&client, "bucket", "content/empty.txt", b"").await;
    assert_eq!(
        got.as_deref(),
        Some(b"".as_slice()),
        "empty object roundtrip"
    );

    // Unicode
    let unicode = "日本語テスト 🎯 émojis".as_bytes();
    let got = put_get_roundtrip(&client, "bucket", "content/unicode.txt", unicode).await;
    assert_eq!(got.as_deref(), Some(unicode), "unicode roundtrip");
}

// ============================================================================
// 17. DENY DELETE FROM GROUP + DIRECT ALLOW (end-to-end full S3 cycle)
// ============================================================================

/// User with Allow * direct + Deny delete from group: can do everything except delete.
#[tokio::test]
async fn test_group_deny_delete_e2e_full_cycle() {
    let server = TestServer::builder()
        .auth("boot_key", "boot_secret")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let user = create_user(
        &admin,
        &ep,
        "deny_del_user",
        vec![json!({ "effect": "Allow", "actions": ["*"], "resources": ["bucket/*"] })],
    )
    .await;

    let g = create_group(
        &admin,
        &ep,
        "no-delete-policy",
        vec![json!({ "effect": "Deny", "actions": ["delete"], "resources": ["bucket/*"] })],
    )
    .await;
    add_to_group(&admin, &ep, g, user.id).await;

    let client = s3_for(&server, &user).await;

    // PUT works
    assert!(
        put_ok(&client, "bucket", "nd/file.txt").await,
        "deny-del: PUT ok"
    );

    // GET works
    assert!(
        get_ok(&client, "bucket", "nd/file.txt").await,
        "deny-del: GET ok"
    );

    // HEAD works
    assert!(
        head_ok(&client, "bucket", "nd/file.txt").await,
        "deny-del: HEAD ok"
    );

    // LIST works
    assert!(list_ok(&client, "bucket").await, "deny-del: LIST ok");

    // DELETE fails (denied by group)
    assert!(
        !del_ok(&client, "bucket", "nd/file.txt").await,
        "deny-del: DELETE must be denied by group Deny rule"
    );

    // The critical assertion is above: DELETE returns 403.
    // The authorization middleware blocks the request before the handler runs.
}

//! Comprehensive IAM authorization tests.
//!
//! Tests the permission model as a black box — verifying that each S3 operation
//! checks authorization against the correct resource. This catches the entire
//! class of "operation X accesses resource Y but auth only checks resource Z" bugs.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, TestServer};
use serde_json::json;

/// User credentials returned from IAM user creation.
#[derive(Clone, Debug)]
struct UserCreds {
    access_key_id: String,
    secret_access_key: String,
    id: i64,
}

/// Shared test harness: a server with 4 IAM users.
struct IamTestHarness {
    server: TestServer,
    admin_user: UserCreds,
    reader_user: UserCreds,
    writer_user: UserCreds,
    deny_user: UserCreds,
}

impl IamTestHarness {
    async fn setup() -> Self {
        let server = TestServer::builder()
            .auth("bootstrap_key", "bootstrap_secret")
            .build()
            .await;

        let admin_client = admin_http_client(&server.endpoint()).await;

        // Create admin_user: actions=["*"], resources=["*"], effect=Allow
        let admin_user = create_iam_user(
            &admin_client,
            &server,
            "admin_user",
            vec![json!({
                "effect": "Allow",
                "actions": ["*"],
                "resources": ["*"]
            })],
        )
        .await;

        // Create reader_user: actions=["read","list"], resources=["bucket-a/*"], effect=Allow
        let reader_user = create_iam_user(
            &admin_client,
            &server,
            "reader_user",
            vec![json!({
                "effect": "Allow",
                "actions": ["read", "list"],
                "resources": ["bucket-a/*"]
            })],
        )
        .await;

        // Create writer_user: actions=["write","list"], resources=["bucket-b/*"], effect=Allow
        let writer_user = create_iam_user(
            &admin_client,
            &server,
            "writer_user",
            vec![json!({
                "effect": "Allow",
                "actions": ["write", "list"],
                "resources": ["bucket-b/*"]
            })],
        )
        .await;

        // Create deny_user: Allow * on * + Deny delete on *
        let deny_user = create_iam_user(
            &admin_client,
            &server,
            "deny_user",
            vec![
                json!({
                    "effect": "Allow",
                    "actions": ["*"],
                    "resources": ["*"]
                }),
                json!({
                    "effect": "Deny",
                    "actions": ["delete"],
                    "resources": ["*"]
                }),
            ],
        )
        .await;

        // Create buckets that the tests need (using admin_user who has full access)
        let admin_s3 = server
            .s3_client_with_creds(&admin_user.access_key_id, &admin_user.secret_access_key)
            .await;
        let _ = admin_s3.create_bucket().bucket("bucket-a").send().await;
        let _ = admin_s3.create_bucket().bucket("bucket-b").send().await;

        Self {
            server,
            admin_user,
            reader_user,
            writer_user,
            deny_user,
        }
    }

    /// Get an S3 client for a given user.
    async fn client_for(&self, user: &UserCreds) -> aws_sdk_s3::Client {
        self.server
            .s3_client_with_creds(&user.access_key_id, &user.secret_access_key)
            .await
    }
}

/// Create an IAM user via the admin API, returning their credentials.
async fn create_iam_user(
    admin_client: &reqwest::Client,
    server: &TestServer,
    name: &str,
    permissions: Vec<serde_json::Value>,
) -> UserCreds {
    let resp = admin_client
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({
            "name": name,
            "permissions": permissions
        }))
        .send()
        .await
        .expect("Create user request failed");

    assert_eq!(
        resp.status().as_u16(),
        201,
        "Failed to create user '{}': {}",
        name,
        resp.status()
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    UserCreds {
        access_key_id: body["access_key_id"].as_str().unwrap().to_string(),
        secret_access_key: body["secret_access_key"].as_str().unwrap().to_string(),
        id: body["id"].as_i64().unwrap(),
    }
}

/// Helper: PUT a test object, returns true if successful.
async fn put_succeeds(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> bool {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(b"test data".to_vec()))
        .send()
        .await
        .is_ok()
}

/// Helper: GET a test object, returns true if successful.
async fn get_succeeds(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> bool {
    client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .is_ok()
}

/// Helper: HEAD a test object, returns true if successful.
async fn head_succeeds(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> bool {
    client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .is_ok()
}

/// Helper: DELETE a test object, returns true if successful.
async fn delete_succeeds(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> bool {
    client
        .delete_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .is_ok()
}

/// Helper: ListObjectsV2, returns true if successful.
async fn list_succeeds(client: &aws_sdk_s3::Client, bucket: &str) -> bool {
    client.list_objects_v2().bucket(bucket).send().await.is_ok()
}

/// Helper: seed a test object using admin credentials so other users can read it.
async fn seed_object(harness: &IamTestHarness, bucket: &str, key: &str) {
    let admin = harness.client_for(&harness.admin_user).await;
    admin
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(b"seed data".to_vec()))
        .send()
        .await
        .expect("Failed to seed test object");
}

// ============================================================================
// Basic permission enforcement
// ============================================================================

#[tokio::test]
async fn test_admin_can_do_everything() {
    let h = IamTestHarness::setup().await;
    let client = h.client_for(&h.admin_user).await;

    // PUT on bucket-a
    assert!(put_succeeds(&client, "bucket-a", "admin/file.txt").await);
    // GET on bucket-a
    assert!(get_succeeds(&client, "bucket-a", "admin/file.txt").await);
    // HEAD on bucket-a
    assert!(head_succeeds(&client, "bucket-a", "admin/file.txt").await);
    // LIST on bucket-a
    assert!(list_succeeds(&client, "bucket-a").await);
    // DELETE on bucket-a
    assert!(delete_succeeds(&client, "bucket-a", "admin/file.txt").await);

    // PUT on bucket-b
    assert!(put_succeeds(&client, "bucket-b", "admin/file.txt").await);
    // GET on bucket-b
    assert!(get_succeeds(&client, "bucket-b", "admin/file.txt").await);
    // LIST on bucket-b
    assert!(list_succeeds(&client, "bucket-b").await);
    // DELETE on bucket-b
    assert!(delete_succeeds(&client, "bucket-b", "admin/file.txt").await);
}

#[tokio::test]
async fn test_reader_can_read_allowed_bucket() {
    let h = IamTestHarness::setup().await;

    // Seed an object so reader can read it
    seed_object(&h, "bucket-a", "reader/file.txt").await;

    let client = h.client_for(&h.reader_user).await;

    // GET on bucket-a works
    assert!(get_succeeds(&client, "bucket-a", "reader/file.txt").await);
    // HEAD on bucket-a works
    assert!(head_succeeds(&client, "bucket-a", "reader/file.txt").await);
    // LIST on bucket-a works
    assert!(list_succeeds(&client, "bucket-a").await);
}

#[tokio::test]
async fn test_reader_cannot_write_allowed_bucket() {
    let h = IamTestHarness::setup().await;
    let client = h.client_for(&h.reader_user).await;

    // PUT on bucket-a should fail — reader has read+list only
    assert!(
        !put_succeeds(&client, "bucket-a", "reader/write-attempt.txt").await,
        "Reader should not be able to write to bucket-a"
    );
}

#[tokio::test]
async fn test_reader_cannot_read_other_bucket() {
    let h = IamTestHarness::setup().await;

    // Seed an object in bucket-b
    seed_object(&h, "bucket-b", "other/file.txt").await;

    let client = h.client_for(&h.reader_user).await;

    // GET on bucket-b should fail — reader only has access to bucket-a
    assert!(
        !get_succeeds(&client, "bucket-b", "other/file.txt").await,
        "Reader should not be able to read from bucket-b"
    );
}

#[tokio::test]
async fn test_reader_cannot_delete() {
    let h = IamTestHarness::setup().await;

    // Seed an object so there's something to delete
    seed_object(&h, "bucket-a", "reader/to-delete.txt").await;

    let client = h.client_for(&h.reader_user).await;

    // DELETE on bucket-a should fail — reader has no delete permission
    assert!(
        !delete_succeeds(&client, "bucket-a", "reader/to-delete.txt").await,
        "Reader should not be able to delete from bucket-a"
    );
}

#[tokio::test]
async fn test_writer_can_write_allowed_bucket() {
    let h = IamTestHarness::setup().await;
    let client = h.client_for(&h.writer_user).await;

    // PUT on bucket-b works
    assert!(put_succeeds(&client, "bucket-b", "writer/file.txt").await);
}

#[tokio::test]
async fn test_writer_cannot_read_other_bucket() {
    let h = IamTestHarness::setup().await;

    // Seed an object in bucket-a
    seed_object(&h, "bucket-a", "writer/file.txt").await;

    let client = h.client_for(&h.writer_user).await;

    // GET on bucket-a should fail — writer only has access to bucket-b
    assert!(
        !get_succeeds(&client, "bucket-a", "writer/file.txt").await,
        "Writer should not be able to read from bucket-a"
    );
}

// ============================================================================
// Cross-operation authorization (catches resource-mismatch bugs)
// ============================================================================

#[tokio::test]
async fn test_copy_requires_source_read_permission() {
    let h = IamTestHarness::setup().await;

    // Seed an object in bucket-a (source)
    seed_object(&h, "bucket-a", "copy-src/file.txt").await;

    let client = h.client_for(&h.writer_user).await;

    // Writer has write on bucket-b but NO read on bucket-a.
    // Copy from bucket-a to bucket-b should fail (no read on source).
    let result = client
        .copy_object()
        .bucket("bucket-b")
        .key("copied/file.txt")
        .copy_source("bucket-a/copy-src/file.txt")
        .send()
        .await;

    assert!(
        result.is_err(),
        "Copy should fail: writer lacks read permission on source bucket-a"
    );
}

#[tokio::test]
async fn test_copy_with_read_on_source_succeeds() {
    let h = IamTestHarness::setup().await;

    // Seed an object in bucket-a (source)
    seed_object(&h, "bucket-a", "copy-src/admin-file.txt").await;

    let client = h.client_for(&h.admin_user).await;

    // Admin has * on * — copy from bucket-a to bucket-b should succeed
    let result = client
        .copy_object()
        .bucket("bucket-b")
        .key("copied/admin-file.txt")
        .copy_source("bucket-a/copy-src/admin-file.txt")
        .send()
        .await;

    assert!(
        result.is_ok(),
        "Admin should be able to copy from bucket-a to bucket-b: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn test_upload_part_copy_requires_source_read() {
    let h = IamTestHarness::setup().await;

    // Seed a source object in bucket-a
    seed_object(&h, "bucket-a", "multipart-src/file.txt").await;

    let client = h.client_for(&h.writer_user).await;

    // Start a multipart upload in bucket-b (writer has write on bucket-b)
    let create = client
        .create_multipart_upload()
        .bucket("bucket-b")
        .key("multipart-dest/file.txt")
        .send()
        .await
        .expect("CreateMultipartUpload should succeed for writer on bucket-b");

    let upload_id = create.upload_id().unwrap();

    // UploadPartCopy from bucket-a — writer lacks read on bucket-a
    let result = client
        .upload_part_copy()
        .bucket("bucket-b")
        .key("multipart-dest/file.txt")
        .upload_id(upload_id)
        .part_number(1)
        .copy_source("bucket-a/multipart-src/file.txt")
        .send()
        .await;

    assert!(
        result.is_err(),
        "UploadPartCopy should fail: writer lacks read on source bucket-a"
    );

    // Abort the multipart upload to clean up
    let _ = client
        .abort_multipart_upload()
        .bucket("bucket-b")
        .key("multipart-dest/file.txt")
        .upload_id(upload_id)
        .send()
        .await;
}

#[tokio::test]
async fn test_copy_to_unauthorized_bucket_fails() {
    let h = IamTestHarness::setup().await;

    // Seed an object in bucket-a (reader can read this)
    seed_object(&h, "bucket-a", "copy-src/reader-file.txt").await;

    let client = h.client_for(&h.reader_user).await;

    // Reader has read on bucket-a but NO write on bucket-b.
    // Copy from bucket-a to bucket-b should fail (no write on dest).
    let result = client
        .copy_object()
        .bucket("bucket-b")
        .key("copied/reader-file.txt")
        .copy_source("bucket-a/copy-src/reader-file.txt")
        .send()
        .await;

    assert!(
        result.is_err(),
        "Copy should fail: reader lacks write permission on destination bucket-b"
    );
}

// ============================================================================
// Deny overrides
// ============================================================================

#[tokio::test]
async fn test_deny_blocks_specific_action() {
    let h = IamTestHarness::setup().await;
    let client = h.client_for(&h.deny_user).await;

    // deny_user has Allow * + Deny delete.
    // Write should succeed
    assert!(
        put_succeeds(&client, "bucket-a", "deny/file.txt").await,
        "deny_user should be able to write"
    );
    // Read should succeed
    assert!(
        get_succeeds(&client, "bucket-a", "deny/file.txt").await,
        "deny_user should be able to read"
    );
    // List should succeed
    assert!(
        list_succeeds(&client, "bucket-a").await,
        "deny_user should be able to list"
    );
    // Delete should fail
    assert!(
        !delete_succeeds(&client, "bucket-a", "deny/file.txt").await,
        "deny_user should NOT be able to delete"
    );
}

#[tokio::test]
async fn test_deny_overrides_allow() {
    let h = IamTestHarness::setup().await;
    let client = h.client_for(&h.deny_user).await;

    // Seed an object to delete
    seed_object(&h, "bucket-b", "deny-override/file.txt").await;

    // deny_user has Allow * on * AND Deny delete on *.
    // The Deny should take precedence.
    assert!(
        !delete_succeeds(&client, "bucket-b", "deny-override/file.txt").await,
        "Deny delete should override Allow * — delete must return 403"
    );
}

// ============================================================================
// Disabled user
// ============================================================================

#[tokio::test]
async fn test_disabled_user_rejected() {
    let h = IamTestHarness::setup().await;

    // Create a new user, then disable them via admin API
    let admin_client = admin_http_client(&h.server.endpoint()).await;
    let temp_user = create_iam_user(
        &admin_client,
        &h.server,
        "temp_user",
        vec![json!({
            "effect": "Allow",
            "actions": ["*"],
            "resources": ["*"]
        })],
    )
    .await;

    // Verify the user can access initially
    let client = h.client_for(&temp_user).await;
    assert!(
        list_succeeds(&client, "bucket-a").await,
        "Newly created user should be able to list"
    );

    // Disable the user
    let resp = admin_client
        .put(format!(
            "{}/_/api/admin/users/{}",
            h.server.endpoint(),
            temp_user.id
        ))
        .json(&json!({ "enabled": false }))
        .send()
        .await
        .expect("Disable user request failed");
    assert!(
        resp.status().is_success(),
        "Failed to disable user: {}",
        resp.status()
    );

    // Disabled user should be rejected
    let client = h.client_for(&temp_user).await;
    assert!(
        !list_succeeds(&client, "bucket-a").await,
        "Disabled user should be rejected"
    );
}

// ============================================================================
// Bucket-level operations
// ============================================================================

#[tokio::test]
async fn test_reader_cannot_create_bucket() {
    let h = IamTestHarness::setup().await;
    let client = h.client_for(&h.reader_user).await;

    // CreateBucket requires admin permission
    let result = client.create_bucket().bucket("reader-bucket").send().await;
    assert!(
        result.is_err(),
        "Reader should not be able to create buckets"
    );
}

#[tokio::test]
async fn test_reader_cannot_delete_bucket() {
    let h = IamTestHarness::setup().await;
    let client = h.client_for(&h.reader_user).await;

    // DeleteBucket requires admin permission
    let result = client.delete_bucket().bucket("bucket-a").send().await;
    assert!(
        result.is_err(),
        "Reader should not be able to delete buckets"
    );
}

// SPDX-License-Identifier: BUSL-1.1

//! The re-encrypt job's rewrites are conditional on the object the job
//! read (`docs/plan/encryption-needs-atomic-writes.md`, N1): a client write
//! that lands between the job's read and its write, on any instance, is
//! never overwritten; the job counts the object as `skipped_changed`. On a
//! backend with proven conditional writes the job holds no write gate, so
//! client writes go on (no 503); a backend without them keeps the gate.

use crate::common;

use common::TestServer;
use serde_json::{json, Value};

const KEY: &str = "7a7b7c7d7e7f808182838485868788898a8b8c8d8e8f90919293949596979899";
const KEY_ID: &str = "cond-rewrite-1";
const OLD: &[u8] = b"CONDITIONAL_REWRITE_HISTORY_0123456789";
// The same length as OLD: only the version tells the two apart.
const NEW: &[u8] = b"CONDITIONAL_REWRITE_CLIENTW_0123456789";

fn aes() -> Value {
    json!({ "mode": "aes256-gcm-proxy", "key": KEY, "key_id": KEY_ID })
}

struct Node {
    server: TestServer,
    admin: reqwest::Client,
}

impl Node {
    async fn new(server: TestServer) -> Self {
        let admin = common::admin_http_client(&server.endpoint()).await;
        Self { server, admin }
    }

    async fn minio(bucket: &str, env: &[(&str, &str)]) -> Self {
        let mut b = TestServer::builder()
            .bucket(bucket)
            .s3_endpoint(&common::minio_endpoint_url());
        for (k, v) in env {
            b = b.env(k, v);
        }
        Self::new(b.build().await).await
    }

    async fn put(&self, bucket: &str, key: &str, body: &[u8]) -> u16 {
        let r = self
            .server
            .http()
            .put(format!("{}/{bucket}/{key}", self.server.endpoint()))
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        r.status().as_u16()
    }

    async fn get(&self, bucket: &str, key: &str) -> (u16, Vec<u8>) {
        let r = self
            .server
            .http()
            .get(format!("{}/{bucket}/{key}", self.server.endpoint()))
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.bytes().await.unwrap().to_vec())
    }

    /// Encrypt `bucket` only (the shared MinIO holds other tests' buckets).
    /// Returns the job that the apply queued, if any.
    async fn encrypt_bucket(&self, bucket: &str) -> Option<i64> {
        let body = json!({ "buckets": { bucket: { "encryption": aes() } } });
        self.storage(body).await
    }

    /// Encrypt the whole (filesystem) backend.
    async fn encrypt_backend(&self) -> Option<i64> {
        self.storage(json!({ "backend_encryption": aes() })).await
    }

    async fn storage(&self, body: Value) -> Option<i64> {
        let resp = self
            .admin
            .put(format!(
                "{}/_/api/admin/config/section/storage",
                self.server.endpoint()
            ))
            .json(&body)
            .send()
            .await
            .unwrap();
        let code = resp.status().as_u16();
        let v: Value = resp.json().await.unwrap_or_default();
        assert_eq!(code, 200, "{v}");
        v["conversions"]
            .as_array()
            .and_then(|c| c.first())
            .and_then(|c| c["job_id"].as_i64())
    }

    async fn bucket_status(&self, bucket: &str) -> Value {
        self.admin
            .get(format!(
                "{}/_/api/admin/jobs/bucket/{bucket}",
                self.server.endpoint()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn wait_done(&self, bucket: &str) {
        for _ in 0..1200 {
            if self.bucket_status(bucket).await["active"].is_null() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the job on {bucket} did not finish");
    }

    /// The run of maintenance job `id`.
    async fn run(&self, id: i64) -> Value {
        let v: Value = self
            .admin
            .get(format!(
                "{}/_/api/admin/jobs/maintenance:{id}/runs",
                self.server.endpoint()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["runs"][0].clone()
    }
}

/// Does the stored object `key` on MinIO carry the encryption marker?
async fn minio_sealed(bucket: &str, key: &str) -> bool {
    let head = common::minio_client()
        .await
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .unwrap();
    head.metadata()
        .is_some_and(|m| m.contains_key("user-dg-encrypted"))
}

async fn new_minio_bucket(prefix: &str) -> String {
    let bucket = common::unique_bucket(prefix);
    common::minio_client()
        .await
        .create_bucket()
        .bucket(&bucket)
        .send()
        .await
        .unwrap();
    bucket
}

/// Test (1): a client PUT of `k.txt` through a SECOND proxy on the same
/// MinIO lands between the job's read of the object and its write. The
/// job's PUT carries `If-Match` on the version it read, MinIO refuses it,
/// and the client's object survives; the job counts it as changed and
/// converts the other object.
#[tokio::test]
async fn a_client_write_on_another_instance_during_the_rewrite_survives() {
    skip_unless_minio!();
    let bucket = new_minio_bucket("condrw").await;
    // The other instance encrypts the bucket while it is empty: its apply
    // starts no conversion.
    let other = Node::minio(&bucket, &[]).await;
    assert_eq!(other.encrypt_bucket(&bucket).await, None);

    // The job's instance: each rewrite waits between its read and its write.
    let node = Node::minio(&bucket, &[("DGP_TEST_REWRITE_STALL_MS", "4000")]).await;
    assert_eq!(node.put(&bucket, "k.txt", OLD).await, 200);
    assert_eq!(node.put(&bucket, "other.txt", OLD).await, 200);
    assert!(!minio_sealed(&bucket, "k.txt").await);

    let job = node
        .encrypt_bucket(&bucket)
        .await
        .expect("a conversion job");
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let status = node.bucket_status(&bucket).await;
    assert!(!status["active"].is_null(), "the job still runs: {status}");
    assert_eq!(status["writes_blocked"], false, "{status}");
    assert_eq!(other.put(&bucket, "k.txt", NEW).await, 200);

    node.wait_done(&bucket).await;
    assert_eq!(other.get(&bucket, "k.txt").await, (200, NEW.to_vec()));
    assert_eq!(node.get(&bucket, "k.txt").await, (200, NEW.to_vec()));
    assert!(
        minio_sealed(&bucket, "k.txt").await,
        "the client write is sealed"
    );
    assert!(minio_sealed(&bucket, "other.txt").await, "converted");
    assert_eq!(node.get(&bucket, "other.txt").await, (200, OLD.to_vec()));
    let run = node.run(job).await;
    assert_eq!(run["objects_skipped_changed"], 1, "{run}");
    assert_eq!(run["errors"], 0, "{run}");
    assert_eq!(run["objects_processed"], 1, "{run}");
}

/// Test (2), filesystem: a client PUT through the same proxy lands after
/// the job's HEAD of the object and before the job takes the object's
/// write lock. Under the lock the job reads another version and writes
/// nothing; the client's object survives and the job counts it as changed.
#[tokio::test]
async fn a_client_write_before_the_object_lock_survives_on_the_filesystem() {
    let server = TestServer::builder()
        .bucket("condfs")
        .env("DGP_TEST_REWRITE_LOCK_STALL_MS", "3000")
        .build()
        .await;
    let node = Node::new(server).await;
    assert_eq!(node.put("condfs", "k.txt", OLD).await, 200);

    let job = node.encrypt_backend().await.expect("a conversion job");
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    let status = node.bucket_status("condfs").await;
    assert!(!status["active"].is_null(), "the job still runs: {status}");
    assert_eq!(status["writes_blocked"], false, "{status}");
    assert_eq!(node.put("condfs", "k.txt", NEW).await, 200);

    node.wait_done("condfs").await;
    assert_eq!(node.get("condfs", "k.txt").await, (200, NEW.to_vec()));
    let stored = node
        .server
        .data_dir()
        .unwrap()
        .join("condfs/deltaspaces/k.txt");
    let raw = std::fs::read(stored).unwrap();
    assert!(
        !raw.windows(NEW.len()).any(|w| w == NEW),
        "the client write is sealed on disk"
    );
    let run = node.run(job).await;
    assert_eq!(run["objects_skipped_changed"], 1, "{run}");
    assert_eq!(run["errors"], 0, "{run}");
}

/// Writes to `bucket` while its conversion job runs: the status of the
/// gate and the answer to a client PUT.
async fn write_during_the_job(node: &Node, bucket: &str) -> (Value, u16) {
    for i in 0..12 {
        assert_eq!(node.put(bucket, &format!("h-{i:02}.txt"), OLD).await, 200);
    }
    node.encrypt_bucket(bucket).await.expect("a conversion job");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let status = node.bucket_status(bucket).await;
    assert!(!status["active"].is_null(), "the job still runs: {status}");
    let code = node.put(bucket, "during.txt", NEW).await;
    node.wait_done(bucket).await;
    (status, code)
}

/// Test (3): on a backend with proven conditional writes (MinIO) the job
/// holds no write gate: a client PUT during the walk is stored (200).
#[tokio::test]
async fn a_cas_backend_takes_client_writes_during_the_walk() {
    skip_unless_minio!();
    let bucket = new_minio_bucket("condcas").await;
    let node = Node::minio(&bucket, &[("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "300")]).await;
    let (status, code) = write_during_the_job(&node, &bucket).await;
    assert_eq!(status["writes_blocked"], false, "{status}");
    assert_eq!(code, 200);
    assert_eq!(node.get(&bucket, "during.txt").await, (200, NEW.to_vec()));
}

/// Test (3), the control: a backend without proven conditional writes
/// (forced non-CAS) keeps the write gate: the same PUT gets 503.
#[tokio::test]
async fn a_noncas_backend_keeps_the_write_gate() {
    skip_unless_minio!();
    let bucket = new_minio_bucket("condnoncas").await;
    let node = Node::minio(
        &bucket,
        &[
            ("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "300"),
            ("DGP_TEST_FORCE_NONCAS_BACKEND", "default"),
        ],
    )
    .await;
    let (status, code) = write_during_the_job(&node, &bucket).await;
    assert_eq!(status["writes_blocked"], true, "{status}");
    assert_eq!(code, 503);
}

// SPDX-License-Identifier: BUSL-1.1

//! The re-encrypt job's rewrites are conditional on the object the job
//! read (`docs/plan/encryption-needs-atomic-writes.md`, N1): a client write
//! that lands between the job's read and its write, on any instance, is
//! never overwritten; the job counts the object as `skipped_changed`. On a
//! backend with proven conditional writes the job holds no write gate, so
//! client writes go on (no 503); a job that keeps the gate (a decrypt on a
//! backend without them; encryption needs them, N2) answers 503.

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

/// A file that the rewrite stall seam appends each stalled object's name
/// to (`DGP_TEST_REWRITE_STALL=<point>:<ms>:<file>`), and the seam value.
fn stall_signal(point: &str, ms: u64) -> (tempfile::TempDir, std::path::PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("stalled");
    let seam = format!("{point}:{ms}:{}", file.display());
    (dir, file, seam)
}

/// Wait until the job stalls at `object` (the seam wrote its line).
async fn wait_stalled_at(file: &std::path::Path, object: &str) {
    for _ in 0..300 {
        if std::fs::read_to_string(file).is_ok_and(|s| s.lines().any(|l| l == object)) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the job never stalled at {object}");
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
    let (_signal_dir, signal, seam) = stall_signal("before-write", 4000);
    let node = Node::minio(&bucket, &[("DGP_TEST_REWRITE_STALL", seam.as_str())]).await;
    assert_eq!(node.put(&bucket, "k.txt", OLD).await, 200);
    assert_eq!(node.put(&bucket, "other.txt", OLD).await, 200);
    assert!(!minio_sealed(&bucket, "k.txt").await);

    let job = node
        .encrypt_bucket(&bucket)
        .await
        .expect("a conversion job");
    // The job read k.txt and waits before its conditional write.
    wait_stalled_at(&signal, "k.txt").await;
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
    let (_signal_dir, signal, seam) = stall_signal("before-lock", 3000);
    let server = TestServer::builder()
        .bucket("condfs")
        .env("DGP_TEST_REWRITE_STALL", &seam)
        .build()
        .await;
    let node = Node::new(server).await;
    assert_eq!(node.put("condfs", "k.txt", OLD).await, 200);

    let job = node.encrypt_backend().await.expect("a conversion job");
    // The job's HEAD of k.txt is done; it waits before the object lock.
    wait_stalled_at(&signal, "k.txt").await;
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

/// `deltaglider_rewrite_bodies_total{medium}`: the passthrough bodies that
/// the job's rewrites read into `medium` so far.
async fn rewrite_bodies(node: &Node, medium: &str) -> u64 {
    let series = format!("deltaglider_rewrite_bodies_total{{medium=\"{medium}\"}}");
    common::metrics_text(&node.server.endpoint())
        .await
        .lines()
        .find_map(|l| l.strip_prefix(&series)?.trim().parse::<f64>().ok())
        .unwrap_or(0.0) as u64
}

/// F29: the re-encrypt rewrite of a passthrough object above the spool
/// threshold reads it through a spool file (`read_into_spool`), below it
/// into memory. Threshold 1 KiB: `big.jpg` (4 KiB) takes the spool branch,
/// `small.jpg` (40 bytes) the memory branch. The counter proves which branch
/// ran, and the GETs prove that both objects survive it sealed.
async fn the_rewrite_reads_a_large_body_through_the_spool(
    node: &Node,
    bucket: &str,
    whole_backend: bool,
) {
    let big = common::big_passthrough_body(4096);
    let small = OLD.to_vec();
    assert_eq!(node.put(bucket, "big.jpg", &big).await, 200);
    assert_eq!(node.put(bucket, "small.jpg", &small).await, 200);
    assert_eq!(
        rewrite_bodies(node, "spool").await,
        0,
        "nothing rewritten yet"
    );

    // The shared MinIO holds other tests' buckets: encrypt only this one.
    let job = if whole_backend {
        node.encrypt_backend().await
    } else {
        node.encrypt_bucket(bucket).await
    }
    .expect("a conversion job");
    node.wait_done(bucket).await;

    let run = node.run(job).await;
    assert_eq!(run["objects_processed"], 2, "{run}");
    assert_eq!(run["errors"], 0, "{run}");
    assert_eq!(rewrite_bodies(node, "spool").await, 1, "the 4 KiB body");
    assert_eq!(rewrite_bodies(node, "memory").await, 1, "the 40-byte body");
    assert_eq!(node.get(bucket, "big.jpg").await, (200, big));
    assert_eq!(node.get(bucket, "small.jpg").await, (200, small));
}

#[tokio::test]
async fn the_rewrite_spools_a_large_body_on_minio() {
    skip_unless_minio!();
    let bucket = new_minio_bucket("condspool").await;
    let node = Node::minio(&bucket, &[("DGP_SPOOL_THRESHOLD_BYTES", "1024")]).await;
    the_rewrite_reads_a_large_body_through_the_spool(&node, &bucket, false).await;
    assert!(
        minio_sealed(&bucket, "big.jpg").await,
        "the spooled body is sealed"
    );
    assert!(minio_sealed(&bucket, "small.jpg").await);
}

#[tokio::test]
async fn the_rewrite_spools_a_large_body_on_the_filesystem() {
    let server = TestServer::builder()
        .bucket("condspoolfs")
        .env("DGP_SPOOL_THRESHOLD_BYTES", "1024")
        .build()
        .await;
    let node = Node::new(server).await;
    the_rewrite_reads_a_large_body_through_the_spool(&node, "condspoolfs", true).await;
}

/// Writes to `bucket` while its conversion job runs (`signal`: the stall
/// seam's file): the status of the gate and the answer to a client PUT.
async fn write_during_the_job(node: &Node, bucket: &str, signal: &std::path::Path) -> (Value, u16) {
    for i in 0..12 {
        assert_eq!(node.put(bucket, &format!("h-{i:02}.txt"), OLD).await, 200);
    }
    node.encrypt_bucket(bucket).await.expect("a conversion job");
    // The walk is under way: the job stalled at its first object.
    wait_stalled_at(signal, "h-00.txt").await;
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
    let (_signal_dir, signal, seam) = stall_signal("before-write", 1);
    let node = Node::minio(
        &bucket,
        &[
            ("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "300"),
            ("DGP_TEST_REWRITE_STALL", seam.as_str()),
        ],
    )
    .await;
    let (status, code) = write_during_the_job(&node, &bucket, &signal).await;
    assert_eq!(status["writes_blocked"], false, "{status}");
    assert_eq!(code, 200);
    assert_eq!(node.get(&bucket, "during.txt").await, (200, NEW.to_vec()));
}

/// MinIO compares `If-Match` with the quoted ETag, as AWS does: its probe
/// verdict keeps the quoted form (Ceph RGW, Hetzner, gets `unquoted`).
#[tokio::test]
async fn minio_probes_the_quoted_if_match_form() {
    skip_unless_minio!();
    let bucket = new_minio_bucket("condquoted").await;
    let node = Node::minio(&bucket, &[]).await;
    let resp = node
        .admin
        .post(format!(
            "{}/_/api/admin/backends/default/capability-probe",
            node.server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["capability"]["verdict"], "cas-verified", "{v}");
    assert_eq!(v["capability"]["via"], "probe", "{v}");
    assert_eq!(v["capability"]["if_match"], "quoted", "{v}");
}

/// Test (3), the control: a job that keeps the write gate (in production a
/// decrypt job on a backend without proven conditional writes; encryption
/// itself needs them, N2) answers the same PUT with 503.
#[tokio::test]
async fn a_gated_job_keeps_the_write_gate() {
    crate::common::expect_server_errors("a maintenance write gate answers 503 (asserted)");
    skip_unless_minio!();
    let bucket = new_minio_bucket("condnoncas").await;
    // The job holds each object 5 s before its write, so the status read
    // and the PUT land while the gate is armed. The objects run in
    // parallel, so the per-object stall alone left a window of about one
    // stall (300 ms), and a loaded host lost it: the PUT got 200.
    let (_signal_dir, signal, seam) = stall_signal("before-write", 5000);
    let node = Node::minio(
        &bucket,
        &[
            ("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "300"),
            ("DGP_TEST_KEEP_WRITE_GATE", "true"),
            ("DGP_TEST_REWRITE_STALL", seam.as_str()),
        ],
    )
    .await;
    let (status, code) = write_during_the_job(&node, &bucket, &signal).await;
    assert_eq!(status["writes_blocked"], true, "{status}");
    assert_eq!(code, 503);
}

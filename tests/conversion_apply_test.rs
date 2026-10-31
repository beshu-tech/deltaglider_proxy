// SPDX-License-Identifier: BUSL-1.1

//! An apply that makes buckets with objects encrypt starts their
//! conversion (`docs/plan/encryption-conversion-marker.md`, section 4,
//! P5), through the real admin API: the window opens before the config
//! commits, the job is queued after it, and the response names both. An
//! empty bucket starts nothing; an active job refuses the apply and
//! leaves no marker; a failed open removes the markers the apply wrote;
//! a marker that is there is reused; `plaintext_reads: serve` is still
//! served, with a deprecation warning.

use crate::common;

use common::TestServer;
use deltaglider_proxy::storage::encrypting::conversion::MarkerState;
use deltaglider_proxy::storage::{EncryptingBackend, EncryptionConfig, EncryptionKey, WitnessGate};
use serde_json::{json, Value};
use std::sync::Arc;

const KEY: &str = "4a4b4c4d4e4f505152535455565758595a5b5c5d5e5f60616263646566676869";
const KEY_ID: &str = "apply-conv-1";
const BODY: &[u8] = b"APPLY_CONVERSION_HISTORY_0123456789";

struct Env {
    server: TestServer,
    admin: reqwest::Client,
}

fn aes() -> Value {
    json!({ "mode": "aes256-gcm-proxy", "key": KEY, "key_id": KEY_ID })
}

impl Env {
    async fn new(bucket: &str, env: &[(&str, &str)]) -> Self {
        let mut b = TestServer::builder().bucket(bucket);
        for (k, v) in env {
            b = b.env(k, v);
        }
        let server = b.build().await;
        let admin = common::admin_http_client(&server.endpoint()).await;
        Self { server, admin }
    }

    async fn minio(bucket: &str) -> Self {
        let server = TestServer::builder()
            .bucket(bucket)
            .s3_endpoint(&common::minio_endpoint_url())
            .build()
            .await;
        let admin = common::admin_http_client(&server.endpoint()).await;
        Self { server, admin }
    }

    async fn create(&self, bucket: &str) {
        self.server
            .s3_client()
            .await
            .create_bucket()
            .bucket(bucket)
            .send()
            .await
            .unwrap();
    }

    async fn put(&self, bucket: &str, key: &str) {
        let r = self
            .server
            .http()
            .put(format!("{}/{bucket}/{key}", self.server.endpoint()))
            .body(BODY.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200, "PUT {bucket}/{key}");
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

    /// The storage section PUT (`validate`: its dry run).
    async fn storage(&self, body: Value, validate: bool) -> (u16, Value) {
        let url = format!(
            "{}/_/api/admin/config/section/storage{}",
            self.server.endpoint(),
            if validate { "/validate" } else { "" }
        );
        let req = if validate {
            self.admin.post(url)
        } else {
            self.admin.put(url)
        };
        let resp = req.json(&body).send().await.unwrap();
        let code = resp.status().as_u16();
        (code, resp.json().await.unwrap_or_default())
    }

    async fn encrypt(&self, validate: bool) -> (u16, Value) {
        self.storage(json!({ "backend_encryption": aes() }), validate)
            .await
    }

    fn marker_file(&self, bucket: &str) -> std::path::PathBuf {
        self.server
            .data_dir()
            .expect("filesystem")
            .join(bucket)
            .join(".dg-conversion")
    }

    /// An encrypting wrapper on the proxy's storage, keyed like the
    /// config the tests apply.
    async fn node(&self) -> EncryptingBackend<deltaglider_proxy::storage::FilesystemBackend> {
        let mut enc = EncryptionConfig::default();
        enc.key = Some(EncryptionKey::from_hex(KEY).unwrap());
        enc.key_id = Some(KEY_ID.into());
        EncryptingBackend::new(
            deltaglider_proxy::storage::FilesystemBackend::new(
                self.server.data_dir().unwrap().to_path_buf(),
            )
            .await
            .unwrap(),
            Arc::new(arc_swap::ArcSwap::new(Arc::new(enc))),
        )
        .with_witness("default", Arc::new(WitnessGate::default()))
    }

    async fn buckets(&self) -> Value {
        self.admin
            .get(format!("{}/_/api/admin/buckets", self.server.endpoint()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn conversion_of(&self, bucket: &str) -> Value {
        let v = self.buckets().await;
        v["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["name"] == bucket)
            .map(|b| b["effective_encryption"]["conversion"].clone())
            .unwrap_or_else(|| panic!("no bucket {bucket}: {v}"))
    }

    async fn wait_done(&self, bucket: &str) {
        for _ in 0..1200 {
            let v: Value = self
                .admin
                .get(format!(
                    "{}/_/api/admin/jobs/bucket/{bucket}",
                    self.server.endpoint()
                ))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if v["active"].is_null() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the job on {bucket} did not finish");
    }

    async fn job(&self, id: i64) -> Value {
        let v: Value = self
            .admin
            .get(format!("{}/_/api/admin/jobs", self.server.endpoint()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"] == format!("maintenance:{id}"))
            .cloned()
            .unwrap_or_else(|| panic!("job {id}: {v}"))
    }

    /// Is the stored blob of `key` plaintext?
    fn plaintext(&self, bucket: &str, key: &str) -> bool {
        let path = self
            .server
            .data_dir()
            .unwrap()
            .join(bucket)
            .join("deltaspaces")
            .join(key);
        std::fs::read(path)
            .unwrap()
            .windows(BODY.len())
            .any(|w| w == BODY)
    }
}

const STALL: (&str, &str) = ("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "300");

/// A bucket with objects: the validate counts them; the apply opens the
/// window, queues the job and names both; clients read the history
/// while the job runs; the bucket list shows the conversion with the
/// job; the job converts everything and closes the window. An empty
/// bucket next to it starts nothing.
#[tokio::test]
async fn an_apply_opens_the_window_and_queues_the_job_of_a_bucket_with_objects() {
    const N: usize = 24;
    let env = Env::new("applyconv", &[STALL]).await;
    env.create("applyempty").await;
    for i in 0..N {
        env.put("applyconv", &format!("h-{i:02}.txt")).await;
    }

    let (code, v) = env.encrypt(true).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(
        v["conversion_preview"],
        json!([{ "bucket": "applyconv", "objects": N, "more": false, "versioned": false }]),
        "{v}"
    );
    assert!(
        !env.marker_file("applyconv").exists(),
        "a validate opens nothing"
    );

    let (code, v) = env.encrypt(false).await;
    assert_eq!(code, 200, "{v}");
    let conv = v["conversions"].as_array().expect("conversions");
    assert_eq!(conv.len(), 1, "the empty bucket starts nothing: {v}");
    assert_eq!(conv[0]["bucket"], "applyconv");
    assert_eq!(conv[0]["reused"], false);
    let job_id = conv[0]["job_id"].as_i64().expect("a job id");
    let run = conv[0]["run"].as_str().unwrap().to_string();
    assert_eq!(run.len(), 32);
    assert!(!env.marker_file("applyempty").exists());

    // While the job runs: the history reads, and the bucket list shows the
    // open window with this node's job.
    let view = env.conversion_of("applyconv").await;
    assert_eq!(view["open"], true, "{view}");
    assert_eq!(view["run"], run.as_str(), "{view}");
    assert_eq!(view["job_on_this_node"]["id"], job_id, "{view}");
    assert_eq!(view["serves_plaintext_deprecated"], false, "{view}");
    assert!(
        view["deadline"].as_i64() > view["cutoff"].as_i64(),
        "{view}"
    );
    assert_eq!(env.get("applyconv", "h-00.txt").await, (200, BODY.to_vec()));
    assert!(env.conversion_of("applyempty").await.is_null());

    env.wait_done("applyconv").await;
    assert_eq!(env.job(job_id).await["status"], "succeeded");
    for i in 0..N {
        let key = format!("h-{i:02}.txt");
        assert!(!env.plaintext("applyconv", &key), "{key} is converted");
        assert_eq!(env.get("applyconv", &key).await, (200, BODY.to_vec()));
    }
    assert_eq!(
        env.node()
            .await
            .conversion_status("applyconv")
            .await
            .unwrap(),
        MarkerState::Absent,
        "the job closed the window"
    );
    assert!(env.conversion_of("applyconv").await.is_null());
}

/// A job that holds the bucket (here a metadata backfill) refuses the
/// apply with 409 that names it, and no marker is left; the validate
/// refuses the same way. After the job, the apply goes through.
#[tokio::test]
async fn an_active_job_refuses_the_apply_and_leaves_no_marker() {
    let env = Env::new(
        "applybusy",
        &[("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "400")],
    )
    .await;
    for i in 0..12 {
        env.put("applybusy", &format!("b-{i:02}.txt")).await;
    }
    let r: Value = env
        .admin
        .post(format!(
            "{}/_/api/admin/jobs/backfill-metadata",
            env.server.endpoint()
        ))
        .json(&json!({ "buckets": ["applybusy"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let backfill = r["started"][0]["job_id"].as_i64().expect("a backfill job");

    let (code, v) = env.encrypt(true).await;
    assert_eq!(code, 409, "the validate refuses too: {v}");
    let (code, v) = env.encrypt(false).await;
    assert_eq!(code, 409, "{v}");
    let error = v["error"].as_str().unwrap();
    assert!(
        error.contains("backfill-metadata") && error.contains(&format!("maintenance:{backfill}")),
        "{error}"
    );
    assert!(!env.marker_file("applybusy").exists(), "no marker is left");
    let section: Value = env
        .admin
        .get(format!(
            "{}/_/api/admin/config/section/storage",
            env.server.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        section["backend_encryption"].is_null() || section["backend_encryption"]["mode"] == "none",
        "the config did not change: {section}"
    );

    env.wait_done("applybusy").await;
    let (code, v) = env.encrypt(false).await;
    assert_eq!(code, 200, "{v}");
    assert!(v["conversions"][0]["job_id"].is_i64(), "{v}");
    env.wait_done("applybusy").await;
}

/// Two buckets start to encrypt; the second holds a marker that this
/// proxy cannot open, so its open fails: the apply fails, the first
/// bucket's marker (written by this apply) is removed, and nothing else
/// changed.
#[tokio::test]
async fn a_failed_open_removes_the_markers_the_apply_wrote() {
    let env = Env::new("applyfirst", &[]).await;
    env.create("applysecond").await;
    env.put("applyfirst", "a.txt").await;
    env.put("applysecond", "b.txt").await;
    std::fs::write(
        env.marker_file("applysecond"),
        b"not a marker of this proxy",
    )
    .unwrap();

    let (code, v) = env.encrypt(false).await;
    assert_eq!(code, 422, "{v}");
    let error = v["error"].as_str().unwrap();
    assert!(
        error.contains("bucket 'applysecond'") && error.contains("cannot open"),
        "{error}"
    );
    assert!(
        !env.marker_file("applyfirst").exists(),
        "the marker this apply wrote is removed"
    );
    assert_eq!(
        std::fs::read(env.marker_file("applysecond")).unwrap(),
        b"not a marker of this proxy",
        "the foreign marker is never written"
    );
    // Nothing changed: the bucket still writes plaintext.
    env.put("applyfirst", "c.txt").await;
    assert!(env.plaintext("applyfirst", "c.txt"));
    assert_eq!(env.get("applyfirst", "a.txt").await, (200, BODY.to_vec()));
}

/// A valid marker that another node opened is reused: same run, no new
/// cutoff, and the response says so.
#[tokio::test]
async fn an_existing_marker_is_reused() {
    let env = Env::new("applyreuse", &[]).await;
    env.put("applyreuse", "a.txt").await;
    let node = env.node().await;
    let run = node.open_conversion("applyreuse", 7).await.unwrap();

    let (code, v) = env.encrypt(false).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["conversions"][0]["reused"], true, "{v}");
    assert_eq!(v["conversions"][0]["run"], hex::encode(run), "{v}");
    assert!(v["conversions"][0]["job_id"].is_i64(), "{v}");
    env.wait_done("applyreuse").await;
    assert!(!env.marker_file("applyreuse").exists(), "the job closed it");
}

/// `plaintext_reads: serve` is still honoured (no marker, no job, every
/// unencrypted object served) with a deprecation warning; removing the
/// line ("Convert now") starts the conversion.
#[tokio::test]
async fn serve_is_still_served_with_a_deprecation_warning() {
    let env = Env::new("applyserve", &[]).await;
    env.put("applyserve", "a.txt").await;
    let mut serve = aes();
    serve["plaintext_reads"] = json!("serve");
    let (code, v) = env
        .storage(json!({ "backend_encryption": serve }), false)
        .await;
    assert_eq!(code, 200, "{v}");
    assert!(v.get("conversions").is_none(), "{v}");
    assert!(
        v["warnings"].as_array().unwrap().iter().any(|w| w
            .as_str()
            .unwrap()
            .contains("plaintext_reads: serve` is deprecated")),
        "{v}"
    );
    assert!(!env.marker_file("applyserve").exists());
    assert_eq!(env.get("applyserve", "a.txt").await, (200, BODY.to_vec()));
    let view = env.conversion_of("applyserve").await;
    assert_eq!(view["serves_plaintext_deprecated"], true, "{view}");
    assert_eq!(view["open"], false, "{view}");

    // Convert now: the line goes, the conversion starts.
    let (code, v) = env
        .storage(
            json!({ "backend_encryption": { "plaintext_reads": null } }),
            false,
        )
        .await;
    assert_eq!(code, 200, "{v}");
    assert!(v["conversions"][0]["job_id"].is_i64(), "{v}");
    env.wait_done("applyserve").await;
    assert!(!env.plaintext("applyserve", "a.txt"));
    assert_eq!(env.get("applyserve", "a.txt").await, (200, BODY.to_vec()));
}

/// A versioned backend bucket (MinIO): the validate and the apply warn
/// that the plaintext versions stay as noncurrent versions.
#[tokio::test]
async fn a_versioned_bucket_warns_that_plaintext_versions_stay() {
    skip_unless_minio!();
    let bucket = common::unique_bucket("applyver");
    let minio = common::minio_client().await;
    minio.create_bucket().bucket(&bucket).send().await.unwrap();
    minio
        .put_bucket_versioning()
        .bucket(&bucket)
        .versioning_configuration(
            aws_sdk_s3::types::VersioningConfiguration::builder()
                .status(aws_sdk_s3::types::BucketVersioningStatus::Enabled)
                .build(),
        )
        .send()
        .await
        .unwrap();
    let env = Env::minio(&bucket).await;
    env.put(&bucket, "a.txt").await;

    let (code, v) = env.encrypt(true).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["conversion_preview"][0]["versioned"], true, "{v}");
    let (code, v) = env.encrypt(false).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["conversions"][0]["versioned"], true, "{v}");
    assert!(
        v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("keeps old versions")),
        "{v}"
    );
    env.wait_done(&bucket).await;
    assert_eq!(env.get(&bucket, "a.txt").await, (200, BODY.to_vec()));
}

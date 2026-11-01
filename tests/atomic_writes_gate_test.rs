// SPDX-License-Identifier: BUSL-1.1

//! Encryption needs atomic writes (`docs/plan/encryption-needs-atomic-writes.md`,
//! N2). Filesystem backend; `DGP_TEST_FORCE_NONCAS_BACKEND=default` makes the
//! fresh atomicity probe of the backend answer non-CAS, and
//! `DGP_TEST_FORCE_NONCAS_WHILE_FILE` lets a test turn the verdict to CAS
//! without a restart ("Check again").
//!
//! - Every action that turns proxy encryption on (config validate/apply,
//!   section validate/PUT, document validate/apply, bucket create, re-encrypt
//!   start and Resume, the boot conversion) is refused with 409 and the
//!   structured `atomic_writes` part.
//! - A bucket that already encrypts on such a backend is READ-ONLY: reads
//!   work; every write surface (S3, form POST, admin bulk, jobs,
//!   replication) is refused.
//! - A fresh probe that proves atomic writes lifts the read-only state.

use crate::common;

use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, TestServer};
use serde_json::{json, Value};

const KEY: &str = "a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0";
const KEY_ID: &str = "atomic-gate-1";
const FORCED: (&str, &str) = ("DGP_TEST_FORCE_NONCAS_BACKEND", "default");
const DOC: &str = "https://deltaglider.com/docs/how-to/backend-capability-validation";

fn aes() -> Value {
    json!({ "mode": "aes256-gcm-proxy", "key": KEY, "key_id": KEY_ID })
}

/// A 409 for lack of atomic writes, with the structured part.
fn assert_refused(code: u16, v: &Value, what: &str) {
    assert_eq!(code, 409, "{what}: {v}");
    let a = &v["atomic_writes"];
    assert_eq!(a["backend"], "default", "{what}: {v}");
    assert_eq!(a["probe"], "non-cas", "{what}: {v}");
    assert_eq!(a["doc_url"], DOC, "{what}: {v}");
    assert!(
        v.to_string().contains("atomic writes"),
        "{what}: the message says why: {v}"
    );
}

async fn send(req: reqwest::RequestBuilder) -> (u16, Value) {
    let r = req.send().await.unwrap();
    let code = r.status().as_u16();
    let text = r.text().await.unwrap();
    (
        code,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

struct Env {
    server: TestServer,
    admin: reqwest::Client,
}

impl Env {
    fn ep(&self) -> String {
        self.server.endpoint()
    }

    async fn new(bucket: &str, env: &[(&str, &str)]) -> Self {
        let mut b = TestServer::builder().bucket(bucket);
        for (k, v) in env {
            b = b.env(k, v);
        }
        let server = b.build().await;
        let admin = admin_http_client(&server.endpoint()).await;
        Self { server, admin }
    }

    async fn storage(&self, body: Value, validate: bool) -> (u16, Value) {
        let url = format!(
            "{}/_/api/admin/config/section/storage{}",
            self.ep(),
            if validate { "/validate" } else { "" }
        );
        let req = if validate {
            self.admin.post(url)
        } else {
            self.admin.put(url)
        };
        send(req.json(&body)).await
    }

    async fn put(&self, bucket: &str, key: &str) -> u16 {
        self.server
            .http()
            .put(format!("{}/{bucket}/{key}", self.ep()))
            .body(b"atomic-writes-body".to_vec())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn bucket_view(&self, bucket: &str) -> Value {
        let (_, v) = send(self.admin.get(format!("{}/_/api/admin/buckets", self.ep()))).await;
        v["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["name"] == bucket)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Wait until the atomicity watch made `bucket` read-only.
    async fn wait_read_only(&self, bucket: &str) -> Value {
        for _ in 0..100 {
            let v = self.bucket_view(bucket).await;
            if v["read_only"].is_string() {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("bucket '{bucket}' never became read-only");
    }

    async fn reencrypt(&self, bucket: &str, open_window: bool) -> (u16, Value) {
        send(
            self.admin
                .post(format!("{}/_/api/admin/jobs/reencrypt", self.ep()))
                .json(&json!({ "buckets": [bucket], "open_window": open_window })),
        )
        .await
    }

    async fn active_job(&self, bucket: &str) -> Value {
        let (_, v) = send(
            self.admin
                .get(format!("{}/_/api/admin/jobs/bucket/{bucket}", self.ep())),
        )
        .await;
        v["active"].clone()
    }
}

/// An encrypted bucket that is read-only: the bucket encrypts with a proxy
/// key (applied while the backend's writes were atomic) and holds `a.txt`;
/// the proxy then restarts where the backend's verdict is non-CAS while
/// `flag` exists. `buckets` are encrypted on their own (the others stay
/// plaintext and writable).
async fn read_only_env(name: &str, buckets: &[&str], flag: &std::path::Path) -> Env {
    let mut env = Env::new(name, &[]).await;
    let client = env.server.s3_client().await;
    let mut policies = serde_json::Map::new();
    for b in buckets {
        if *b != name {
            client.create_bucket().bucket(*b).send().await.unwrap();
        }
        policies.insert(b.to_string(), json!({ "encryption": aes() }));
    }
    let (code, v) = env.storage(json!({ "buckets": policies }), false).await;
    assert_eq!(code, 200, "encrypting on a filesystem backend: {v}");
    for b in buckets {
        assert_eq!(env.put(b, "a.txt").await, 200);
    }
    std::fs::write(flag, b"non-cas").unwrap();
    let flag = flag.to_str().unwrap().to_string();
    env.server
        .respawn_with_env(&[FORCED, ("DGP_TEST_FORCE_NONCAS_WHILE_FILE", &flag)])
        .await;
    env.admin = admin_http_client(&env.ep()).await;
    for b in buckets {
        env.wait_read_only(b).await;
    }
    env
}

fn assert_access_denied<E, R>(err: &aws_sdk_s3::error::SdkError<E, R>, what: &str)
where
    E: ProvideErrorMetadata + std::fmt::Debug,
    R: std::fmt::Debug,
{
    assert_eq!(err.meta().code(), Some("AccessDenied"), "{what}: {err:?}");
    let m = err.meta().message().unwrap_or("");
    assert!(
        m.contains("read-only") && m.contains("does not support atomic writes") && m.contains(DOC),
        "{what}: the message says why, in a fixed phrase: {m}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// A: refusal at every entry point
// ═══════════════════════════════════════════════════════════════════════

/// The config surfaces that turn proxy encryption on: section validate and
/// PUT, document validate and apply (the CLI `config apply` sends the
/// document apply). Each answers 409 with `atomic_writes`, and nothing
/// changes. A change without encryption still applies.
#[tokio::test]
async fn config_writes_that_encrypt_are_refused() {
    let env = Env::new("atomcfg", &[FORCED]).await;
    let body = json!({ "backend_encryption": aes() });
    let (code, v) = env.storage(body.clone(), true).await;
    assert_refused(code, &v, "section validate");
    let (code, v) = env.storage(body, false).await;
    assert_refused(code, &v, "section PUT");
    let bucket_body = json!({ "buckets": { "atomcfg": { "encryption": aes() } } });
    let (code, v) = env.storage(bucket_body, false).await;
    assert_refused(code, &v, "section PUT of a bucket block");

    let file = std::fs::read_to_string(env.server.config_path()).unwrap();
    let doc = format!(
        "{file}\nbackend_encryption:\n  mode: aes256-gcm-proxy\n  key: \"{KEY}\"\n  key_id: {KEY_ID}\n"
    );
    for path in ["validate", "apply"] {
        let (code, v) = send(
            env.admin
                .post(format!("{}/_/api/admin/config/{path}", env.ep()))
                .json(&json!({ "yaml": doc })),
        )
        .await;
        assert_refused(code, &v, &format!("document {path}"));
    }

    let (_, export) = send(
        env.admin
            .get(format!("{}/_/api/admin/config/export", env.ep())),
    )
    .await;
    assert!(
        !export.to_string().contains("aes256-gcm-proxy"),
        "nothing was applied: {export}"
    );
    // Control: a change that encrypts nothing applies.
    let (code, v) = env
        .storage(
            json!({ "buckets": { "atomcfg": { "quota_bytes": 1000000 } } }),
            false,
        )
        .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(
        env.put("atomcfg", "w.txt").await,
        200,
        "writable: no encryption"
    );
}

/// `POST /buckets` with an encryption block on a backend without atomic
/// writes: 409 `atomic_writes_required`, and no bucket is created.
#[tokio::test]
async fn bucket_create_with_encryption_is_refused() {
    let env = Env::new("atomcreate", &[FORCED]).await;
    let (code, v) = send(
        env.admin
            .post(format!("{}/_/api/admin/buckets", env.ep()))
            .json(&json!({ "name": "atomnew", "backend_name": "default", "encryption": aes() })),
    )
    .await;
    assert_eq!(code, 409, "{v}");
    assert_eq!(v["error"], "atomic_writes_required", "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("backend 'default'"),
        "{v}"
    );
    assert!(env.bucket_view("atomnew").await.is_null(), "not created");
    // Control: without encryption the bucket is created.
    let (code, v) = send(
        env.admin
            .post(format!("{}/_/api/admin/buckets", env.ep()))
            .json(&json!({ "name": "atomplain", "backend_name": "default" })),
    )
    .await;
    assert_eq!(code, 200, "{v}");
}

/// The boot conversion of a bucket that a restart made encrypt (edited
/// file) runs a fresh probe first: without atomic writes it opens no
/// window and queues no job, and says so in the audit log.
#[tokio::test]
async fn the_boot_conversion_is_refused_without_atomic_writes() {
    let mut env = Env::new("atomboot", &[]).await;
    for i in 0..3 {
        assert_eq!(env.put("atomboot", &format!("p-{i}.txt")).await, 200);
    }
    let file = std::fs::read_to_string(env.server.config_path()).unwrap();
    std::fs::write(
        env.server.config_path(),
        format!(
            "{file}\nbackend_encryption:\n  mode: aes256-gcm-proxy\n  key: \"{KEY}\"\n  key_id: {KEY_ID}\n"
        ),
    )
    .unwrap();
    env.server
        .respawn_with_env(&[FORCED, ("DGP_BOOT_PLAINTEXT_CHECK", "true")])
        .await;
    env.admin = admin_http_client(&env.ep()).await;
    let mut refused = Value::Null;
    for _ in 0..300 {
        let (_, v) = send(
            env.admin
                .get(format!("{}/_/api/admin/audit?limit=200", env.ep())),
        )
        .await;
        if let Some(e) = v["entries"].as_array().and_then(|a| {
            a.iter()
                .find(|e| e["action"] == "encryption_conversion_refused")
        }) {
            refused = e.clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(refused["bucket"], "atomboot", "the boot refused: {refused}");
    assert!(env.active_job("atomboot").await.is_null(), "no job queued");
    env.wait_read_only("atomboot").await;
}

// ═══════════════════════════════════════════════════════════════════════
// B: read-only encrypted buckets
// ═══════════════════════════════════════════════════════════════════════

/// S3 surface family: PUT, DELETE, DeleteObjects, CreateMultipartUpload and
/// CopyObject into the bucket get 403 AccessDenied naming the bucket, the
/// backend and why; GET and LIST work; a plaintext bucket stays writable.
#[tokio::test]
async fn a_read_only_bucket_refuses_s3_writes_and_serves_reads() {
    let dir = tempfile::tempdir().unwrap();
    let env = read_only_env("atoms3", &["atoms3"], &dir.path().join("flag")).await;
    let c = env.server.s3_client().await;
    c.create_bucket()
        .bucket("atoms3plain")
        .send()
        .await
        .unwrap();
    c.put_object()
        .bucket("atoms3plain")
        .key("src.txt")
        .body(ByteStream::from_static(b"src"))
        .send()
        .await
        .expect("a plaintext bucket stays writable");

    let got = c
        .get_object()
        .bucket("atoms3")
        .key("a.txt")
        .send()
        .await
        .unwrap();
    assert_eq!(
        &got.body.collect().await.unwrap().into_bytes()[..],
        b"atomic-writes-body"
    );
    let listed = c.list_objects_v2().bucket("atoms3").send().await.unwrap();
    assert_eq!(listed.key_count(), Some(1));

    let err = c
        .put_object()
        .bucket("atoms3")
        .key("new.txt")
        .body(ByteStream::from_static(b"x"))
        .send()
        .await
        .expect_err("PUT refused");
    assert_access_denied(&err, "PutObject");
    let err = c
        .delete_object()
        .bucket("atoms3")
        .key("a.txt")
        .send()
        .await
        .expect_err("DELETE refused");
    assert_access_denied(&err, "DeleteObject");
    let err = c
        .delete_objects()
        .bucket("atoms3")
        .delete(
            aws_sdk_s3::types::Delete::builder()
                .objects(
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key("a.txt")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect_err("DeleteObjects refused");
    assert_access_denied(&err, "DeleteObjects");
    let err = c
        .create_multipart_upload()
        .bucket("atoms3")
        .key("big.bin")
        .send()
        .await
        .expect_err("CreateMultipartUpload refused");
    assert_access_denied(&err, "CreateMultipartUpload");
    let err = c
        .copy_object()
        .bucket("atoms3")
        .key("copied.txt")
        .copy_source("atoms3plain/src.txt")
        .send()
        .await
        .expect_err("CopyObject into refused");
    assert_access_denied(&err, "CopyObject");
    // A copy OUT of the read-only bucket is a read.
    c.copy_object()
        .bucket("atoms3plain")
        .key("out.txt")
        .copy_source("atoms3/a.txt")
        .send()
        .await
        .expect("copy out of a read-only bucket");
    // Still there.
    assert!(c
        .head_object()
        .bucket("atoms3")
        .key("a.txt")
        .send()
        .await
        .is_ok());
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// A browser form POST (PostObject) signed with the test credentials.
fn signed_form_post(endpoint: &str, bucket: &str) -> reqwest::RequestBuilder {
    use base64::Engine;
    let (date, amz_date) = ("20260507", "20260507T120000Z");
    let credential = format!(
        "{}/{date}/us-east-1/s3/aws4_request",
        common::TEST_ACCESS_KEY
    );
    let policy = json!({
        "expiration": "2099-01-01T00:00:00.000Z",
        "conditions": [
            { "bucket": bucket },
            ["starts-with", "$key", "post/"],
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date },
            ["content-length-range", 1, 1048576]
        ]
    });
    let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
    let k = hmac(
        format!("AWS4{}", common::TEST_SECRET_KEY).as_bytes(),
        date.as_bytes(),
    );
    let k = hmac(&hmac(&hmac(&k, b"us-east-1"), b"s3"), b"aws4_request");
    let signature = hex::encode(hmac(&k, policy_b64.as_bytes()));
    let form = reqwest::multipart::Form::new()
        .text("key", "post/x.txt")
        .text("policy", policy_b64)
        .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
        .text("x-amz-credential", credential)
        .text("x-amz-date", amz_date)
        .text("x-amz-signature", signature)
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"hi".to_vec())
                .file_name("x.txt")
                .mime_str("text/plain")
                .unwrap(),
        );
    reqwest::Client::new()
        .post(format!("{endpoint}/{bucket}"))
        .multipart(form)
}

/// A signed browser form POST (PostObject) into a read-only bucket: 403
/// with the reason; into a plaintext bucket: stored (204).
#[tokio::test]
async fn a_read_only_bucket_refuses_form_post() {
    let dir = tempfile::tempdir().unwrap();
    let env = read_only_env("atomform", &["atomform"], &dir.path().join("flag")).await;
    let r = signed_form_post(&env.ep(), "atomform")
        .send()
        .await
        .unwrap();
    let code = r.status().as_u16();
    let text = r.text().await.unwrap();
    assert_eq!(code, 403, "{text}");
    assert!(text.contains("read-only"), "{text}");
    let c = env.server.s3_client().await;
    c.create_bucket()
        .bucket("atomformplain")
        .send()
        .await
        .unwrap();
    let r = signed_form_post(&env.ep(), "atomformplain")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        204,
        "a plaintext bucket takes the form"
    );
}

/// Admin bulk surface family: copy into, move out of, delete, and a delete
/// job on the read-only bucket: 403 with the reason.
#[tokio::test]
async fn a_read_only_bucket_refuses_admin_bulk_writes() {
    let dir = tempfile::tempdir().unwrap();
    let env = read_only_env("atombulk", &["atombulk"], &dir.path().join("flag")).await;
    let c = env.server.s3_client().await;
    c.create_bucket()
        .bucket("atombulkplain")
        .send()
        .await
        .unwrap();
    assert_eq!(env.put("atombulkplain", "f.txt").await, 200);
    let ep = env.ep();
    let item = json!([{ "source_key": "f.txt", "relative": "f.txt" }]);
    let cases = [
        (
            "objects/copy",
            json!({ "source_bucket": "atombulkplain", "dest_bucket": "atombulk",
                    "dest_prefix": "", "items": item }),
        ),
        (
            "objects/move",
            json!({ "source_bucket": "atombulk", "dest_bucket": "atombulkplain",
                    "dest_prefix": "", "items": [{ "source_key": "a.txt", "relative": "a.txt" }] }),
        ),
        (
            "objects/delete",
            json!({ "bucket": "atombulk", "keys": ["a.txt"] }),
        ),
        (
            "objects/delete-jobs",
            json!({ "bucket": "atombulk", "keys": ["a.txt"], "expected": 1 }),
        ),
    ];
    for (path, body) in cases {
        let (code, v) = send(
            env.admin
                .post(format!("{ep}/_/api/admin/{path}"))
                .json(&body),
        )
        .await;
        assert_eq!(code, 403, "{path}: {v}");
        assert!(v.to_string().contains("read-only"), "{path}: {v}");
    }
    assert!(c
        .head_object()
        .bucket("atombulk")
        .key("a.txt")
        .send()
        .await
        .is_ok());
}

/// Jobs surface family: a re-encrypt (and Resume, which opens a window)
/// gets a fresh probe and 409 `atomic_writes`, opening no window; a
/// metadata backfill (an in-place rewrite) gets 403; a replication run
/// into the read-only bucket is refused with the reason.
#[tokio::test]
async fn a_read_only_bucket_refuses_jobs_that_write_into_it() {
    let dir = tempfile::tempdir().unwrap();
    let env = read_only_env("atomjobs", &["atomjobs"], &dir.path().join("flag")).await;
    for open_window in [false, true] {
        let (code, v) = env.reencrypt("atomjobs", open_window).await;
        assert_eq!(code, 409, "open_window={open_window}: {v}");
        let e = &v["errors"][0];
        assert_eq!(e["atomic_writes"]["backend"], "default", "{v}");
        assert_eq!(e["atomic_writes"]["probe"], "non-cas", "{v}");
        assert!(v["started"].as_array().unwrap().is_empty(), "{v}");
    }
    assert!(env.active_job("atomjobs").await.is_null(), "no job");
    let marker = env.server.data_dir().unwrap().join("atomjobs/.dg/facts");
    assert!(
        !std::fs::read_dir(&marker)
            .map(|d| d
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains("conversion")))
            .unwrap_or(false),
        "no conversion window was opened"
    );
    let (code, v) = send(
        env.admin
            .post(format!("{}/_/api/admin/jobs/backfill-metadata", env.ep()))
            .json(&json!({ "buckets": ["atomjobs"] })),
    )
    .await;
    assert_eq!(code, 403, "backfill: {v}");
    assert!(v.to_string().contains("read-only"), "{v}");
}

/// Replication into a read-only bucket: the run is refused with the reason
/// (the scheduler and the event consumer defer on the same rule).
#[tokio::test]
async fn replication_into_a_read_only_bucket_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("flag");
    let mut env = Env::new("atomrsrc", &[]).await;
    let client = env.server.s3_client().await;
    client
        .create_bucket()
        .bucket("atomrdst")
        .send()
        .await
        .unwrap();
    let (code, v) = env
        .storage(
            json!({
                "buckets": { "atomrdst": { "encryption": aes() } },
                "replication": { "enabled": true, "rules": [{
                    "name": "atom-rule", "enabled": true,
                    "source": { "bucket": "atomrsrc", "prefix": "" },
                    "destination": { "bucket": "atomrdst", "prefix": "" },
                    "interval": "1h", "batch_size": 100
                }]}
            }),
            false,
        )
        .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(env.put("atomrsrc", "r.txt").await, 200);
    std::fs::write(&flag, b"x").unwrap();
    let f = flag.to_str().unwrap().to_string();
    env.server
        .respawn_with_env(&[FORCED, ("DGP_TEST_FORCE_NONCAS_WHILE_FILE", &f)])
        .await;
    env.admin = admin_http_client(&env.ep()).await;
    env.wait_read_only("atomrdst").await;
    let (code, v) = send(env.admin.post(format!(
        "{}/_/api/admin/jobs/replication:atom-rule/run-now",
        env.ep()
    )))
    .await;
    assert_eq!(code, 409, "{v}");
    assert!(v.to_string().contains("read-only"), "{v}");
}

/// "Check again": the verdict turns CAS (the forced verdict ends with its
/// file), a fresh probe stores it, and the bucket is writable at once,
/// without a restart. The backend view shows the verdict and the
/// read-only buckets before.
#[tokio::test]
async fn check_again_lifts_read_only_when_the_backend_has_atomic_writes() {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("flag");
    let env = read_only_env("atomlift", &["atomlift"], &flag).await;
    let (_, v) = send(env.admin.get(format!("{}/_/api/admin/backends", env.ep()))).await;
    let b = &v["backends"][0];
    assert_eq!(b["capability"]["verdict"], "non-cas", "{v}");
    assert_eq!(b["read_only_buckets"], json!(["atomlift"]), "{v}");
    assert_eq!(env.put("atomlift", "x.txt").await, 403);

    std::fs::remove_file(&flag).unwrap();
    let (code, v) = send(env.admin.post(format!(
        "{}/_/api/admin/backends/default/capability-probe",
        env.ep()
    )))
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["capability"]["verdict"], "cas-verified", "{v}");
    assert_eq!(v["read_only_buckets"], json!([]), "{v}");
    assert!(env.bucket_view("atomlift").await["read_only"].is_null());
    assert_eq!(env.put("atomlift", "x.txt").await, 200, "writable again");
}

// ═══════════════════════════════════════════════════════════════════════
// Review gaps: fail closed before the first verdict; CreateBucket; every
// read-only bucket listed
// ═══════════════════════════════════════════════════════════════════════

/// Before the backend of an encrypted bucket has its first verdict (here
/// the probe takes 8 s; the boot waits 3 s for it), a write gets `503
/// SlowDown` that says the check runs; a read is served. Once the verdict
/// (CAS) exists, the same write is stored.
#[tokio::test]
async fn writes_wait_with_503_until_the_first_verdict() {
    let mut env = Env::new("atompend", &[]).await;
    let (code, v) = env
        .storage(
            json!({ "buckets": { "atompend": { "encryption": aes() } } }),
            false,
        )
        .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(env.put("atompend", "a.txt").await, 200);
    env.server
        .respawn_with_env(&[("DGP_TEST_ATOMICITY_PROBE_DELAY_MS", "8000")])
        .await;
    env.admin = admin_http_client(&env.ep()).await;

    let r = env
        .server
        .http()
        .put(format!("{}/atompend/b.txt", env.ep()))
        .body(b"x".to_vec())
        .send()
        .await
        .unwrap();
    let code = r.status().as_u16();
    let text = r.text().await.unwrap();
    assert_eq!(code, 503, "fail closed before the first verdict: {text}");
    assert!(
        text.contains("SlowDown") && text.contains("retry in a few seconds"),
        "{text}"
    );
    let r = env
        .server
        .http()
        .get(format!("{}/atompend/a.txt", env.ep()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "reads are never blocked");
    // The pending state shows in /_/ready and never fails readiness.
    let r = reqwest::get(format!("{}/_/ready", env.ep())).await.unwrap();
    assert_eq!(r.status().as_u16(), 200, "pending is not unready");
    let ready: Value = r.json().await.unwrap();
    assert_eq!(ready["atomic_writes"]["default"], "checking", "{ready}");
    // And on the backend card of the admin API.
    let backends: Value = env
        .admin
        .get(format!("{}/_/api/admin/backends", env.ep()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        backends["backends"][0]["atomic_writes_checking"], true,
        "{backends}"
    );

    let mut stored = 0;
    for _ in 0..200 {
        stored = env.put("atompend", "b.txt").await;
        if stored == 200 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(stored, 200, "writable once the verdict (CAS) exists");
}

/// The whole default backend encrypts (its own site, so buckets without a
/// policy inherit it), then the proxy restarts where the backend has no
/// atomic writes.
async fn backend_wide_read_only(name: &str) -> Env {
    let mut env = Env::new(name, &[]).await;
    let (code, v) = env
        .storage(json!({ "backend_encryption": aes() }), false)
        .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(env.put(name, "a.txt").await, 200);
    env.server.respawn_with_env(&[FORCED]).await;
    env.admin = admin_http_client(&env.ep()).await;
    env
}

/// S3 CreateBucket of a bucket that would inherit the proxy encryption of
/// a backend without atomic writes: 403 AccessDenied (not a 409, which
/// clients read as "already exists"), with the backend and the probe
/// result in the message; no bucket is created.
#[tokio::test]
async fn s3_create_bucket_that_would_be_read_only_is_refused() {
    let env = backend_wide_read_only("atomcrs3").await;
    env.wait_read_only("atomcrs3").await;
    let c = env.server.s3_client().await;
    let err = c
        .create_bucket()
        .bucket("atomcrs3new")
        .send()
        .await
        .expect_err("CreateBucket refused");
    assert_eq!(err.meta().code(), Some("AccessDenied"), "{err:?}");
    let m = err.meta().message().unwrap_or("");
    assert!(
        m.contains("cannot be created")
            && m.contains("does not support atomic writes")
            && !m.contains("[atomic_writes")
            && m.contains(DOC),
        "{m}"
    );
    assert!(
        !env.server.data_dir().unwrap().join("atomcrs3new").exists(),
        "no bucket was created"
    );
    assert!(c.head_bucket().bucket("atomcrs3new").send().await.is_err());
}

/// `read_only_buckets` of the backend and the bucket view list a bucket
/// without a policy that inherits the backend's encryption.
#[tokio::test]
async fn read_only_buckets_list_buckets_without_a_policy() {
    let env = backend_wide_read_only("atomnopol").await;
    let v = env.wait_read_only("atomnopol").await;
    assert!(
        v["read_only"].as_str().unwrap().contains("read-only"),
        "{v}"
    );
    let (_, v) = send(env.admin.get(format!("{}/_/api/admin/backends", env.ep()))).await;
    let ro = v["backends"][0]["read_only_buckets"].clone();
    assert!(
        ro.as_array()
            .is_some_and(|a| a.iter().any(|b| b == "atomnopol")),
        "the bucket without a policy is listed: {v}"
    );
    assert_eq!(env.put("atomnopol", "x.txt").await, 403);
}

/// Review: the synthesized single backend (`is_synthesized: true`, no named
/// backends) without atomic writes. A plaintext bucket on it is not offered
/// proxy AES: `bucket_modes` lacks `aes256-gcm-proxy`, `proxy_unavailable`
/// says why, and the backend's `bucket_encryption_modes` lacks it too.
#[tokio::test]
async fn the_synthesized_backend_without_atomic_writes_offers_no_proxy_aes() {
    let env = Env::new("atomsyn", &[FORCED]).await;
    let (_, v) = send(env.admin.get(format!("{}/_/api/admin/backends", env.ep()))).await;
    let b = &v["backends"][0];
    assert_eq!(b["is_synthesized"], true, "{v}");
    assert_eq!(b["capability"]["verdict"], "non-cas", "{v}");
    assert!(
        !b["bucket_encryption_modes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "aes256-gcm-proxy"),
        "{v}"
    );
    let e = env.bucket_view("atomsyn").await["effective_encryption"].clone();
    assert_eq!(e["mode"], "none", "{e}");
    assert_eq!(e["bucket_modes"], json!(["none"]), "{e}");
    assert_eq!(e["capability"]["verdict"], "non-cas", "{e}");
    let why = e["proxy_unavailable"].as_str().unwrap_or("");
    assert!(
        why.contains("atomic writes") && why.contains("'default'") && why.contains(DOC),
        "{e}"
    );
    // The create dialog's preview says the same.
    let (code, p) = send(env.admin.get(format!(
        "{}/_/api/admin/buckets/atomsynnew/encryption-preview?backend=default",
        env.ep()
    )))
    .await;
    assert_eq!(code, 200, "{p}");
    assert_eq!(p["bucket_modes"], json!(["none"]), "{p}");
}

/// An unconfigured bucket on a NON-default backend whose own site
/// encrypts with the proxy, and whose verdict is non-CAS.
#[tokio::test]
async fn read_only_holds_before_the_router_resolves_the_bucket() {
    let data = tempfile::TempDir::new().unwrap();
    let local = data.path().join("local");
    let dr = data.path().join("dr");
    std::fs::create_dir_all(dr.join("photos")).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    let doc = format!(
        "access:\n  access_key_id: {}\n  secret_access_key: {}\n\
         storage:\n  backends:\n    - name: local-disk\n      type: filesystem\n      path: {}\n\
         \x20   - name: aws-dr\n      type: filesystem\n      path: {}\n      encryption: {{ mode: aes256-gcm-proxy, key: \"{KEY}\", key_id: {KEY_ID} }}\n\
         \x20 default_backend: local-disk\n\
         advanced:\n  listen_addr: {}\n  bootstrap_password_hash: \"{}\"\n",
        common::TEST_ACCESS_KEY,
        common::TEST_SECRET_KEY,
        local.display(),
        dr.display(),
        common::LISTEN_ADDR_PLACEHOLDER,
        common::TEST_BOOTSTRAP_PASSWORD_HASH
    );
    let server = TestServer::from_config_document(
        &doc,
        data,
        (common::TEST_ACCESS_KEY, common::TEST_SECRET_KEY),
        "first",
        vec![(
            "DGP_TEST_FORCE_NONCAS_BACKEND".to_string(),
            "aws-dr".to_string(),
        )],
    )
    .await;
    async fn put(server: &TestServer, key: &str) -> u16 {
        server
            .http()
            .put(format!("{}/photos/{key}", server.endpoint()))
            .body(b"x".to_vec())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
    let first = put(&server, "a.txt").await;
    let second = put(&server, "b.txt").await;
    // Any CreateBucket clears the router's resolutions.
    let c = server.s3_client().await;
    c.create_bucket().bucket("other").send().await.unwrap();
    let third = put(&server, "c.txt").await;
    eprintln!("PUT photos/a: {first}, photos/b: {second}, after CreateBucket photos/c: {third}");
    let stored: Vec<_> = std::fs::read_dir(dr.join("photos"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .collect();
    eprintln!("stored on aws-dr/photos: {stored:?}");
    assert_eq!((first, second, third), (403, 403, 403), "read-only bucket");
}

/// Compatibility review C4: replication into a read-only destination. The
/// rule shows `deferred` with the reason (Jobs row and `detail.deferred`);
/// its events do not pin the outbox (the cursor moves on and the pruner
/// removes them); once the destination is writable again (Check again),
/// the rule's catch-up run copies what it skipped.
#[tokio::test]
async fn replication_into_a_read_only_destination_is_visible_and_does_not_pin_the_outbox() {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("flag");
    let mut env = Env::new("atomcsrc", &[]).await;
    let client = env.server.s3_client().await;
    client
        .create_bucket()
        .bucket("atomcdst")
        .send()
        .await
        .unwrap();
    let (code, v) = env
        .storage(
            json!({
                "buckets": { "atomcdst": { "encryption": aes() } },
                "replication": { "enabled": true, "tick_interval": "5s", "rules": [{
                    "name": "atomc-rule", "enabled": true,
                    "source": { "bucket": "atomcsrc", "prefix": "" },
                    "destination": { "bucket": "atomcdst", "prefix": "" },
                    "interval": "1h", "batch_size": 100
                }]}
            }),
            false,
        )
        .await;
    assert_eq!(code, 200, "{v}");
    // The outbox pruner runs on the dispatcher's tick.
    let (code, v) = send(
        env.admin
            .put(format!("{}/_/api/admin/config/section/advanced", env.ep()))
            .json(&json!({ "event_delivery": { "tick_interval": "1s" } })),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    std::fs::write(&flag, b"x").unwrap();
    let f = flag.to_str().unwrap().to_string();
    env.server
        .respawn_with_env(&[FORCED, ("DGP_TEST_FORCE_NONCAS_WHILE_FILE", &f)])
        .await;
    env.admin = admin_http_client(&env.ep()).await;
    env.wait_read_only("atomcdst").await;
    assert_eq!(env.put("atomcsrc", "late.txt").await, 200);

    let row = || async {
        let (_, v) = send(env.admin.get(format!("{}/_/api/admin/jobs", env.ep()))).await;
        v["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"] == "replication:atomc-rule")
            .cloned()
            .unwrap()
    };
    let outbox_total = || async {
        let (_, v) = send(
            env.admin
                .get(format!("{}/_/api/admin/event-outbox", env.ep())),
        )
        .await;
        v["total"].as_i64().unwrap()
    };
    let (mut seen, mut total) = (Value::Null, -1);
    for _ in 0..400 {
        seen = row().await;
        total = outbox_total().await;
        if seen["status"] == "deferred" && total == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        seen["status"], "deferred",
        "visible on the Jobs row: {seen}"
    );
    let why = seen["detail"]["deferred"].as_str().unwrap_or("");
    assert!(why.contains("read-only"), "the reason: {seen}");
    assert_eq!(total, 0, "the rule's events do not pin the outbox");

    // Writable again: the catch-up run copies what the rule skipped.
    std::fs::remove_file(&flag).unwrap();
    let (code, v) = send(env.admin.post(format!(
        "{}/_/api/admin/backends/default/capability-probe",
        env.ep()
    )))
    .await;
    assert_eq!(code, 200, "{v}");
    let mut copied = 0;
    for _ in 0..300 {
        copied = env
            .server
            .http()
            .get(format!("{}/atomcdst/late.txt", env.ep()))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        if copied == 200 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(copied, 200, "the rule caught up");
    let seen = row().await;
    assert_ne!(seen["status"], "deferred", "{seen}");
}

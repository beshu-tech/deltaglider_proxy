// SPDX-License-Identifier: BUSL-1.1

//! Per-bucket encryption at rest (issue #116, P1): a bucket's `encryption`
//! override, end to end. Each scenario checks two things: every object
//! stays readable, and each stored blob carries the at-rest form (the key
//! id, or plaintext) that its bucket's config asks for.
//!
//! Filesystem backends, plus the MinIO variants (`*_minio`) of the shared
//! raw-backend scenarios.

use crate::common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, generate_binary, mutate_binary, TestServer};
use serde_json::json;
use std::path::PathBuf;

const AUTH: (&str, &str) = ("BUCKETENC", "BUCKETENCSECRET");
const KA: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const KB: &str = "2222222222222222222222222222222222222222222222222222222222222222";

/// Where the raw backend `local-disk` keeps its buckets.
enum Store {
    /// A filesystem backend at this path.
    Fs(PathBuf),
    /// The test MinIO.
    Minio,
}

struct Env {
    server: TestServer,
    admin: reqwest::Client,
    store: Store,
    /// Path of the second filesystem backend `aws-dr` (named configs).
    dr: Option<PathBuf>,
}

/// The shape of the test config.
enum Shape {
    /// One filesystem backend (`storage.filesystem`): the pin is implicit.
    Single,
    /// Named backends: `local-disk` (filesystem) and `aws-dr` (filesystem).
    NamedFs,
    /// Named backends: `local-disk` (MinIO) and `aws-dr` (filesystem).
    NamedMinio,
}

/// Spawn a proxy. `backend_enc` is the YAML flow map of `local-disk`'s
/// encryption (`None`: no encryption). `buckets` is the YAML of
/// `storage.buckets` entries, 4-space indented, one per line; with named
/// backends each entry must name `backend: local-disk` (or `aws-dr`).
/// `first` is the bucket the harness creates.
async fn spawn(
    shape: Shape,
    backend_enc: Option<&str>,
    buckets: &str,
    extra_storage: &str,
    first: &str,
) -> Env {
    let data = tempfile::TempDir::new().unwrap();
    let local = data.path().join("local");
    let dr = data.path().join("dr");
    let enc = |indent: &str| {
        backend_enc
            .map(|e| format!("{indent}encryption: {e}\n"))
            .unwrap_or_default()
    };
    let storage = match shape {
        Shape::Single => format!(
            "  filesystem: {}\n{}",
            local.display(),
            backend_enc
                .map(|e| format!("  backend_encryption: {e}\n"))
                .unwrap_or_default()
        ),
        Shape::NamedFs | Shape::NamedMinio => {
            let local_def = match shape {
                Shape::NamedMinio => format!(
                    "    - name: local-disk\n      type: s3\n      endpoint: \"{}\"\n      \
                     region: us-east-1\n      force_path_style: true\n      \
                     access_key_id: {}\n      secret_access_key: {}\n",
                    common::minio_endpoint_url(),
                    common::MINIO_ACCESS_KEY,
                    common::MINIO_SECRET_KEY
                ),
                _ => format!(
                    "    - name: local-disk\n      type: filesystem\n      path: {}\n",
                    local.display()
                ),
            };
            format!(
                "  backends:\n{local_def}{}    - name: aws-dr\n      type: filesystem\n      \
                 path: {}\n  default_backend: local-disk\n",
                enc("      "),
                dr.display()
            )
        }
    };
    let doc = format!(
        "access:\n  access_key_id: {}\n  secret_access_key: {}\n\
         storage:\n{storage}  buckets:\n{buckets}{extra_storage}\
         advanced:\n  listen_addr: {}\n  bootstrap_password_hash: \"{}\"\n",
        AUTH.0,
        AUTH.1,
        common::LISTEN_ADDR_PLACEHOLDER,
        common::TEST_BOOTSTRAP_PASSWORD_HASH
    );
    let store = match shape {
        Shape::NamedMinio => Store::Minio,
        _ => Store::Fs(local),
    };
    let dr = (!matches!(shape, Shape::Single)).then_some(dr);
    let server = TestServer::from_config_document(&doc, data, AUTH, first, Vec::new()).await;
    let admin = admin_http_client(&server.endpoint()).await;
    Env {
        server,
        admin,
        store,
        dr,
    }
}

impl Env {
    async fn create(&self, bucket: &str) {
        let client = self.server.s3_client().await;
        let _ = client.create_bucket().bucket(bucket).send().await;
    }

    async fn put(&self, bucket: &str, key: &str, body: &[u8]) {
        self.server
            .s3_client()
            .await
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(body.to_vec()))
            .send()
            .await
            .unwrap_or_else(|e| panic!("PUT {bucket}/{key}: {e:?}"));
    }

    async fn get(&self, bucket: &str, key: &str) -> Vec<u8> {
        self.server
            .s3_client()
            .await
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {bucket}/{key}: {e:?}"))
            .body
            .collect()
            .await
            .unwrap()
            .to_vec()
    }

    async fn copy(&self, from: (&str, &str), to: (&str, &str)) {
        self.server
            .s3_client()
            .await
            .copy_object()
            .copy_source(format!("{}/{}", from.0, from.1))
            .bucket(to.0)
            .key(to.1)
            .send()
            .await
            .unwrap_or_else(|e| panic!("COPY {from:?} -> {to:?}: {e:?}"));
    }

    /// PUT a merge-patch of the storage section; the parsed response.
    async fn storage_patch(&self, body: serde_json::Value) -> (u16, serde_json::Value) {
        self.storage_patch_with(body, &[]).await
    }

    /// [`Self::storage_patch`] with extra request headers (confirmations).
    async fn storage_patch_with(
        &self,
        body: serde_json::Value,
        headers: &[(&str, &str)],
    ) -> (u16, serde_json::Value) {
        let mut req = self.admin.put(format!(
            "{}/_/api/admin/config/section/storage",
            self.server.endpoint()
        ));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.json(&body).send().await.expect("section PUT");
        let code = resp.status().as_u16();
        (code, resp.json().await.unwrap_or_default())
    }

    async fn storage_patch_ok(&self, body: serde_json::Value) -> serde_json::Value {
        let (code, v) = self.storage_patch(body.clone()).await;
        assert!(code == 200 && v["ok"] == true, "PUT {body}: {code} {v}");
        v
    }

    /// The at-rest form of the stored blob `stored` (a key, `<key>.delta`,
    /// or `<prefix>/reference.bin`) in `bucket` on `local-disk`:
    /// `None` = plaintext, `Some(id)` = encrypted under key id `id`.
    async fn at_rest(&self, bucket: &str, stored: &str) -> Option<String> {
        match &self.store {
            Store::Fs(root) => fs_at_rest(&fs_blob(root, bucket, stored)),
            Store::Minio => {
                let head = common::minio_client()
                    .await
                    .head_object()
                    .bucket(bucket)
                    .key(stored)
                    .send()
                    .await
                    .unwrap_or_else(|e| panic!("MinIO HEAD {bucket}/{stored}: {e:?}"));
                let meta = head.metadata().cloned().unwrap_or_default();
                // The S3 backend stores user metadata as `user-<key>`.
                meta.get("user-dg-encrypted")?;
                Some(
                    meta.get("user-dg-encryption-key-id")
                        .cloned()
                        .expect("an encrypted blob carries its key id"),
                )
            }
        }
    }

    async fn reencrypt(&self, bucket: &str) {
        let endpoint = self.server.endpoint();
        let resp = self
            .admin
            .post(format!("{endpoint}/_/api/admin/jobs/reencrypt"))
            .json(&json!({ "buckets": [bucket] }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "reencrypt: {}", resp.status());
        wait_job_done(&self.admin, &endpoint, bucket).await;
    }
}

/// Where the filesystem backend at `root` keeps the blob `stored`.
fn fs_blob(root: &std::path::Path, bucket: &str, stored: &str) -> PathBuf {
    root.join(bucket).join("deltaspaces").join(stored)
}

/// The at-rest form of a filesystem blob, from its `user.dg.metadata` xattr.
fn fs_at_rest(path: &std::path::Path) -> Option<String> {
    let raw = xattr::get(path, "user.dg.metadata")
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .unwrap_or_else(|| panic!("{}: no metadata", path.display()));
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let um = &v["user_metadata"];
    um.get("dg-encrypted")?;
    Some(
        um["dg-encryption-key-id"]
            .as_str()
            .expect("an encrypted blob carries its key id")
            .to_string(),
    )
}

async fn wait_job_done(admin: &reqwest::Client, endpoint: &str, bucket: &str) {
    for _ in 0..600 {
        let v: serde_json::Value = admin
            .get(format!("{endpoint}/_/api/admin/jobs/bucket/{bucket}"))
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
    panic!("the job on '{bucket}' did not finish within 60s");
}

fn override_json(key: &str, kid: &str) -> serde_json::Value {
    json!({ "mode": "aes256-gcm-proxy", "key": key, "key_id": kid })
}

// ── Scenario: an override added to a bucket with history ────────────────

/// The bucket's objects AND its `reference.bin` were written under the
/// backend key. After the override, they still read; a delta PUT into the
/// existing deltaspace works (it reads the old reference, writes its delta
/// under the bucket key); the apply response names the re-encrypt job,
/// and the job moves every blob to the bucket key.
async fn override_added_on_history(env: Env, bucket: &str, backend_patch: serde_json::Value) {
    let v1 = generate_binary(120_000, 7);
    let v2 = mutate_binary(&v1, 0.05);
    let v3 = mutate_binary(&v2, 0.05);
    env.put(bucket, "photo.jpg", b"old passthrough body").await;
    env.put(bucket, "app.zip", &v1).await;
    env.put(bucket, "app2.zip", &v2).await;
    assert_eq!(
        env.at_rest(bucket, "photo.jpg").await.as_deref(),
        Some("local-a")
    );
    assert_eq!(
        env.at_rest(bucket, "reference.bin").await.as_deref(),
        Some("local-a")
    );
    assert_eq!(
        env.at_rest(bucket, "app2.zip.delta").await.as_deref(),
        Some("local-a")
    );

    let mut patch = backend_patch;
    patch["buckets"][bucket]["encryption"] = override_json(KB, "releases-b");
    let resp = env.storage_patch_ok(patch).await;
    let warnings = resp["warnings"].to_string();
    assert!(
        warnings.contains("/jobs/reencrypt") && warnings.contains("releases-b"),
        "the apply names the re-encrypt job: {resp}"
    );

    // History reads under the backend key.
    assert_eq!(env.get(bucket, "photo.jpg").await, b"old passthrough body");
    assert_eq!(env.get(bucket, "app2.zip").await, v2);
    // A delta into the existing deltaspace.
    env.put(bucket, "app3.zip", &v3).await;
    assert_eq!(env.get(bucket, "app3.zip").await, v3);
    assert_eq!(
        env.at_rest(bucket, "app3.zip.delta").await.as_deref(),
        Some("releases-b"),
        "the new delta is written under the bucket key"
    );
    assert_eq!(
        env.at_rest(bucket, "reference.bin").await.as_deref(),
        Some("local-a"),
        "the reference keeps its form until the re-encrypt job"
    );

    env.reencrypt(bucket).await;
    for stored in ["photo.jpg", "app2.zip.delta", "reference.bin"] {
        assert_eq!(
            env.at_rest(bucket, stored).await.as_deref(),
            Some("releases-b"),
            "{stored} after the re-encrypt job"
        );
    }
    assert_eq!(env.get(bucket, "app.zip").await, v1);
    assert_eq!(env.get(bucket, "app2.zip").await, v2);
    assert_eq!(env.get(bucket, "app3.zip").await, v3);
}

const BACKEND_A: &str = "{ mode: aes256-gcm-proxy, key: \"1111111111111111111111111111111111111111111111111111111111111111\", key_id: local-a }";

#[tokio::test]
async fn an_override_on_a_bucket_with_history_single_backend() {
    let env = spawn(
        Shape::Single,
        Some(BACKEND_A),
        "    releases: {}\n",
        "",
        "releases",
    )
    .await;
    override_added_on_history(env, "releases", json!({ "buckets": { "releases": {} } })).await;
}

#[tokio::test]
async fn an_override_on_a_bucket_with_history_named_backends() {
    let env = spawn(
        Shape::NamedFs,
        Some(BACKEND_A),
        "    releases: { backend: local-disk }\n",
        "",
        "releases",
    )
    .await;
    override_added_on_history(
        env,
        "releases",
        json!({ "buckets": { "releases": { "backend": "local-disk" } } }),
    )
    .await;
}

#[tokio::test]
async fn an_override_on_a_bucket_with_history_minio() {
    skip_unless_minio!();
    let bucket = common::unique_bucket("be-hist");
    let env = spawn(
        Shape::NamedMinio,
        Some(BACKEND_A),
        &format!("    {bucket}: {{ backend: local-disk }}\n"),
        "",
        &bucket,
    )
    .await;
    let mut patch = json!({ "buckets": {} });
    patch["buckets"][&bucket] = json!({ "backend": "local-disk" });
    override_added_on_history(env, &bucket, patch).await;
}

// ── Scenario: rotation A → B ────────────────────────────────────────────

/// A new key on the override: the admin API keeps the old one as the
/// legacy key (its objects stay readable); new writes use the new key.
#[tokio::test]
async fn a_bucket_key_rotation_keeps_old_objects_readable() {
    let env = spawn(
        Shape::NamedFs,
        None,
        &format!(
            "    releases: {{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KA}\", key_id: releases-a }} }}\n"
        ),
        "",
        "releases",
    )
    .await;
    env.put("releases", "old.txt", b"under key A").await;
    assert_eq!(
        env.at_rest("releases", "old.txt").await.as_deref(),
        Some("releases-a")
    );

    env.storage_patch_ok(
        json!({ "buckets": { "releases": { "encryption": override_json(KB, "releases-b") } } }),
    )
    .await;
    assert_eq!(env.get("releases", "old.txt").await, b"under key A");
    env.put("releases", "new.txt", b"under key B").await;
    assert_eq!(
        env.at_rest("releases", "new.txt").await.as_deref(),
        Some("releases-b")
    );
    assert_eq!(env.get("releases", "new.txt").await, b"under key B");

    // The same id with another key is refused (objects would decrypt
    // with the wrong key).
    let (code, v) = env
        .storage_patch(
            json!({ "buckets": { "releases": { "encryption": override_json(KA, "releases-b") } } }),
        )
        .await;
    assert!(code >= 400 && v["ok"] != true, "{code} {v}");
}

// ── Scenario: the override removed ──────────────────────────────────────

/// Removing an active key in one step is refused. `mode: none` keeps the
/// key as legacy: the ciphertext stays readable and new writes are
/// plaintext. After the re-encrypt job the block can go.
#[tokio::test]
async fn removing_a_bucket_override_takes_two_steps() {
    let env = spawn(
        Shape::NamedFs,
        None,
        &format!(
            "    releases: {{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KA}\", key_id: releases-a }} }}\n"
        ),
        "",
        "releases",
    )
    .await;
    env.put("releases", "old.txt", b"under key A").await;

    let (code, v) = env
        .storage_patch(json!({ "buckets": { "releases": { "encryption": null } } }))
        .await;
    assert!(
        code >= 400 && v.to_string().contains("releases-a"),
        "removing the active key in one step is refused: {code} {v}"
    );
    assert_eq!(env.get("releases", "old.txt").await, b"under key A");

    // `mode: none` is a downgrade: it needs its confirmation (issue #116 P3).
    let (code, v) = env
        .storage_patch_with(
            json!({ "buckets": { "releases": { "encryption": { "mode": "none" } } } }),
            &[("X-DGP-Confirm-Encryption-Downgrade", "releases")],
        )
        .await;
    assert!(code == 200 && v["ok"] == true, "{code} {v}");
    assert_eq!(env.get("releases", "old.txt").await, b"under key A");
    env.put("releases", "new.txt", b"plaintext now").await;
    assert_eq!(env.at_rest("releases", "new.txt").await, None);

    env.reencrypt("releases").await;
    assert_eq!(env.at_rest("releases", "old.txt").await, None);
    // Removing the legacy key needs its id confirmed (review A); the scan
    // covers every bucket on the backend and says it is safe.
    let remove = json!({ "buckets": { "releases": { "encryption": null } } });
    let (code, v) = env.storage_patch(remove.clone()).await;
    assert!(
        code >= 400
            && v.to_string()
                .contains("X-DGP-Confirm-Key-Removal: releases-a"),
        "{code} {v}"
    );
    let endpoint = env.server.endpoint();
    let scan: serde_json::Value = env
        .admin
        .get(format!(
            "{endpoint}/_/api/admin/backends/local-disk/legacy-key-usage?bucket=releases&fresh=true"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(scan["legacy_key_id"], "releases-a", "{scan}");
    assert_eq!(scan["safe_to_clear"], true, "{scan}");
    let resp = env
        .admin
        .put(format!("{endpoint}/_/api/admin/config/section/storage"))
        .header("X-DGP-Confirm-Key-Removal", "releases-a")
        .json(&remove)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "{:?}", resp.text().await);
    assert_eq!(env.get("releases", "old.txt").await, b"under key A");
}

/// Review A: the legacy-key usage scan of a bucket's key id counts EVERY
/// bucket stored on the backend. `downloads` keeps objects under the id
/// `shared-a` that `releases` retires: the scan for `releases` must not
/// say safe_to_clear.
#[tokio::test]
async fn a_bucket_legacy_scan_counts_its_neighbours() {
    let a = format!("{{ mode: aes256-gcm-proxy, key: \"{KA}\", key_id: shared-a }}");
    let env = spawn(
        Shape::NamedFs,
        None,
        &format!(
            "    releases: {{ backend: local-disk, encryption: {{ mode: none, legacy_key: \"{KA}\", legacy_key_id: shared-a }} }}\n    \
             downloads: {{ backend: local-disk, encryption: {a} }}\n"
        ),
        "",
        "releases",
    )
    .await;
    env.create("downloads").await;
    env.put("downloads", "x.txt", b"under shared-a").await;
    let endpoint = env.server.endpoint();
    let scan: serde_json::Value = env
        .admin
        .get(format!(
            "{endpoint}/_/api/admin/backends/local-disk/legacy-key-usage?bucket=releases&fresh=true"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(scan["legacy_key_id"], "shared-a", "{scan}");
    assert_eq!(scan["objects_under_legacy_key"], 1, "{scan}");
    assert_eq!(scan["safe_to_clear"], false, "{scan}");
}

// ── Scenario: buckets in different modes on one raw backend ─────────────

const RULES: &str = "  replication:\n    enabled: true\n    tick_interval: 1h\n    rules:\n      \
     - name: a1-to-plain\n        source: { bucket: BKT-A1, prefix: \"\" }\n        \
     destination: { bucket: BKT-P, prefix: \"\" }\n        interval: 1h\n        batch_size: 100\n  lifecycle:\n    enabled: true\n    tick_interval: 1h\n    rules:\n      \
     - name: b-to-a1\n        enabled: true\n        bucket: BKT-B\n        prefix: old\n        action:\n          \
     type: transition\n          destination: { bucket: BKT-A1, prefix: cold }\n          \
     delete_source_after_success: false\n        expire_after: 1ms\n        batch_size: 100\n        \
     include_globs: [\"old/**\"]\n        exclude_globs: []\n";

/// Four buckets on `local-disk` (no backend key): `p` plaintext, `a1` and
/// `a2` under key A (one id), `b` under key B. CopyObject, replication and
/// a lifecycle transition between them write the DESTINATION's form, and
/// every copy reads back.
async fn mixed_modes_on_one_backend(shape: Shape, names: [&str; 4]) {
    let [p, a1, a2, b] = names;
    let a = format!("{{ mode: aes256-gcm-proxy, key: \"{KA}\", key_id: shared-a }}");
    let bb = format!("{{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: only-b }}");
    let buckets = format!(
        "    {p}: {{ backend: local-disk }}\n    {a1}: {{ backend: local-disk, encryption: {a} }}\n    \
         {a2}: {{ backend: local-disk, encryption: {a} }}\n    {b}: {{ backend: local-disk, encryption: {bb} }}\n"
    );
    let rules = RULES
        .replace("BKT-A1", a1)
        .replace("BKT-P", p)
        .replace("BKT-B", b);
    let env = spawn(shape, None, &buckets, &rules, p).await;
    for bucket in [a1, a2, b] {
        env.create(bucket).await;
    }
    env.put(p, "x.txt", b"plain source").await;
    env.put(a1, "x.txt", b"a1 source").await;
    env.put(b, "old/y.txt", b"b source").await;
    assert_eq!(env.at_rest(p, "x.txt").await, None);
    assert_eq!(env.at_rest(a1, "x.txt").await.as_deref(), Some("shared-a"));
    assert_eq!(env.at_rest(b, "old/y.txt").await.as_deref(), Some("only-b"));

    // (none → A), (A → B), (A → A), (B → none).
    env.copy((p, "x.txt"), (a1, "from-p.txt")).await;
    env.copy((a1, "x.txt"), (b, "from-a1.txt")).await;
    env.copy((a1, "x.txt"), (a2, "from-a1.txt")).await;
    env.copy((b, "old/y.txt"), (p, "from-b.txt")).await;
    for (bucket, key, form, body) in [
        (a1, "from-p.txt", Some("shared-a"), &b"plain source"[..]),
        (b, "from-a1.txt", Some("only-b"), b"a1 source"),
        (a2, "from-a1.txt", Some("shared-a"), b"a1 source"),
        (p, "from-b.txt", None, b"b source"),
    ] {
        assert_eq!(
            env.at_rest(bucket, key).await.as_deref(),
            form,
            "{bucket}/{key}"
        );
        assert_eq!(env.get(bucket, key).await, body, "{bucket}/{key}");
    }

    // Replication a1 (A) → p (plaintext).
    let endpoint = env.server.endpoint();
    let resp = env
        .admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/replication:a1-to-plain/run-now"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    let run = common::wait_for_run(&env.admin, &endpoint, "a1-to-plain").await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(env.at_rest(p, "x.txt").await, None);
    assert_eq!(env.at_rest(p, "from-p.txt").await, None);
    assert_eq!(env.get(p, "from-p.txt").await, b"plain source");

    // Lifecycle transition b (B) → a1 (A). The boot tick can hold the
    // rule's lease for a moment (409): wait for that run, then run now.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let run = loop {
        let resp = env
            .admin
            .post(format!(
                "{endpoint}/_/api/admin/jobs/lifecycle:b-to-a1/run-now"
            ))
            .send()
            .await
            .unwrap();
        let code = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        if code == 202 {
            let id = body["run_id"].as_i64().expect("run-now returns run_id");
            break common::wait_for_job_run(&env.admin, &endpoint, "lifecycle:b-to-a1", id).await;
        }
        assert_eq!(code, 409, "{body}");
        assert!(
            std::time::Instant::now() < deadline,
            "run-now kept 409: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(
        env.at_rest(a1, "cold/y.txt").await.as_deref(),
        Some("shared-a")
    );
    assert_eq!(env.get(a1, "cold/y.txt").await, b"b source");
}

#[tokio::test]
async fn buckets_in_different_modes_on_one_backend() {
    mixed_modes_on_one_backend(
        Shape::NamedFs,
        ["releases", "db-archive", "downloads", "backups"],
    )
    .await;
}

#[tokio::test]
async fn buckets_in_different_modes_on_one_backend_minio() {
    skip_unless_minio!();
    let names = ["be-p", "be-a1", "be-a2", "be-b"].map(common::unique_bucket);
    mixed_modes_on_one_backend(
        Shape::NamedMinio,
        [&names[0], &names[1], &names[2], &names[3]],
    )
    .await;
}

// ── Scenario: migrate keeps the bucket's mode ───────────────────────────

/// A migrate moves the override with its bucket: the copies on the target
/// backend are written under the bucket key, and the flip keeps it.
#[tokio::test]
async fn migrate_keeps_a_buckets_own_encryption() {
    let env = spawn(
        Shape::NamedFs,
        Some(BACKEND_A),
        &format!(
            "    releases: {{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: releases-b }} }}\n"
        ),
        "",
        "releases",
    )
    .await;
    env.put("releases", "x.txt", b"moves with its key").await;
    let endpoint = env.server.endpoint();
    let resp = env
        .admin
        .post(format!("{endpoint}/_/api/admin/buckets/releases/migrate"))
        .json(&json!({ "target_backend": "aws-dr", "delete_source": false }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "migrate: {}", resp.status());
    wait_job_done(&env.admin, &endpoint, "releases").await;

    let dr = env.dr.as_ref().unwrap();
    assert_eq!(
        fs_at_rest(&fs_blob(dr, "releases", "x.txt")).as_deref(),
        Some("releases-b"),
        "the copy on the target is under the bucket key, not aws-dr's mode"
    );
    assert_eq!(env.get("releases", "x.txt").await, b"moves with its key");
    let cfg: serde_json::Value = env
        .admin
        .get(format!("{endpoint}/_/api/admin/config/section/storage"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let policy = &cfg["buckets"]["releases"];
    assert_eq!(policy["backend"], "aws-dr", "{cfg}");
    assert_eq!(policy["encryption"]["key_id"], "releases-b", "{cfg}");
    env.put("releases", "after.txt", b"after the flip").await;
    assert_eq!(
        fs_at_rest(&fs_blob(dr, "releases", "after.txt")).as_deref(),
        Some("releases-b")
    );
}

// ── Scenario: secrets ───────────────────────────────────────────────────

/// The bucket key never appears in GET /config, the section GET or the
/// export, and a GUI round trip (GET the section, PUT it back) keeps it.
#[tokio::test]
async fn a_bucket_key_never_leaves_and_survives_a_round_trip() {
    let env = spawn(
        Shape::NamedFs,
        None,
        &format!(
            "    releases: {{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: releases-b }} }}\n"
        ),
        "",
        "releases",
    )
    .await;
    env.put("releases", "x.txt", b"secret body").await;
    let endpoint = env.server.endpoint();
    let mut section = serde_json::Value::Null;
    for path in ["config", "config/section/storage", "config/export"] {
        let text = env
            .admin
            .get(format!("{endpoint}/_/api/admin/{path}"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!text.contains(KB), "{path} leaks the bucket key: {text}");
        assert!(text.contains("releases-b"), "{path} shows the key id");
        if path == "config/section/storage" {
            section = serde_json::from_str(&text).unwrap();
        }
    }
    // The GUI sends back what it read (the key is absent).
    env.storage_patch_ok(section).await;
    assert_eq!(env.get("releases", "x.txt").await, b"secret body");
    env.put("releases", "y.txt", b"still encrypted").await;
    assert_eq!(
        env.at_rest("releases", "y.txt").await.as_deref(),
        Some("releases-b")
    );
    let file = std::fs::read_to_string(env.server.config_path()).unwrap();
    assert!(file.contains(KB), "the persisted file keeps the key");
}

/// Config rules refused through the apply: an alias that disagrees with
/// the bucket it aliases, and a key id that another key already uses.
#[tokio::test]
async fn an_apply_refuses_alias_disagreement_and_key_id_collision() {
    let env = spawn(
        Shape::NamedFs,
        Some(BACKEND_A),
        &format!(
            "    releases: {{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: releases-b }} }}\n"
        ),
        "",
        "releases",
    )
    .await;
    let (code, v) = env
        .storage_patch(
            json!({ "buckets": { "downloads": { "backend": "local-disk", "alias": "releases" } } }),
        )
        .await;
    assert!(
        code >= 400 && v.to_string().contains("do not agree on its encryption"),
        "{code} {v}"
    );
    let (code, v) = env
        .storage_patch(json!({ "buckets": { "downloads": { "backend": "local-disk",
            "encryption": override_json(KB, "local-a") } } }))
        .await;
    assert!(
        code >= 400 && v.to_string().contains("key id 'local-a'"),
        "{code} {v}"
    );
}

// ── Scenario: native SSE per bucket (issue #116 P3) ─────────────────────

/// The SSE mode and the proxy form of a blob on the test MinIO.
async fn minio_sse(bucket: &str, stored: &str) -> (Option<String>, Option<String>, bool) {
    let head = common::minio_client()
        .await
        .head_object()
        .bucket(bucket)
        .key(stored)
        .send()
        .await
        .unwrap_or_else(|e| panic!("MinIO HEAD {bucket}/{stored}: {e:?}"));
    let meta = head.metadata().cloned().unwrap_or_default();
    (
        head.server_side_encryption()
            .map(|s| s.as_str().to_string()),
        head.ssekms_key_id().map(str::to_string),
        meta.contains_key("user-dg-encrypted"),
    )
}

/// One S3 backend (own mode SSE-S3) holds four buckets: `kms` (SSE-KMS),
/// `aes` (proxy AES), `plain` (`mode: none`) and `inherit` (no override).
/// Every request that stores bytes takes the SSE headers of ITS bucket:
/// PUT, the delta's reference and delta, a server-side copy between modes
/// and a multipart upload. Nothing is encrypted twice and nothing is
/// skipped. Needs the KMS-capable MinIO (key `dgp-test-key`).
#[tokio::test]
async fn native_sse_per_bucket_minio() {
    skip_unless_minio!();
    let names = ["sse-kms", "sse-aes", "sse-plain", "sse-inh"].map(common::unique_bucket);
    let [kms, aes, plain, inherit] = [&names[0], &names[1], &names[2], &names[3]];
    let buckets = format!(
        "    {kms}: {{ backend: local-disk, encryption: {{ mode: sse-kms, kms_key_id: \"arn:aws:kms:dgp-test-key\" }} }}\n    \
         {aes}: {{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: sse-aes-b }} }}\n    \
         {plain}: {{ backend: local-disk, encryption: {{ mode: none }} }}\n    \
         {inherit}: {{ backend: local-disk }}\n"
    );
    let env = spawn(
        Shape::NamedMinio,
        Some("{ mode: sse-s3 }"),
        &buckets,
        "",
        kms,
    )
    .await;
    for b in [aes, plain, inherit] {
        env.create(b).await;
    }
    let kms_form = (Some("aws:kms".to_string()), true, false);
    let form = |(sse, kid, proxy): (Option<String>, Option<String>, bool)| {
        (sse, kid.is_some_and(|k| k.ends_with("dgp-test-key")), proxy)
    };
    for b in [kms, aes, plain, inherit] {
        env.put(b, "x.txt", format!("body of {b}").as_bytes()).await;
        assert_eq!(env.get(b, "x.txt").await, format!("body of {b}").as_bytes());
    }
    assert_eq!(form(minio_sse(kms, "x.txt").await), kms_form);
    assert_eq!(
        form(minio_sse(aes, "x.txt").await),
        (None, false, true),
        "proxy AES and no SSE: never encrypted twice"
    );
    assert_eq!(form(minio_sse(plain, "x.txt").await), (None, false, false));
    assert_eq!(
        form(minio_sse(inherit, "x.txt").await),
        (Some("AES256".to_string()), false, false),
        "a bucket without an override takes the backend's SSE-S3"
    );

    // A delta: the reference and the delta take the bucket's SSE.
    let v1 = generate_binary(120_000, 11);
    let v2 = mutate_binary(&v1, 0.05);
    env.put(kms, "app.zip", &v1).await;
    env.put(kms, "app2.zip", &v2).await;
    assert_eq!(env.get(kms, "app2.zip").await, v2);
    assert_eq!(form(minio_sse(kms, "reference.bin").await), kms_form);
    assert_eq!(form(minio_sse(kms, "app2.zip.delta").await), kms_form);

    // Copies between modes write the DESTINATION's form.
    env.copy((kms, "x.txt"), (plain, "from-kms.txt")).await;
    env.copy((plain, "x.txt"), (kms, "from-plain.txt")).await;
    env.copy((aes, "x.txt"), (kms, "from-aes.txt")).await;
    env.copy((kms, "x.txt"), (aes, "from-kms.txt")).await;
    assert_eq!(
        form(minio_sse(plain, "from-kms.txt").await),
        (None, false, false)
    );
    assert_eq!(form(minio_sse(kms, "from-plain.txt").await), kms_form);
    assert_eq!(form(minio_sse(kms, "from-aes.txt").await), kms_form);
    assert_eq!(
        form(minio_sse(aes, "from-kms.txt").await),
        (None, false, true)
    );
    assert_eq!(
        env.get(plain, "from-kms.txt").await,
        format!("body of {kms}").as_bytes()
    );
    assert_eq!(
        env.get(kms, "from-aes.txt").await,
        format!("body of {aes}").as_bytes()
    );
    assert_eq!(
        env.get(aes, "from-kms.txt").await,
        format!("body of {kms}").as_bytes()
    );

    // A multipart upload: the native upload carries the bucket's SSE.
    let client = env.server.s3_client().await;
    let part1 = vec![7u8; 5 * 1024 * 1024];
    let part2 = b"tail".to_vec();
    let up = client
        .create_multipart_upload()
        .bucket(kms)
        .key("big.bin")
        .send()
        .await
        .unwrap();
    let id = up.upload_id().unwrap();
    let mut done = Vec::new();
    for (n, body) in [(1, part1.clone()), (2, part2.clone())] {
        let p = client
            .upload_part()
            .bucket(kms)
            .key("big.bin")
            .upload_id(id)
            .part_number(n)
            .body(ByteStream::from(body))
            .send()
            .await
            .unwrap();
        done.push(
            aws_sdk_s3::types::CompletedPart::builder()
                .part_number(n)
                .e_tag(p.e_tag().unwrap())
                .build(),
        );
    }
    client
        .complete_multipart_upload()
        .bucket(kms)
        .key("big.bin")
        .upload_id(id)
        .multipart_upload(
            aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .set_parts(Some(done))
                .build(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(form(minio_sse(kms, "big.bin").await), kms_form);
    let mut want = part1;
    want.extend_from_slice(&part2);
    assert_eq!(env.get(kms, "big.bin").await, want);
}

// ── Scenario: audit and confirmed downgrade (issue #116 P3) ─────────────

const KC: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const KD: &str = "4444444444444444444444444444444444444444444444444444444444444444";

/// The audit entries of `action`, newest first, as their targets.
async fn audit_targets(env: &Env, action: &str) -> Vec<String> {
    let v: serde_json::Value = env
        .admin
        .get(format!(
            "{}/_/api/admin/audit?limit=500",
            env.server.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = v
        .get("entries")
        .and_then(|e| e.as_array())
        .or_else(|| v.as_array())
        .cloned()
        .unwrap_or_default();
    entries
        .iter()
        .filter(|e| e["action"] == action)
        .map(|e| e["target"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Every change of a bucket's encryption is one audit entry (key ids,
/// never keys), and a change that makes the bucket store plaintext where
/// it encrypted needs its confirmation: the section PUT and the document
/// apply (through the CLI) refuse it without one and take it with one.
#[tokio::test]
async fn a_downgrade_needs_confirmation_and_every_change_is_audited() {
    let env = spawn(
        Shape::NamedFs,
        None,
        &format!(
            "    releases: {{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: releases-b }} }}\n    downloads: {{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KC}\", key_id: downloads-c }} }}\n"
        ),
        "",
        "releases",
    )
    .await;
    let endpoint = env.server.endpoint();

    // Section PUT: `mode: none` is refused without the header.
    let downgrade = json!({ "buckets": { "releases": { "encryption": { "mode": "none" } } } });
    let (code, v) = env.storage_patch(downgrade.clone()).await;
    assert!(
        code == 422
            && v.to_string()
                .contains("X-DGP-Confirm-Encryption-Downgrade: releases"),
        "{code} {v}"
    );
    env.put("releases", "still.txt", b"still encrypted").await;
    assert_eq!(
        env.at_rest("releases", "still.txt").await.as_deref(),
        Some("releases-b"),
        "the refused change changed nothing"
    );
    let (code, v) = env
        .storage_patch_with(
            downgrade,
            &[("X-DGP-Confirm-Encryption-Downgrade", "releases")],
        )
        .await;
    assert!(code == 200 && v["ok"] == true, "{code} {v}");
    env.put("releases", "plain.txt", b"now plaintext").await;
    assert_eq!(env.at_rest("releases", "plain.txt").await, None);
    let cleared = audit_targets(&env, "bucket_encryption_cleared").await;
    assert!(
        cleared
            .iter()
            .any(|t| t == "releases: aes256-gcm-proxy (key id 'releases-b') -> none"),
        "{cleared:?}"
    );

    // Back to encrypting needs no confirmation: `set`.
    env.storage_patch_ok(
        json!({ "buckets": { "releases": { "encryption": override_json(KA, "releases-a") } } }),
    )
    .await;
    let set = audit_targets(&env, "bucket_encryption_set").await;
    assert!(
        set.iter()
            .any(|t| t == "releases: none -> aes256-gcm-proxy (key id 'releases-a')"),
        "{set:?}"
    );
    // A rotation: `key_rotated`. The key moves to the legacy slot, which
    // drops the older legacy id `releases-b` (confirmed).
    let (code, v) = env
        .storage_patch_with(
            json!({ "buckets": { "releases": { "encryption": {
                "mode": "aes256-gcm-proxy", "key": KD, "key_id": "releases-d",
                "legacy_key": KA, "legacy_key_id": "releases-a" } } } }),
            &[("X-DGP-Confirm-Key-Removal", "releases-b")],
        )
        .await;
    assert!(code == 200 && v["ok"] == true, "{code} {v}");
    let rotated = audit_targets(&env, "bucket_encryption_key_rotated").await;
    assert!(
        rotated.iter().any(|t| t
            == "releases: aes256-gcm-proxy (key id 'releases-a') -> aes256-gcm-proxy (key id 'releases-d')"),
        "{rotated:?}"
    );

    // The document apply, through the CLI: `downloads` loses its block.
    let file = std::fs::read_to_string(env.server.config_path()).unwrap();
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&file).unwrap();
    doc["storage"]["buckets"]["downloads"]
        .as_mapping_mut()
        .unwrap()
        .remove("encryption");
    // The key stays readable as the backend's legacy key (the key gate).
    doc["storage"]["backends"][0]["encryption"] = serde_yaml::from_str(&format!(
        "{{ mode: none, legacy_key: \"{KC}\", legacy_key_id: downloads-c }}"
    ))
    .unwrap();
    let yaml_path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(yaml_path.path(), serde_yaml::to_string(&doc).unwrap()).unwrap();
    let cli = |confirm: &[&str]| {
        let mut args = vec![
            "config".to_string(),
            "apply".into(),
            yaml_path.path().display().to_string(),
            "--server".into(),
            endpoint.clone(),
        ];
        for c in confirm {
            args.push("--confirm-encryption-downgrade".into());
            args.push(c.to_string());
        }
        std::process::Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"))
            .current_dir(std::env::temp_dir())
            .args(args)
            .env("DGP_BOOTSTRAP_PASSWORD", common::TEST_BOOTSTRAP_PASSWORD)
            .output()
            .unwrap()
    };
    let refused = cli(&[]);
    let stderr = String::from_utf8_lossy(&refused.stderr).to_string();
    assert_eq!(refused.status.code(), Some(6), "{stderr}");
    assert!(
        stderr.contains("--confirm-encryption-downgrade downloads"),
        "{stderr}"
    );
    let applied = cli(&["downloads"]);
    assert_eq!(
        applied.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    env.put("downloads", "x.txt", b"plain").await;
    assert_eq!(env.at_rest("downloads", "x.txt").await, None);

    // No audit target ever holds key material.
    for action in [
        "bucket_encryption_set",
        "bucket_encryption_cleared",
        "bucket_encryption_key_rotated",
    ] {
        for t in audit_targets(&env, action).await {
            for k in [KA, KB, KC, KD] {
                assert!(!t.contains(k), "{action}: {t}");
            }
        }
    }
}

/// The migrate start judges the flip: a bucket that inherits an encrypting
/// backend and moves to a plaintext one needs the confirmation (400
/// without it); the flip then runs in the background, ungated.
#[tokio::test]
async fn a_migrate_onto_plaintext_needs_confirmation() {
    let env = spawn(
        Shape::NamedFs,
        Some(BACKEND_A),
        "    releases: { backend: local-disk }\n",
        "",
        "releases",
    )
    .await;
    env.put("releases", "x.txt", b"encrypted at the source")
        .await;
    let endpoint = env.server.endpoint();
    let start = |confirm: Option<&'static str>| {
        let mut req = env
            .admin
            .post(format!("{endpoint}/_/api/admin/buckets/releases/migrate"))
            .json(&json!({ "target_backend": "aws-dr", "delete_source": false }));
        if let Some(c) = confirm {
            req = req.header("X-DGP-Confirm-Encryption-Downgrade", c);
        }
        req.send()
    };
    let resp = start(None).await.unwrap();
    let code = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert!(
        code == 400 && body.contains("X-DGP-Confirm-Encryption-Downgrade: releases"),
        "{code} {body}"
    );
    let resp = start(Some("releases")).await.unwrap();
    assert!(resp.status().is_success(), "migrate: {}", resp.status());
    wait_job_done(&env.admin, &endpoint, "releases").await;
    env.put("releases", "after.txt", b"plaintext on aws-dr")
        .await;
    let dr = env.dr.as_ref().unwrap();
    assert_eq!(fs_at_rest(&fs_blob(dr, "releases", "after.txt")), None);
    assert_eq!(
        env.get("releases", "x.txt").await,
        b"encrypted at the source"
    );
    let cleared = audit_targets(&env, "bucket_encryption_cleared").await;
    assert!(
        cleared
            .iter()
            .any(|t| t.starts_with("releases: aes256-gcm-proxy")),
        "the background flip is audited: {cleared:?}"
    );
}

// ── Scenario: the multi-instance encryption witness (issue #116 P3) ─────

/// One instance of a two-instance deployment: the singleton S3 backend on
/// the test MinIO, the coordination bucket `coord`, and the data bucket
/// `data` with the bucket policy `policy` (a YAML flow map).
async fn ha_node(node: &str, coord: &str, data: &str, policy: &str) -> Env {
    ha_node_with(node, coord, data, policy, "").await
}

/// [`ha_node`] with extra `storage:` lines (2-space indented).
async fn ha_node_with(node: &str, coord: &str, data: &str, policy: &str, storage: &str) -> Env {
    ha_node_raw(
        node,
        coord,
        data,
        &format!("    {data}: {policy}\n"),
        storage,
    )
    .await
}

/// One instance whose `storage.buckets` is `buckets` (4-space indented
/// lines); `data` is the bucket the harness creates.
async fn ha_node_raw(node: &str, coord: &str, data: &str, buckets: &str, storage: &str) -> Env {
    let tmp = tempfile::TempDir::new().unwrap();
    let doc = format!(
        "access:\n  access_key_id: {}\n  secret_access_key: {}\n\
         storage:\n  backend:\n    type: s3\n    endpoint: \"{}\"\n    region: us-east-1\n    \
         force_path_style: true\n    access_key_id: {}\n    secret_access_key: {}\n{storage}  \
         buckets:\n{buckets}\
         advanced:\n  listen_addr: {}\n  bootstrap_password_hash: \"{}\"\n  config_sync_bucket: {coord}\n",
        AUTH.0,
        AUTH.1,
        common::minio_endpoint_url(),
        common::MINIO_ACCESS_KEY,
        common::MINIO_SECRET_KEY,
        common::LISTEN_ADDR_PLACEHOLDER,
        common::TEST_BOOTSTRAP_PASSWORD_HASH
    );
    let env = vec![
        (
            "DGP_CONFIG_DB_KEY".to_string(),
            common::TEST_CONFIG_DB_KEY.to_string(),
        ),
        ("DGP_ENCRYPTION_WITNESS_POLL_SECS".into(), "1".into()),
        ("DGP_NODE_ID".into(), node.to_string()),
    ];
    let server = TestServer::from_config_document(&doc, tmp, AUTH, data, env).await;
    let admin = admin_http_client(&server.endpoint()).await;
    Env {
        server,
        admin,
        store: Store::Minio,
        dr: None,
    }
}

/// `deltaglider_proxy witness publish <subjects> [--confirm…]` against `env`.
fn cli_publish(env: &Env, subjects: &[&str], confirm: &[&str]) -> std::process::Output {
    let mut args = vec!["witness".to_string(), "publish".to_string()];
    args.extend(subjects.iter().map(|s| s.to_string()));
    for c in confirm {
        args.push("--confirm-encryption-downgrade".into());
        args.push(c.to_string());
    }
    args.push("--server".into());
    args.push(env.server.endpoint());
    std::process::Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"))
        .current_dir(std::env::temp_dir())
        .args(args)
        .env("DGP_BOOTSTRAP_PASSWORD", common::TEST_BOOTSTRAP_PASSWORD)
        .output()
        .unwrap()
}

impl Env {
    /// A raw signed PUT: `(status, body)`. No SDK retry hides a 503.
    async fn raw_put(&self, bucket: &str, key: &str, body: &[u8]) -> (u16, String) {
        let resp = self
            .server
            .http()
            .put(format!("{}/{bucket}/{key}", self.server.endpoint()))
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        (
            resp.status().as_u16(),
            resp.text().await.unwrap_or_default(),
        )
    }

    /// The witness verdict from the admin API (`/_/ready` shows the status
    /// only, and no bucket name).
    async fn witness(&self) -> serde_json::Value {
        let ready = reqwest::get(format!("{}/_/ready", self.server.endpoint()))
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["encryption_witness"]
            .clone();
        let full: serde_json::Value = self
            .admin
            .get(format!(
                "{}/_/api/admin/encryption-witness",
                self.server.endpoint()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            ready
                .as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>()),
            Some(vec!["status".to_string()]),
            "/_/ready shows the status only: {ready}"
        );
        // The two answers can straddle a poll; the status of the full one
        // is what the caller judges.
        full
    }

    /// Wait until the witness verdict is `status` (the poll runs every 1 s).
    async fn wait_witness(&self, status: &str) -> serde_json::Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let w = self.witness().await;
            if w["status"] == status {
                return w;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "witness never became {status}: {w}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

/// Two instances share a coordination bucket. They agree → both write.
/// One disagrees (it stores the bucket in plaintext; it can still read the
/// ciphertext through its legacy key) → it refuses writes with a 503 that
/// names the mismatch, reads keep working, and `/_/ready` reports it while
/// the node stays ready. An apply of the same encryption on it converges.
/// A rotation applied on one instance publishes; the other refuses until
/// it applies the rotation too.
#[tokio::test]
async fn the_witness_makes_a_disagreeing_instance_refuse_writes_minio() {
    skip_unless_minio!();
    let coord = common::unique_bucket("enc-witness");
    common::minio_client()
        .await
        .create_bucket()
        .bucket(&coord)
        .send()
        .await
        .expect("create the coordination bucket");
    let data = common::unique_bucket("enc-data");
    let aes_b =
        format!("{{ encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: ha-b }} }}");
    let a = ha_node("node-a", &coord, &data, &aes_b).await;
    assert_eq!(a.wait_witness("agree").await["status"], "agree");
    let (code, body) = a.raw_put(&data, "a.txt", b"written by A").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(a.at_rest(&data, "a.txt").await.as_deref(), Some("ha-b"));

    // B stores the bucket in plaintext: it disagrees.
    let stale =
        format!("{{ encryption: {{ mode: none, legacy_key: \"{KB}\", legacy_key_id: ha-b }} }}");
    let b = ha_node("node-b", &coord, &data, &stale).await;
    let w = b.wait_witness("mismatch").await;
    assert_eq!(w["mismatches"][0]["bucket"], data.as_str(), "{w}");
    assert_eq!(w["mismatches"][0]["published"]["key_id"], "ha-b", "{w}");
    let ready = reqwest::get(format!("{}/_/ready", b.server.endpoint()))
        .await
        .unwrap();
    assert_eq!(
        ready.status().as_u16(),
        200,
        "a mismatch keeps the node ready"
    );
    let (code, body) = b.raw_put(&data, "b.txt", b"written by B").await;
    assert_eq!(code, 503, "{body}");
    assert!(
        body.contains("SlowDown") && body.contains("key id &apos;ha-b&apos;") && !body.contains(KB),
        "{body}"
    );
    assert_eq!(
        b.get(&data, "a.txt").await,
        b"written by A",
        "reads keep working"
    );
    // A is not affected.
    assert_eq!(a.raw_put(&data, "a2.txt", b"A again").await.0, 200);

    // Converge: B applies A's encryption.
    b.storage_patch_ok(
        json!({ "buckets": { data.as_str(): { "encryption": override_json(KB, "ha-b") } } }),
    )
    .await;
    b.wait_witness("agree").await;
    let (code, body) = b.raw_put(&data, "b.txt", b"written by B").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(b.at_rest(&data, "b.txt").await.as_deref(), Some("ha-b"));

    // A rotation applied on A publishes; B refuses until it applies it too.
    a.storage_patch_ok(
        json!({ "buckets": { data.as_str(): { "encryption": override_json(KC, "ha-c") } } }),
    )
    .await;
    assert_eq!(a.wait_witness("agree").await["status"], "agree");
    let w = b.wait_witness("mismatch").await;
    assert_eq!(w["mismatches"][0]["published"]["key_id"], "ha-c", "{w}");
    assert_eq!(b.raw_put(&data, "b2.txt", b"refused").await.0, 503);
    assert_eq!(a.raw_put(&data, "a3.txt", b"A rotated").await.0, 200);
    b.storage_patch_ok(
        json!({ "buckets": { data.as_str(): { "encryption": override_json(KC, "ha-c") } } }),
    )
    .await;
    b.wait_witness("agree").await;
    assert_eq!(b.raw_put(&data, "b2.txt", b"converged").await.0, 200);
    assert_eq!(b.at_rest(&data, "b2.txt").await.as_deref(), Some("ha-c"));
    assert_eq!(b.get(&data, "a3.txt").await, b"A rotated");

    // The manifest holds key ids only.
    let manifest = common::minio_client()
        .await
        .get_object()
        .bucket(&coord)
        .key("_dgp/encryption/intent.json")
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .to_vec();
    let text = String::from_utf8(manifest).unwrap();
    assert!(
        text.contains("ha-c") && !text.contains(KB) && !text.contains(KC),
        "{text}"
    );
}

/// A restart-based rollout (an env key, a ConfigMap): the instance that
/// published `ha-b` stops, and the new instance with `ha-c` keeps refusing
/// writes: there is no automatic takeover (second review, owner decision;
/// before, it took over once the old writer was gone). `witness publish`
/// converges, and its plaintext form needs the downgrade confirmation.
#[tokio::test]
async fn a_restart_rollout_converges_by_witness_publish_minio() {
    skip_unless_minio!();
    let coord = common::unique_bucket("enc-witness");
    common::minio_client()
        .await
        .create_bucket()
        .bucket(&coord)
        .send()
        .await
        .expect("create the coordination bucket");
    let data = common::unique_bucket("enc-data");
    let block = |key: &str, id: &str| {
        format!("{{ encryption: {{ mode: aes256-gcm-proxy, key: \"{key}\", key_id: {id}, legacy_key: \"{KB}\", legacy_key_id: ha-b }} }}")
    };
    let old =
        format!("{{ encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: ha-b }} }}");
    let a = ha_node("node-a", &coord, &data, &old).await;
    a.wait_witness("agree").await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let b = ha_node("node-b", &coord, &data, &block(KC, "ha-c")).await;
    b.wait_witness("mismatch").await;
    drop(a);
    // Well past any heartbeat: still refused, and the 503 names the command.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(b.witness().await["status"], "mismatch");
    let (code, body) = b.raw_put(&data, "x.txt", b"refused").await;
    assert_eq!(code, 503, "{body}");
    assert!(body.contains("witness publish"), "{body}");
    // The operator publishes B's encryption.
    let out = cli_publish(&b, &[&data], &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    b.wait_witness("agree").await;
    let (code, body) = b.raw_put(&data, "x.txt", b"after the rollout").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(b.at_rest(&data, "x.txt").await.as_deref(), Some("ha-c"));
    let published = audit_targets(&b, "bucket_encryption_witness_publish").await;
    assert!(
        published.iter().any(|t| t.contains("ha-c")),
        "{published:?}"
    );
    // A subject that names no storage is refused (exit 6).
    let out = cli_publish(&b, &["nope-not-a-bucket"], &[]);
    assert_eq!(out.status.code(), Some(6));
}

/// Review of P3 (probe-verified): a node that booted with `mode: none` on
/// a bucket that the other node encrypts by INHERITANCE (no block) was the
/// first writer of its entry, the encrypting node went mismatch, and the
/// downgrading node wrote plaintext. Now the inheriting node seeds the
/// entry, so the downgrading node is the one that refuses. The supported
/// path to finish a downgrade: an apply with the confirmation publishes it
/// even when the node's config does not change.
#[tokio::test]
async fn the_witness_never_lets_a_downgrade_win_by_boot_minio() {
    skip_unless_minio!();
    let coord = common::unique_bucket("enc-witness");
    common::minio_client()
        .await
        .create_bucket()
        .bucket(&coord)
        .send()
        .await
        .expect("create the coordination bucket");
    let data = common::unique_bucket("enc-data");
    // A: the bucket inherits an encrypting singleton backend.
    let a = ha_node_with(
        "node-a",
        &coord,
        &data,
        "{}",
        &format!(
            "  backend_encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: ha-b }}\n"
        ),
    )
    .await;
    a.wait_witness("agree").await;
    // B boots later with the bucket set to plaintext.
    let b = ha_node_with(
        "node-b",
        &coord,
        &data,
        &format!("{{ encryption: {{ mode: none, legacy_key: \"{KB}\", legacy_key_id: ha-b }} }}"),
        &format!(
            "  backend_encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: ha-b }}\n"
        ),
    )
    .await;
    b.wait_witness("mismatch").await;
    assert_eq!(b.raw_put(&data, "b.txt", b"plaintext?").await.0, 503);
    assert_eq!(a.witness().await["status"], "agree");
    assert_eq!(a.raw_put(&data, "a.txt", b"encrypted").await.0, 200);
    assert_eq!(a.at_rest(&data, "a.txt").await.as_deref(), Some("ha-b"));

    // The supported downgrade: an apply on B with the confirmation, and no
    // config change, publishes plaintext; A then refuses until it agrees.
    let (code, v) = b
        .storage_patch_with(
            json!({}),
            &[("X-DGP-Confirm-Encryption-Downgrade", data.as_str())],
        )
        .await;
    assert!(code == 200, "{code} {v}");
    b.wait_witness("agree").await;
    assert_eq!(b.raw_put(&data, "b.txt", b"plaintext").await.0, 200);
    a.wait_witness("mismatch").await;
    assert_eq!(a.raw_put(&data, "a2.txt", b"refused").await.0, 503);
}

/// Review of P3 (probe-verified): when the only up-to-date node stopped,
/// a lagging node took over and reverted a key rotation. A takeover needs
/// a config newer than the published entry: the lagging node's config is
/// older than the rotation, so it keeps refusing.
#[tokio::test]
async fn a_lagging_instance_never_reverts_a_rotation_minio() {
    skip_unless_minio!();
    let coord = common::unique_bucket("enc-witness");
    common::minio_client()
        .await
        .create_bucket()
        .bucket(&coord)
        .send()
        .await
        .expect("create the coordination bucket");
    let data = common::unique_bucket("enc-data");
    let old =
        format!("{{ encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: ha-b }} }}");
    let a = ha_node("node-a", &coord, &data, &old).await;
    a.wait_witness("agree").await;
    let b = ha_node("node-b", &coord, &data, &old).await;
    b.wait_witness("agree").await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    a.storage_patch_ok(
        json!({ "buckets": { data.as_str(): { "encryption": override_json(KC, "ha-c") } } }),
    )
    .await;
    b.wait_witness("mismatch").await;
    drop(a);
    // (1) An unrelated, engine-rebuilding apply on the lagging node.
    b.storage_patch_ok(json!({ "buckets": { "unrelated-bucket": { "quota_bytes": 1000 } } }))
        .await;
    // (2) The downgrade header with an empty patch republishes nothing:
    // B's intent encrypts.
    let (code, v) = b
        .storage_patch_with(
            json!({}),
            &[("X-DGP-Confirm-Encryption-Downgrade", data.as_str())],
        )
        .await;
    assert_eq!(code, 200, "{v}");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let w = b.witness().await;
    assert_eq!(w["status"], "mismatch", "{w}");
    assert_eq!(w["mismatches"][0]["published"]["key_id"], "ha-c", "{w}");
    assert_eq!(b.raw_put(&data, "x.txt", b"refused").await.0, 503);
    // (3) The lagging config restarted in a re-rendered (new) file.
    let c = ha_node("node-c", &coord, &data, &old).await;
    c.wait_witness("mismatch").await;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(c.witness().await["status"], "mismatch");
    assert_eq!(c.raw_put(&data, "y.txt", b"refused").await.0, 503);
}

/// Second review, finding 1 (probe-verified): A encrypts the bucket only
/// through the singleton's `backend_encryption` (the `*` entry; A does not
/// name the bucket). B boots with `buckets.<b>.encryption: {mode: none}`.
/// Both agreed and B wrote plaintext; now B is judged against `*` and
/// refuses.
#[tokio::test]
async fn a_bucket_block_downgrade_against_the_backends_star_refuses_minio() {
    skip_unless_minio!();
    let coord = common::unique_bucket("enc-witness");
    common::minio_client()
        .await
        .create_bucket()
        .bucket(&coord)
        .send()
        .await
        .expect("create the coordination bucket");
    let data = common::unique_bucket("enc-data");
    let backend_aes = format!(
        "  backend_encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: ha-b }}\n"
    );
    let a = ha_node_raw(
        "node-a",
        &coord,
        &data,
        "    other-bucket: {}\n",
        &backend_aes,
    )
    .await;
    a.wait_witness("agree").await;
    let b = ha_node_raw(
        "node-b",
        &coord,
        &data,
        &format!("    {data}: {{ encryption: {{ mode: none, legacy_key: \"{KB}\", legacy_key_id: ha-b }} }}\n"),
        &backend_aes,
    )
    .await;
    let w = b.wait_witness("mismatch").await;
    assert_eq!(w["mismatches"][0]["bucket"], data.as_str(), "{w}");
    assert_eq!(b.raw_put(&data, "x.txt", b"plaintext?").await.0, 503);
    assert_eq!(a.raw_put(&data, "x.txt", b"encrypted").await.0, 200);
}

// ── P2: the admin bucket list and the UI's request bodies ────────────────

/// The bucket list (`GET /_/api/admin/buckets`) of `bucket`.
async fn effective(env: &Env, bucket: &str) -> serde_json::Value {
    let v: serde_json::Value = env
        .admin
        .get(format!("{}/_/api/admin/buckets", env.server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !v.to_string().contains(KA) && !v.to_string().contains(KB),
        "the bucket list never carries a key: {v}"
    );
    v["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == bucket)
        .unwrap_or_else(|| panic!("{bucket} not listed: {v}"))["effective_encryption"]
        .clone()
}

/// Issue #116 P2: the bucket list reports each bucket's encryption as the
/// engine writes it (the bucket's own block, the backend that holds it), and
/// the request bodies of the bucket card's Encryption group do what the UI
/// says: a bucket key with a backend pin, Off (the key stays as legacy),
/// and the create dialog's policy written before the bucket exists.
#[tokio::test]
async fn the_bucket_list_reports_each_buckets_encryption() {
    let env = spawn(
        Shape::NamedFs,
        Some(BACKEND_A),
        "    releases: { backend: local-disk }\n",
        "",
        "releases",
    )
    .await;
    // A bucket without a route that lives on aws-dr only.
    std::fs::create_dir_all(env.dr.as_ref().unwrap().join("stray")).unwrap();
    env.create("downloads").await;

    let rel = effective(&env, "releases").await;
    assert_eq!(rel["source"], "backend", "{rel}");
    assert_eq!(rel["backend"], "local-disk", "{rel}");
    assert_eq!(rel["key_id"], "local-a", "{rel}");
    assert_eq!(rel["encrypts"], true, "{rel}");
    assert_eq!(rel["pinned"], true, "{rel}");
    let stray = effective(&env, "stray").await;
    assert_eq!(
        (&stray["backend"], &stray["mode"], &stray["pinned"]),
        (&json!("aws-dr"), &json!("none"), &json!(false)),
        "an unrouted bucket takes the encryption of the backend that holds it: {stray}"
    );
    // downloads has no route: the UI pins the backend that holds it.
    let dl = effective(&env, "downloads").await;
    assert_eq!(dl["pinned"], false, "{dl}");
    env.put("downloads", "old.txt", b"under local-a").await;

    // The Encryption group's "Encrypt with a bucket key" body.
    env.storage_patch_ok(json!({ "buckets": { "downloads": {
        "backend": "local-disk",
        "encryption": { "mode": "aes256-gcm-proxy", "key": KB, "key_id": "downloads-2026-10" }
    } } }))
    .await;
    let dl = effective(&env, "downloads").await;
    assert_eq!(
        (&dl["source"], &dl["key_id"], &dl["pinned"]),
        (&json!("bucket"), &json!("downloads-2026-10"), &json!(true)),
        "{dl}"
    );
    env.put("downloads", "new.txt", b"under the bucket key")
        .await;
    assert_eq!(
        env.at_rest("downloads", "new.txt").await.as_deref(),
        Some("downloads-2026-10")
    );
    assert_eq!(
        env.at_rest("downloads", "old.txt").await.as_deref(),
        Some("local-a")
    );

    // "Off for this bucket": plaintext writes, the key becomes legacy. The
    // UI sends the downgrade confirmation after its dialog (P3's gate).
    let resp = env
        .admin
        .put(format!(
            "{}/_/api/admin/config/section/storage",
            env.server.endpoint()
        ))
        .header("X-DGP-Confirm-Encryption-Downgrade", "downloads")
        .json(&json!({ "buckets": { "downloads": { "encryption": { "mode": "none" } } } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "{:?}", resp.text().await);
    let dl = effective(&env, "downloads").await;
    assert_eq!(
        (
            &dl["mode"],
            &dl["source"],
            &dl["legacy_key_id"],
            &dl["encrypts"]
        ),
        (
            &json!("none"),
            &json!("bucket"),
            &json!("downloads-2026-10"),
            &json!(false)
        ),
        "{dl}"
    );
    assert_eq!(dl["inherited"]["mode"], "aes256-gcm-proxy", "{dl}");
    assert_eq!(dl["reencrypt"], true, "{dl}");
    assert_eq!(
        env.get("downloads", "new.txt").await,
        b"under the bucket key"
    );

    // The create dialog: the bucket and its block in one request.
    let (code, body) = create_with_key(&env, "db-archive", "local-disk").await;
    assert_eq!(code, 200, "{body}");
    env.put("db-archive", "dump.sql", b"first object").await;
    assert_eq!(
        env.at_rest("db-archive", "dump.sql").await.as_deref(),
        Some("db-archive-2026-10"),
        "the first object of a new bucket is written with its own key"
    );
    let db = effective(&env, "db-archive").await;
    assert_eq!(db["source"], "bucket", "{db}");
}

/// The storage section's policy of `bucket` (`null` when it has none).
async fn policy(env: &Env, bucket: &str) -> serde_json::Value {
    let v: serde_json::Value = env
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
    v["buckets"][bucket].clone()
}

async fn create_with_key(env: &Env, bucket: &str, backend: &str) -> (u16, String) {
    let resp = env
        .admin
        .post(format!("{}/_/api/admin/buckets", env.server.endpoint()))
        .json(&json!({
            "name": bucket,
            "backend_name": backend,
            "encryption": override_json(KB, &format!("{bucket}-2026-10")),
        }))
        .send()
        .await
        .unwrap();
    (
        resp.status().as_u16(),
        resp.text().await.unwrap_or_default(),
    )
}

/// Review P2, defect 1 (the probe): the create dialog wrote the policy of
/// the typed name before POST /buckets. For an EXISTING bucket the create
/// then answered 409, but the route to another backend stayed, and the
/// bucket's objects were gone (NoSuchBucket). The create endpoint now
/// takes the block, and checks the name before it changes anything.
#[tokio::test]
async fn creating_an_existing_bucket_with_a_key_changes_nothing() {
    let env = spawn(Shape::NamedFs, None, "", "", "downloads").await;
    env.create("downloads").await;
    env.put("downloads", "old.txt", b"still here").await;
    let (code, body) = create_with_key(&env, "downloads", "aws-dr").await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(env.get("downloads", "old.txt").await, b"still here");
    assert!(
        policy(&env, "downloads").await.is_null(),
        "no policy was written"
    );
}

/// One request creates the bucket with its own key: the first object is
/// encrypted under it.
#[tokio::test]
async fn a_bucket_created_with_a_key_writes_its_first_object_under_it() {
    let env = spawn(Shape::NamedFs, None, "", "", "releases").await;
    let (code, body) = create_with_key(&env, "db-archive", "local-disk").await;
    assert_eq!(code, 200, "{body}");
    env.put("db-archive", "dump.sql", b"first object").await;
    assert_eq!(
        env.at_rest("db-archive", "dump.sql").await.as_deref(),
        Some("db-archive-2026-10")
    );
    assert_eq!(policy(&env, "db-archive").await["backend"], "local-disk");
}

/// A create that fails takes the route AND the key back: the bucket does
/// not exist, so no object can need the key (defect 2's gate).
#[tokio::test]
async fn a_failed_create_takes_the_key_back() {
    let env = spawn(Shape::NamedFs, None, "", "", "releases").await;
    // aws-dr cannot create the bucket: a FILE holds its name (the engine
    // still builds, so the create fails AFTER the config write).
    let dr = env.dr.clone().unwrap();
    std::fs::create_dir_all(&dr).unwrap();
    std::fs::write(dr.join("db-archive"), b"not a bucket").unwrap();
    let (code, body) = create_with_key(&env, "db-archive", "aws-dr").await;
    assert!(
        code >= 400 && !body.contains("rebuild engine"),
        "{code} {body}"
    );
    assert!(
        policy(&env, "db-archive").await.is_null(),
        "the failed create left a policy behind"
    );
}

/// Review P2, defect 2: settings with a key for a bucket that does not
/// exist (pre-provisioned, or left by an old failed create) can be removed
/// without a scan or a confirmation: no object can need the key.
#[tokio::test]
async fn settings_of_a_bucket_that_does_not_exist_can_be_removed() {
    let env = spawn(Shape::NamedFs, None, "", "", "releases").await;
    env.storage_patch_ok(json!({ "buckets": { "ghost": {
        "backend": "local-disk",
        "encryption": override_json(KB, "ghost-2026-10"),
    } } }))
    .await;
    // No key-removal confirmation; the removal of the block is a confirmed
    // downgrade (P3's gate), as the UI's "Remove these settings" sends it.
    let resp = env
        .admin
        .put(format!(
            "{}/_/api/admin/config/section/storage",
            env.server.endpoint()
        ))
        .header("X-DGP-Confirm-Encryption-Downgrade", "ghost")
        .json(&json!({ "buckets": { "ghost": null } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "{:?}", resp.text().await);
    assert!(policy(&env, "ghost").await.is_null());
    // An existing bucket keeps the gate.
    env.storage_patch_ok(json!({ "buckets": { "releases": {
        "backend": "local-disk",
        "encryption": override_json(KA, "releases-2026-10"),
    } } }))
    .await;
    let (code, v) = env
        .storage_patch(json!({ "buckets": { "releases": null } }))
        .await;
    assert!(
        code >= 400,
        "removing a key of an existing bucket: {code} {v}"
    );
}

/// Review P2, defect 5: the create dialog asks the server what a new
/// bucket would get on a backend, also for a name that already has
/// settings (pre-provisioned with its own block).
#[tokio::test]
async fn the_encryption_preview_of_a_new_bucket() {
    let env = spawn(
        Shape::NamedFs,
        Some(BACKEND_A),
        "    ghost: { backend: aws-dr, encryption: { mode: none } }\n",
        "",
        "releases",
    )
    .await;
    let preview = |bucket: &'static str, backend: &'static str| {
        let admin = env.admin.clone();
        let url = format!(
            "{}/_/api/admin/buckets/{bucket}/encryption-preview?backend={backend}",
            env.server.endpoint()
        );
        async move {
            let resp = admin.get(url).send().await.unwrap();
            (
                resp.status().as_u16(),
                resp.json::<serde_json::Value>().await.unwrap_or_default(),
            )
        }
    };
    let (code, v) = preview("db-archive", "local-disk").await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(
        (&v["mode"], &v["source"], &v["key_id"]),
        (
            &json!("aes256-gcm-proxy"),
            &json!("backend"),
            &json!("local-a")
        ),
        "{v}"
    );
    let (_, v) = preview("db-archive", "aws-dr").await;
    assert_eq!(
        (&v["mode"], &v["backend"]),
        (&json!("none"), &json!("aws-dr")),
        "{v}"
    );
    // A pre-provisioned name keeps its own block.
    let (_, v) = preview("ghost", "aws-dr").await;
    assert_eq!(
        (&v["mode"], &v["source"]),
        (&json!("none"), &json!("bucket")),
        "{v}"
    );
    let (code, _) = preview("db-archive", "gone").await;
    assert_eq!(code, 400);
}

// ── The witness at the library level, against MinIO (final review) ──────

mod witness_lib {
    use super::*;
    use deltaglider_proxy::config::Config;
    use deltaglider_proxy::coordination::EncryptionWitness;
    use deltaglider_proxy::storage::WitnessGate;
    use std::sync::Arc;

    fn cfg(data: &str, block: &str) -> Config {
        Config::from_yaml_str(&format!(
            "storage:\n  filesystem: /tmp/dgp-witness-lib\n  buckets:\n    {data}: {block}\n"
        ))
        .unwrap()
    }

    fn aes(key: &str, id: &str) -> String {
        format!("{{ encryption: {{ mode: aes256-gcm-proxy, key: \"{key}\", key_id: {id} }} }}")
    }

    async fn witness(
        bucket: &str,
        node: &str,
        cfg: &Config,
        cache: Option<&std::path::Path>,
    ) -> (EncryptionWitness, Arc<WitnessGate>) {
        let gate = Arc::new(WitnessGate::default());
        let w = EncryptionWitness::new(
            Some(common::minio_client().await),
            bucket.to_string(),
            node.to_string(),
            std::time::Duration::from_secs(30),
            gate.clone(),
            cfg,
            cache,
        );
        (w, gate)
    }

    async fn new_bucket(prefix: &str) -> String {
        let b = common::unique_bucket(prefix);
        common::minio_client()
            .await
            .create_bucket()
            .bucket(&b)
            .send()
            .await
            .unwrap();
        b
    }

    async fn manifest_text(bucket: &str) -> String {
        let out = common::minio_client()
            .await
            .get_object()
            .bucket(bucket)
            .key("_dgp/encryption/intent.json")
            .send()
            .await
            .unwrap();
        String::from_utf8(out.body.collect().await.unwrap().to_vec()).unwrap()
    }

    /// Final review, follow-up 4: a node with a lagging PLAINTEXT config
    /// that starts while the coordination bucket does not answer (here: it
    /// is missing, which was read as "no manifest") witnessed nothing and
    /// wrote plaintext into a bucket that the others encrypt. Now a missing
    /// bucket is an error, and the node judges against the manifest that it
    /// saved at its last good sync: the bucket refuses writes.
    #[tokio::test]
    async fn a_node_that_starts_during_an_outage_judges_its_saved_manifest_minio() {
        skip_unless_minio!();
        let coord = new_bucket("enc-wlib").await;
        let data = common::unique_bucket("enc-data");
        let (enc, plain) = (cfg(&data, &aes(KB, "lib-b")), cfg(&data, "{}"));
        let (wa, _) = witness(&coord, "node-a", &enc, None).await;
        wa.sync(&enc).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (wb, _) = witness(&coord, "node-b", &plain, Some(dir.path())).await;
        wb.sync(&plain).await.unwrap();
        assert_eq!(wb.status().status, "mismatch");
        let saved = dir.path().join("encryption-witness.json");
        assert!(saved.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // B restarts; its coordination bucket is missing.
        let missing = common::unique_bucket("enc-missing");
        let (wb2, gate) = witness(&missing, "node-b", &plain, Some(dir.path())).await;
        let err = wb2
            .sync(&plain)
            .await
            .expect_err("NoSuchBucket is an error");
        assert!(err.contains("NoSuchBucket"), "{err}");
        assert_eq!(wb2.status().status, "unknown");
        assert!(
            gate.refusal("default", &data).is_some(),
            "the saved manifest still covers the bucket"
        );
    }

    /// Final review, follow-up 5: a publish that failed during an apply was
    /// never retried (a sync writes only absent entries), so the applying
    /// node refused its own change forever. The failed publish is kept and
    /// retried at the next sync.
    #[tokio::test]
    async fn a_failed_apply_publish_is_retried_at_the_next_sync_minio() {
        skip_unless_minio!();
        let coord = new_bucket("enc-wlib").await;
        let data = common::unique_bucket("enc-data");
        let (old, new) = (cfg(&data, &aes(KB, "lib-b")), cfg(&data, &aes(KC, "lib-c")));
        let (w, gate) = witness(&coord, "node-a", &old, None).await;
        w.sync(&old).await.unwrap();
        let before = manifest_text(&coord).await;
        assert!(before.contains("lib-b"), "{before}");
        // The coordination bucket goes away for the apply.
        let s3 = common::minio_client().await;
        s3.delete_object()
            .bucket(&coord)
            .key("_dgp/encryption/intent.json")
            .send()
            .await
            .unwrap();
        s3.delete_bucket().bucket(&coord).send().await.unwrap();
        assert!(w.publish_changes(&old, &new, &[]).await.is_err());
        // It comes back with the old manifest.
        s3.create_bucket().bucket(&coord).send().await.unwrap();
        s3.put_object()
            .bucket(&coord)
            .key("_dgp/encryption/intent.json")
            .body(ByteStream::from(before.into_bytes()))
            .send()
            .await
            .unwrap();
        w.sync(&new).await.unwrap();
        assert_eq!(w.status().status, "agree", "{:?}", w.status());
        assert!(manifest_text(&coord).await.contains("lib-c"));
        assert_eq!(gate.refusal("default", &data), None);
    }

    /// Final review, follow-up 6 (probe): the rollback of a failed create
    /// (a restoring transition) ran the apply's publish from the config
    /// with the block to the one without: it published PLAINTEXT with no
    /// confirmation. A rollback restores what the failed change published:
    /// a new entry is removed, a replaced one is put back.
    #[tokio::test]
    async fn a_rollback_restores_what_the_failed_change_published_minio() {
        skip_unless_minio!();
        let coord = new_bucket("enc-wlib").await;
        let data = common::unique_bucket("enc-data");
        let (without, with_block) = (cfg(&data, "{}"), cfg(&data, &aes(KB, "lib-b")));
        let (w, _) = witness(&coord, "node-a", &without, None).await;
        w.sync(&without).await.unwrap();
        w.publish_changes(&without, &with_block, &[]).await.unwrap();
        assert!(manifest_text(&coord).await.contains("lib-b"));
        w.restore_last_publish(&without).await.unwrap();
        let after = manifest_text(&coord).await;
        assert!(!after.contains(&data), "the new entry is removed: {after}");
        assert_eq!(w.status().status, "agree");
        // A replaced entry is put back.
        let rotated = cfg(&data, &aes(KC, "lib-c"));
        w.publish_changes(&without, &with_block, &[]).await.unwrap();
        w.publish_changes(&with_block, &rotated, &[]).await.unwrap();
        assert!(manifest_text(&coord).await.contains("lib-c"));
        w.restore_last_publish(&with_block).await.unwrap();
        let after = manifest_text(&coord).await;
        assert!(
            after.contains("lib-b") && !after.contains("lib-c"),
            "{after}"
        );
    }
}

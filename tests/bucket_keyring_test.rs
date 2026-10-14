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

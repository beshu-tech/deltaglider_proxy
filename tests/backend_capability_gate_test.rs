// SPDX-License-Identifier: BUSL-1.1

//! Guard B integration tests: the startup backend write-capability gate and
//! the hot-apply pre-commit gate. A real non-CAS backend can't exist in the
//! MinIO-only harness, so the documented `DGP_TEST_FORCE_NONCAS_BACKEND` seam
//! forces the verdict — the gates' decision + observability (doc-linked FATAL,
//! apply rejection) are what these tests prove.

use crate::common;

use common::{admin_http_client, TestServer};

/// The named-backend fragment shared by every case. The b2sim endpoint is
/// never contacted: the forced verdict short-circuits the probe. `local-disk`
/// comes FIRST, so it is the default backend: unrouted buckets and the
/// coordination bucket resolve to a filesystem backend (sync degrades to a
/// warning) and only the capability gate is under test.
fn b2sim_yaml(local: &std::path::Path) -> String {
    format!(
        r#"backends:
  - name: local-disk
    type: filesystem
    path: "{}"
  - name: b2sim
    type: s3
    endpoint: "http://127.0.0.1:1"
    region: "us-east-1"
    force_path_style: true
    access_key_id: "x"
    secret_access_key: "y"
"#,
        local.display()
    )
}

/// Spawn the proxy binary directly with a config that must FAIL boot, and
/// return (exit_ok, combined_output). TestServer can't be used here — it
/// panics when the child exits before ready.
fn spawn_expect_exit(config: &str) -> (std::process::ExitStatus, String) {
    spawn_expect_exit_with_db_key(config, Some(common::TEST_CONFIG_DB_KEY))
}

/// `spawn_expect_exit` with a chosen `DGP_CONFIG_DB_KEY`. Every config here
/// sets a sync bucket, and a sync bucket without the key is its own fatal
/// error (S8), which boot reports before the capability gate: the gate
/// tests pass the key so that the gate is what they see.
fn spawn_expect_exit_with_db_key(
    config: &str,
    db_key: Option<&str>,
) -> (std::process::ExitStatus, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("test.yaml");
    std::fs::write(&config_path, config).expect("write config");
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"));
    cmd.env_remove("DGP_CONFIG_DB_KEY");
    if let Some(k) = db_key {
        cmd.env("DGP_CONFIG_DB_KEY", k);
    }
    let out = cmd
        .env("DGP_CONFIG", &config_path)
        .env("RUST_LOG", "deltaglider_proxy=info")
        .env("DGP_TEST_FORCE_NONCAS_BACKEND", "b2sim")
        .env("DGP_BACKEND_ALLOW_LOCAL", "true")
        .env_remove("DGP_BOOTSTRAP_PASSWORD_HASH")
        .output()
        .expect("spawn proxy binary");
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status, combined)
}

#[test]
fn test_noncas_backend_with_client_writable_bucket_fails_boot() {
    let dir = tempfile::tempdir().expect("data dir");
    let config = format!(
        "listen_addr: \"127.0.0.1:0\"\n\
         access_key_id: \"k\"\n\
         secret_access_key: \"s\"\n\
         config_sync_bucket: \"dgp-sync\"\n\
         backend:\n  type: filesystem\n  path: \"{}\"\n\
         {}\
         buckets:\n  mirror:\n    backend: b2sim\n",
        dir.path().display(),
        b2sim_yaml(&dir.path().join("local"))
    );
    let (status, output) = spawn_expect_exit(&config);
    assert!(
        !status.success(),
        "boot must FAIL with a client-writable bucket on a non-CAS backend, output:\n{output}"
    );
    assert!(
        output.contains("FATAL") && output.contains("does not support conditional writes"),
        "FATAL line must name the failure, output:\n{output}"
    );
    assert!(
        output.contains("b2sim") && output.contains("mirror"),
        "FATAL line must name the backend and bucket, output:\n{output}"
    );
    assert!(
        output.contains("replication targets only")
            && output.contains("deltaglider.com/docs/how-to/backend-capability-validation"),
        "FATAL line must state both fixes + the doc link, output:\n{output}"
    );
}

/// S8: a sync bucket without DGP_CONFIG_DB_KEY fails boot with a FATAL line
/// that names the variable (every instance must share the key).
#[test]
fn test_sync_bucket_without_config_db_key_fails_boot() {
    let dir = tempfile::tempdir().expect("data dir");
    let config = format!(
        "listen_addr: \"127.0.0.1:0\"\n\
         access_key_id: \"k\"\n\
         secret_access_key: \"s\"\n\
         config_sync_bucket: \"dgp-sync\"\n\
         backend:\n  type: filesystem\n  path: \"{}\"\n",
        dir.path().display(),
    );
    let (status, output) = spawn_expect_exit_with_db_key(&config, None);
    assert!(!status.success(), "boot must FAIL, output:\n{output}");
    assert!(
        output.contains("FATAL") && output.contains("DGP_CONFIG_DB_KEY"),
        "FATAL line must name DGP_CONFIG_DB_KEY, output:\n{output}"
    );
}

#[tokio::test]
async fn test_single_instance_and_marked_bucket_boot_fine() {
    let local = tempfile::tempdir().expect("local-disk dir");
    // (a) Single instance (no config_sync_bucket): the gate skips entirely —
    //     the same forced-non-CAS backend + routed bucket boots.
    let server = TestServer::builder()
        .auth("k", "s")
        .bucket_policy("mirror", "backend: b2sim")
        .extra_yaml_root(&b2sim_yaml(local.path()))
        .env("DGP_TEST_FORCE_NONCAS_BACKEND", "b2sim")
        .env("DGP_BACKEND_ALLOW_LOCAL", "true")
        .build()
        .await;
    drop(server);

    // (b) Multi-instance BUT the bucket is replication_target_only: no client
    //     writers → exempt → boots. (config_sync on a filesystem singleton
    //     degrades to a warning; only the capability gate is under test.)
    let server = TestServer::builder()
        .auth("k", "s")
        .config_sync_bucket("dgp-sync")
        .bucket_policy("mirror", "backend: b2sim\nreplication_target_only: true")
        .extra_yaml_root(&b2sim_yaml(local.path()))
        .env("DGP_TEST_FORCE_NONCAS_BACKEND", "b2sim")
        .env("DGP_BACKEND_ALLOW_LOCAL", "true")
        .build()
        .await;
    drop(server);
}

#[tokio::test]
async fn test_hot_apply_rejects_routing_client_writable_bucket_to_noncas_backend() {
    let local = tempfile::tempdir().expect("local-disk dir");
    // Boot single-instance (gate skipped), then try to APPLY a config that
    // turns on multi-instance with the client-writable bucket still routed to
    // the forced-non-CAS backend → the pre-commit gate must refuse.
    let server = TestServer::builder()
        .auth("k", "s")
        .bucket_policy("mirror", "backend: b2sim")
        .extra_yaml_root(&b2sim_yaml(local.path()))
        .env("DGP_TEST_FORCE_NONCAS_BACKEND", "b2sim")
        .env("DGP_BACKEND_ALLOW_LOCAL", "true")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Export the current document, add config_sync_bucket under advanced,
    // re-apply. Edited via serde_yaml so section nesting stays correct.
    let current = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .expect("export")
        .text()
        .await
        .unwrap();
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&current).expect("export parses");
    doc["advanced"]["config_sync_bucket"] = "dgp-sync".into();
    let modified = serde_yaml::to_string(&doc).unwrap();

    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&serde_json::json!({ "yaml": modified }))
        .send()
        .await
        .expect("apply");
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert_ne!(status, 200, "apply must be refused, got {status}: {body}");
    assert!(
        body.contains("does not support conditional writes")
            && body.contains("backend-capability-validation"),
        "rejection must be doc-linked and name the cause, got: {body}"
    );

    // The marked variant of the same transition is ACCEPTED: mark the bucket
    // replication_target_only and the gate exempts it.
    doc["storage"]["buckets"]["mirror"]["replication_target_only"] = true.into();
    let marked = serde_yaml::to_string(&doc).unwrap();
    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&serde_json::json!({ "yaml": marked }))
        .send()
        .await
        .expect("apply marked");
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert_eq!(
        status, 200,
        "marked bucket must make the same transition acceptable, got {status}: {body}"
    );
}

/// `POST /_/api/admin/buckets` routes a NEW bucket onto a backend. Under
/// multi-instance it must pass the same capability gate as a config apply,
/// or a client-writable bucket lands on a non-CAS backend and the next boot
/// exit(1)s on the persisted config.
#[tokio::test]
async fn test_create_bucket_on_noncas_backend_is_refused_multi_instance() {
    let local = tempfile::tempdir().expect("local-disk dir");
    let server = TestServer::builder()
        .auth("k", "s")
        .config_sync_bucket("dgp-sync")
        .extra_yaml_root(&b2sim_yaml(local.path()))
        .env("DGP_TEST_FORCE_NONCAS_BACKEND", "b2sim")
        .env("DGP_BACKEND_ALLOW_LOCAL", "true")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!("{}/_/api/admin/buckets", server.endpoint()))
        .json(&serde_json::json!({ "name": "downloads", "backend_name": "b2sim" }))
        .send()
        .await
        .expect("create bucket");
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert_ne!(status, 200, "create must be refused, got {status}: {body}");
    assert!(
        body.contains("does not support conditional writes")
            && body.contains("backend-capability-validation"),
        "refusal must name the cause, got: {body}"
    );
    // Nothing was routed: the export carries no `downloads` policy.
    let export = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .expect("export")
        .text()
        .await
        .unwrap();
    assert!(
        !export.contains("downloads"),
        "route must roll back: {export}"
    );
}

/// Buckets WITHOUT a policy land on the default backend, and clients can
/// write them. A non-CAS default under multi-instance must fail boot even
/// when no bucket policy routes there.
#[test]
fn test_noncas_default_backend_without_policies_fails_boot() {
    let dir = tempfile::tempdir().expect("data dir");
    let config = format!(
        "listen_addr: \"127.0.0.1:0\"\n\
         access_key_id: \"k\"\n\
         secret_access_key: \"s\"\n\
         config_sync_bucket: \"dgp-sync\"\n\
         backend:\n  type: filesystem\n  path: \"{}\"\n\
         backends:\n  - name: b2sim\n    type: s3\n    endpoint: \"http://127.0.0.1:1\"\n    \
         region: \"us-east-1\"\n    force_path_style: true\n    access_key_id: \"x\"\n    \
         secret_access_key: \"y\"\n  - name: local-disk\n    type: filesystem\n    \
         path: \"{}\"\n",
        dir.path().display(),
        dir.path().join("local").display()
    );
    let (status, output) = spawn_expect_exit(&config);
    assert!(
        !status.success(),
        "boot must FAIL with a non-CAS default backend, output:\n{output}"
    );
    assert!(
        output.contains("FATAL")
            && output.contains("does not support conditional writes")
            && output.contains("b2sim"),
        "FATAL line must name the backend, output:\n{output}"
    );
}

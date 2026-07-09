// SPDX-License-Identifier: GPL-3.0-only

//! Guard B integration tests: the startup backend write-capability gate and
//! the hot-apply pre-commit gate. A real non-CAS backend can't exist in the
//! MinIO-only harness, so the documented `DGP_TEST_FORCE_NONCAS_BACKEND` seam
//! forces the verdict — the gates' decision + observability (doc-linked FATAL,
//! apply rejection) are what these tests prove.

mod common;

use common::{admin_http_client, TestServer};

/// The named-backend + routed-bucket fragment shared by every case. The
/// endpoint is never contacted: the forced verdict short-circuits the probe.
const B2SIM_YAML: &str = r#"backends:
  - name: b2sim
    type: s3
    endpoint: "http://127.0.0.1:1"
    region: "us-east-1"
    force_path_style: true
    access_key_id: "x"
    secret_access_key: "y"
"#;

/// Spawn the proxy binary directly with a config that must FAIL boot, and
/// return (exit_ok, combined_output). TestServer can't be used here — it
/// panics when the child exits before ready.
fn spawn_expect_exit(config: &str) -> (std::process::ExitStatus, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("test.yaml");
    std::fs::write(&config_path, config).expect("write config");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"))
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
         {B2SIM_YAML}\
         buckets:\n  mirror:\n    backend: b2sim\n",
        dir.path().display()
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

#[tokio::test]
async fn test_single_instance_and_marked_bucket_boot_fine() {
    // (a) Single instance (no config_sync_bucket): the gate skips entirely —
    //     the same forced-non-CAS backend + routed bucket boots.
    let server = TestServer::builder()
        .auth("k", "s")
        .bucket_policy("mirror", "backend: b2sim")
        .extra_yaml_root(B2SIM_YAML)
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
        .extra_yaml_root(B2SIM_YAML)
        .env("DGP_TEST_FORCE_NONCAS_BACKEND", "b2sim")
        .env("DGP_BACKEND_ALLOW_LOCAL", "true")
        .build()
        .await;
    drop(server);
}

#[tokio::test]
async fn test_hot_apply_rejects_routing_client_writable_bucket_to_noncas_backend() {
    // Boot single-instance (gate skipped), then try to APPLY a config that
    // turns on multi-instance with the client-writable bucket still routed to
    // the forced-non-CAS backend → the pre-commit gate must refuse.
    let server = TestServer::builder()
        .auth("k", "s")
        .bucket_policy("mirror", "backend: b2sim")
        .extra_yaml_root(B2SIM_YAML)
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

//! Integration tests for the Phase 4 admin-API CLI wrappers
//! (`config apply`, `admission trace`).
//!
//! Each test spawns a real `TestServer`, invokes the CLI binary via
//! `CARGO_BIN_EXE_deltaglider_proxy`, and asserts exit code + output.
//! Purpose is to cover the wire-level behaviour: env-var discipline,
//! cookie-based session flow, error mapping → exit codes.

mod common;

use common::TestServer;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_deltaglider_proxy");

/// Known-good bcrypt hash for plaintext "dgptest". Verified via the
/// bcrypt crate; the test harness threads it into the server via
/// `TestServer` (whose builder sets the hash file directly).
///
/// We don't go through the env var because `--config` skips
/// `apply_env_overrides` — that's a pre-existing Config loader quirk
/// outside this CLI's scope. The TestServer harness writes the hash to
/// the bootstrap-hash file directly which the server honours regardless
/// of load path.
const TEST_PASSWORD: &str = common::TEST_BOOTSTRAP_PASSWORD;

#[tokio::test]
async fn test_config_apply_happy_path() {
    let server = TestServer::builder()
        .auth("CLIAPPLYK", "CLIAPPLYS")
        .max_delta_ratio(0.75)
        .build()
        .await;
    let endpoint = server.endpoint();

    // Get a canonical YAML, flip max_delta_ratio, apply back.
    let exported = {
        let admin = common::admin_http_client(&endpoint).await;
        admin
            .get(format!("{}/_/api/admin/config/export", endpoint))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    };
    let modified = exported
        .lines()
        .map(|line| {
            if line.starts_with("max_delta_ratio:") {
                "max_delta_ratio: 0.25".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let yaml_path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(yaml_path.path(), modified).unwrap();

    let output = Command::new(BIN)
        .args([
            "config",
            "apply",
            yaml_path.path().to_str().unwrap(),
            "--server",
            &endpoint,
        ])
        .env("DGP_BOOTSTRAP_PASSWORD", TEST_PASSWORD)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("applied: yes"),
        "expected 'applied: yes' in stderr, got: {stderr}"
    );
}

#[tokio::test]
async fn test_config_apply_missing_password_exits_7() {
    // With DGP_BOOTSTRAP_PASSWORD unset, the CLI must refuse to run and
    // return the documented EXIT_AUTH (7). Never try to connect, never
    // leak anything in argv.
    let yaml_path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        yaml_path.path(),
        "listen_addr: \"127.0.0.1:9999\"\nbackend:\n  type: filesystem\n  path: /tmp/dgp-x\n",
    )
    .unwrap();

    let output = Command::new(BIN)
        .args(["config", "apply", yaml_path.path().to_str().unwrap()])
        // Explicitly clear the env var; rustc inherits the parent environment
        // so a shell export could otherwise leak in.
        .env_remove("DGP_BOOTSTRAP_PASSWORD")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("DGP_BOOTSTRAP_PASSWORD"),
        "expected env-var hint in stderr, got: {stderr}"
    );
}

#[tokio::test]
async fn test_config_apply_wrong_password_exits_7() {
    let server = TestServer::builder()
        .auth("CLIWRONGK", "CLIWRONGS")
        .build()
        .await;

    let yaml_path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(yaml_path.path(), "listen_addr: \"127.0.0.1:9999\"\n").unwrap();

    let output = Command::new(BIN)
        .args([
            "config",
            "apply",
            yaml_path.path().to_str().unwrap(),
            "--server",
            &server.endpoint(),
        ])
        .env("DGP_BOOTSTRAP_PASSWORD", "definitely-not-the-password")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
}

#[tokio::test]
async fn test_config_apply_empty_file_rejected_before_hitting_server() {
    // Empty YAML would deserialize to Config::default() on the server and
    // silently reset every field. The CLI catches this pre-flight so the
    // server never even sees the request.
    let yaml_path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(yaml_path.path(), "   \n\n   \n").unwrap();

    let output = Command::new(BIN)
        .args(["config", "apply", yaml_path.path().to_str().unwrap()])
        .env("DGP_BOOTSTRAP_PASSWORD", TEST_PASSWORD)
        .output()
        .unwrap();
    // We never reach the server (whose URL we didn't even provide), so
    // the error comes from the pre-flight empty-body check in
    // apply_async. Exit code 6 = REJECTED (server-rejected semantics;
    // used here because the guard models the same contract — "the
    // request was not honoured").
    assert_eq!(output.status.code(), Some(6));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("empty"),
        "expected 'empty' in stderr, got: {stderr}"
    );
}

#[tokio::test]
async fn test_admission_trace_smoke() {
    let server = TestServer::builder()
        .auth("CLITRACEK", "CLITRACES")
        .build()
        .await;

    let output = Command::new(BIN)
        .args([
            "admission",
            "trace",
            "--method",
            "GET",
            "--path",
            "/some-bucket/some-key",
            "--server",
            &server.endpoint(),
        ])
        .env("DGP_BOOTSTRAP_PASSWORD", TEST_PASSWORD)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output is JSON — parse it to verify shape.
    let body: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(body["admission"]["decision"], "continue");
    assert_eq!(body["resolved"]["method"], "GET");
    assert_eq!(body["resolved"]["bucket"], "some-bucket");
}

#[tokio::test]
async fn test_admission_trace_matches_public_prefix_after_apply() {
    // End-to-end dogfood: use `config apply` to install a public prefix,
    // then use `admission trace` to verify the chain rebuilt and the
    // synthesised block is live. Both subcommands under test, one
    // running against the other's side effects.
    let server = TestServer::builder()
        .auth("CLIDOGK", "CLIDOGS")
        .build()
        .await;
    let endpoint = server.endpoint();

    // 1. Export current config.
    let exported = {
        let admin = common::admin_http_client(&endpoint).await;
        admin
            .get(format!("{}/_/api/admin/config/export", endpoint))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    };
    // 2. Patch `buckets` to add a public-prefixed entry. Doing this via
    //    YAML-parse-and-reserialize (rather than string append) because
    //    the exported doc may already carry a `buckets:` key — a
    //    duplicate top-level key would fail to parse.
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&exported).unwrap();
    let buckets = doc
        .as_mapping_mut()
        .unwrap()
        .entry(serde_yaml::Value::String("buckets".into()))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let buckets_map = buckets.as_mapping_mut().expect("buckets must be a mapping");
    let mut policy = serde_yaml::Mapping::new();
    policy.insert(
        serde_yaml::Value::String("public_prefixes".into()),
        serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("open/".into())]),
    );
    buckets_map.insert(
        serde_yaml::Value::String("dogfood-bucket".into()),
        serde_yaml::Value::Mapping(policy),
    );
    let patched = serde_yaml::to_string(&doc).unwrap();

    let yaml_path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(yaml_path.path(), patched).unwrap();

    let apply = Command::new(BIN)
        .args([
            "config",
            "apply",
            yaml_path.path().to_str().unwrap(),
            "--server",
            &endpoint,
        ])
        .env("DGP_BOOTSTRAP_PASSWORD", TEST_PASSWORD)
        .output()
        .unwrap();
    assert_eq!(
        apply.status.code(),
        Some(0),
        "apply failed: {}",
        String::from_utf8_lossy(&apply.stderr)
    );

    // 3. Trace an anonymous GET on the public path. Must now be
    //    `allow-anonymous` with the synthesised block matched.
    let trace = Command::new(BIN)
        .args([
            "admission",
            "trace",
            "--method",
            "GET",
            "--path",
            "/dogfood-bucket/open/file.zip",
            "--server",
            &endpoint,
        ])
        .env("DGP_BOOTSTRAP_PASSWORD", TEST_PASSWORD)
        .output()
        .unwrap();
    assert_eq!(trace.status.code(), Some(0));
    let body: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&trace.stdout))
        .expect("trace stdout must be JSON");
    assert_eq!(body["admission"]["decision"], "allow-anonymous");
    assert_eq!(body["admission"]["matched"], "public-prefix:dogfood-bucket");
}

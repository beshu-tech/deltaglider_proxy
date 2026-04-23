//! Shared test infrastructure for integration tests
//!
//! Provides TestServer (filesystem and S3 backends), data generators,
//! and MinIO availability gating.

#![allow(dead_code)]

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::Client;
use rand::{Rng, SeedableRng};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

/// Port counter to avoid conflicts between tests.
/// Single port per server (UI served under /_/ on the same port).
static PORT_COUNTER: AtomicU16 = AtomicU16::new(19000);

/// Known bootstrap password used by all test servers.
/// The hash is bcrypt($2b$04$) of "testpass" with a low cost factor for speed.
pub const TEST_BOOTSTRAP_PASSWORD: &str = "testpass";

/// MinIO configuration constants
pub const MINIO_BUCKET: &str = "deltaglider-test";

/// MinIO endpoint — reads MINIO_ENDPOINT env var, falls back to localhost:9000
pub fn minio_endpoint_url() -> String {
    std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".to_string())
}
pub const MINIO_ACCESS_KEY: &str = "minioadmin";
pub const MINIO_SECRET_KEY: &str = "minioadmin";

/// Test server wrapper that spawns a real deltaglider_proxy binary
pub struct TestServer {
    process: Child,
    port: u16,
    _data_dir: Option<TempDir>,
    bucket: String,
    /// Auth credentials for the test server (None = open access).
    auth_creds: Option<(String, String)>,
    /// Absolute path of the config file the server was spawned with (via
    /// `DGP_CONFIG`). Exposed so tests can verify that admin-API config
    /// mutations persist to this specific file rather than to a
    /// CWD-relative default.
    config_path: std::path::PathBuf,
}

impl TestServer {
    // ── Builder ──

    /// Returns a builder for configuring and spawning a test server.
    pub fn builder() -> TestServerBuilder {
        TestServerBuilder::default()
    }

    // ── Convenience factory methods (delegate to builder) ──

    /// Start a test server with filesystem backend (no Docker needed)
    pub async fn filesystem() -> Self {
        Self::builder().build().await
    }

    /// Start a test server with filesystem backend and a custom max delta ratio
    pub async fn filesystem_with_max_delta_ratio(max_delta_ratio: f32) -> Self {
        Self::builder()
            .max_delta_ratio(max_delta_ratio)
            .build()
            .await
    }

    /// Start a test server with filesystem backend and a custom max object size
    pub async fn filesystem_with_max_object_size(max_size: u64) -> Self {
        Self::builder().max_object_size(max_size).build().await
    }

    /// Start a test server with filesystem backend and custom codec concurrency
    pub async fn filesystem_with_codec_concurrency(concurrency: usize) -> Self {
        Self::builder().codec_concurrency(concurrency).build().await
    }

    /// Start a test server with S3 backend (needs MinIO running)
    pub async fn s3() -> Self {
        Self::builder()
            .s3_endpoint(&minio_endpoint_url())
            .bucket(MINIO_BUCKET)
            .build()
            .await
    }

    /// Start a test server with S3 backend pointing at a custom endpoint/bucket.
    pub async fn s3_with_endpoint(endpoint: &str, bucket: &str) -> Self {
        Self::builder()
            .s3_endpoint(endpoint)
            .bucket(bucket)
            .build()
            .await
    }

    /// Start a test server with S3 backend and a custom max delta ratio.
    pub async fn s3_with_endpoint_and_delta_ratio(
        endpoint: &str,
        bucket: &str,
        max_delta_ratio: f32,
    ) -> Self {
        Self::builder()
            .s3_endpoint(endpoint)
            .bucket(bucket)
            .max_delta_ratio(max_delta_ratio)
            .build()
            .await
    }

    // ── Shared spawn logic ──

    /// Allocate a port, write a TOML config, spawn the proxy, wait for readiness,
    /// and create the test bucket. All factory methods delegate here.
    async fn spawn_with_config(
        config_body: &str,
        bucket: &str,
        data_dir: Option<TempDir>,
        auth_creds: Option<(String, String)>,
        encryption_key: Option<String>,
        yaml_config: bool,
    ) -> Self {
        let port = PORT_COUNTER.fetch_add(1, Ordering::SeqCst);

        // Build full config with listen_addr prepended. Syntax depends
        // on the target format: TOML uses `key = "value"`, YAML uses
        // `key: "value"`.
        let full_config = if yaml_config {
            format!("listen_addr: \"127.0.0.1:{}\"\n{}", port, config_body)
        } else {
            format!("listen_addr = \"127.0.0.1:{}\"\n{}", port, config_body)
        };

        // Write config to a temp file inside a per-instance directory.
        // config_db_path() derives the DB path from the config file's parent,
        // so each test instance MUST have its own directory to avoid sharing
        // the encrypted config DB (which causes mismatch errors).
        let config_dir = match &data_dir {
            Some(d) => d.path().to_path_buf(),
            None => {
                let d = tempfile::tempdir().expect("Failed to create config temp dir");
                // Leak the TempDir so it lives until the test process ends
                let path = d.path().to_path_buf();
                std::mem::forget(d);
                path
            }
        };
        let config_path = if yaml_config {
            config_dir.join("test.yaml")
        } else {
            config_dir.join("test.toml")
        };
        std::fs::write(&config_path, &full_config).expect("Failed to write test config");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"));
        cmd.env("DGP_CONFIG", &config_path)
            .env("RUST_LOG", "deltaglider_proxy=warn")
            .env("DGP_DEBUG_HEADERS", "true")
            .env("DGP_TRUST_PROXY_HEADERS", "true");
        if let Some(ref key) = encryption_key {
            cmd.env("DGP_ENCRYPTION_KEY", key);
        }
        let process = cmd.spawn().expect("Failed to start server");

        let mut server = Self {
            process,
            port,
            _data_dir: data_dir,
            bucket: bucket.to_string(),
            auth_creds,
            config_path,
        };
        server.wait_ready().await;
        server.ensure_bucket().await;
        server
    }

    // ── Instance methods ──

    async fn wait_ready(&mut self) {
        // Use the health endpoint instead of raw TCP connect — the HTTP server
        // may accept TCP connections before routes and middleware are fully
        // initialized, causing "connection refused" on the first real request.
        let health_url = format!("http://127.0.0.1:{}/_/health", self.port);
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("health check client");

        for _ in 0..150 {
            if let Ok(resp) = client.get(&health_url).send().await {
                if resp.status().is_success() {
                    return;
                }
            }

            if let Ok(Some(status)) = self.process.try_wait() {
                panic!("Server exited before becoming ready: {}", status);
            }

            sleep(Duration::from_millis(100)).await;
        }

        let _ = self.process.kill();
        panic!(
            "Timed out waiting for server health on 127.0.0.1:{}",
            self.port
        );
    }

    /// Create the test bucket via the S3 API (replaces the removed DGP_BUCKET auto-create)
    async fn ensure_bucket(&self) {
        let client = self.s3_client().await;
        let _ = client.create_bucket().bucket(&self.bucket).send().await;
    }

    /// Create an S3 client configured for this test server (uses server's auth creds if set).
    pub async fn s3_client(&self) -> Client {
        let (key, secret) = match &self.auth_creds {
            Some((k, s)) => (k.as_str(), s.as_str()),
            None => ("test", "test"),
        };
        self.s3_client_with_creds(key, secret).await
    }

    /// Create an S3 client with specific credentials.
    pub async fn s3_client_with_creds(&self, access_key: &str, secret_key: &str) -> Client {
        let credentials = Credentials::new(access_key, secret_key, None, None, "test");

        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(self.endpoint())
            .credentials_provider(credentials)
            .force_path_style(true)
            .build();

        Client::from_conf(config)
    }

    /// Get the HTTP endpoint URL
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Get the bucket name
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Get the child process PID
    pub fn pid(&self) -> u32 {
        self.process.id()
    }

    /// Get the data directory path (filesystem backend only)
    /// Path of the config file the server was spawned with. Tests can read
    /// this to verify that admin-API config mutations persist to the
    /// correct file (regression coverage for the `backends.rs`
    /// `DEFAULT_CONFIG_FILENAME` hardcoding bug).
    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    pub fn data_dir(&self) -> Option<&std::path::Path> {
        self._data_dir.as_ref().map(|d| d.path())
    }
}

/// Builder for constructing `TestServer` instances with arbitrary config knobs.
///
/// Defaults to a filesystem backend. Call `.s3_endpoint()` to switch to S3.
/// Adding a new config knob is a one-line method + one line in `build_config()`.
pub struct TestServerBuilder {
    bucket: String,
    max_delta_ratio: Option<f32>,
    max_object_size: Option<u64>,
    codec_concurrency: Option<usize>,
    /// When set, uses S3 backend pointing at this endpoint instead of filesystem.
    s3_endpoint: Option<String>,
    /// SigV4 auth credentials (access_key_id, secret_access_key).
    auth_creds: Option<(String, String)>,
    /// Per-bucket TOML snippets: (bucket_name, toml_body)
    bucket_policies: Vec<(String, String)>,
    /// AES-256 encryption key (64-char hex). When set, DGP_ENCRYPTION_KEY env var is passed.
    encryption_key: Option<String>,
    /// When true, the test config is written as `test.yaml` (canonical
    /// sectioned shape) instead of the legacy `test.toml`. Required
    /// for any test that applies YAML-only fields
    /// (`admission.blocks`, `access.iam_mode: declarative`) — TOML
    /// persistence refuses those via H4.
    yaml_config: bool,
    /// S3 bucket for config DB sync (multi-replica HA mode). When set,
    /// `config_sync_bucket` is written to the config; server's startup
    /// downloads if newer, and every IAM mutation re-uploads.
    config_sync_bucket: Option<String>,
    /// Override the bootstrap password. Default: [`TEST_BOOTSTRAP_PASSWORD`].
    /// Used by HA-sync tests that want server A and server B to have
    /// DIFFERENT passwords (to verify the wrong-passphrase-rejection
    /// path).
    bootstrap_password: Option<String>,
}

impl Default for TestServerBuilder {
    fn default() -> Self {
        Self {
            bucket: "bucket".to_string(),
            max_delta_ratio: None,
            max_object_size: None,
            codec_concurrency: None,
            s3_endpoint: None,
            auth_creds: None,
            bucket_policies: Vec::new(),
            encryption_key: None,
            yaml_config: false,
            config_sync_bucket: None,
            bootstrap_password: None,
        }
    }
}

impl TestServerBuilder {
    pub fn bucket(mut self, bucket: &str) -> Self {
        self.bucket = bucket.to_string();
        self
    }

    /// Write the server's config as YAML (`test.yaml`) instead of
    /// TOML. Required for tests that apply YAML-only fields —
    /// operator-authored admission blocks or `iam_mode: declarative`
    /// — via the admin API, because `persist_to_file` refuses to
    /// serialise those to TOML.
    pub fn yaml_config(mut self) -> Self {
        self.yaml_config = true;
        self
    }

    pub fn max_delta_ratio(mut self, ratio: f32) -> Self {
        self.max_delta_ratio = Some(ratio);
        self
    }

    pub fn max_object_size(mut self, size: u64) -> Self {
        self.max_object_size = Some(size);
        self
    }

    pub fn codec_concurrency(mut self, n: usize) -> Self {
        self.codec_concurrency = Some(n);
        self
    }

    pub fn s3_endpoint(mut self, endpoint: &str) -> Self {
        self.s3_endpoint = Some(endpoint.to_string());
        self
    }

    pub fn auth(mut self, access_key_id: &str, secret_access_key: &str) -> Self {
        self.auth_creds = Some((access_key_id.to_string(), secret_access_key.to_string()));
        self
    }

    /// Add a per-bucket TOML policy section. Example:
    /// `.bucket_policy("releases", r#"public_prefixes = ["builds/"]"#)`
    pub fn bucket_policy(mut self, bucket: &str, toml_body: &str) -> Self {
        self.bucket_policies
            .push((bucket.to_string(), toml_body.to_string()));
        self
    }

    /// Set AES-256 encryption key (64-char hex string).
    pub fn encryption_key(mut self, hex_key: &str) -> Self {
        self.encryption_key = Some(hex_key.to_string());
        self
    }

    /// Set the S3 bucket for config DB HA sync. When set, the server
    /// syncs its encrypted IAM database to/from this bucket on startup
    /// + every IAM mutation + every 5-minute poll tick.
    ///
    /// Tests that want to observe propagation between two replicas
    /// point both at the same sync_bucket (with the same bootstrap
    /// password). Tests that want to observe rejection of a wrong-
    /// password replica point at the same sync_bucket with DIFFERENT
    /// bootstrap passwords via [`bootstrap_password`].
    ///
    /// Requires an S3 backend (`s3_endpoint`); the proxy refuses to
    /// start with a filesystem backend + sync_bucket.
    pub fn config_sync_bucket(mut self, bucket: &str) -> Self {
        self.config_sync_bucket = Some(bucket.to_string());
        self
    }

    /// Override the bootstrap password for this server. Default is
    /// [`TEST_BOOTSTRAP_PASSWORD`] (`testpass`) shared by every
    /// TestServer — giving HA-sync tests a way to spawn a replica
    /// with a DIFFERENT password to exercise the wrong-passphrase
    /// rejection path in `download_if_newer`.
    pub fn bootstrap_password(mut self, password: &str) -> Self {
        self.bootstrap_password = Some(password.to_string());
        self
    }

    /// Build the config string and spawn the test server. Format
    /// depends on the `yaml_config` flag (TOML by default).
    pub async fn build(self) -> TestServer {
        let (config, data_dir) = self.build_config();
        let auth = self.auth_creds.clone();
        let yaml = self.yaml_config;
        TestServer::spawn_with_config(
            &config,
            &self.bucket,
            data_dir,
            auth,
            self.encryption_key,
            yaml,
        )
        .await
    }

    /// Assemble a config string in the selected format (TOML by
    /// default, YAML when `yaml_config` is set) and, for filesystem-
    /// backend tests, a TempDir holding the backing storage path.
    fn build_config(&self) -> (String, Option<TempDir>) {
        if self.yaml_config {
            self.build_yaml_config()
        } else {
            self.build_toml_config()
        }
    }

    fn build_toml_config(&self) -> (String, Option<TempDir>) {
        let mut config = String::new();

        // Set a known bootstrap password hash so tests can log into
        // the admin API. HA-sync tests can override via
        // `bootstrap_password()` to spawn a replica with a different
        // password (wrong-passphrase rejection test).
        let password_plaintext = self
            .bootstrap_password
            .as_deref()
            .unwrap_or(TEST_BOOTSTRAP_PASSWORD);
        let bootstrap_hash = bcrypt::hash(password_plaintext, 4).expect("bcrypt hash failed");
        config.push_str(&format!(
            "bootstrap_password_hash = \"{}\"\n",
            bootstrap_hash
        ));

        // Top-level knobs
        if let Some(ratio) = self.max_delta_ratio {
            config.push_str(&format!("max_delta_ratio = {}\n", ratio));
        }
        if let Some(size) = self.max_object_size {
            config.push_str(&format!("max_object_size = {}\n", size));
        }
        if let Some(n) = self.codec_concurrency {
            config.push_str(&format!("codec_concurrency = {}\n", n));
        }
        if let Some(ref sync_bucket) = self.config_sync_bucket {
            config.push_str(&format!("config_sync_bucket = \"{}\"\n", sync_bucket));
        }
        if let Some((ref key_id, ref secret)) = self.auth_creds {
            config.push_str(&format!(
                "access_key_id = \"{}\"\nsecret_access_key = \"{}\"\n",
                key_id, secret
            ));
        } else {
            // Explicitly opt in to open access — the proxy refuses to start
            // without credentials unless authentication = "none" is set.
            config.push_str("authentication = \"none\"\n");
        }

        // Per-bucket policy sections
        for (bucket, body) in &self.bucket_policies {
            config.push_str(&format!("\n[buckets.{}]\n{}\n", bucket, body));
        }

        // Backend section
        if let Some(ref endpoint) = self.s3_endpoint {
            config.push_str(&format!(
                concat!(
                    "\n[backend]\n",
                    "type = \"s3\"\n",
                    "endpoint = \"{}\"\n",
                    "region = \"us-east-1\"\n",
                    "force_path_style = true\n",
                    "access_key_id = \"{}\"\n",
                    "secret_access_key = \"{}\"\n",
                ),
                endpoint, MINIO_ACCESS_KEY, MINIO_SECRET_KEY,
            ));
            (config, None)
        } else {
            let data_dir = TempDir::new().expect("Failed to create temp dir");
            config.push_str(&format!(
                "\n[backend]\ntype = \"filesystem\"\npath = \"{}\"\n",
                data_dir.path().display()
            ));
            (config, Some(data_dir))
        }
    }

    /// Minimal YAML version of `build_toml_config`. Used by tests that
    /// apply YAML-only fields (admission blocks, iam_mode:declarative)
    /// — persist_to_file refuses to write those to a TOML target.
    ///
    /// Emits the flat shape (TOML-equivalent field layout at the root)
    /// because the admission-mode tests don't need the sectioned
    /// exporter round-trip in their initial config. The server's own
    /// apply path will re-emit sectioned YAML on persist.
    fn build_yaml_config(&self) -> (String, Option<TempDir>) {
        let mut config = String::new();

        let password_plaintext = self
            .bootstrap_password
            .as_deref()
            .unwrap_or(TEST_BOOTSTRAP_PASSWORD);
        let bootstrap_hash = bcrypt::hash(password_plaintext, 4).expect("bcrypt hash failed");
        config.push_str(&format!(
            "bootstrap_password_hash: \"{}\"\n",
            bootstrap_hash
        ));

        if let Some(ratio) = self.max_delta_ratio {
            config.push_str(&format!("max_delta_ratio: {}\n", ratio));
        }
        if let Some(size) = self.max_object_size {
            config.push_str(&format!("max_object_size: {}\n", size));
        }
        if let Some(n) = self.codec_concurrency {
            config.push_str(&format!("codec_concurrency: {}\n", n));
        }
        if let Some(ref sync_bucket) = self.config_sync_bucket {
            config.push_str(&format!("config_sync_bucket: \"{}\"\n", sync_bucket));
        }
        if let Some((ref key_id, ref secret)) = self.auth_creds {
            config.push_str(&format!(
                "access_key_id: \"{}\"\nsecret_access_key: \"{}\"\n",
                key_id, secret
            ));
        } else {
            config.push_str("authentication: \"none\"\n");
        }

        if !self.bucket_policies.is_empty() {
            config.push_str("buckets:\n");
            for (bucket, body) in &self.bucket_policies {
                config.push_str(&format!("  {}:\n", bucket));
                // Each line of the TOML body becomes a YAML line with
                // 4-space indent + `:` separator instead of `=`. Tests
                // that use bucket_policies pass TOML-like `key = value`
                // bodies; we translate trivially.
                for line in body.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Some((k, v)) = trimmed.split_once(" = ") {
                        config.push_str(&format!("    {}: {}\n", k, v));
                    }
                }
            }
        }

        if let Some(ref endpoint) = self.s3_endpoint {
            config.push_str(&format!(
                concat!(
                    "backend:\n",
                    "  type: s3\n",
                    "  endpoint: \"{}\"\n",
                    "  region: \"us-east-1\"\n",
                    "  force_path_style: true\n",
                    "  access_key_id: \"{}\"\n",
                    "  secret_access_key: \"{}\"\n",
                ),
                endpoint, MINIO_ACCESS_KEY, MINIO_SECRET_KEY,
            ));
            (config, None)
        } else {
            let data_dir = TempDir::new().expect("Failed to create temp dir");
            config.push_str(&format!(
                "backend:\n  type: filesystem\n  path: \"{}\"\n",
                data_dir.path().display()
            ));
            (config, Some(data_dir))
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

/// Create a reqwest client that is logged in to the admin API.
/// Uses the known [`TEST_BOOTSTRAP_PASSWORD`] to authenticate.
pub async fn admin_http_client(endpoint: &str) -> reqwest::Client {
    admin_http_client_with_password(endpoint, TEST_BOOTSTRAP_PASSWORD).await
}

/// Like [`admin_http_client`] but with an explicit bootstrap password.
/// Used by HA-sync tests that spawn a replica with a non-default
/// password via [`TestServerBuilder::bootstrap_password`].
pub async fn admin_http_client_with_password(endpoint: &str, password: &str) -> reqwest::Client {
    let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
    let client = reqwest::Client::builder()
        .cookie_provider(jar)
        .build()
        .unwrap();

    let resp = client
        .post(format!("{}/_/api/admin/login", endpoint))
        .json(&serde_json::json!({ "password": password }))
        .send()
        .await
        .expect("Admin login request failed");
    assert!(
        resp.status().is_success(),
        "Admin login failed: {}",
        resp.status()
    );
    client
}

// === Shared HTTP helpers (reqwest) ===

/// Build an S3 object URL from endpoint, bucket, and key.
fn object_url(endpoint: &str, bucket: &str, key: &str) -> String {
    format!("{}/{}/{}", endpoint, bucket, key)
}

/// PUT an object via reqwest and return the response.
pub async fn put_object(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
    content_type: &str,
) -> reqwest::Response {
    let url = object_url(endpoint, bucket, key);
    let resp = client
        .put(&url)
        .header("content-type", content_type)
        .body(data)
        .send()
        .await
        .expect("PUT failed");
    assert!(
        resp.status().is_success(),
        "PUT {} failed: {}",
        key,
        resp.status()
    );
    resp
}

/// PUT an object and return the x-amz-storage-type header value.
pub async fn put_and_get_storage_type(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
    content_type: &str,
) -> String {
    let resp = put_object(client, endpoint, bucket, key, data, content_type).await;
    resp.headers()
        .get("x-amz-storage-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// GET an object and return the body bytes.
pub async fn get_bytes(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> Vec<u8> {
    let url = object_url(endpoint, bucket, key);
    let resp = client.get(&url).send().await.expect("GET failed");
    assert!(
        resp.status().is_success(),
        "GET {} failed: {}",
        key,
        resp.status()
    );
    resp.bytes().await.unwrap().to_vec()
}

/// HEAD an object and return response headers.
pub async fn head_headers(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> reqwest::header::HeaderMap {
    let url = object_url(endpoint, bucket, key);
    let resp = client.head(&url).send().await.expect("HEAD failed");
    assert!(
        resp.status().is_success(),
        "HEAD {} failed: {}",
        key,
        resp.status()
    );
    resp.headers().clone()
}

/// DELETE an object via reqwest (tolerates 204 and 404).
pub async fn delete_object(client: &reqwest::Client, endpoint: &str, bucket: &str, key: &str) {
    let url = object_url(endpoint, bucket, key);
    let resp = client.delete(&url).send().await.expect("DELETE failed");
    assert!(
        resp.status().is_success()
            || resp.status().as_u16() == 204
            || resp.status().as_u16() == 404,
        "DELETE {} failed: {}",
        key,
        resp.status()
    );
}

/// Fetch the current IAM rebuild counter from the proxy.
///
/// Backed by `GET /_/api/admin/iam/version`, which is incremented by
/// [`src/api/admin/users.rs::rebuild_iam_index`] after every IAM
/// mutation (user/group CRUD, OAuth provider changes, etc.). Used by
/// [`wait_for_iam_rebuild`] as the barrier primitive.
pub async fn get_iam_version(client: &reqwest::Client, endpoint: &str) -> u64 {
    let resp = client
        .get(format!("{endpoint}/_/api/admin/iam/version"))
        .send()
        .await
        .expect("iam/version GET");
    assert!(
        resp.status().is_success(),
        "iam/version must return 2xx, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("iam/version JSON");
    body["version"].as_u64().expect("version is u64")
}

/// Wait until the proxy's IAM rebuild counter advances past `baseline`.
///
/// Call pattern:
/// 1. `let v = get_iam_version(&http, &endpoint).await;` BEFORE the mutation
/// 2. Perform the IAM mutation (POST /users, PUT /groups/..., etc.)
/// 3. `wait_for_iam_rebuild(&http, &endpoint, v).await;` — returns as soon as
///    the counter has advanced, up to 5 seconds of polling at 20ms intervals.
///
/// Replaces the earlier `sleep(1s)` pattern, which was both slow (every
/// test paid 1s whether the rebuild took 5ms or 50ms) and flake-prone
/// on slower CI runners where 1s wasn't always enough.
///
/// Panics if the counter hasn't advanced within 5s — that either
/// indicates a rebuild regression or test-setup bug, both of which
/// should fail loudly.
pub async fn wait_for_iam_rebuild(client: &reqwest::Client, endpoint: &str, baseline: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut attempts = 0u32;
    loop {
        let current = get_iam_version(client, endpoint).await;
        if current > baseline {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "wait_for_iam_rebuild timed out after 5s: baseline={baseline}, \
                 current={current}, attempts={attempts} — either the IAM \
                 mutation didn't trigger rebuild_iam_index or the counter \
                 isn't being bumped"
            );
        }
        attempts += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

/// Make a raw ListObjectsV2 request and return the XML body.
pub async fn list_objects_raw(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    params: &str,
) -> String {
    let url = format!("{}/{}?list-type=2&{}", endpoint, bucket, params);
    let resp = client.get(&url).send().await.unwrap();
    assert!(
        resp.status().is_success(),
        "ListObjects failed: {}",
        resp.status()
    );
    resp.text().await.unwrap()
}

// === Quick-setup helpers (reduce test boilerplate) ===

/// Quick setup: filesystem server + reqwest client
pub async fn test_setup() -> (TestServer, reqwest::Client) {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();
    (server, http)
}

/// Upload a simple test file, return its bytes
pub async fn upload_test_data(
    http: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    size: usize,
) -> Vec<u8> {
    let data = generate_binary(size, 42);
    put_object(
        http,
        endpoint,
        bucket,
        key,
        data.clone(),
        "application/octet-stream",
    )
    .await;
    data
}

// === Data generators ===

/// Generate deterministic binary data
pub fn generate_binary(size: usize, seed: u64) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut data = vec![0u8; size];
    rng.fill(&mut data[..]);
    data
}

/// Mutate binary data by changing a percentage of bytes
pub fn mutate_binary(data: &[u8], change_ratio: f64) -> Vec<u8> {
    let mut result = data.to_vec();
    let changes = (data.len() as f64 * change_ratio) as usize;
    let mut rng = rand::thread_rng();

    for _ in 0..changes {
        let idx = rng.gen_range(0..result.len());
        result[idx] = rng.gen();
    }

    result
}

// === MinIO gating ===

/// Create an S3 client pointing directly at MinIO (not through the proxy)
pub async fn minio_client() -> Client {
    let credentials = Credentials::new(MINIO_ACCESS_KEY, MINIO_SECRET_KEY, None, None, "test");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(minio_endpoint_url())
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    Client::from_conf(config)
}

/// Check if MinIO is available (TCP probe + HeadBucket with 2s timeout)
pub async fn minio_available() -> bool {
    // Quick TCP check first — parse host:port from endpoint URL
    let endpoint = minio_endpoint_url();
    let addr = endpoint
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    if std::net::TcpStream::connect(addr).is_err() {
        return false;
    }

    let client = minio_client().await;

    // Verify the specific test bucket exists (not just any S3-compatible service)
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        client.head_bucket().bucket(MINIO_BUCKET).send(),
    )
    .await;
    matches!(result, Ok(Ok(_)))
}

/// Macro to skip a test if MinIO is not available.
/// Use at the start of any test that requires MinIO.
#[macro_export]
macro_rules! skip_unless_minio {
    () => {
        if !common::minio_available().await {
            eprintln!("MinIO not available, skipping test");
            return;
        }
    };
}

/// Check if Docker is available by running `docker version`
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Macro to skip a test if Docker is not available.
/// Use at the start of any test that requires an ephemeral container.
#[macro_export]
macro_rules! skip_unless_docker {
    () => {
        if !common::docker_available() {
            eprintln!("Docker not available, skipping test");
            return;
        }
    };
}

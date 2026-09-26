// SPDX-License-Identifier: BUSL-1.1

//! Shared test infrastructure for integration tests
//!
//! Provides TestServer (filesystem and S3 backends), data generators,
//! and MinIO availability gating.

#![allow(dead_code)]

mod signed_http;
pub use signed_http::{S3Http, S3Requests};

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::Client;
use rand::{Rng, SeedableRng};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

/// First port the harness hands out, and how many it cycles through.
const PORT_BASE: u32 = 19000;
const PORT_SPAN: u32 = 40000;

/// Per-process cursor into the port range.
static PORT_COUNTER: AtomicU32 = AtomicU32::new(0);

/// A port reserved for one [`TestServer`] across EVERY test process on the
/// machine, for the server's whole life (respawns included).
///
/// A per-process counter plus a probe-bind was not enough: two concurrent
/// `cargo test` processes (parallel CI jobs, several sessions on one dev
/// box) both started at 19000, both saw a port free, and a test then talked
/// to the OTHER process's proxy (AccessDenied on seed PUTs, another test's
/// maintenance gate, "exited before becoming ready"). The reservation is an
/// advisory lock (`flock`) on `<tmp>/dgp-test-ports/<port>.lock`: the kernel
/// drops it when the holder exits, so a crashed run leaks nothing. The
/// probe-bind still skips ports that non-harness processes hold.
pub struct PortLease {
    port: u16,
    _lock: std::fs::File,
}

impl PortLease {
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Path of the lock file that reserves `port` (see [`PortLease`]).
pub fn port_lock_path(port: u16) -> std::path::PathBuf {
    std::env::temp_dir()
        .join("dgp-test-ports")
        .join(format!("{port}.lock"))
}

/// Reserve the next port no harness process holds and nothing is bound to.
pub fn lease_free_port() -> PortLease {
    let dir = port_lock_path(0).parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).expect("create port lock dir");
    for _ in 0..PORT_SPAN {
        let n = PORT_COUNTER.fetch_add(1, Ordering::SeqCst);
        let port = (PORT_BASE + n % PORT_SPAN) as u16;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(port_lock_path(port))
            .expect("open port lock file");
        // Held by another test process (or another lease in this one).
        if lock.try_lock().is_err() {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return PortLease { port, _lock: lock };
        }
    }
    panic!(
        "no free test port in {PORT_BASE}..{}",
        PORT_BASE + PORT_SPAN
    );
}

/// SigV4 credentials every [`TestServer`] gets unless the test calls
/// [`TestServerBuilder::open_access`]. Auth is ON by default so the suite
/// exercises the same auth pipeline as production; open access is opt-in.
pub const TEST_ACCESS_KEY: &str = "test";
pub const TEST_SECRET_KEY: &str = "test";

/// Known bootstrap password used by all test servers.
pub const TEST_BOOTSTRAP_PASSWORD: &str = "testpass";

/// Deterministic bcrypt hash of [`TEST_BOOTSTRAP_PASSWORD`] (cost 4).
///
/// The admin-login verifier of every default [`TestServer`]. A fresh
/// `bcrypt::hash(...)` per build would give each server a different salt and
/// hash; a stable constant keeps multi-process scenarios (HA replicas that
/// share one admin password) and the `mismatch_boot_test` legacy-key path
/// deterministic. It is NOT the config DB key: that is
/// [`TEST_CONFIG_DB_KEY`] (sync servers) or a per-server key file.
pub const TEST_BOOTSTRAP_PASSWORD_HASH: &str =
    "$2b$04$s7/yy6Z363jZoQodArpuDeP00U.zE1QPi0bxM/o9BOZDs6tDbss5q";

/// `DGP_CONFIG_DB_KEY` for every server built with a config sync bucket: HA
/// replicas share one encrypted DB, so they need one key (the proxy refuses
/// to start with a sync bucket and no env key). A test that wants a replica
/// with another key sets `.env("DGP_CONFIG_DB_KEY", ...)`.
/// Stands for `127.0.0.1:<leased port>` in a document passed to
/// [`TestServer::from_config_document`].
pub const LISTEN_ADDR_PLACEHOLDER: &str = "__DGP_TEST_LISTEN_ADDR__";

pub const TEST_CONFIG_DB_KEY: &str = "test-config-db-key-0123456789abcdef0123456789abcdef";

/// MinIO configuration constants
pub const MINIO_BUCKET: &str = "deltaglider-test";

/// A bucket name that is unique per call within this process AND across
/// processes: nanosecond timestamp plus a process-wide counter, so two calls
/// in the same clock tick (parallel tests start within microseconds of each
/// other) still get different names.
pub fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{ts}-{n}")
}

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
    /// Keeps `port` reserved against other test processes until drop.
    _port_lease: PortLease,
    _data_dir: Option<TempDir>,
    bucket: String,
    /// Auth credentials for the test server (None = open access).
    auth_creds: Option<(String, String)>,
    /// Absolute path of the config file the server was spawned with (via
    /// `DGP_CONFIG`). Exposed so tests can verify that admin-API config
    /// mutations persist to this specific file rather than to a
    /// CWD-relative default.
    config_path: std::path::PathBuf,
    /// Extra environment variables used when respawning this server.
    extra_env: Vec<(String, String)>,
    /// See [`TestServerBuilder::production_security_defaults`].
    production_security: bool,
}

/// The proxy child command every spawn path shares (first spawn and both
/// respawns), so the harness env is defined once.
///
/// Hermetic: every `DGP_*` variable of the parent process (a developer
/// shell, a CI job `env:`) is removed, so a test sees the same child env
/// locally and in CI. The two test-convenience relaxations are set here,
/// per child, instead of job-wide in CI:
/// - `DGP_BACKEND_ALLOW_LOCAL=true`: tests use http://127.0.0.1 MinIO and
///   dead local endpoints, which the SSRF guard refuses by default.
/// - `DGP_REPLAY_WINDOW_SECS=0`: the SDK signs at one-second granularity, so
///   two identical assertion-style mutations in one second collide.
///
/// `production_security` leaves both at their production defaults.
fn proxy_command(config_path: &std::path::Path, production_security: bool) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("DGP_") {
            cmd.env_remove(key);
        }
    }
    // cwd = the temp config dir: the proxy writes state files
    // (`.deltaglider_bootstrap_hash`) relative to its cwd, and those
    // must never land in the repo root (see `spawns_set_current_dir`).
    cmd.current_dir(config_path.parent().expect("config dir"))
        .env("DGP_CONFIG", config_path)
        .env("RUST_LOG", "deltaglider_proxy=warn")
        .env("DGP_DEBUG_HEADERS", "true")
        .env("DGP_TRUST_PROXY_HEADERS", "true")
        // Boot backend-health probe: OFF by default in the harness — many
        // tests deliberately spawn against dead/absent endpoints and must
        // not exit(1) or pay probe timeouts. Gate tests opt back in via
        // .env("DGP_BOOT_BACKEND_PROBE", "enforce").
        .env("DGP_BOOT_BACKEND_PROBE", "off");
    if !production_security {
        cmd.env("DGP_BACKEND_ALLOW_LOCAL", "true")
            .env("DGP_REPLAY_WINDOW_SECS", "0");
    }
    cmd
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

    /// Allocate a port, write a YAML config, spawn the proxy, wait for readiness,
    /// and create the test bucket. All factory methods delegate here.
    async fn spawn_with_config(
        config_body: &str,
        bucket: &str,
        data_dir: Option<TempDir>,
        auth_creds: Option<(String, String)>,
        encryption_key: Option<String>,
        extra_env: Vec<(String, String)>,
        production_security: bool,
    ) -> Self {
        let port_lease = lease_free_port();
        let port = port_lease.port();

        // A whole document (e.g. the sectioned prod-shape fixture) names
        // its listen address with the placeholder; the flat shape gets it
        // prepended.
        let listen = format!("127.0.0.1:{port}");
        let full_config = if config_body.contains(LISTEN_ADDR_PLACEHOLDER) {
            config_body.replace(LISTEN_ADDR_PLACEHOLDER, &listen)
        } else {
            format!("listen_addr: \"{listen}\"\n{config_body}")
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
        let config_path = config_dir.join("test.yaml");
        std::fs::write(&config_path, &full_config).expect("Failed to write test config");

        let mut cmd = proxy_command(&config_path, production_security);
        if let Some(ref key) = encryption_key {
            cmd.env("DGP_ENCRYPTION_KEY", key);
        }
        for (key, value) in &extra_env {
            cmd.env(key, value);
        }
        let process = cmd.spawn().expect("Failed to start server");

        let mut server = Self {
            process,
            port,
            _port_lease: port_lease,
            _data_dir: data_dir,
            bucket: bucket.to_string(),
            auth_creds,
            config_path,
            extra_env,
            production_security,
        };
        server.wait_ready().await;
        server.ensure_bucket().await;
        server
    }

    /// Spawn the proxy from a complete config document (any shape) instead
    /// of the builder's generated flat one. The document must carry
    /// [`LISTEN_ADDR_PLACEHOLDER`] where the listen address goes;
    /// `data_dir` owns every filesystem backend path in it. `bucket` is
    /// created through the S3 API, signed with `auth`.
    pub async fn from_config_document(
        document: &str,
        data_dir: TempDir,
        auth: (&str, &str),
        bucket: &str,
        extra_env: Vec<(String, String)>,
    ) -> Self {
        assert!(
            document.contains(LISTEN_ADDR_PLACEHOLDER),
            "the config document must name its listen address as {LISTEN_ADDR_PLACEHOLDER}"
        );
        Self::spawn_with_config(
            document,
            bucket,
            Some(data_dir),
            Some((auth.0.to_string(), auth.1.to_string())),
            None,
            extra_env,
            false,
        )
        .await
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

        // 60 s wall-clock budget. The old 15 s was enough on an idle box but
        // not on the shared CI host under a load average past 50. A proxy
        // that actually exits is still caught immediately below.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            // Check the child process FIRST. If an earlier stray
            // server is holding our port, our child will fail to bind
            // and exit non-zero — we must detect that before the
            // health check, otherwise we'd observe the stray server's
            // /health and incorrectly report ready, then the test
            // fires requests at a server whose auth config it doesn't
            // know (classic cause of "AccessDenied" with no obvious
            // explanation).
            if let Ok(Some(status)) = self.process.try_wait() {
                // The child inherits stderr, so its OWN error line (the real
                // cause) is already in this test's output — but it is printed
                // when the child dies, which with parallel tests can be many
                // lines above this panic. Point at it instead of asserting a
                // single cause: this message used to claim a port collision,
                // which sent readers chasing `lsof` while the actual failure
                // was a startup validation error sitting further up the log.
                panic!(
                    "Test proxy on port {} exited before becoming ready: {status}.\n\
                     Look for the proxy's own `Error:`/`FATAL` line ABOVE this panic — \
                     that is the real cause. Common ones:\n\
                     - config/backend validation rejected the config (e.g. an http:// \
                       S3 endpoint without DGP_BACKEND_ALLOW_LOCAL=true)\n\
                     - a required credential/env var missing for this test's config\n\
                     - the port really is taken by a stray process (`lsof -i :{}`)",
                    self.port, self.port
                );
            }

            if let Ok(resp) = client.get(&health_url).send().await {
                if resp.status().is_success() {
                    return;
                }
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

    /// Raw-HTTP client for hand-built S3 requests: signs with this server's
    /// credentials (unsigned when the server has open access).
    pub fn http(&self) -> S3Http {
        match &self.auth_creds {
            Some((k, s)) => S3Http::signed(k, s),
            None => S3Http::unsigned(),
        }
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
    /// hardcoded-default-filename bug).
    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    pub fn data_dir(&self) -> Option<&std::path::Path> {
        self._data_dir.as_ref().map(|d| d.path())
    }

    /// Kill the current proxy process and spawn a new one against the SAME
    /// config file, data dir, and port — but WITHOUT `DGP_ENCRYPTION_KEY`
    /// set. Used by the B1 regression test: an operator who disables
    /// encryption must NOT get silent ciphertext-as-plaintext reads on
    /// historical encrypted objects; the storage wrapper is always in
    /// place and errors when the marker says encrypted but no key is
    /// configured.
    ///
    /// Preserves `auth_creds` and `bucket`; only the env-var side of the
    /// config changes. Falls through the same readiness probe as the
    /// initial spawn.
    pub async fn respawn_without_encryption_key(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        // Poll until the kernel has actually released the listening
        // socket before we spawn the new child. A hard 200 ms sleep
        // was racy on slow hosts (EADDRINUSE) and over-long on fast
        // ones. Bounded to ~2s — if we can't bind in that window
        // something is genuinely stuck and a loud panic is better
        // than silently waiting forever.
        let addr = format!("127.0.0.1:{}", self.port);
        let mut rebind_ok = false;
        for _ in 0..40 {
            match std::net::TcpListener::bind(&addr) {
                Ok(listener) => {
                    drop(listener);
                    rebind_ok = true;
                    break;
                }
                Err(_) => sleep(Duration::from_millis(50)).await,
            }
        }
        assert!(
            rebind_ok,
            "port {} did not free within ~2s after killing the old child; \
             another process may be holding it (try `lsof -i :{}`)",
            self.port, self.port
        );

        // Explicitly NOT setting DGP_ENCRYPTION_KEY (`proxy_command` strips
        // any inherited one).
        let mut cmd = proxy_command(&self.config_path, self.production_security);
        for (key, value) in &self.extra_env {
            cmd.env(key, value);
        }
        self.process = cmd.spawn().expect("Failed to respawn server");
        self.wait_ready().await;
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
    /// Per-bucket YAML snippets: (bucket_name, yaml_body)
    bucket_policies: Vec<(String, String)>,
    /// AES-256 encryption key (64-char hex). When set, DGP_ENCRYPTION_KEY env var is passed.
    encryption_key: Option<String>,
    /// Native SSE mode tag for the singleton backend (Step 4). When
    /// set, emits `[backend_encryption] mode = "<value>"` into the
    /// generated config; the S3Backend then applies SSE headers per
    /// PutObject. `"sse-s3"` is tested against MinIO; `"sse-kms"`
    /// would need an ARN and is out of scope for the test harness.
    native_sse_mode: Option<String>,
    /// S3 bucket for config DB sync (multi-replica HA mode). When set,
    /// `config_sync_bucket` is written to the config; server's startup
    /// downloads if newer, and every IAM mutation re-uploads.
    config_sync_bucket: Option<String>,
    /// S3 object key for config DB sync (written as `config_sync_object_key`).
    config_sync_object_key: Option<String>,
    /// Override the bootstrap password. Default: [`TEST_BOOTSTRAP_PASSWORD`].
    /// Used by HA-sync tests that want server A and server B to have
    /// DIFFERENT passwords (to verify the wrong-passphrase-rejection
    /// path).
    bootstrap_password: Option<String>,
    /// Raw YAML fragment to append INSIDE the `storage:` section of the
    /// generated config. Intended for tests that exercise features
    /// like replication rules.
    extra_storage_yaml: Option<String>,
    /// Raw YAML appended at the document ROOT (flat shape). Used to seed a full
    /// `iam_mode: declarative` + `iam_users` / `iam_groups` block so the proxy
    /// reconciles it AT STARTUP — exercising the cold-start IaC path.
    extra_root_yaml: Option<String>,
    /// Extra process environment variables for this test proxy.
    extra_env: Vec<(String, String)>,
    /// See [`Self::production_security_defaults`].
    production_security: bool,
}

impl Default for TestServerBuilder {
    fn default() -> Self {
        Self {
            bucket: "bucket".to_string(),
            max_delta_ratio: None,
            max_object_size: None,
            codec_concurrency: None,
            s3_endpoint: None,
            auth_creds: Some((TEST_ACCESS_KEY.to_string(), TEST_SECRET_KEY.to_string())),
            bucket_policies: Vec::new(),
            encryption_key: None,
            native_sse_mode: None,
            config_sync_bucket: None,
            config_sync_object_key: None,
            bootstrap_password: None,
            extra_storage_yaml: None,
            extra_root_yaml: None,
            extra_env: Vec::new(),
            production_security: false,
        }
    }
}

impl TestServerBuilder {
    pub fn bucket(mut self, bucket: &str) -> Self {
        self.bucket = bucket.to_string();
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

    /// No SigV4 credentials: the proxy runs with `authentication: none`.
    /// Auth is on by default; call this only when the test needs unsigned
    /// requests (raw reqwest, anonymous clients, open-access behaviour).
    pub fn open_access(mut self) -> Self {
        self.auth_creds = None;
        self
    }

    /// Keep the production defaults for replay protection
    /// (`DGP_REPLAY_WINDOW_SECS` unset = the clock-skew window) and for the
    /// SSRF guard (`DGP_BACKEND_ALLOW_LOCAL` unset = http:// and private
    /// endpoints refused). The harness relaxes both by default (see
    /// `proxy_command`); a local MinIO backend cannot start in this mode.
    pub fn production_security_defaults(mut self) -> Self {
        self.production_security = true;
        self
    }

    /// Add an environment variable to the spawned proxy process.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.extra_env.push((key.to_string(), value.to_string()));
        self
    }

    /// Add a per-bucket YAML policy section (one `key: value` per line).
    /// Example: `.bucket_policy("releases", r#"public_prefixes: ["builds/"]"#)`
    pub fn bucket_policy(mut self, bucket: &str, yaml_body: &str) -> Self {
        self.bucket_policies
            .push((bucket.to_string(), yaml_body.to_string()));
        self
    }

    /// Set AES-256 encryption key (64-char hex string).
    pub fn encryption_key(mut self, hex_key: &str) -> Self {
        self.encryption_key = Some(hex_key.to_string());
        self
    }

    /// Enable S3 native SSE-S3 (AES256, AWS-managed keys) on the
    /// singleton backend. Requires `s3_endpoint()` — the encryption
    /// happens inside the S3 backend via `x-amz-server-side-encryption`
    /// headers.
    pub fn sse_s3(mut self) -> Self {
        self.native_sse_mode = Some("sse-s3".to_string());
        self
    }

    /// Set the S3 bucket for config DB HA sync. When set, the server
    /// syncs its encrypted IAM database to/from this bucket on startup
    /// + every IAM mutation + every 5-minute poll tick.
    ///
    /// Tests that want to observe propagation between two replicas
    /// point both at the same sync_bucket (they share
    /// [`TEST_CONFIG_DB_KEY`]). Tests that want to observe rejection of a
    /// wrong-key replica set `.env("DGP_CONFIG_DB_KEY", <other>)`.
    ///
    /// Requires an S3 backend (`s3_endpoint`); the proxy refuses to
    /// start with a filesystem backend + sync_bucket.
    pub fn config_sync_bucket(mut self, bucket: &str) -> Self {
        self.config_sync_bucket = Some(bucket.to_string());
        self
    }

    /// Set the S3 object key for config DB sync (parallel to
    /// [`Self::config_sync_bucket`]). Prefer this over `DGP_CONFIG_SYNC_KEY`
    /// env so the spawned binary always reads the key from `DGP_CONFIG`.
    pub fn config_sync_object_key(mut self, key: &str) -> Self {
        self.config_sync_object_key = Some(key.to_string());
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

    /// Append a raw YAML fragment at the document ROOT (flat shape) — e.g. a
    /// full `iam_mode: declarative` + `iam_users:` block. The proxy reconciles
    /// declarative IAM at startup, so this exercises the cold-start IaC path
    /// (a fresh DB populated from YAML with no `config apply`).
    pub fn extra_yaml_root(mut self, yaml: &str) -> Self {
        self.extra_root_yaml = Some(yaml.to_string());
        self
    }

    /// Append a raw YAML fragment INSIDE the `storage:` section. Used
    /// by replication tests to seed rules without going through the
    /// section-apply dance.
    pub fn extra_yaml_storage_section(mut self, yaml: &str) -> Self {
        self.extra_storage_yaml = Some(yaml.to_string());
        self
    }

    /// Returns the config document this builder would pass to the proxy
    /// (no process spawn). Filesystem backends allocate a throwaway `TempDir`
    /// for the `path =` value; callers that need a stable data directory must
    /// use [`build`](Self::build).
    pub fn generated_config_document(&self) -> String {
        self.build_config().0
    }

    /// Build the config string (YAML) and spawn the test server.
    pub async fn build(self) -> TestServer {
        let (config, data_dir) = self.build_config();
        let auth = self.auth_creds.clone();
        let mut extra_env = self.extra_env.clone();
        if self.config_sync_bucket.is_some()
            && !extra_env.iter().any(|(k, _)| k == "DGP_CONFIG_DB_KEY")
        {
            // Before the test's own env, so `.env(...)` still wins.
            extra_env.insert(
                0,
                (
                    "DGP_CONFIG_DB_KEY".to_string(),
                    TEST_CONFIG_DB_KEY.to_string(),
                ),
            );
        }
        TestServer::spawn_with_config(
            &config,
            &self.bucket,
            data_dir,
            auth,
            self.encryption_key,
            extra_env,
            self.production_security,
        )
        .await
    }

    /// Assemble the YAML config string and, for filesystem-backend
    /// tests, a TempDir holding the backing storage path.
    ///
    /// Emits the flat shape (field layout at the document root) — the
    /// server's own apply path re-emits canonical sectioned YAML on
    /// persist.
    fn build_config(&self) -> (String, Option<TempDir>) {
        let mut config = String::new();

        let bootstrap_hash = match self.bootstrap_password.as_deref() {
            Some(pw) => bcrypt::hash(pw, 4).expect("bcrypt hash failed"),
            None => TEST_BOOTSTRAP_PASSWORD_HASH.to_string(),
        };
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
        if let Some(ref sync_key) = self.config_sync_object_key {
            config.push_str(&format!(
                "config_sync_object_key: \"{}\"\n",
                sync_key.replace('\\', "\\\\").replace('"', "\\\"")
            ));
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
                // Each line of the YAML body (`key: value`) is re-emitted
                // with a 4-space indent under the bucket key.
                for line in body.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    config.push_str(&format!("    {}\n", trimmed));
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
            if let Some(ref mode) = self.native_sse_mode {
                config.push_str(&format!("backend_encryption:\n  mode: {}\n", mode));
            } else if self.encryption_key.is_some() {
                config.push_str("backend_encryption:\n  mode: aes256-gcm-proxy\n");
            }
            if let Some(ref yaml) = self.extra_storage_yaml {
                config.push_str(yaml);
            }
            if let Some(ref yaml) = self.extra_root_yaml {
                config.push_str(yaml);
            }
            (config, None)
        } else {
            let data_dir = TempDir::new().expect("Failed to create temp dir");
            config.push_str(&format!(
                "backend:\n  type: filesystem\n  path: \"{}\"\n",
                data_dir.path().display()
            ));
            if self.encryption_key.is_some() {
                config.push_str("backend_encryption:\n  mode: aes256-gcm-proxy\n");
            }
            // Append any extra storage-level YAML (replication rules
            // etc). Emitted at root because the generated config is
            // the flat shape where `replication:` is a root key.
            if let Some(ref yaml) = self.extra_storage_yaml {
                config.push_str(yaml);
            }
            if let Some(ref yaml) = self.extra_root_yaml {
                config.push_str(yaml);
            }
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

// === Shared HTTP helpers (raw S3 requests) ===
//
// Each takes `&impl S3Requests`: `server.http()` (signed when auth is on) or
// a plain reqwest client (unsigned, open-access servers only).

/// Build an S3 object URL from endpoint, bucket, and key.
fn object_url(endpoint: &str, bucket: &str, key: &str) -> String {
    format!("{}/{}/{}", endpoint, bucket, key)
}

/// PUT an object via reqwest and return the response.
pub async fn put_object(
    client: &impl S3Requests,
    endpoint: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
    content_type: &str,
) -> reqwest::Response {
    let url = object_url(endpoint, bucket, key);
    let resp = client
        .s3_request(reqwest::Method::PUT, &url)
        .header("content-type", content_type)
        .body(data)
        .send()
        .await
        .expect("PUT failed");
    if !resp.status().is_success() {
        let st = resp.status();
        let body = resp.text().await.unwrap_or_default();
        panic!("PUT {} failed: {} body={}", key, st, body);
    }
    resp
}

/// PUT an object and return the x-amz-storage-type header value.
pub async fn put_and_get_storage_type(
    client: &impl S3Requests,
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
    client: &impl S3Requests,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> Vec<u8> {
    let url = object_url(endpoint, bucket, key);
    let resp = client
        .s3_request(reqwest::Method::GET, &url)
        .send()
        .await
        .expect("GET failed");
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
    client: &impl S3Requests,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> reqwest::header::HeaderMap {
    let url = object_url(endpoint, bucket, key);
    let resp = client
        .s3_request(reqwest::Method::HEAD, &url)
        .send()
        .await
        .expect("HEAD failed");
    assert!(
        resp.status().is_success(),
        "HEAD {} failed: {}",
        key,
        resp.status()
    );
    resp.headers().clone()
}

/// DELETE an object via reqwest (tolerates 204 and 404).
pub async fn delete_object(client: &impl S3Requests, endpoint: &str, bucket: &str, key: &str) {
    let url = object_url(endpoint, bucket, key);
    let resp = client
        .s3_request(reqwest::Method::DELETE, &url)
        .send()
        .await
        .expect("DELETE failed");
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

/// Read the usage-scan refresh counter (`GET …/usage-scan-version`), bumped
/// after every completed usage-scan cache insert. Sibling of
/// [`get_iam_version`]; used by [`wait_for_usage_scan_refresh`].
pub async fn get_usage_scan_version(client: &reqwest::Client, endpoint: &str) -> u64 {
    let resp = client
        .get(format!("{endpoint}/_/api/admin/usage-scan-version"))
        .send()
        .await
        .expect("usage-scan-version GET");
    assert!(
        resp.status().is_success(),
        "usage-scan-version must return 2xx, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("usage-scan-version JSON");
    body["version"].as_u64().expect("version is u64")
}

/// Wait until the usage-scan refresh counter advances past `baseline` — i.e.
/// a background scan has completed and inserted its result into the cache.
/// Replaces the blind `sleep(500ms)×N` polling the quota tests used to use
/// (CLAUDE.md testability rule: observable counter over sleep). `deadline_secs`
/// defaults to 15 because a scan lists the prefix (heavier than an IAM rebuild)
/// and the TTL-shortened test scans re-trigger on each stale probe.
///
/// Call pattern: capture `baseline` BEFORE the action that triggers a scan
/// (a PUT's `check_quota` → `get_or_scan` enqueues one when the cache is
/// missing/stale), then `wait_for_usage_scan_refresh(&http, &endpoint, baseline)`.
pub async fn wait_for_usage_scan_refresh(client: &reqwest::Client, endpoint: &str, baseline: u64) {
    wait_for_usage_scan_refresh_within(client, endpoint, baseline, Duration::from_secs(15)).await
}

pub async fn wait_for_usage_scan_refresh_within(
    client: &reqwest::Client,
    endpoint: &str,
    baseline: u64,
    deadline_dur: Duration,
) {
    let deadline = std::time::Instant::now() + deadline_dur;
    let mut attempts = 0u32;
    loop {
        let current = get_usage_scan_version(client, endpoint).await;
        if current > baseline {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "wait_for_usage_scan_refresh timed out after {deadline_dur:?}: \
                 baseline={baseline}, current={current}, attempts={attempts} — no \
                 usage scan completed; either nothing triggered get_or_scan or \
                 the counter isn't being bumped"
            );
        }
        attempts += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

/// Bounded, NON-panicking variant of [`wait_for_usage_scan_refresh`]: waits up
/// to `max` for the counter to advance past `baseline`, returning the highest
/// version seen (== `baseline` if no scan completed in the window). For retry
/// loops that must fall through on no-advance (e.g. the first iteration before
/// any scan is triggered) instead of panicking — replaces the old blind
/// `sleep(500ms)` polls in the quota tests with a signal-driven wait that still
/// preserves their retry-until-cache-reflects-reality structure.
pub async fn wait_usage_scan_refresh_bounded(
    client: &reqwest::Client,
    endpoint: &str,
    baseline: u64,
    max: Duration,
) -> u64 {
    let deadline = std::time::Instant::now() + max;
    loop {
        let current = get_usage_scan_version(client, endpoint).await;
        if current > baseline {
            return current;
        }
        if std::time::Instant::now() >= deadline {
            return current;
        }
        sleep(Duration::from_millis(20)).await;
    }
}

/// Read the event-driven replication drain counter
/// (`GET …/jobs/replication-event-version`), bumped each time the event
/// consumer advances its cursor after handling real events.
pub async fn get_replication_event_version(client: &reqwest::Client, endpoint: &str) -> u64 {
    let resp = client
        .get(format!(
            "{endpoint}/_/api/admin/jobs/replication-event-version"
        ))
        .send()
        .await
        .expect("replication-event-version GET");
    assert!(
        resp.status().is_success(),
        "replication-event-version must return 2xx (the route is public — no auth needed), got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("event-version JSON");
    body["version"].as_u64().expect("version is u64")
}

/// Wait until the event consumer has drained at least once past `baseline`.
/// Deadline is generous (35s) because the consumer ticks on its own interval
/// (≈5s) — the barrier replaces a `for _ in 0..30 { sleep(1s); get_object }`
/// loop, so a settled drain (not S3 polling) is the observable. Panics on
/// timeout so a broken consumer fails loudly.
pub async fn wait_for_replication_event(client: &reqwest::Client, endpoint: &str, baseline: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(35);
    loop {
        if get_replication_event_version(client, endpoint).await > baseline {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "wait_for_replication_event timed out after 35s (baseline={baseline}) — \
                 the event consumer didn't drain (cursor never advanced)"
            );
        }
        sleep(Duration::from_millis(200)).await;
    }
}

/// Poll a replication rule's run history until the latest run reaches a terminal
/// status, then return that run object. `run-now` is fire-and-forget (202) — a
/// large sync can't block the HTTP response — so tests assert on the settled
/// run-history row (`objects_processed`, `status`), not the run-now response.
///
/// Firing run-now MORE THAN ONCE in a test? Baseline with [`latest_run_id`]
/// before the fire and use [`wait_for_run_after`] — the new run's history row
/// only appears once its background task starts, so this max-id variant can
/// return the PREVIOUS run's terminal row.
pub async fn wait_for_run(
    admin: &reqwest::Client,
    endpoint: &str,
    rule: &str,
) -> serde_json::Value {
    wait_for_run_after(admin, endpoint, rule, -1).await
}

/// `id` of the newest run in a rule's history, or 0 when none exists yet.
pub async fn latest_run_id(admin: &reqwest::Client, endpoint: &str, rule: &str) -> i64 {
    let url = format!("{endpoint}/_/api/admin/jobs/replication:{rule}/runs");
    let h: serde_json::Value = admin.get(&url).send().await.unwrap().json().await.unwrap();
    h["runs"]
        .as_array()
        .and_then(|r| r.iter().filter_map(|x| x["id"].as_i64()).max())
        .unwrap_or(0)
}

/// Like [`wait_for_run`] but only accepts a run with `id > after_id` — the
/// baseline that makes back-to-back run-now assertions race-free.
pub async fn wait_for_run_after(
    admin: &reqwest::Client,
    endpoint: &str,
    rule: &str,
    after_id: i64,
) -> serde_json::Value {
    let url = format!("{endpoint}/_/api/admin/jobs/replication:{rule}/runs");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let h: serde_json::Value = admin.get(&url).send().await.unwrap().json().await.unwrap();
        if let Some(run) = h["runs"].as_array().and_then(|r| {
            r.iter()
                .max_by_key(|x| x["id"].as_i64().unwrap_or(i64::MIN))
        }) {
            let id = run["id"].as_i64().unwrap_or(0);
            let st = run["status"].as_str().unwrap_or("");
            if id > after_id
                && matches!(
                    st,
                    "succeeded" | "failed" | "completed_with_errors" | "cancelled" | "stopped"
                )
            {
                return run.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "new run (id > {after_id}) for rule '{rule}' did not settle in 60s; last history: {h}"
        );
        sleep(Duration::from_millis(200)).await;
    }
}

/// Poll a job's run history until the run `run_id` reaches a terminal status,
/// then return that run-history row. `job` is the unified job id
/// (`lifecycle:<rule>`, `replication:<rule>`).
pub async fn wait_for_job_run(
    admin: &reqwest::Client,
    endpoint: &str,
    job: &str,
    run_id: i64,
) -> serde_json::Value {
    let url = format!("{endpoint}/_/api/admin/jobs/{job}/runs");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let h: serde_json::Value = admin.get(&url).send().await.unwrap().json().await.unwrap();
        if let Some(run) = h["runs"]
            .as_array()
            .and_then(|r| r.iter().find(|x| x["id"].as_i64() == Some(run_id)))
        {
            if !matches!(
                run["status"].as_str(),
                Some("running" | "queued" | "cancelling")
            ) {
                return run.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run {run_id} of {job} did not settle in 60s; last history: {h}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// Fire a lifecycle rule's run-now (async: 202 + `run_id`) and wait for that
/// run to settle. Returns the run-history row (`status`, `objects_processed`,
/// `errors`, ...).
pub async fn lifecycle_run_now_and_wait(
    admin: &reqwest::Client,
    endpoint: &str,
    rule: &str,
) -> serde_json::Value {
    let resp = admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/lifecycle:{rule}/run-now"
        ))
        .send()
        .await
        .expect("run-now request");
    let code = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    assert_eq!(code, 202, "lifecycle run-now must be accepted: {body}");
    assert_eq!(body["status"].as_str(), Some("running"), "{body}");
    let run_id = body["run_id"].as_i64().expect("run-now returns run_id");
    wait_for_job_run(admin, endpoint, &format!("lifecycle:{rule}"), run_id).await
}

/// Read the proxy's external-auth (OAuth/OIDC provider) version counter.
///
/// Backed by `GET /_/api/admin/ext-auth/version`, incremented by
/// `rebuild_external_auth` (src/api/admin/external_auth.rs) AFTER the
/// rebuilt provider set is live. Sibling of [`get_iam_version`]. Used by
/// [`wait_for_ext_auth_rebuild`] as the barrier primitive for provider
/// mutations.
pub async fn get_ext_auth_version(client: &reqwest::Client, endpoint: &str) -> u64 {
    let resp = client
        .get(format!("{endpoint}/_/api/admin/ext-auth/version"))
        .send()
        .await
        .expect("ext-auth/version GET");
    assert!(
        resp.status().is_success(),
        "ext-auth/version must return 2xx, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("ext-auth/version JSON");
    body["version"].as_u64().expect("version is u64")
}

/// Wait until the proxy's external-auth rebuild counter advances past
/// `baseline`. Same call pattern as [`wait_for_iam_rebuild`] but for
/// OAuth/OIDC provider mutations (create/update/delete provider). Panics
/// after 5s — a missing counter advance means `rebuild_external_auth`
/// either didn't run or failed (and so correctly did NOT bump on error).
pub async fn wait_for_ext_auth_rebuild(client: &reqwest::Client, endpoint: &str, baseline: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut attempts = 0u32;
    loop {
        let current = get_ext_auth_version(client, endpoint).await;
        if current > baseline {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "wait_for_ext_auth_rebuild timed out after 5s: baseline={baseline}, \
                 current={current}, attempts={attempts} — provider mutation didn't \
                 trigger rebuild_external_auth or the counter isn't being bumped"
            );
        }
        attempts += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

/// Make a raw ListObjectsV2 request and return the XML body.
pub async fn list_objects_raw(
    client: &impl S3Requests,
    endpoint: &str,
    bucket: &str,
    params: &str,
) -> String {
    let url = format!("{}/{}?list-type=2&{}", endpoint, bucket, params);
    let resp = client
        .s3_request(reqwest::Method::GET, &url)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "ListObjects failed: {}",
        resp.status()
    );
    resp.text().await.unwrap()
}

// === Quick-setup helpers (reduce test boilerplate) ===

/// Quick setup: OPEN-ACCESS filesystem server + an unsigned reqwest client.
/// The client sends no SigV4, so the server must run with
/// `authentication: none`; the name keeps that visible at the call site.
pub async fn open_access_setup() -> (TestServer, reqwest::Client) {
    let server = TestServer::builder().open_access().build().await;
    let http = reqwest::Client::new();
    (server, http)
}

/// Quick setup: filesystem server (auth on) + a client that signs with its
/// credentials.
pub async fn signed_setup() -> (TestServer, S3Http) {
    let server = TestServer::filesystem().await;
    let http = server.http();
    (server, http)
}

/// Upload a simple test file, return its bytes
pub async fn upload_test_data(
    http: &impl S3Requests,
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

/// Deterministic pseudo-random, incompressible body (xorshift). Stored
/// passthrough (not delta-eligible) and large enough to span multiple
/// multipart parts. Shared by `streaming_copy_test` and
/// `large_object_e2e_test`.
pub fn big_passthrough_body(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u64 = 0x1234_5678_9abc_def0;
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

/// Parsed snapshot of the un-labelled streaming-copy metrics scraped from
/// `GET /_/metrics`. Fields map 1:1 to the `deltaglider_*` series; absent
/// lines default to 0.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub part_bytes_resident: u64,
    pub part_bytes_resident_peak: u64,
    pub parts_inflight: u64,
    pub parts_inflight_peak: u64,
    pub objects_inflight: u64,
    pub objects_inflight_peak: u64,
    pub multipart_parts_total: u64,
    pub part_retries_total: u64,
    pub bytes_streamed_total: u64,
    pub delta_bytes_saved_total: u64,
    pub delta_passthrough_bytes_saved_total: u64,
    pub list_calls_total: u64,
    pub head_calls_total: u64,
    pub dirs_completed_total: u64,
    pub process_peak_rss_bytes: u64,
}

/// Scrape `GET /_/metrics` and parse the un-labelled Prometheus lines
/// (`name value`) into a [`MetricsSnapshot`]. Snapshot ONCE after a
/// synchronous run-now returns 200 (peaks have settled).
pub async fn metrics_snapshot(endpoint: &str) -> MetricsSnapshot {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("metrics client");
    let url = format!("{}/_/metrics", endpoint);
    let body = client
        .get(&url)
        .send()
        .await
        .expect("GET /_/metrics failed")
        .text()
        .await
        .expect("read /_/metrics body");

    let mut snap = MetricsSnapshot::default();
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        // Un-labelled series: `name value`. Skip labelled lines (name{..}).
        let Some((name, value)) = line.rsplit_once(' ') else {
            continue;
        };
        if name.contains('{') {
            continue;
        }
        let parsed = value.trim().parse::<f64>().unwrap_or(0.0) as u64;
        match name {
            "deltaglider_replication_list_calls_total" => snap.list_calls_total = parsed,
            "deltaglider_replication_head_calls_total" => snap.head_calls_total = parsed,
            "deltaglider_replication_dirs_completed_total" => snap.dirs_completed_total = parsed,
            "deltaglider_replication_part_bytes_resident" => snap.part_bytes_resident = parsed,
            "deltaglider_replication_part_bytes_resident_peak" => {
                snap.part_bytes_resident_peak = parsed
            }
            "deltaglider_replication_parts_inflight" => snap.parts_inflight = parsed,
            "deltaglider_replication_parts_inflight_peak" => snap.parts_inflight_peak = parsed,
            "deltaglider_replication_objects_inflight" => snap.objects_inflight = parsed,
            "deltaglider_replication_objects_inflight_peak" => snap.objects_inflight_peak = parsed,
            "deltaglider_replication_multipart_parts_total" => snap.multipart_parts_total = parsed,
            "deltaglider_replication_part_retries_total" => snap.part_retries_total = parsed,
            "deltaglider_replication_bytes_streamed_total" => snap.bytes_streamed_total = parsed,
            "deltaglider_delta_bytes_saved_total" => snap.delta_bytes_saved_total = parsed,
            "deltaglider_replication_delta_passthrough_bytes_saved_total" => {
                snap.delta_passthrough_bytes_saved_total = parsed
            }
            "process_peak_rss_bytes" => snap.process_peak_rss_bytes = parsed,
            _ => {}
        }
    }
    snap
}

pub async fn metrics_text(endpoint: &str) -> String {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("metrics client");
    let url = format!("{}/_/metrics", endpoint);
    client
        .get(&url)
        .send()
        .await
        .expect("GET /_/metrics failed")
        .text()
        .await
        .expect("read /_/metrics body")
}

pub fn prometheus_counter_has_labels(metrics: &str, name: &str, labels: &[&str]) -> bool {
    metrics.lines().any(|line| {
        if !line.starts_with(name) || labels.iter().any(|label| !line.contains(label)) {
            return false;
        }
        line.rsplit_once(' ')
            .and_then(|(_, value)| value.trim().parse::<f64>().ok())
            .map(|value| value > 0.0)
            .unwrap_or(false)
    })
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

/// Walk a filesystem-backend data directory and return every file's
/// `user.dg.metadata` xattr parsed as JSON. The key matches
/// `src/storage/xattr_meta.rs::XATTR_NAME` — tests depend on the
/// concrete name rather than the constant so they also catch the
/// case where the constant is accidentally renamed without a test
/// update.
///
/// Files without a `user.dg.metadata` xattr are skipped silently
/// (directories, partial writes, CAS-style staged blobs, etc.).
/// Shared with the encryption integration suite so we don't have N
/// copies of the same walkdir + xattr::get + serde_json::parse
/// ladder.
#[cfg(unix)]
pub fn read_xattr_metadata(
    data_dir: &std::path::Path,
) -> Vec<(std::path::PathBuf, serde_json::Value)> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(data_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let bytes = match xattr::get(entry.path(), "user.dg.metadata") {
            Ok(Some(b)) => b,
            _ => continue,
        };
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            out.push((entry.path().to_path_buf(), v));
        }
    }
    out
}

impl TestServer {
    /// Kill + respawn against the SAME config file, data dir, and port —
    /// with `extra` env vars applied AFTER the default `env_remove` calls
    /// (so a test can inject e.g. `DGP_BOOTSTRAP_PASSWORD_HASH`).
    /// Stop the proxy process (the data dir and config stay). A test edits
    /// on-disk state here, then calls `respawn_with_env`.
    pub fn kill(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }

    pub async fn respawn_with_env(&mut self, extra: &[(&str, &str)]) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        // Poll until the kernel releases the listening socket (mirrors
        // `respawn_without_encryption_key`); bounded to ~2s.
        let addr = format!("127.0.0.1:{}", self.port);
        let mut rebind_ok = false;
        for _ in 0..40 {
            match std::net::TcpListener::bind(&addr) {
                Ok(listener) => {
                    drop(listener);
                    rebind_ok = true;
                    break;
                }
                Err(_) => sleep(Duration::from_millis(50)).await,
            }
        }
        assert!(
            rebind_ok,
            "port {} did not free within ~2s after killing the old child; \
             another process may be holding it (try `lsof -i :{}`)",
            self.port, self.port
        );

        let mut cmd = proxy_command(&self.config_path, self.production_security);
        for (key, value) in &self.extra_env {
            cmd.env(key, value);
        }
        // `extra` LAST so it wins over the removals above.
        for (key, value) in extra {
            cmd.env(key, value);
        }
        self.process = cmd.spawn().expect("Failed to respawn server");
        self.wait_ready().await;
    }
}

// SPDX-License-Identifier: BUSL-1.1

//! The proxy child process: port leases, the shared test credentials,
//! `TestServer` and its builder, spawn, stop and respawn.

use super::*;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::Client;
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

/// Stands for `127.0.0.1:<leased port>` in a document passed to
/// [`TestServer::from_config_document`].
pub const LISTEN_ADDR_PLACEHOLDER: &str = "__DGP_TEST_LISTEN_ADDR__";

/// `DGP_CONFIG_DB_KEY` for every server built with a config sync bucket: HA
/// replicas share one encrypted DB, so they need one key (the proxy refuses
/// to start with a sync bucket and no env key). A test that wants a replica
/// with another key sets `.env("DGP_CONFIG_DB_KEY", ...)`.
pub const TEST_CONFIG_DB_KEY: &str = "test-config-db-key-0123456789abcdef0123456789abcdef";

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
    /// The listener speaks HTTPS (see [`TestServerBuilder::tls`]).
    tls: bool,
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
        // The test client is the "reverse proxy": its X-Forwarded-For names
        // the client, so tests key per-IP state on distinct addresses.
        .env("DGP_TRUST_PROXY_HEADERS", "true")
        .env("DGP_TRUSTED_PROXY_CIDRS", "127.0.0.0/8,::1/128")
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

    // ── Shared spawn logic ──

    /// Allocate a port, write a YAML config, spawn the proxy, wait for readiness,
    /// and create the test bucket. All factory methods delegate here.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_with_config(
        config_body: &str,
        bucket: &str,
        data_dir: Option<TempDir>,
        auth_creds: Option<(String, String)>,
        encryption_key: Option<String>,
        extra_env: Vec<(String, String)>,
        production_security: bool,
        tls: bool,
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
            tls,
        };
        server.wait_ready().await;
        // The SDK client does not trust the test certificate; TLS tests
        // make their own requests.
        if !tls {
            server.ensure_bucket().await;
        }
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
            false,
        )
        .await
    }

    // ── Instance methods ──

    async fn wait_ready(&mut self) {
        // Use the health endpoint instead of raw TCP connect — the HTTP server
        // may accept TCP connections before routes and middleware are fully
        // initialized, causing "connection refused" on the first real request.
        let health_url = format!("{}/_/health", self.endpoint());
        let client = reqwest::Client::builder()
            .no_proxy()
            // Readiness only: the TLS tests verify the certificate themselves.
            .danger_accept_invalid_certs(self.tls)
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
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://127.0.0.1:{}", self.port)
    }

    /// Get the bucket name
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Path of the config file the server was spawned with. Tests can read
    /// this to verify that admin-API config mutations persist to the
    /// correct file (regression coverage for the `backends.rs`
    /// hardcoded-default-filename bug).
    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    /// Get the data directory path (filesystem backend only)
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
        // `proxy_command` strips any inherited DGP_ENCRYPTION_KEY.
        self.respawn_with_env(&[]).await;
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
    /// See [`Self::tls`].
    tls: Option<TestTls>,
    /// See [`Self::client_credentials_only`].
    omit_bootstrap_creds: bool,
}

/// TLS mode of a test listener.
#[derive(Clone)]
pub enum TestTls {
    /// `tls.enabled: true` with no files: the proxy generates a certificate.
    SelfSigned,
    /// A user-provided PEM certificate and key.
    Pem { cert_path: String, key_path: String },
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
            tls: None,
            omit_bootstrap_creds: false,
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

    /// The test's S3 clients sign with this pair, but the config carries
    /// no bootstrap SigV4 pair and no `authentication` field: the pair must
    /// come from IAM (e.g. declarative `iam_users`).
    pub fn client_credentials_only(mut self, access_key_id: &str, secret_access_key: &str) -> Self {
        self.auth_creds = Some((access_key_id.to_string(), secret_access_key.to_string()));
        self.omit_bootstrap_creds = true;
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
    /// Serve HTTPS (YAML `tls:` block). [`TestServer::endpoint`] then
    /// returns an `https://` URL, and the builder does not create the
    /// test bucket (the SDK client does not trust the test certificate).
    pub fn tls(mut self, tls: TestTls) -> Self {
        self.tls = Some(tls);
        self
    }

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
            self.tls.is_some(),
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
        match &self.tls {
            None => {}
            Some(TestTls::SelfSigned) => config.push_str("tls:\n  enabled: true\n"),
            Some(TestTls::Pem {
                cert_path,
                key_path,
            }) => config.push_str(&format!(
                "tls:\n  enabled: true\n  cert_path: \"{cert_path}\"\n  key_path: \"{key_path}\"\n"
            )),
        }
        if self.omit_bootstrap_creds {
            // Neither a bootstrap pair nor `authentication: none`.
        } else if let Some((ref key_id, ref secret)) = self.auth_creds {
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
        stop_child(&mut self.process);
    }
}

/// Stop a proxy child. Normally SIGKILL (fast). Under coverage
/// (`LLVM_PROFILE_FILE` set, e.g. `cargo llvm-cov`) a killed child writes
/// no profile, so it gets SIGTERM, which the proxy handles with a graceful
/// shutdown and a normal exit, and SIGKILL only after 10 s.
fn stop_child(child: &mut Child) {
    if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        sigterm_then_wait(child);
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// SIGTERM, wait up to 10 s for the graceful exit, then SIGKILL.
fn sigterm_then_wait(child: &mut Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    // SAFETY: plain kill(2) on our own child's pid.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl TestServer {
    /// Stop the proxy process (the data dir and config stay). A test edits
    /// on-disk state here, then calls `respawn_with_env`.
    pub fn kill(&mut self) {
        stop_child(&mut self.process);
    }

    /// Stop the proxy the way Kubernetes does on a rolling deploy: SIGTERM,
    /// then wait for the graceful exit (SIGKILL only after 10 s). Unlike
    /// [`Self::kill`], this sends SIGTERM in every run, not only under
    /// coverage. Follow with `respawn_with_env`.
    pub fn terminate(&mut self) {
        sigterm_then_wait(&mut self.process);
    }

    /// Kill + respawn against the SAME config file, data dir, and port —
    /// with `extra` env vars applied AFTER the default `env_remove` calls
    /// (so a test can inject e.g. `DGP_BOOTSTRAP_PASSWORD_HASH`).
    pub async fn respawn_with_env(&mut self, extra: &[(&str, &str)]) {
        stop_child(&mut self.process);
        // Poll until the kernel releases the listening socket. A fixed
        // sleep was racy on slow hosts (EADDRINUSE); bounded to ~2s so a
        // stuck port panics loudly.
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

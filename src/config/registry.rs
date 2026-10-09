// SPDX-License-Identifier: BUSL-1.1

//! The `DGP_*` environment variable registry.

/// A single entry in the environment variable registry.
pub struct EnvVarEntry {
    /// The environment variable name (e.g. `DGP_LISTEN_ADDR`)
    pub name: &'static str,
    /// Short human-readable description
    pub description: &'static str,
    /// Example value
    pub example: &'static str,
    /// Grouping category for display
    pub category: &'static str,
}

/// The standard `tracing` filter variable. It is not a `DGP_*` setting, so it
/// is not in [`ENV_VAR_REGISTRY`], but it overrides `advanced.log_level`.
pub const RUST_LOG: &str = "RUST_LOG";

/// Single source of truth for every `DGP_*` environment variable.
///
/// `every_dgp_literal_in_src_is_registered` scans `src/` and fails when a
/// `DGP_*` literal is missing here, or an entry here is read nowhere.
pub const ENV_VAR_REGISTRY: &[EnvVarEntry] = &[
    // ── Server ──────────────────────────────────────────────
    EnvVarEntry {
        name: "DGP_LISTEN_ADDR",
        description: "Listen address (ip:port)",
        example: "0.0.0.0:9000",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_LOG_LEVEL",
        description: "Log level filter (overridden by RUST_LOG)",
        example: "deltaglider_proxy=info,tower_http=info",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_LOG_FORMAT",
        description: "Log output format: 'text' (default, human-readable) or 'json' (one JSON object per line, greppable with jq). Startup-only — not hot-reloadable.",
        example: "json",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_LOG_RING_SIZE",
        description: "Max entries in the in-memory log ring powering the admin GUI log viewer (default: 2000).",
        example: "2000",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_LOG_RING_LEVEL",
        description: "Minimum severity captured into the GUI log ring/stream (error|warn|info|debug|trace; default: info). The ring sees only events that the global log filter (log_level) lets through, so this floor can narrow it but not widen it.",
        example: "info",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_CODEC_CONCURRENCY",
        description: "Max concurrent delta encode/decode ops (default: 4 per CPU core, at least 16)",
        example: "4",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_BLOCKING_THREADS",
        description: "Max tokio blocking threads (default: 512)",
        example: "64",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_CONFIG",
        description: "Path to YAML config file",
        example: "/etc/deltaglider_proxy/config.yaml",
        category: "Server",
    },
    // ── Delta engine ────────────────────────────────────────
    EnvVarEntry {
        name: "DGP_MAX_DELTA_RATIO",
        description: "Max delta/original ratio to keep a delta (0.0–1.0)",
        example: "0.5",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_MAX_OBJECT_SIZE",
        description: "Max object size in bytes for delta processing",
        example: "104857600",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_MAX_PASSTHROUGH_OBJECT_SIZE",
        description: "Max passthrough object size in bytes for the streaming multipart copy path",
        example: "68719476736",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_FILTERED_LIST_MAX_ENGINE_PAGES",
        description: "Backend pages one filtered LIST may scan for a visible key before it fails with InvalidRequest (default: 50)",
        example: "50",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_RANGE_SPOOL_TTL_SECS",
        description: "Seconds a verified reconstruction of a large delta object stays cached for further range reads (0 = off; default: 60)",
        example: "60",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_STREAM_COPY_THRESHOLD",
        description: "Object size (bytes) at/above which passthrough copies stream via multipart",
        example: "67108864",
        category: "Replication",
    },
    EnvVarEntry {
        name: "DGP_MULTIPART_PART_SIZE",
        description: "Multipart part size (bytes) for the streaming copy path",
        example: "67108864",
        category: "Replication",
    },
    EnvVarEntry {
        name: "DGP_UPLOAD_CONCURRENCY",
        description: "In-flight parts per streaming multipart object copy. Overrides storage.replication.upload_concurrency (clamped 1-16) and is the default for lifecycle and migrate copies",
        example: "4",
        category: "Replication",
    },
    EnvVarEntry {
        name: "DGP_REPLICATION_TRANSFERS",
        description: "Concurrent objects per replication run (rclone --transfers). Overrides storage.replication.transfers (clamped 1-64)",
        example: "4",
        category: "Replication",
    },
    EnvVarEntry {
        name: "DGP_BACKEND_REQUEST_TIMEOUT_SECS",
        description: "Deadline in seconds for one S3-backend request without a large body (HEAD, GET until the first byte, LIST, DELETE), retries included. A backend that does not answer in time gets a 503 naming it, and it is marked unhealthy until the next health probe succeeds. Uploads and server-side copies are not capped. 0 turns it off (default: 30)",
        example: "30",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_BOOT_CREATE_DECLARED_BUCKETS",
        description: "At boot, create every bucket declared under storage.buckets that its backend does not have: a directory on a filesystem backend, a CreateBucket on an S3 backend (after a HeadBucket). false turns this off for every backend (default: true)",
        example: "false",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_BACKEND_HEALTH_INTERVAL_SECS",
        description: "How often every storage backend is health-probed (seconds). An unhealthy backend's buckets answer 503 until a probe succeeds. 0 turns the probe loop off, and so does DGP_BOOT_BACKEND_PROBE=off (default: 30)",
        example: "30",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_S3_READ_TIMEOUT_SECS",
        description: "S3 client per-attempt read timeout (seconds)",
        example: "60",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_S3_CONNECT_TIMEOUT_SECS",
        description: "S3 client connect timeout (seconds)",
        example: "10",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_S3_OPERATION_ATTEMPT_TIMEOUT_SECS",
        description: "S3 client per-attempt operation timeout (seconds)",
        example: "300",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_S3_STALL_GRACE_SECS",
        description: "S3 stalled-stream protection grace period (seconds)",
        example: "20",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_PARITY_HEAD_CONCURRENCY",
        description: "Concurrent HEADs during a replication Verify (parity) audit \
                      on an S3 backend; higher is faster, lower is gentler on a \
                      throttling backend (clamped 1-64)",
        example: "15",
        category: "Replication",
    },
    EnvVarEntry {
        name: "DGP_PARITY_MAX_OBJECTS",
        description: "Max objects a replication Verify (parity) audit scans across \
                      both sides before it caps and reports a partial result; a \
                      runaway-scan safety ceiling (default 1000000, ≈500k/side), \
                      raise for even larger mirrors (min 1000)",
        example: "1000000",
        category: "Replication",
    },
    EnvVarEntry {
        name: "DGP_BOOT_BACKEND_PROBE",
        description: "Boot-time backend health gate: 'enforce' (default) probes every \
                      configured backend's connectivity+credentials and refuses to start \
                      when ALL fail; 'warn' probes and logs but never exits; 'off' skips \
                      probing. Unhealthy backends' buckets answer 503 until recovery",
        example: "enforce",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_BACKEND_LIST_COOLDOWN_SECS",
        description: "How long a backend that fails a bucket listing is skipped \
                      (served from last-known-good) before the next re-probe",
        example: "30",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_BACKEND_LIST_TIMEOUT_SECS",
        description: "Per-backend timeout for a single bucket-listing call \
                      (bounds a hung backend)",
        example: "5",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_BACKEND_LIST_FRESH_SECS",
        description: "How long a successful bucket listing is served without \
                      re-probing upstream (coalesces the GUI's paired \
                      ListBuckets + origins calls); 0 disables",
        example: "5",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_CACHE_MB",
        description: "Reference cache size in MB",
        example: "100",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_METADATA_CACHE_MB",
        description: "Metadata cache size in MB (object metadata, eliminates HEAD requests)",
        example: "50",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_LIST_SIZE_CACHE_MB",
        description: "Listing-size cache size in MB (original sizes of stored deltas for LIST, no TTL; default: 32)",
        example: "32",
        category: "Delta Engine",
    },
    // ── Filesystem backend ──────────────────────────────────
    EnvVarEntry {
        name: "DGP_DATA_DIR",
        description: "Data directory (activates filesystem backend)",
        example: "./data",
        category: "Filesystem Backend",
    },
    // ── S3 backend ──────────────────────────────────────────
    EnvVarEntry {
        name: "DGP_S3_ENDPOINT",
        description: "S3 endpoint URL (activates S3 backend)",
        example: "http://localhost:9000",
        category: "S3 Backend",
    },
    EnvVarEntry {
        name: "DGP_S3_REGION",
        description: "AWS region",
        example: "us-east-1",
        category: "S3 Backend",
    },
    EnvVarEntry {
        name: "DGP_S3_PATH_STYLE",
        description: "Use path-style URLs (true/1 for MinIO/LocalStack)",
        example: "true",
        category: "S3 Backend",
    },
    EnvVarEntry {
        name: "DGP_BACKEND_ALLOW_LOCAL",
        description: "Allow http:// and private-IP endpoints for the S3 backend (MinIO, dev, CI)",
        example: "false",
        category: "S3 Backend",
    },
    EnvVarEntry {
        name: "DGP_BE_AWS_ACCESS_KEY_ID",
        description: "AWS access key for S3 backend",
        example: "minioadmin",
        category: "S3 Backend",
    },
    EnvVarEntry {
        name: "DGP_BE_AWS_SECRET_ACCESS_KEY",
        description: "AWS secret key for S3 backend",
        example: "minioadmin",
        category: "S3 Backend",
    },
    // ── Authentication ──────────────────────────────────────
    EnvVarEntry {
        name: "DGP_AUTHENTICATION",
        description:
            "Auth mode: omit to auto-detect (requires credentials), or \"none\" for open access",
        example: "none",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_ACCESS_KEY_ID",
        description: "Proxy access key (enables SigV4 auth when both set)",
        example: "my-access-key",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_SECRET_ACCESS_KEY",
        description: "Proxy secret key (enables SigV4 auth when both set)",
        example: "my-secret-key",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_BOOTSTRAP_PASSWORD_HASH",
        description: "Bcrypt hash of the bootstrap password (admin GUI login and session signing)",
        example: "$2b$12$...",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_CONFIG_DB_KEY",
        description: "Encryption key of the IAM config DB, at least 32 characters (default: key file next to the DB, generated on first boot). Required, and the same on every instance, when config_sync_bucket is set",
        example: "<openssl rand -hex 32>",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_CONFIG_DB_KEY_PREVIOUS",
        description: "Previous DGP_CONFIG_DB_KEY during a key rotation: a config DB (or synced copy) that opens only with it is re-encrypted with DGP_CONFIG_DB_KEY at boot or sync. Remove it after the rotation",
        example: "<the old DGP_CONFIG_DB_KEY>",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC",
        description: "Transitional, for a rolling upgrade from a release before DGP_CONFIG_DB_KEY: accept a synced config DB that opens only with the bootstrap password hash (default false). Remove it when every instance runs this release",
        example: "true",
        category: "Authentication",
    },
    // ── TLS ─────────────────────────────────────────────────
    EnvVarEntry {
        name: "DGP_TLS_ENABLED",
        description: "Enable TLS (true/1)",
        example: "true",
        category: "TLS",
    },
    EnvVarEntry {
        name: "DGP_TLS_CERT",
        description: "Path to PEM certificate (auto-generates self-signed if omitted)",
        example: "/etc/ssl/certs/proxy.pem",
        category: "TLS",
    },
    EnvVarEntry {
        name: "DGP_TLS_KEY",
        description: "Path to PEM private key",
        example: "/etc/ssl/private/proxy-key.pem",
        category: "TLS",
    },
    // ── Config DB Sync ─────────────────────────────────────
    EnvVarEntry {
        name: "DGP_CONFIG_SYNC_BUCKET",
        description: "S3 bucket for config DB sync (enables multi-instance IAM sync)",
        example: "my-config-bucket",
        category: "Config Sync",
    },
    EnvVarEntry {
        name: "DGP_CONFIG_SYNC_KEY",
        description: "Overrides config_sync_object_key / advanced.config_sync_object_key (default object: .deltaglider/config.db)",
        example: ".deltaglider/config.db",
        category: "Config Sync",
    },
    // ── Security / Runtime ─────────────────────────────────
    EnvVarEntry {
        name: "DGP_DEBUG_HEADERS",
        description: "Expose debug/fingerprinting headers (x-amz-storage-type etc.)",
        example: "true",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_TRUST_PROXY_HEADERS",
        description: "Trust X-Forwarded-For/X-Real-IP from the DGP_TRUSTED_PROXY_CIDRS peers (required with true; boot refuses otherwise) for the client IP: rate limits, sessions, admission source_ip, aws:SourceIp",
        example: "false",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_TRUSTED_PROXY_CIDRS",
        description: "Comma-separated CIDRs of trusted reverse proxies; XFF honored only from these peers. Required when DGP_TRUST_PROXY_HEADERS=true",
        example: "10.0.0.0/8,192.168.1.5",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_CONFIG_ENV_ALLOWLIST",
        description: "Comma-separated env var names (a trailing * matches a prefix) that admin config apply, import and restore may resolve as ${env:NAME}, beyond those the boot config uses. A * pattern never matches a DGP_* name (list it exactly), and a DGP_* name containing BOOTSTRAP_, ENCRYPTION_KEY, SECRET, DB_KEY, PASSWORD or TOKEN never matches",
        example: "LOG_LEVEL,APP_*",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_SESSION_TTL_HOURS",
        description: "Admin session TTL in hours (default: 4)",
        example: "4",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_MAX_MULTIPART_UPLOADS",
        description: "Max concurrent multipart uploads (default: 1000)",
        example: "1000",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_MULTIPART_SWEEP_INTERVAL_SECS",
        description: "Multipart sweeper interval in seconds (default: 300)",
        example: "300",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_BUCKET_USAGE_FLUSH_SECS",
        description: "Bucket-usage counter flush interval in seconds (default: 10)",
        example: "10",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_MULTIPART_SWEEP_MAX_AGE_SECS",
        description: "Multipart max age cutoff for Open uploads in seconds (default: 3600)",
        example: "3600",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_MULTIPART_COMPLETING_TIMEOUT_SECS",
        description: "Multipart Completing-state timeout in seconds (default: sweep max age)",
        example: "3600",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_MAX_TOTAL_MULTIPART_BYTES",
        description: "Cap on total buffered multipart bytes across all uploads (default: max_object_size * max_uploads / 4)",
        example: "1073741824",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_MULTIPART_IDLE_TTL_HOURS",
        description: "Idle multipart upload TTL in hours before garbage collection (default: 24)",
        example: "24",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_AUDIT_RING_SIZE",
        description: "In-memory audit-log ring buffer capacity (default: 500)",
        example: "500",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_CLOCK_SKEW_SECONDS",
        description: "SigV4 clock skew tolerance in seconds (default: 900)",
        example: "900",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_MAX_CONCURRENT_REQUESTS",
        description: "Max concurrent HTTP requests (tower ConcurrencyLimit, default: 1024)",
        example: "1024",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_CORS_PERMISSIVE",
        description: "Enable permissive CORS for dev mode (default: false)",
        example: "true",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_METRICS_BEARER_TOKEN",
        description: "When set, /_/metrics requires `Authorization: Bearer <token>` (Prometheus `authorization:` scrape setting) or an admin session; unset keeps the scrape endpoint public",
        example: "a-long-random-scrape-token",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_METRICS_EXPOSE_VERSION",
        description: "Put the exact build version in the `version` label of `deltaglider_build_info` on the unauthenticated /_/metrics endpoint (default: false — the label is empty; the version stays available through the authenticated admin API)",
        example: "true",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_REQUEST_TIMEOUT_SECS",
        description: "Per-request timeout in seconds (default: 300)",
        example: "300",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_READY_TIMEOUT_SECS",
        description: "Per-attempt backend timeout for the /_/ready probe (default: 3)",
        example: "3",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_READY_CACHE_TTL_SECS",
        description: "Last-known-good window for /_/ready. 0 (default) = strict: the ListBuckets probe must succeed. When >0, a throttled list falls back to a cheap HEAD reachability check, then to a backend call that succeeded within this many seconds — so a provider LIST throttle doesn't pull a serving node out of rotation",
        example: "300",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_READY_RETRIES",
        description: "Extra /_/ready backend attempts after the first before reporting not-ready, with a short backoff (default: 2)",
        example: "2",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_CODEC_TIMEOUT_SECS",
        description: "xdelta3 subprocess timeout in seconds for the buffered path (default: 60)",
        example: "60",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_CODEC_STALL_SECS",
        description: "Streaming codec: kill xdelta3 if no stdout progress for this long (default: 30)",
        example: "30",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_CODEC_ABSOLUTE_SECS",
        description: "Streaming codec: absolute ceiling for one op, regardless of progress (default: 7200)",
        example: "7200",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_SPOOL_DIR",
        description: "Directory for every scratch file: codec files, multipart relay parts, encrypted-write temps (default: <system temp>/dgp-spool)",
        example: "/var/lib/deltaglider/spool",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_SPOOL_MAX_BYTES",
        description: "Byte budget for all spool files; a request that holds none waits, a holder gets 503 SlowDown (default: 16 GiB)",
        example: "17179869184",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_LISTING_FACTS_GC",
        description: "Periodic removal (every 6 h) of S3 listing-facts entries whose object is gone (default: true)",
        example: "false",
        category: "S3 Backend",
    },
    EnvVarEntry {
        name: "DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES",
        description: "Max spool bytes one relayed multipart upload may hold (default: half of DGP_SPOOL_MAX_BYTES; 0 = no per-upload cap)",
        example: "8589934592",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_SPOOL_THRESHOLD_BYTES",
        description: "Delta objects larger than this reconstruct (GET) or encode (PUT) through a spool file (default: 16 MiB, capped at max_object_size)",
        example: "16777216",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_SPOOL_ACQUIRE_TIMEOUT_SECS",
        description: "Max wait in seconds for spool budget before a request that needs spool space (a large PUT or POST, a copy, a delta GET) fails with 503 SlowDown (default: 120)",
        example: "120",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_RATE_LIMIT_MAX_ATTEMPTS",
        description: "Max failed auth attempts before IP lockout (default: 100)",
        example: "100",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_RATE_LIMIT_WINDOW_SECS",
        description: "Rate limit rolling window in seconds (default: 300)",
        example: "300",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_RATE_LIMIT_LOCKOUT_SECS",
        description: "Rate limit lockout duration in seconds (default: 600)",
        example: "600",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_REPLAY_WINDOW_SECS",
        description: "SigV4 replay detection window for mutating requests, in seconds (default: twice the clock skew, DGP_CLOCK_SKEW_SECONDS; 0 disables)",
        example: "900",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_SECURE_COOKIES",
        description: "Secure flag on admin session cookies: true always, false never (default: automatic — set when the listener serves TLS, from YAML or env, or a trusted X-Forwarded-Proto: https arrives)",
        example: "true",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_ADMIN_PASSWORD_HASH",
        description: "Legacy alias of DGP_BOOTSTRAP_PASSWORD_HASH (used when that is unset)",
        example: "$2b$12$...",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_BOOTSTRAP_PASSWORD",
        description: "Plaintext bootstrap password for the admin CLI (`config apply`, `admin ...`) only; never read by the server",
        example: "change-me",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_RATE_LIMIT_ACCOUNT_MAX_ATTEMPTS",
        description: "Failed logins per account (any IP) before that account locks (default: 10)",
        example: "10",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_RATE_LIMIT_ACCOUNT_WINDOW_SECS",
        description: "Rolling window for the per-account login-failure count (default: 3600)",
        example: "3600",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_RATE_LIMIT_ACCOUNT_LOCKOUT_SECS",
        description: "Per-account lockout duration after the limit (default: 3600)",
        example: "3600",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_ENCRYPTION_KEY",
        description: "Singleton-backend AES-256 key (64 hex chars). Named backends use DGP_BACKEND_<NAME>_ENCRYPTION_KEY",
        example: "<64 hex chars>",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_SSE_KMS_KEY_ID",
        description: "Singleton-backend SSE-KMS key ARN or alias. Named backends use DGP_BACKEND_<NAME>_SSE_KMS_KEY_ID",
        example: "alias/dgp",
        category: "Storage",
    },
    EnvVarEntry {
        name: "DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES",
        description: "Largest multipart upload that CompleteMultipartUpload assembles in memory to try a delta. The parts of an upload stay in memory up to this total, then go to relay files in the spool. A larger upload, or one that tries no delta, is stored from its parts without a delta (default: 64 MiB)",
        example: "67108864",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_REFERENCE_LOCK_TTL_SECS",
        description: "Cross-instance reference.bin lock lifetime when config sync is on (default: 120)",
        example: "120",
        category: "Config Sync",
    },
    EnvVarEntry {
        name: "DGP_REFERENCE_LOCK_ACQUIRE_TIMEOUT_SECS",
        description: "How long a PUT waits for the cross-instance reference lock before it fails (default: 30)",
        example: "30",
        category: "Config Sync",
    },
    EnvVarEntry {
        name: "DGP_NODE_ID",
        description: "Stable node label for coordination leases (default: derived and saved next to the config DB)",
        example: "dgp-0",
        category: "Config Sync",
    },
    EnvVarEntry {
        name: "DGP_REFERENCE_SCAN_LIMIT",
        description: "Max reference baselines the savings panel reads per request (default: built-in cap)",
        example: "10000",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_USAGE_CACHE_TTL_SECS",
        description: "Lifetime of a cached prefix-usage scan result (default: 300)",
        example: "300",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_RELAY_FOREIGN_MIN_AGE_SECS",
        description: "Min age before startup removes another process's multipart relay dir (default: 3600)",
        example: "3600",
        category: "Server",
    },
];

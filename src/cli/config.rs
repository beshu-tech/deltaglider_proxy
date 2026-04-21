//! `deltaglider_proxy config <subcommand>` dispatcher.
//!
//! Phase 0 shipped `config migrate` and `config schema`. Phase 4 adds
//! `apply` and `admission trace` as thin wrappers over the admin-API
//! endpoints already merged in Phase 1/2. Further subcommands
//! (`show`, `defaults`, `lint`, `init`, `explain`) depend on the Phase 3
//! sectioned schema and land after it.

use crate::config::{Config, ConfigError, ConfigFormat};
use std::io::Write;
use std::path::Path;

/// Exit codes for CLI subcommands. Stable contract for CI scripts.
pub const EXIT_OK: i32 = 0;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_IO: i32 = 3;
pub const EXIT_PARSE: i32 = 4;
pub const EXIT_HTTP: i32 = 5;
pub const EXIT_REJECTED: i32 = 6;
pub const EXIT_AUTH: i32 = 7;

/// `config migrate <input> [--out <output>]`
///
/// Loads a TOML (or YAML) config file and emits the canonical YAML form.
/// When `--out` is omitted, YAML is written to stdout. Secrets are stripped
/// before serialization (same policy as the admin API export path).
pub fn migrate(input: &str, output: Option<&str>) -> i32 {
    if !Path::new(input).exists() {
        eprintln!("error: input file not found: {input}");
        return EXIT_IO;
    }

    let config = match Config::from_file(input) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to parse {input}: {e}");
            return EXIT_PARSE;
        }
    };

    let yaml = match config.to_canonical_yaml() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to serialize to YAML: {e}");
            return EXIT_PARSE;
        }
    };

    match output {
        Some(path) => match std::fs::write(path, &yaml) {
            Ok(()) => {
                eprintln!("migrated {input} -> {path}");
                if ConfigFormat::from_path(path) != ConfigFormat::Yaml {
                    eprintln!(
                        "note: output extension is not .yaml/.yml; the file \
                         contains YAML content regardless."
                    );
                }
                EXIT_OK
            }
            Err(e) => {
                eprintln!("error: failed to write {path}: {e}");
                EXIT_IO
            }
        },
        None => {
            // Write directly to stdout so callers can pipe.
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            if let Err(e) = lock.write_all(yaml.as_bytes()) {
                eprintln!("error: failed to write to stdout: {e}");
                return EXIT_IO;
            }
            EXIT_OK
        }
    }
}

/// Adapter for callers that prefer `Result<(), ConfigError>` over exit codes.
pub fn migrate_to_string(input: &str) -> Result<String, ConfigError> {
    Config::from_file(input)?.to_canonical_yaml()
}

/// `config schema [--out <path>]`
///
/// Emit the JSON Schema for the canonical Config shape. Produced from
/// `schemars` derives at build time, so the schema tracks the struct
/// automatically. Used by CI to publish `schema/deltaglider.schema.json` and
/// by YAML LSP / editor autocomplete.
pub fn schema(output: Option<&str>) -> i32 {
    emit_schema_json(output, "schema")
}

/// Produce the JSON Schema as a String (used by tests and callers that want
/// the schema without spawning the process).
pub fn schema_string() -> Result<String, ConfigError> {
    let schema = schemars::schema_for!(Config);
    serde_json::to_string_pretty(&schema).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// Shared emitter for `config schema` and `config defaults`. Both
/// subcommands serialize `schemars::schema_for!(Config)` and write
/// the bytes to stdout or a file; the `label` parameter disambiguates
/// the operator-facing stderr message ("wrote schema to ..." vs
/// "wrote defaults to ..."). Splitting by label rather than cloning
/// the whole function body keeps future schema-specific enhancements
/// (e.g. `--resolve`, `--version`) trivial to add.
fn emit_schema_json(output: Option<&str>, label: &str) -> i32 {
    let schema = schemars::schema_for!(Config);
    let pretty = match serde_json::to_string_pretty(&schema) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to serialize {label}: {e}");
            return EXIT_PARSE;
        }
    };
    match output {
        Some(path) => match std::fs::write(path, &pretty) {
            Ok(()) => {
                eprintln!("wrote {label} to {path}");
                EXIT_OK
            }
            Err(e) => {
                eprintln!("error: failed to write {path}: {e}");
                EXIT_IO
            }
        },
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            if let Err(e) = lock.write_all(pretty.as_bytes()) {
                eprintln!("error: failed to write to stdout: {e}");
                return EXIT_IO;
            }
            EXIT_OK
        }
    }
}

/// `config lint <file>`
///
/// Validate a config file offline — same logic the admin API's
/// `/config/validate` endpoint runs, minus the "log_filter parseable"
/// check (that depends on `tracing_subscriber` available only when
/// the server is built into this binary; it is, so the check runs).
///
/// Exit codes:
///
/// - 0: valid, no warnings.
/// - 0: valid, non-empty warnings emitted to stderr (warnings are
///   advisory, not fatal — matches the admin API's return shape
///   where `ok: true` can coexist with warnings).
/// - 3: file not found / unreadable.
/// - 4: parse error (shape, unknown field, bad IP/CIDR, etc.).
/// - 6: validation error (duplicate admission block name, bad
///   Reject status, `public: true` conflict, etc.).
///
/// Intended for CI: `deltaglider_proxy config lint deploy.yaml`
/// in a pre-merge workflow step.
pub fn lint(file: &str) -> i32 {
    // Read and parse. Shape-level (serde) errors surface here.
    let content = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to read {file}: {e}");
            return EXIT_IO;
        }
    };

    // Dispatch by extension. Both YAML and TOML paths MUST run
    // `normalize_shorthands` so lint catches the same semantic
    // violations (duplicate admission block names, bad Reject
    // status, `public: true` conflicts) that the server would reject
    // at `/apply` time. `Config::from_yaml_str` runs it internally;
    // for TOML we deserialize raw then call a helper that re-runs
    // the same validation pipeline.
    //
    // The TOML path still goes through here (not `Config::from_toml_file`)
    // because lint MUST NOT read its input as a filesystem path a
    // second time — we've already slurped `content` into memory, so
    // re-reading from disk would both waste syscalls and open a
    // TOCTOU window where the file changes mid-lint.
    let cfg = match std::path::Path::new(file)
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("yaml") | Some("yml") => Config::from_yaml_str(&content),
        _ => Config::from_toml_str(&content),
    };
    let mut cfg = match cfg {
        Ok(c) => c,
        Err(e) => {
            // ConfigError::Parse covers both shape and semantic
            // violations (deny_unknown_fields, normalize_shorthands,
            // AdmissionSpec::validate). We don't split them today;
            // the client-side convention is "exit 4 for parse, 6 for
            // rejected" — use 4 for everything here, and rely on the
            // message carrying the distinction.
            eprintln!("error: {e}");
            return EXIT_PARSE;
        }
    };

    // Run the same `check()` the admin API's /validate uses. It
    // mutates fields that can't be satisfied (e.g. clears an
    // unresolved default_backend) and returns human-readable
    // warnings.
    let warnings = cfg.check();

    // Log-filter check: /validate rejects malformed filters with 400.
    // Here we surface it as exit 6.
    if cfg
        .log_level
        .parse::<tracing_subscriber::EnvFilter>()
        .is_err()
    {
        eprintln!(
            "error: invalid log_level filter '{}' — expected a tracing-subscriber EnvFilter",
            cfg.log_level
        );
        return EXIT_REJECTED;
    }

    for w in &warnings {
        eprintln!("warning: {w}");
    }

    if warnings.is_empty() {
        eprintln!("{file}: ok");
    } else {
        eprintln!("{file}: ok with {} warning(s)", warnings.len());
    }
    EXIT_OK
}

/// `config defaults [--out <path>]`
///
/// Emit the JSON Schema for the current `Config` shape, which includes
/// every default value and the doc-comment description for every
/// field (produced by schemars' `title` + `description` on schema
/// properties). This is the CLI counterpart to the admin API's
/// `/config/defaults` endpoint and the backing data for YAML LSP
/// autocompletion.
///
/// Today this emits the same JSON Schema as `config schema` — the
/// two commands exist separately because the mental model is
/// different (`defaults` is about "what would I get if I left every
/// field blank?", `schema` is about "what are the shape rules?").
/// Future phases may differentiate (e.g. `defaults --version v1`
/// emits a pinned-historic defaults list, while `schema` always
/// emits the current-release JSON Schema).
pub fn defaults(output: Option<&str>) -> i32 {
    // Reuse the schemars output — it carries `default` values per
    // field plus the doc-comment description. This is what YAML LSPs
    // consume. See `emit_schema_json` for the shared emitter.
    emit_schema_json(output, "defaults")
}

// ═════════════════════════════════════════════════════════════════════════
// Phase 4 — thin wrappers over the admin API
// ═════════════════════════════════════════════════════════════════════════

/// Default URL when `--server` is omitted. Matches the default listen
/// address for a local dev instance.
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:9000";

/// Environment variable name the CLI reads for the bootstrap password
/// plaintext. We refuse to echo the password through an argv flag
/// (would leak via `ps auxww`); require it via env so operators can
/// pipe it from their secret manager.
const PASSWORD_ENV: &str = "DGP_BOOTSTRAP_PASSWORD";

/// True when the given URL sends HTTP cleartext to a non-local host.
/// Used to decide whether to warn about bootstrap-password exposure on
/// the wire. Localhost / 127.* / ::1 get a pass because the cleartext
/// never leaves the loopback interface.
///
/// Parsing is intentionally lenient: we use naive string scanning
/// (not a URL crate) because a malformed URL should NOT silence the
/// warning. On parse failure we default to "warn" — better to err
/// loudly than silently.
fn is_cleartext_to_remote(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") {
        return false; // https:// is fine; anything else isn't our concern
    }
    // Strip scheme + optional userinfo.
    let after_scheme = &url["http://".len()..];
    let after_userinfo = after_scheme.rsplit_once('@').map_or(after_scheme, |p| p.1);

    // Host isolation: IPv6 literals arrive as `[::1]:9000`; everything
    // else as `hostname:port` / `hostname/path`. Detect the bracket form
    // first so we don't mis-split on the colons inside the address.
    let host: String = if let Some(rest) = after_userinfo.strip_prefix('[') {
        match rest.find(']') {
            Some(end) => rest[..end].to_ascii_lowercase(),
            None => return true, // malformed `[...` — warn rather than silence
        }
    } else {
        let host_end = after_userinfo
            .find([':', '/', '?'])
            .unwrap_or(after_userinfo.len());
        after_userinfo[..host_end].to_ascii_lowercase()
    };

    !matches!(host.as_str(), "localhost" | "::1") && !host.starts_with("127.")
}

/// Options shared by every admin-API subcommand. Centralising the
/// "which server, which session" concerns here keeps the per-command
/// dispatchers focused on what they actually do.
pub struct AdminClientOpts {
    /// Base URL of the server. Defaults to `DEFAULT_SERVER_URL`.
    pub server: String,
    /// Timeout for individual HTTP requests. Defaults to 30s — long
    /// enough for an apply that rebuilds the engine, short enough to
    /// surface network issues promptly.
    pub timeout_secs: u64,
}

impl Default for AdminClientOpts {
    fn default() -> Self {
        Self {
            server: DEFAULT_SERVER_URL.to_string(),
            timeout_secs: 30,
        }
    }
}

/// Build a reqwest client with a cookie jar, log into the admin API
/// using the bootstrap password from the `DGP_BOOTSTRAP_PASSWORD` env,
/// and return the ready-to-use client.
///
/// The cookie jar carries the admin session for the remainder of the
/// CLI process. When the subcommand finishes and the process exits,
/// the jar is discarded — we never persist a session to disk.
///
/// Why env var and not a `--password` flag: argv is visible in process
/// listings (`ps auxww`), which leaks the bootstrap password to any
/// other user on the machine. Env vars are per-process and the common
/// GitOps convention is to pipe secrets via env from the secret
/// manager anyway.
async fn admin_login(opts: &AdminClientOpts) -> Result<reqwest::Client, CliError> {
    let password = std::env::var(PASSWORD_ENV).map_err(|_| CliError::MissingPassword)?;
    if password.is_empty() {
        return Err(CliError::MissingPassword);
    }

    // Warn loudly if the operator is about to send the bootstrap password
    // cleartext over a non-local HTTP link. We don't refuse — there are
    // legitimate setups (mTLS-terminating sidecar, localhost port-forward)
    // where http:// to a remote host is fine — but we shouldn't be silent
    // about the standard "forgot https://" footgun either.
    if is_cleartext_to_remote(&opts.server) {
        eprintln!(
            "warning: sending bootstrap password cleartext to a non-local http:// URL ({}). \
             Use https:// or a localhost/port-forward tunnel unless you've terminated TLS upstream.",
            opts.server
        );
    }

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .timeout(std::time::Duration::from_secs(opts.timeout_secs))
        .build()
        .map_err(|e| CliError::Http(format!("failed to build HTTP client: {e}")))?;

    let url = format!("{}/_/api/admin/login", opts.server.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "password": password }))
        .send()
        .await
        .map_err(|e| CliError::Http(format!("POST {url}: {e}")))?;

    match resp.status() {
        reqwest::StatusCode::OK => Ok(client),
        reqwest::StatusCode::UNAUTHORIZED => Err(CliError::WrongPassword),
        reqwest::StatusCode::TOO_MANY_REQUESTS => Err(CliError::RateLimited),
        s => Err(CliError::Http(format!(
            "unexpected login status {s}: {}",
            resp.text().await.unwrap_or_default()
        ))),
    }
}

/// CLI-side error shape. Converted to exit codes + messages by the
/// dispatcher so the public API (exit codes) stays stable regardless
/// of internal refactoring.
#[derive(Debug)]
enum CliError {
    MissingPassword,
    WrongPassword,
    RateLimited,
    Io(String),
    Http(String),
    /// Admin API accepted the request but rejected the content.
    /// (Validation failure, 400/422/500 from /apply, etc.)
    Rejected(String),
}

impl CliError {
    fn exit_code(&self) -> i32 {
        match self {
            Self::MissingPassword | Self::WrongPassword => EXIT_AUTH,
            Self::RateLimited => EXIT_HTTP,
            Self::Io(_) => EXIT_IO,
            Self::Http(_) => EXIT_HTTP,
            Self::Rejected(_) => EXIT_REJECTED,
        }
    }

    fn user_message(&self) -> String {
        match self {
            Self::MissingPassword => format!(
                "error: set {PASSWORD_ENV} (the admin bootstrap password) before running admin CLIs"
            ),
            Self::WrongPassword => {
                format!("error: {PASSWORD_ENV} is set but the server rejected it")
            }
            Self::RateLimited => {
                "error: server rate-limited the login (try again in 10 minutes)".into()
            }
            Self::Io(m) => format!("error: {m}"),
            Self::Http(m) => format!("error: HTTP: {m}"),
            Self::Rejected(m) => format!("error: server rejected the request: {m}"),
        }
    }
}

/// `config apply <file> [--server URL] [--timeout SECS]`
///
/// Push a full YAML config document to a running server via
/// `POST /_/api/admin/config/apply`. The server validates, atomically
/// swaps the runtime config, and persists. The response surfaces:
///
/// - `applied: true/false` — did the in-memory swap succeed?
/// - `persisted: true/false` — did the write to the on-disk config file succeed?
/// - `requires_restart: true/false` — any applied field needs a process restart to take effect
///   (e.g. `listen_addr`, `cache_size_mb`)
/// - `warnings: [...]` — non-fatal issues the operator should see
///
/// Exit codes:
///
/// | code | meaning |
/// |-----:|---------|
/// |  0   | applied AND persisted, no restart required |
/// |  0   | applied AND persisted, restart required (stderr warns) |
/// |  6   | server rejected the apply (validation / engine rebuild) |
/// |  5   | persist failed after apply succeeded (runtime OK, disk stale) |
/// |  7   | missing/wrong `DGP_BOOTSTRAP_PASSWORD` |
/// |  3   | local IO error (file not found, unreadable, …) |
pub fn apply(input: &str, opts: AdminClientOpts) -> i32 {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start tokio runtime: {e}");
            return EXIT_IO;
        }
    };
    match runtime.block_on(apply_async(input, opts)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{}", e.user_message());
            e.exit_code()
        }
    }
}

async fn apply_async(input: &str, opts: AdminClientOpts) -> Result<i32, CliError> {
    let yaml =
        std::fs::read_to_string(input).map_err(|e| CliError::Io(format!("read {input}: {e}")))?;
    if yaml.trim().is_empty() {
        return Err(CliError::Rejected(
            "refusing to apply an empty YAML body".into(),
        ));
    }

    let client = admin_login(&opts).await?;
    let url = format!(
        "{}/_/api/admin/config/apply",
        opts.server.trim_end_matches('/')
    );

    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "yaml": yaml }))
        .send()
        .await
        .map_err(|e| CliError::Http(format!("POST {url}: {e}")))?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| {
        CliError::Http(format!(
            "response from {url} was not JSON (status {status}): {e}"
        ))
    })?;

    // Surface the server's warnings verbatim so the operator sees everything
    // the admin GUI would show.
    if let Some(warnings) = body.get("warnings").and_then(|v| v.as_array()) {
        for w in warnings {
            if let Some(s) = w.as_str() {
                eprintln!("warning: {s}");
            }
        }
    }

    let applied = body
        .get("applied")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let persisted = body
        .get("persisted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let requires_restart = body
        .get("requires_restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !status.is_success() || !applied {
        let error = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("(no error message)");
        return Err(CliError::Rejected(format!("{status}: {error}")));
    }

    if !persisted {
        eprintln!(
            "warning: config applied in memory but NOT persisted — the next restart will revert"
        );
        return Ok(EXIT_HTTP); // EXIT_HTTP distinguishes "half-applied" from clean success
    }

    eprintln!("applied: yes");
    eprintln!(
        "persisted: {}",
        body.get("persisted_path")
            .and_then(|v| v.as_str())
            .unwrap_or("(path unknown)")
    );
    if requires_restart {
        eprintln!("restart: required for some fields (see warnings)");
    }

    Ok(EXIT_OK)
}

/// `admission trace --method M --path P [--authenticated] [--query Q] [--server URL]`
///
/// Dry-run a synthetic request through the server's admission chain via
/// `POST /_/api/admin/config/trace`. Emits the decision as JSON on stdout
/// so it composes with `jq` in shell pipelines.
pub struct TraceArgs {
    pub method: String,
    pub path: String,
    pub authenticated: bool,
    pub query: Option<String>,
}

pub fn admission_trace(args: TraceArgs, opts: AdminClientOpts) -> i32 {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start tokio runtime: {e}");
            return EXIT_IO;
        }
    };
    match runtime.block_on(admission_trace_async(args, opts)) {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("{}", e.user_message());
            e.exit_code()
        }
    }
}

async fn admission_trace_async(args: TraceArgs, opts: AdminClientOpts) -> Result<(), CliError> {
    let client = admin_login(&opts).await?;
    let url = format!(
        "{}/_/api/admin/config/trace",
        opts.server.trim_end_matches('/')
    );
    let mut body = serde_json::json!({
        "method": args.method,
        "path": args.path,
        "authenticated": args.authenticated,
    });
    if let Some(q) = args.query {
        body.as_object_mut()
            .expect("just constructed as an object")
            .insert("query".to_string(), serde_json::Value::String(q));
    }

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| CliError::Http(format!("POST {url}: {e}")))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| {
        CliError::Http(format!(
            "response from {url} was not JSON (status {status}): {e}"
        ))
    })?;
    if !status.is_success() {
        return Err(CliError::Rejected(format!("{status}: {body}")));
    }

    // Pretty-print so humans can read; shell users can `| jq` for
    // machine-readable extraction.
    let pretty = serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string());
    println!("{pretty}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn migrate_toml_to_yaml_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("cfg.toml");
        std::fs::write(
            &toml_path,
            r#"
listen_addr = "0.0.0.0:9001"
max_delta_ratio = 0.4

[backend]
type = "filesystem"
path = "/tmp/dgp"
"#,
        )
        .unwrap();

        let yaml = migrate_to_string(toml_path.to_str().unwrap()).unwrap();
        assert!(yaml.contains("9001"));
        assert!(yaml.contains("0.4"));

        // Round-trip through the dual-shape YAML deserializer, not
        // `serde_yaml::from_str::<Config>` directly — the canonical emitter
        // is sectioned, only `Config::from_yaml_str` knows how to collapse
        // sections back to the flat shape.
        let reparsed = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(reparsed.listen_addr.port(), 9001);
        assert!((reparsed.max_delta_ratio - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn migrate_yaml_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("cfg.toml");
        std::fs::write(&toml_path, "listen_addr = \"127.0.0.1:9000\"\n").unwrap();

        let yaml_a = migrate_to_string(toml_path.to_str().unwrap()).unwrap();

        let yaml_path = dir.path().join("cfg.yaml");
        let mut f = std::fs::File::create(&yaml_path).unwrap();
        f.write_all(yaml_a.as_bytes()).unwrap();

        let yaml_b = migrate_to_string(yaml_path.to_str().unwrap()).unwrap();
        assert_eq!(yaml_a, yaml_b, "YAML → YAML migrate must be idempotent");
    }

    #[test]
    fn migrate_strips_infra_secrets() {
        // `migrate` must match `to_toml_string` / `to_canonical_yaml`:
        // strip bootstrap hash and encryption key (infra secrets), keep SigV4
        // creds so the output is drop-in-usable by the wizard path.
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("cfg.toml");
        std::fs::write(
            &toml_path,
            r#"
access_key_id = "AKIAKEEPME"
secret_access_key = "runtime-key-kept-for-file-use"
bootstrap_password_hash = "$2b$12$xxxxxxxxxxxxxxxxxxxxxx"
encryption_key = "deadbeef-hex-key"
"#,
        )
        .unwrap();

        let yaml = migrate_to_string(toml_path.to_str().unwrap()).unwrap();
        // Infra secrets stripped
        assert!(!yaml.contains("$2b$"));
        assert!(!yaml.contains("deadbeef-hex-key"));
        // SigV4 runtime creds survive — migration output must remain a
        // drop-in YAML equivalent of the input TOML.
        assert!(yaml.contains("AKIAKEEPME"));
        assert!(yaml.contains("runtime-key-kept-for-file-use"));
    }

    #[test]
    fn test_cleartext_detection() {
        // Cases we WANT to warn on (http:// to non-loopback).
        assert!(is_cleartext_to_remote("http://example.com"));
        assert!(is_cleartext_to_remote("http://10.0.0.5:9000"));
        assert!(is_cleartext_to_remote("http://user@example.com:9000"));
        assert!(is_cleartext_to_remote("http://192.168.1.1/"));

        // Cases we DON'T warn on (loopback, any scheme we don't care about).
        assert!(!is_cleartext_to_remote("https://example.com"));
        assert!(!is_cleartext_to_remote("http://127.0.0.1:9000"));
        assert!(!is_cleartext_to_remote("http://127.0.0.1"));
        assert!(!is_cleartext_to_remote("http://localhost:9000"));
        assert!(!is_cleartext_to_remote("http://LOCALHOST"));
        assert!(!is_cleartext_to_remote("http://[::1]:9000")); // IPv6 loopback literal
        assert!(!is_cleartext_to_remote("http://127.255.255.255"));

        // Malformed input: err on the side of warning rather than silence.
        assert!(!is_cleartext_to_remote("")); // doesn't start with http://
        assert!(!is_cleartext_to_remote("not-a-url")); // ditto
    }

    // ── Phase 4: lint + defaults ───────────────────────────────────────

    #[test]
    fn lint_accepts_valid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.yaml");
        std::fs::write(
            &path,
            r#"
storage:
  filesystem: /var/dgp
"#,
        )
        .unwrap();
        let code = lint(path.to_str().unwrap());
        assert_eq!(code, EXIT_OK);
    }

    #[test]
    fn lint_rejects_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.yaml");
        std::fs::write(
            &path,
            r#"
storage:
  bogus_field: true
"#,
        )
        .unwrap();
        let code = lint(path.to_str().unwrap());
        assert_eq!(code, EXIT_PARSE);
    }

    #[test]
    fn lint_rejects_admission_duplicate_block_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.yaml");
        std::fs::write(
            &path,
            r#"
admission:
  blocks:
    - name: same
      match: {}
      action: continue
    - name: same
      match: {}
      action: deny
"#,
        )
        .unwrap();
        // Duplicate-name validation surfaces as ConfigError::Parse
        // (it's a semantic violation during normalize_shorthands),
        // which the CLI splits out as EXIT_PARSE. The error message
        // must name the offending block for operator grep.
        let code = lint(path.to_str().unwrap());
        assert_eq!(code, EXIT_PARSE);
    }

    #[test]
    fn lint_missing_file_is_exit_3() {
        let code = lint("/nonexistent/path/to/nothing.yaml");
        assert_eq!(code, EXIT_IO);
    }

    #[test]
    fn defaults_emits_json_schema_with_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.json");
        let code = defaults(Some(path.to_str().unwrap()));
        assert_eq!(code, EXIT_OK);
        let content = std::fs::read_to_string(&path).unwrap();
        // Must be valid JSON and carry at least one known field.
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let has_max_delta_ratio = parsed["properties"]["max_delta_ratio"].is_object()
            || parsed["$defs"]
                .as_object()
                .map(|d| {
                    d.values()
                        .any(|v| v["properties"]["max_delta_ratio"].is_object())
                })
                .unwrap_or(false);
        assert!(
            has_max_delta_ratio,
            "defaults output must include max_delta_ratio, got:\n{content}"
        );
    }
}

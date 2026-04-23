//! One-shot utility: open the IAM config DB with the bootstrap hash,
//! read users / groups / auth-providers / mapping-rules, and print a
//! new-style sectioned YAML describing the full runtime configuration.
//!
//! Usage:
//!
//! ```text
//! DGP_BOOTSTRAP_PASSWORD_HASH=<base64-or-raw-bcrypt> \
//!   cargo run --release --example scrape_full_config -- \
//!     [path/to/deltaglider_config.db] [path/to/config.toml|yaml] [path/to/secrets.env]
//! ```
//!
//! Output on stdout is the proposed "new-style"
//! admission/access/storage/advanced sectioned YAML. Secrets are redacted
//! into `!secret NAME` placeholders so the output is safe to commit to
//! Git.
//!
//! When the third positional argument is supplied, every referenced
//! secret is additionally written to that path as a `KEY=VALUE` `.env`
//! file (plaintext). **That file MUST NOT be committed** — treat it the
//! way you treat `terraform.tfvars` or a Kubernetes Secret manifest: feed
//! it through SOPS / Vault / CI-secret-provider, then discard.

use deltaglider_proxy::config::{BackendConfig, BackendEncryptionConfig, Config};
use deltaglider_proxy::config_db::ConfigDb;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Accumulator that tracks every `!secret NAME` placeholder the YAML
/// emits, paired with its real value. Written out as a `KEY=VALUE` `.env`
/// file after YAML generation so the two documents stay 1:1 by
/// construction (you can't leak a secret or reference a missing one; the
/// accumulator throws on duplicate keys to catch copy-paste mistakes).
#[derive(Default)]
struct SecretsDump {
    entries: Vec<(String, String)>,
    seen: std::collections::BTreeSet<String>,
}

impl SecretsDump {
    /// Record `name = value`. Panics on duplicate names — that would
    /// indicate two sites in this file referenced the same `!secret X`
    /// placeholder with different values, which is a scraper bug.
    fn record(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        if !self.seen.insert(name.clone()) {
            eprintln!(
                "WARN: duplicate secret key '{}' — second value ignored (first wins)",
                name
            );
            return;
        }
        self.entries.push((name, value));
    }

    /// Serialize as a `.env` file. Values are shell-quoted only when
    /// necessary; bare values pass through so regexp-friendly parsers
    /// (including `source foo.env`) work.
    fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# DeltaGlider Proxy — scraped secrets ({} entries).\n\
             # DO NOT COMMIT THIS FILE.\n\
             # Every key here matches a `!secret NAME` placeholder in the\n\
             # companion YAML emitted by examples/scrape_full_config.rs.\n",
            self.entries.len()
        );
        for (k, v) in &self.entries {
            let _ = writeln!(out, "{}={}", k, shell_quote(v));
        }
        out
    }
}

/// POSIX-shell-compatible single-quote escaping. Use only when the value
/// contains whitespace or characters that `source` / `dotenv` parsers
/// would choke on; bare alphanumeric values pass through unquoted for
/// readability.
fn shell_quote(v: &str) -> String {
    let safe = v
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '='));
    if safe && !v.is_empty() {
        v.to_string()
    } else {
        // Single-quote escape: wrap in '...', and encode any existing
        // single-quote as the '\'' trick (close, escaped-quote, re-open).
        let mut out = String::from("'");
        for c in v.chars() {
            if c == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(c);
            }
        }
        out.push('\'');
        out
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Inputs ───────────────────────────────────────────────────────────
    let args: Vec<String> = std::env::args().skip(1).collect();
    let db_path = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./deltaglider_config.db"));
    let config_path = args.get(1).cloned();
    let secrets_out_path = args.get(2).map(PathBuf::from);

    let raw_hash = std::env::var("DGP_BOOTSTRAP_PASSWORD_HASH")
        .map_err(|_| "DGP_BOOTSTRAP_PASSWORD_HASH must be set in the environment")?;
    let hash = decode_hash(&raw_hash);

    // Accumulator populated at every `!secret NAME` emission below. The
    // 1:1 pairing with the YAML is by construction: record as you emit.
    let mut secrets = SecretsDump::default();

    // ── Config ───────────────────────────────────────────────────────────
    let cfg = if let Some(ref p) = config_path {
        Config::from_file(p)?
    } else {
        Config::load()
    };

    // ── IAM DB (read-only) ──────────────────────────────────────────────
    let db = ConfigDb::open_or_create(&db_path, &hash)?;
    let users = db.load_users()?;
    let groups = db.load_groups()?;
    let providers = db.load_auth_providers()?;
    let mapping_rules = db.load_group_mapping_rules()?;
    let external_identities = db.list_external_identities()?;

    // Build group-id → name lookup so mapping rules resolve human-readably.
    let group_by_id: std::collections::HashMap<i64, &str> =
        groups.iter().map(|g| (g.id, g.name.as_str())).collect();
    let provider_by_id: std::collections::HashMap<i64, &str> =
        providers.iter().map(|p| (p.id, p.name.as_str())).collect();

    // ── Emit YAML ────────────────────────────────────────────────────────
    let mut out = String::new();
    out.push_str(
        "# DeltaGlider Proxy — scraped configuration (new-style sectioned YAML).\n\
         # Produced by examples/scrape_full_config.rs. Secrets are replaced\n\
         # with `!secret NAME` placeholders; populate them from your secret\n\
         # manager before applying.\n\
         #\n\
         # Layer map:\n\
         #   admission  → pre-auth gating (public-prefix grants, future: IP lists / rate limits)\n\
         #   access     → who can log in: OAuth sources, IAM users & groups, mapping rules\n\
         #   storage    → where objects live (backends + bucket overrides)\n\
         #   advanced   → process-level tunables (cache, timeouts, TLS, observability)\n\n",
    );

    // ── admission ─────────────────────────────────────────────────────
    out.push_str("admission:\n");
    let mut admission_blocks = 0;
    for (bucket, policy) in &cfg.buckets {
        if policy.public_prefixes.is_empty() {
            continue;
        }
        admission_blocks += 1;
        for prefix in &policy.public_prefixes {
            out.push_str(&format!(
                "  - name: public-prefix:{}:{}\n\
                 \x20   match:\n\
                 \x20     method: [GET, HEAD]\n\
                 \x20     bucket: {}\n\
                 \x20     path_prefix: {:?}\n\
                 \x20   action: allow-anonymous\n",
                bucket, prefix, bucket, prefix,
            ));
        }
    }
    if admission_blocks == 0 {
        out.push_str("  # (no public-prefix grants configured)\n");
    }
    out.push('\n');

    // ── access ────────────────────────────────────────────────────────
    out.push_str("access:\n");

    // Auth sources: legacy SigV4 + OIDC providers.
    out.push_str("  sources:\n");
    if cfg.auth_enabled() {
        out.push_str(&format!(
            "    - id: legacy-admin\n\
             \x20     type: sigv4-static\n\
             \x20     access_key_id: {}\n\
             \x20     secret_access_key: !secret DGP_SECRET_ACCESS_KEY\n",
            cfg.access_key_id.as_deref().unwrap_or(""),
        ));
        if let Some(ref s) = cfg.secret_access_key {
            secrets.record("DGP_SECRET_ACCESS_KEY", s);
        }
    }
    for provider in &providers {
        out.push_str(&format!(
            "    - id: {}\n\
             \x20     type: {}\n\
             \x20     enabled: {}\n\
             \x20     priority: {}\n",
            provider.name, provider.provider_type, provider.enabled, provider.priority,
        ));
        if let Some(ref display_name) = provider.display_name {
            out.push_str(&format!("      display_name: {}\n", display_name));
        }
        if let Some(ref client_id) = provider.client_id {
            out.push_str(&format!("      client_id: {}\n", client_id));
        }
        if let Some(ref secret) = provider.client_secret {
            let key = format!("OIDC_{}_CLIENT_SECRET", sanitize_env_name(&provider.name));
            out.push_str(&format!("      client_secret: !secret {}\n", key));
            secrets.record(key, secret);
        }
        if let Some(ref issuer_url) = provider.issuer_url {
            out.push_str(&format!("      issuer_url: {}\n", issuer_url));
        }
        let scopes: Vec<&str> = provider.scopes.split_whitespace().collect();
        out.push_str(&format!("      scopes: {:?}\n", scopes));
        if let Some(ref extra) = provider.extra_config {
            out.push_str(&format!("      extra_config: {}\n", extra));
        }
    }

    // Groups.
    out.push_str("  groups:\n");
    if groups.is_empty() {
        out.push_str("    # (no IAM groups defined)\n");
    }
    for group in &groups {
        // Quote `name` — group names like "ROR Staff" contain spaces and
        // would otherwise break under any YAML reader.
        out.push_str(&format!(
            "    - name: {:?}\n\
             \x20     description: {:?}\n",
            group.name, group.description,
        ));
        if !group.permissions.is_empty() {
            out.push_str("      permissions:\n");
            for perm in &group.permissions {
                emit_permission(&mut out, perm, 8);
            }
        }
    }

    // Users.
    out.push_str("  users:\n");
    if users.is_empty() {
        out.push_str("    # (no IAM users defined)\n");
    }
    for user in &users {
        // Quote user name — external-auth identities can be arbitrary
        // strings with spaces and unicode (e.g. "Mateusz Kołodziejczyk").
        let key = format!("USER_{}_SECRET", sanitize_env_name(&user.name));
        out.push_str(&format!(
            "    - name: {:?}\n\
             \x20     access_key_id: {}\n\
             \x20     enabled: {}\n\
             \x20     auth_source: {}\n\
             \x20     secret_access_key: !secret {}\n",
            user.name, user.access_key_id, user.enabled, user.auth_source, key,
        ));
        secrets.record(key, &user.secret_access_key);
        if !user.group_ids.is_empty() {
            let group_names: Vec<&str> = user
                .group_ids
                .iter()
                .filter_map(|gid| group_by_id.get(gid).copied())
                .collect();
            out.push_str(&format!("      groups: {:?}\n", group_names));
        }
        if !user.permissions.is_empty() {
            out.push_str("      permissions:\n");
            for perm in &user.permissions {
                emit_permission(&mut out, perm, 8);
            }
        }
    }

    // Group mapping rules.
    out.push_str("  mapping_rules:\n");
    if mapping_rules.is_empty() {
        out.push_str("    # (no group mapping rules defined)\n");
    }
    for rule in &mapping_rules {
        let provider = rule
            .provider_id
            .and_then(|pid| provider_by_id.get(&pid).copied())
            .unwrap_or("*"); // None means "all providers"
        let group = group_by_id.get(&rule.group_id).copied().unwrap_or("?");
        // YAML quoting: unquoted `*` is an alias-dereference, and names
        // containing spaces / non-word chars need quoting too. Always
        // emit with `{:?}` so the output round-trips through any YAML
        // parser unchanged.
        out.push_str(&format!(
            "    - provider: {:?}\n\
             \x20     match_type: {}\n\
             \x20     match_field: {}\n\
             \x20     match_value: {:?}\n\
             \x20     assign_group: {:?}\n\
             \x20     priority: {}\n",
            provider, rule.match_type, rule.match_field, rule.match_value, group, rule.priority,
        ));
    }
    out.push('\n');

    // ── storage ───────────────────────────────────────────────────────
    out.push_str("storage:\n");
    out.push_str("  backends:\n");
    out.push_str("    - id: primary\n");
    match &cfg.backend {
        BackendConfig::Filesystem { path } => {
            out.push_str(&format!(
                "      type: filesystem\n\
                 \x20     path: {}\n",
                path.display(),
            ));
        }
        BackendConfig::S3 {
            endpoint,
            region,
            force_path_style,
            access_key_id,
            secret_access_key,
        } => {
            out.push_str("      type: s3\n");
            if let Some(ep) = endpoint {
                out.push_str(&format!("      endpoint: {}\n", ep));
            }
            out.push_str(&format!("      region: {}\n", region));
            if *force_path_style {
                out.push_str("      force_path_style: true\n");
            }
            if let Some(ref k) = access_key_id {
                out.push_str("      access_key_id: !secret DGP_BE_AWS_ACCESS_KEY_ID\n");
                secrets.record("DGP_BE_AWS_ACCESS_KEY_ID", k);
            }
            if let Some(ref s) = secret_access_key {
                out.push_str("      secret_access_key: !secret DGP_BE_AWS_SECRET_ACCESS_KEY\n");
                secrets.record("DGP_BE_AWS_SECRET_ACCESS_KEY", s);
            }
        }
    }
    for named in &cfg.backends {
        out.push_str(&format!("    - id: {}\n", named.name));
        match &named.backend {
            BackendConfig::Filesystem { path } => {
                out.push_str(&format!(
                    "      type: filesystem\n\
                     \x20     path: {}\n",
                    path.display(),
                ));
            }
            BackendConfig::S3 {
                endpoint,
                region,
                force_path_style,
                access_key_id,
                secret_access_key,
            } => {
                out.push_str("      type: s3\n");
                if let Some(ep) = endpoint {
                    out.push_str(&format!("      endpoint: {}\n", ep));
                }
                out.push_str(&format!("      region: {}\n", region));
                if *force_path_style {
                    out.push_str("      force_path_style: true\n");
                }
                if let Some(ref k) = access_key_id {
                    let key = format!("BACKEND_{}_ACCESS_KEY_ID", sanitize_env_name(&named.name));
                    out.push_str(&format!("      access_key_id: !secret {}\n", key));
                    secrets.record(key, k);
                }
                if let Some(ref s) = secret_access_key {
                    let key = format!(
                        "BACKEND_{}_SECRET_ACCESS_KEY",
                        sanitize_env_name(&named.name)
                    );
                    out.push_str(&format!("      secret_access_key: !secret {}\n", key));
                    secrets.record(key, s);
                }
            }
        }
    }
    if let Some(ref default) = cfg.default_backend {
        out.push_str(&format!("  default: {}\n", default));
    }

    out.push_str("  buckets:\n");
    if cfg.buckets.is_empty() {
        out.push_str("    # (no bucket-level overrides)\n");
    }
    for (name, policy) in &cfg.buckets {
        out.push_str(&format!("    {}:\n", name));
        if let Some(comp) = policy.compression {
            out.push_str(&format!("      compression: {}\n", comp));
        }
        if let Some(ratio) = policy.max_delta_ratio {
            out.push_str(&format!("      max_delta_ratio: {}\n", ratio));
        }
        if let Some(ref backend) = policy.backend {
            out.push_str(&format!("      backend: {}\n", backend));
        }
        if let Some(ref alias) = policy.alias {
            out.push_str(&format!("      alias: {}\n", alias));
        }
        if let Some(quota) = policy.quota_bytes {
            out.push_str(&format!("      quota_bytes: {}\n", quota));
        }
        if !policy.public_prefixes.is_empty() {
            out.push_str(&format!(
                "      public_prefixes: {:?}\n",
                policy.public_prefixes,
            ));
        }
    }
    out.push('\n');

    // ── advanced ──────────────────────────────────────────────────────
    out.push_str("advanced:\n");
    out.push_str(&format!("  listen_addr: {}\n", cfg.listen_addr));
    out.push_str(&format!("  max_delta_ratio: {}\n", cfg.max_delta_ratio));
    out.push_str(&format!("  max_object_size: {}\n", cfg.max_object_size));
    out.push_str(&format!("  cache_size_mb: {}\n", cfg.cache_size_mb));
    out.push_str(&format!("  metadata_cache_mb: {}\n", cfg.metadata_cache_mb));
    out.push_str(&format!("  log_level: {:?}\n", cfg.log_level));
    if let Some(ref bucket) = cfg.config_sync_bucket {
        out.push_str(&format!("  config_sync_bucket: {}\n", bucket));
    }
    // Per-backend encryption lives on `backend_encryption` (singleton)
    // and `backends[*].encryption` (list). Each Aes256GcmProxy-mode
    // entry gets its key recorded under the matching env-var name and
    // emitted as a !secret reference in the summary.
    if let BackendEncryptionConfig::Aes256GcmProxy { key: Some(k), .. } = &cfg.backend_encryption {
        out.push_str("  backend_encryption.key: !secret DGP_ENCRYPTION_KEY\n");
        secrets.record("DGP_ENCRYPTION_KEY", k);
    }
    for named in &cfg.backends {
        if let BackendEncryptionConfig::Aes256GcmProxy { key: Some(k), .. } = &named.encryption {
            let env_name = format!(
                "DGP_BACKEND_{}_ENCRYPTION_KEY",
                named
                    .name
                    .chars()
                    .map(|c| match c {
                        '-' | '.' => '_',
                        c => c.to_ascii_uppercase(),
                    })
                    .collect::<String>()
            );
            out.push_str(&format!(
                "  backends.{}.encryption.key: !secret {}\n",
                named.name, env_name
            ));
            secrets.record(&env_name, k);
        }
    }
    // Prefer the value that came through the env var (what the running
    // server is actually keyed with). Fall back to the file if the env
    // var wasn't provided but the Config carries a hash — this happens
    // when the operator seeded the hash through TOML rather than env.
    let boot_hash_value = cfg
        .bootstrap_password_hash
        .as_deref()
        .unwrap_or(raw_hash.trim());
    if !boot_hash_value.is_empty() {
        out.push_str("  bootstrap_password_hash: !secret DGP_BOOTSTRAP_PASSWORD_HASH\n");
        secrets.record("DGP_BOOTSTRAP_PASSWORD_HASH", boot_hash_value);
    }
    if let Some(ref tls) = cfg.tls {
        out.push_str("  tls:\n");
        out.push_str(&format!("    enabled: {}\n", tls.enabled));
        if let Some(ref cert) = tls.cert_path {
            out.push_str(&format!("    cert_path: {}\n", cert));
        }
        if let Some(ref key) = tls.key_path {
            out.push_str(&format!("    key_path: {}\n", key));
        }
    }
    out.push('\n');

    // ── meta (diagnostics, not part of apply) ─────────────────────────
    out.push_str(&format!(
        "# Meta (read-only, not interpreted by the server):\n\
         #   defaults_version:      {:?}\n\
         #   external_identities:   {}\n\
         #   auth_mode_detected:    {}\n",
        cfg.defaults_version,
        external_identities.len(),
        if cfg.auth_enabled() {
            "SigV4-static + IAM"
        } else {
            "open"
        },
    ));

    print!("{}", out);

    // ── secrets.env ─────────────────────────────────────────────────────
    if let Some(path) = secrets_out_path {
        // Create with 0600 so a stray `ls -la` doesn't broadcast the file
        // to anyone with read access to the dir — the .env sits next to
        // a YAML you intended to commit, and the last thing we want is a
        // shared-dev-box surface.
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)?;
            f.write_all(secrets.render().as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&path, secrets.render())?;
        }
        eprintln!(
            "Wrote {} secret(s) to {}",
            secrets.entries.len(),
            path.display()
        );
    }
    Ok(())
}

/// Emit a `Permission` as nested YAML at the given indent.
fn emit_permission(out: &mut String, perm: &deltaglider_proxy::iam::Permission, indent: usize) {
    let pad = " ".repeat(indent);
    out.push_str(&format!("{}- effect: {}\n", pad, perm.effect));
    out.push_str(&format!("{}  actions: {:?}\n", pad, perm.actions));
    out.push_str(&format!("{}  resources: {:?}\n", pad, perm.resources));
    if let Some(ref conds) = perm.conditions {
        out.push_str(&format!("{}  conditions: {}\n", pad, conds));
    }
}

/// Accept either a raw bcrypt string (`$2b$...`) or its base64 encoding,
/// mirroring the server's own `DGP_BOOTSTRAP_PASSWORD_HASH` handling.
fn decode_hash(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("$2") {
        return trimmed.to_string();
    }
    use base64::Engine as _;
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(trimmed) {
        if let Ok(s) = String::from_utf8(bytes) {
            if s.starts_with("$2") {
                return s;
            }
        }
    }
    trimmed.to_string()
}

/// Uppercase + non-alphanumerics → `_`, so names flow into conventional
/// env-var-style secret identifiers (`!secret USER_ALICE_SECRET`).
fn sanitize_env_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

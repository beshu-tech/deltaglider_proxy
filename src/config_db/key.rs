// SPDX-License-Identifier: BUSL-1.1

//! The SQLCipher key for the config DB.
//!
//! The key is independent of the bootstrap password: the bcrypt hash only
//! verifies admin logins. Source, in order:
//!
//! 1. `DGP_CONFIG_DB_KEY` (required, and identical on every node, when a
//!    config sync bucket is set);
//! 2. the key file `<db>.key` next to the DB (mode 0600), generated on the
//!    first boot.
//!
//! Earlier releases keyed the DB with the bootstrap password hash. That hash,
//! and a key file that an env key replaces, stay as FALLBACK keys: a DB that
//! opens only with a fallback is re-encrypted with the primary key on boot
//! (see [`super::ConfigDb::open_with_keys`]).

use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// The env var that sets the config DB key.
pub const CONFIG_DB_KEY_ENV: &str = "DGP_CONFIG_DB_KEY";

/// Minimum length of a `DGP_CONFIG_DB_KEY` value. `openssl rand -hex 32`
/// gives 64 characters.
pub const MIN_CONFIG_DB_KEY_LEN: usize = 32;

/// Where the primary key comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbKeySource {
    Env,
    /// An existing key file.
    File(PathBuf),
    /// A key file that this boot generated.
    Generated(PathBuf),
}

impl DbKeySource {
    pub fn describe(&self) -> String {
        match self {
            DbKeySource::Env => CONFIG_DB_KEY_ENV.to_string(),
            DbKeySource::File(p) => format!("key file {}", p.display()),
            DbKeySource::Generated(p) => format!("new key file {}", p.display()),
        }
    }
}

/// Why a fallback key is on the list (for log lines; never the key itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackKind {
    /// The key file, when `DGP_CONFIG_DB_KEY` replaces it.
    KeyFile,
    /// The bootstrap password hash (the key of earlier releases).
    LegacyBootstrapHash,
}

impl FallbackKind {
    pub fn describe(self) -> &'static str {
        match self {
            FallbackKind::KeyFile => "the key file",
            FallbackKind::LegacyBootstrapHash => "the bootstrap password hash (legacy key)",
        }
    }
}

/// A secret string whose `Debug` never shows the value and whose memory is
/// wiped on drop.
#[derive(Clone)]
pub struct DbSecret(Zeroizing<String>);

impl DbSecret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(Zeroizing::new(s.into()))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for DbSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DbSecret(<redacted>)")
    }
}

impl PartialEq for DbSecret {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}

/// The primary key plus the fallback keys that a boot may migrate from.
#[derive(Debug, Clone)]
pub struct ConfigDbKeys {
    pub primary: DbSecret,
    pub source: DbKeySource,
    pub fallbacks: Vec<(FallbackKind, DbSecret)>,
}

impl ConfigDbKeys {
    /// Keys with no fallback (tests, and callers that hold a migrated DB).
    pub fn primary_only(key: &str) -> Self {
        Self {
            primary: DbSecret::new(key),
            source: DbKeySource::Env,
            fallbacks: Vec::new(),
        }
    }

    /// Add a fallback key. An empty key, or one equal to a key already on
    /// the list, is skipped.
    pub fn with_fallback(mut self, kind: FallbackKind, key: &str) -> Self {
        let dup =
            key == self.primary.expose() || self.fallbacks.iter().any(|(_, k)| k.expose() == key);
        if !key.is_empty() && !dup {
            self.fallbacks.push((kind, DbSecret::new(key)));
        }
        self
    }
}

/// `<db>.key` next to the DB file.
pub fn key_file_path(db_path: &Path) -> PathBuf {
    let mut name = db_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "deltaglider_config.db".into());
    name.push(".key");
    db_path.with_file_name(name)
}

/// Pure: validate a `DGP_CONFIG_DB_KEY` value. `Ok(None)` = unset (or blank).
pub fn classify_env_key(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(v) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if v.chars().count() < MIN_CONFIG_DB_KEY_LEN {
        return Err(format!(
            "{CONFIG_DB_KEY_ENV} is too short: it must have at least \
             {MIN_CONFIG_DB_KEY_LEN} characters (generate one with `openssl rand -hex 32`)"
        ));
    }
    Ok(Some(v.to_string()))
}

/// Pure: a config sync bucket shares one encrypted DB between instances, so
/// every instance needs the same key — a per-node key file cannot be it.
pub fn check_sync_needs_env_key(
    sync_bucket: Option<&str>,
    env_key: Option<&str>,
) -> Result<(), String> {
    let sync = sync_bucket.is_some_and(|b| !b.trim().is_empty());
    let has_env = env_key.is_some_and(|k| !k.trim().is_empty());
    if sync && !has_env {
        return Err(format!(
            "config_sync_bucket is set, but {CONFIG_DB_KEY_ENV} is not. The synced config DB \
             is encrypted with that key, so every instance must set {CONFIG_DB_KEY_ENV} to the \
             same value (at least {MIN_CONFIG_DB_KEY_LEN} characters, for example from \
             `openssl rand -hex 32`). On the first start with it, each instance re-encrypts \
             its local config DB from the bootstrap password hash to the new key"
        ));
    }
    Ok(())
}

/// Resolve the config DB keys for `db_path`: the primary key (env, else key
/// file, else a new key file) and the fallbacks. `env` is injected for tests.
///
/// A key file that exists but is empty or unreadable is an error, never
/// replaced: a new key would make the DB unreadable.
pub fn resolve_config_db_keys(
    db_path: &Path,
    legacy_bootstrap_hash: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<ConfigDbKeys, String> {
    let key_file = key_file_path(db_path);
    let env_key = classify_env_key(env(CONFIG_DB_KEY_ENV).as_deref())?;
    let mut keys = match env_key {
        Some(k) => {
            let mut keys = ConfigDbKeys {
                primary: DbSecret::new(k),
                source: DbKeySource::Env,
                fallbacks: Vec::new(),
            };
            // A node that moves from the key file to the env key migrates.
            if key_file.exists() {
                let file_key = read_key_file(&key_file)?;
                keys = keys.with_fallback(FallbackKind::KeyFile, file_key.expose());
            }
            keys
        }
        None if key_file.exists() => ConfigDbKeys {
            primary: read_key_file(&key_file)?,
            source: DbKeySource::File(key_file),
            fallbacks: Vec::new(),
        },
        None => ConfigDbKeys {
            primary: generate_key_file(&key_file)?,
            source: DbKeySource::Generated(key_file),
            fallbacks: Vec::new(),
        },
    };
    if let Some(h) = legacy_bootstrap_hash {
        keys = keys.with_fallback(FallbackKind::LegacyBootstrapHash, h);
    }
    Ok(keys)
}

fn read_key_file(path: &Path) -> Result<DbSecret, String> {
    let raw = Zeroizing::new(
        std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the config DB key file {}: {e}", path.display()))?,
    );
    let key = raw.trim();
    if key.is_empty() {
        return Err(format!(
            "the config DB key file {} is empty. Restore it from a backup, or remove it \
             only if the config DB may be lost",
            path.display()
        ));
    }
    repair_key_file_mode(path);
    Ok(DbSecret::new(key))
}

#[cfg(unix)]
fn repair_key_file_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 {
            match std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                Ok(()) => tracing::warn!(
                    "config DB key file {} was readable by other users; its mode is now 0600",
                    path.display()
                ),
                Err(e) => tracing::warn!(
                    "config DB key file {} is readable by other users and chmod 0600 failed: {e}",
                    path.display()
                ),
            }
        }
    }
}

#[cfg(not(unix))]
fn repair_key_file_mode(_path: &Path) {}

/// Write a new random key (32 bytes, hex) with mode 0600. `create_new` makes
/// sure that a concurrent writer never replaces a key file.
fn generate_key_file(path: &Path) -> Result<DbSecret, String> {
    use rand::RngCore;
    use std::io::Write;
    let mut bytes = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(bytes.as_mut());
    let key = DbSecret::new(hex::encode(bytes.as_ref()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| {
        format!(
            "cannot create the config DB key file {}: {e}",
            path.display()
        )
    })?;
    f.write_all(key.expose().as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| {
            format!(
                "cannot write the config DB key file {}: {e}",
                path.display()
            )
        })?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn env_key_truth_table() {
        assert_eq!(classify_env_key(None), Ok(None));
        assert_eq!(classify_env_key(Some("   ")), Ok(None));
        assert!(classify_env_key(Some("short")).is_err());
        let ok = "k".repeat(MIN_CONFIG_DB_KEY_LEN);
        assert_eq!(classify_env_key(Some(&format!(" {ok} "))), Ok(Some(ok)));
    }

    #[test]
    fn sync_bucket_requires_the_env_key() {
        assert!(check_sync_needs_env_key(None, None).is_ok());
        assert!(check_sync_needs_env_key(Some(""), None).is_ok());
        assert!(check_sync_needs_env_key(None, Some("k")).is_ok());
        assert!(check_sync_needs_env_key(Some("b"), Some("k")).is_ok());
        let err = check_sync_needs_env_key(Some("b"), None).unwrap_err();
        assert!(err.contains(CONFIG_DB_KEY_ENV), "{err}");
        assert!(check_sync_needs_env_key(Some("b"), Some("  ")).is_err());
    }

    #[test]
    fn key_file_sits_next_to_the_db() {
        assert_eq!(
            key_file_path(Path::new("/data/deltaglider_config.db")),
            PathBuf::from("/data/deltaglider_config.db.key")
        );
    }

    #[test]
    fn first_boot_generates_a_0600_key_file_and_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("deltaglider_config.db");
        let a = resolve_config_db_keys(&db, Some("$2b$hash"), no_env).unwrap();
        assert!(matches!(a.source, DbKeySource::Generated(_)));
        assert_eq!(a.primary.expose().len(), 64);
        assert_eq!(a.fallbacks.len(), 1);
        assert_eq!(a.fallbacks[0].0, FallbackKind::LegacyBootstrapHash);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(key_file_path(&db))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let b = resolve_config_db_keys(&db, None, no_env).unwrap();
        assert!(matches!(b.source, DbKeySource::File(_)));
        assert_eq!(
            a.primary, b.primary,
            "a second boot must reuse the key file"
        );
    }

    #[test]
    fn env_key_wins_and_keeps_the_key_file_as_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("deltaglider_config.db");
        let file = resolve_config_db_keys(&db, None, no_env).unwrap();
        let env_key = "e".repeat(40);
        let keys = resolve_config_db_keys(&db, Some("$2b$hash"), |n| {
            (n == CONFIG_DB_KEY_ENV).then(|| env_key.clone())
        })
        .unwrap();
        assert_eq!(keys.source, DbKeySource::Env);
        assert_eq!(keys.primary.expose(), env_key);
        assert_eq!(
            keys.fallbacks
                .iter()
                .map(|(k, s)| (*k, s.expose().to_string()))
                .collect::<Vec<_>>(),
            vec![
                (FallbackKind::KeyFile, file.primary.expose().to_string()),
                (FallbackKind::LegacyBootstrapHash, "$2b$hash".to_string()),
            ]
        );
    }

    #[test]
    fn env_key_writes_no_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("deltaglider_config.db");
        let env_key = "e".repeat(40);
        resolve_config_db_keys(&db, None, |_| Some(env_key.clone())).unwrap();
        assert!(!key_file_path(&db).exists());
    }

    #[test]
    fn empty_key_file_is_an_error_not_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("deltaglider_config.db");
        std::fs::write(key_file_path(&db), "  \n").unwrap();
        let err = resolve_config_db_keys(&db, None, no_env).unwrap_err();
        assert!(err.contains("is empty"), "{err}");
        assert_eq!(std::fs::read_to_string(key_file_path(&db)).unwrap(), "  \n");
    }

    #[test]
    fn short_env_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("deltaglider_config.db");
        let err = resolve_config_db_keys(&db, None, |_| Some("short".into())).unwrap_err();
        assert!(err.contains(CONFIG_DB_KEY_ENV), "{err}");
    }

    #[test]
    fn debug_never_shows_the_key() {
        let keys = ConfigDbKeys::primary_only("super-secret-value");
        assert!(!format!("{keys:?}").contains("super-secret-value"));
    }

    #[test]
    fn fallback_dedup() {
        let k = ConfigDbKeys::primary_only("a")
            .with_fallback(FallbackKind::KeyFile, "a")
            .with_fallback(FallbackKind::LegacyBootstrapHash, "")
            .with_fallback(FallbackKind::LegacyBootstrapHash, "b")
            .with_fallback(FallbackKind::KeyFile, "b");
        assert_eq!(k.fallbacks.len(), 1);
    }
}

// SPDX-License-Identifier: BUSL-1.1

//! Engine construction: backend build, per-backend encryption wrapping, builders.

use super::*;

impl DynEngine {
    /// Create a new engine with the appropriate backend based on configuration.
    /// Pass `metrics` to enable Prometheus instrumentation (None disables it).
    ///
    /// When `config.backends` is non-empty, constructs a `RoutingBackend` that
    /// routes calls to the correct underlying backend per bucket. Otherwise,
    /// uses the legacy single-backend path from `config.backend`.
    pub async fn new(config: &Config, metrics: Option<Arc<Metrics>>) -> Result<Self, StorageError> {
        // Per-backend encryption wrapping.
        //
        // Every backend ends up wrapped by `EncryptingBackend`, whether
        // or not it has a key configured. The wrapper's read path checks
        // the `dg-encrypted` metadata marker + sniffs for the DGE1 magic
        // on "not-encrypted" responses, so even a mode:none backend gets
        // the xattr-strip defense (if the xattr is lost during a
        // backup/restore round-trip, the wrapper refuses to serve
        // DGE1-prefixed ciphertext as plaintext).
        //
        // Two orthogonal encryption layers live here:
        //   - Proxy-side AES-256-GCM via `EncryptingBackend` when the
        //     mode is Aes256GcmProxy. The wrapper encrypts bytes before
        //     they reach `S3Backend::put_object`.
        //   - S3-native SSE (SseKms / SseS3) when mode is one of
        //     those. The proxy passes `NativeEncryptionConfig` into
        //     `S3Backend::new`, which adds `x-amz-server-side-encryption`
        //     headers to every PutObject; AWS encrypts on write and
        //     decrypts transparently on read for callers with KMS perms.
        //
        // The two layers are mutually exclusive on a given backend: you
        // get ONE of {proxy AES-GCM, SSE-KMS, SSE-S3, none}. The
        // encryption config enum enforces this by construction.
        let storage: Box<dyn StorageBackend> = if config.backends.is_empty() {
            // Singleton backend path. Synthetic name "default" matches
            // what `apply_backend_encryption_env` uses for this entry.
            let raw =
                build_raw_backend("default", &config.backend, &config.backend_encryption).await?;
            wrap_backend_with_encryption(
                "default",
                raw,
                &config.backend_encryption,
                &mut KeyIdCollisionCheck::new(),
            )?
        } else {
            // Multi-backend routing. Each named entry is constructed
            // raw (with native-SSE config already baked in), wrapped
            // with its own proxy-AES config if any, then handed to
            // the router.
            let mut backends = std::collections::HashMap::new();
            let mut kid_collisions = KeyIdCollisionCheck::new();
            for named in &config.backends {
                let raw = build_raw_backend(&named.name, &named.backend, &named.encryption).await?;
                let wrapped = wrap_backend_with_encryption(
                    &named.name,
                    raw,
                    &named.encryption,
                    &mut kid_collisions,
                )?;
                backends.insert(named.name.clone(), Arc::new(wrapped));
            }
            let default_name = config.default_backend_name();

            let registry = crate::bucket_policy::BucketPolicyRegistry::new(
                config.buckets.clone(),
                config.max_delta_ratio,
            );
            let routes = registry.routing_table();

            Box::new(crate::storage::RoutingBackend::new(
                backends,
                routes,
                default_name,
            )?)
        };

        Ok(Self::new_with_backend(Arc::new(storage), config, metrics))
    }
}

/// Translate the on-wire `BackendEncryptionConfig` into the
/// S3-specific `NativeEncryptionConfig` for the raw backend
/// constructor. Returns `None` variant for every non-native mode
/// (proxy-AES or mode:none): those are handled by the
/// `EncryptingBackend` wrapper layer above.
pub(super) fn native_encryption_for(
    enc: &crate::config::BackendEncryptionConfig,
) -> crate::storage::NativeEncryptionConfig {
    use crate::config::BackendEncryptionConfig as E;
    use crate::storage::NativeEncryptionConfig as N;
    match enc {
        E::None { .. } | E::Aes256GcmProxy { .. } => N::None,
        E::SseS3 { .. } => N::SseS3,
        E::SseKms {
            kms_key_id,
            bucket_key_enabled,
            ..
        } => N::SseKms {
            kms_key_id: kms_key_id.clone(),
            bucket_key_enabled: *bucket_key_enabled,
        },
    }
}

/// Build ONE storage backend from a `BackendConfig` variant + its
/// encryption config. Native SSE modes are baked into the S3 client
/// here; proxy-AES encryption is layered on top by
/// `wrap_backend_with_encryption`. Filesystem backends ignore native
/// modes (rejected at `Config::check` time).
async fn build_raw_backend(
    name: &str,
    cfg: &BackendConfig,
    enc: &crate::config::BackendEncryptionConfig,
) -> Result<Box<dyn StorageBackend>, StorageError> {
    match cfg {
        BackendConfig::Filesystem { path } => {
            Ok(Box::new(FilesystemBackend::new(path.clone()).await?))
        }
        BackendConfig::S3 { .. } => {
            let native = native_encryption_for(enc);
            Ok(Box::new(
                S3Backend::new(cfg, native)
                    .await?
                    .with_health_name(name, cfg),
            ))
        }
    }
}

/// Tracks explicit `key_id` → `key` pairs seen during construction so
/// we can fail-fast on "two backends claim the same key_id but carry
/// different key material" — the same invariant `Config::check`
/// warns about, re-enforced at engine-construction time (the warnings
/// path is advisory; this is load-bearing for the read-side key_id
/// mismatch check in [`crate::storage::encrypting`]).
pub(super) struct KeyIdCollisionCheck {
    seen: std::collections::BTreeMap<String, Vec<u8>>,
}

impl KeyIdCollisionCheck {
    pub(super) fn new() -> Self {
        Self {
            seen: std::collections::BTreeMap::new(),
        }
    }
    fn record(
        &mut self,
        backend_name: &str,
        key_id: &str,
        key_bytes: &[u8],
    ) -> Result<(), StorageError> {
        if let Some(prev) = self.seen.get(key_id) {
            if prev != key_bytes {
                return Err(StorageError::Encryption(format!(
                    "backend '{}' declares key_id='{}' but a prior backend uses the SAME \
                     key_id with DIFFERENT key bytes — the read-side key_id mismatch check \
                     would then fire on every cross-backend read. Give each backend a \
                     distinct key_id, or set both to the same key (documented portability \
                     escape hatch).",
                    backend_name, key_id
                )));
            }
        } else {
            self.seen.insert(key_id.to_string(), key_bytes.to_vec());
        }
        Ok(())
    }
}

/// Wrap one raw backend with its encryption config. Always wraps
/// (even for mode:none, which produces a no-op wrapper that still
/// fires the xattr-strip sniffer on reads — see B9 from the earlier
/// audit).
///
/// Resolves:
///   * `Aes256GcmProxy` → proxy key + key_id, write_mode Encrypt.
///   * `SseKms` / `SseS3` → primary key None, write_mode PassThrough.
///     Inner S3Backend does the encryption (Step 4); wrapper stays
///     in the stack for read-side sniffer defense + legacy shim.
///   * `None` → no key, write_mode Encrypt (vacuous; encrypt_if_enabled
///     short-circuits when key is None).
///   * `legacy_key` / `legacy_key_id` (Step 5) → populated on the
///     wrapper config when the YAML carries them. Used by the
///     shim-aware read path to decrypt proxy-AES objects while the
///     backend is running in native or no-key mode.
pub(super) fn wrap_backend_with_encryption(
    backend_name: &str,
    inner: Box<dyn StorageBackend>,
    enc: &crate::config::BackendEncryptionConfig,
    collisions: &mut KeyIdCollisionCheck,
) -> Result<Box<dyn StorageBackend>, StorageError> {
    use crate::config::BackendEncryptionConfig as E;
    // Resolve primary (key, key_id) + pick the write_mode.
    let (primary_key, primary_kid, write_mode): (
        Option<crate::storage::EncryptionKey>,
        Option<String>,
        crate::storage::WriteMode,
    ) = match enc {
        E::Aes256GcmProxy {
            key: Some(hex),
            key_id,
            ..
        } => {
            let parsed =
                crate::storage::EncryptionKey::from_hex(hex).map_err(StorageError::Encryption)?;
            // Resolve the id: explicit wins over derived. Derivation
            // mixes the backend name in so same-key/different-name
            // backends get distinct ids (see derive_key_id comment).
            let kid = match key_id {
                Some(explicit) => explicit.clone(),
                None => derive_key_id(backend_name, &parsed.0),
            };
            collisions.record(backend_name, &kid, &parsed.0)?;
            tracing::info!(
                "backend '{}' encryption: ENABLED (AES-256-GCM proxy, key_id={})",
                backend_name,
                kid
            );
            let env_name = env_name_for_backend(backend_name);
            if std::env::var(&env_name).is_err() {
                tracing::warn!(
                    "backend '{}' encryption key was loaded from config file (not {}). \
                     Keep an off-box backup of the key; if the config file is lost, all \
                     encrypted objects on this backend become unrecoverable.",
                    backend_name,
                    env_name
                );
            }
            (Some(parsed), Some(kid), crate::storage::WriteMode::Encrypt)
        }
        E::Aes256GcmProxy { key: None, .. } => {
            tracing::warn!(
                "backend '{}' has encryption mode aes256-gcm-proxy but no key is \
                 configured — writes will NOT be encrypted on this backend. Check YAML \
                 or env var.",
                backend_name
            );
            (None, None, crate::storage::WriteMode::Encrypt)
        }
        E::SseKms { .. } | E::SseS3 { .. } => {
            // Native S3-side encryption — the S3Backend constructor
            // already received the matching `NativeEncryptionConfig`
            // via `build_raw_backend`. The wrapper's primary key is
            // None and writes ALWAYS skip encryption (PassThrough).
            // The inner backend handles encryption at its layer.
            tracing::info!(
                "backend '{}' encryption: ENABLED (native {})",
                backend_name,
                enc.mode_tag()
            );
            (None, None, crate::storage::WriteMode::PassThrough)
        }
        // mode: none — no primary key. WriteMode::Encrypt with key=None
        // is passthrough by construction (see WriteMode doc comment).
        // Leaving it Encrypt keeps the degenerate case indistinguishable
        // from "no encryption configured at all".
        E::None { .. } => (None, None, crate::storage::WriteMode::Encrypt),
    };

    // Resolve the decrypt-only shim from the legacy_* fields (Step 5).
    // Both halves must be present; otherwise the shim silently
    // ignores itself (matches the "needs both id + key to fire"
    // invariant in `pick_decrypt_key`).
    let (legacy_key_opt, legacy_kid_opt) = resolve_legacy_shim(backend_name, enc)?;
    if let (Some(_), Some(ref kid)) = (&legacy_key_opt, &legacy_kid_opt) {
        tracing::info!(
            "backend '{}' decrypt-only shim active (legacy key_id='{}') — reads of \
             objects stamped with that id will decrypt with legacy_key; new writes \
             use the current mode. Remove legacy_key / legacy_key_id from the \
             backend's encryption config once all historical objects have been \
             re-written or deleted.",
            backend_name,
            kid
        );
    }

    let enc_config = Arc::new(ArcSwap::new(Arc::new(crate::storage::EncryptionConfig {
        key: primary_key,
        key_id: primary_kid,
        write_mode,
        legacy_key: legacy_key_opt,
        legacy_key_id: legacy_kid_opt,
    })));
    Ok(Box::new(crate::storage::EncryptingBackend::new(
        inner, enc_config,
    )))
}

/// Pull the legacy_key / legacy_key_id pair out of the per-backend
/// encryption config, parse the hex key, and derive the id if the
/// operator left it implicit. Returns a pair of Options — BOTH
/// present means "shim active"; either one alone is silently
/// ignored (matches the wrapper's bilateral check).
///
/// Unlike the primary key path, the legacy key_id uses a reserved
/// backend-name suffix `{backend_name}::legacy` so an operator who
/// derives both from the same key material (rotation-shaped transition)
/// still gets distinct primary and legacy ids.
pub(super) fn resolve_legacy_shim(
    backend_name: &str,
    enc: &crate::config::BackendEncryptionConfig,
) -> Result<(Option<crate::storage::EncryptionKey>, Option<String>), StorageError> {
    let Some(hex) = enc.legacy_key() else {
        return Ok((None, None));
    };
    let parsed = crate::storage::EncryptionKey::from_hex(hex).map_err(|e| {
        StorageError::Encryption(format!("backend '{}' legacy_key: {}", backend_name, e))
    })?;
    let kid = legacy_key_id_for(backend_name, enc.legacy_key_id(), &parsed);
    Ok((Some(parsed), Some(kid)))
}

/// The id stamped on objects written under a backend's legacy key: the
/// explicit `legacy_key_id`, else derived from `{backend_name}::legacy`.
fn legacy_key_id_for(
    backend_name: &str,
    explicit: Option<&str>,
    key: &crate::storage::EncryptionKey,
) -> String {
    match explicit {
        Some(explicit) => explicit.to_string(),
        None => derive_key_id(&format!("{backend_name}::legacy"), &key.0),
    }
}

/// The legacy (decrypt-only) key id of a backend, as the wrapper resolves
/// it; `None` when no legacy key is configured or it does not parse.
pub(crate) fn effective_legacy_key_id(
    backend_name: &str,
    enc: &crate::config::BackendEncryptionConfig,
) -> Option<String> {
    let parsed = crate::storage::EncryptionKey::from_hex(enc.legacy_key()?).ok()?;
    Some(legacy_key_id_for(
        backend_name,
        enc.legacy_key_id(),
        &parsed,
    ))
}

/// Derive the per-object `key_id` from the backend name + the 32 key
/// bytes. Name is hashed in first, followed by a 0x00 separator, then
/// the key bytes. Truncated to 16 hex chars of SHA-256.
///
/// Name mixing disambiguates "two backends with the same key material"
/// so objects don't accidentally decrypt across backends — the read
/// path's `check_key_id_match` would reject with a specific error
/// rather than the underlying AEAD having any chance to succeed on
/// ciphertext that "happened to" come from a different backend.
///
/// Operators who WANT cross-backend portability pin an explicit
/// matching `key_id` on both — that's the documented escape hatch,
/// exercised by `test_key_id_collision_allowed_with_same_key`.
///
/// Shared with the admin-API summary path (`field_level::derive_key_id_for_summary`)
/// so the stamped id on disk ALWAYS matches the id the operator sees
/// in the Backends panel; drift between the two surfaces would mean
/// a "rotated key" badge that doesn't correspond to any real object
/// metadata.
pub(crate) fn derive_key_id(backend_name: &str, key_bytes: &[u8; 32]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(backend_name.as_bytes());
    hasher.update(b"\0"); // separator: "ab"+"c" ≠ "a"+"bc"
    hasher.update(key_bytes);
    hex::encode(&hasher.finalize()[..8])
}

/// Canonical env var name for a backend's encryption key. Matches the
/// `apply_backend_encryption_env` pairing so an operator who sets
/// `DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY` has that key land on
/// backend `eu-archive` and the "key loaded from file" log points
/// back at the SAME env var name.
fn env_name_for_backend(backend_name: &str) -> String {
    if backend_name == "default" {
        "DGP_ENCRYPTION_KEY".to_string()
    } else {
        format!(
            "DGP_BACKEND_{}_ENCRYPTION_KEY",
            crate::config::env_suffix_for_backend_name(backend_name)
        )
    }
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// Create a new engine with a custom storage backend.
    pub fn new_with_backend(
        storage: Arc<S>,
        config: &Config,
        metrics: Option<Arc<Metrics>>,
    ) -> Self {
        // PERF: codec_concurrency controls how many xdelta3 subprocesses can run
        // in parallel. Defaults to num_cpus * 4 (xdelta3 decode is fast — the bottleneck
        // is network I/O fetching reference+delta from S3, not CPU). Minimum 8.
        // Configurable via DGP_CODEC_CONCURRENCY.
        let codec_concurrency = config.codec_concurrency.unwrap_or_else(|| {
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            (cpus * 4).max(16)
        });
        let spool = Arc::new(
            crate::deltaglider::spool::SpoolDir::shared()
                .unwrap_or_else(|e| panic!("failed to init spool dir: {e}")),
        );
        let range_spools = crate::deltaglider::range_spool::RangeSpoolCache::new(
            std::time::Duration::from_secs(config.range_spool_ttl_secs),
            crate::deltaglider::range_spool::MAX_ENTRIES,
        );
        spool.register_evictor(Arc::downgrade(&range_spools) as _);
        Self {
            storage,
            codec: Arc::new(DeltaCodec::new(config.max_object_size as usize)),
            file_router: FileRouter::new(),
            cache: ReferenceCache::new(config.cache_size_mb),
            max_object_size: config.max_object_size,
            max_passthrough_object_size: config.max_passthrough_object_size,
            codec_semaphore: Arc::new(Semaphore::new(codec_concurrency)),
            prefix_locks: shared_prefix_locks(),
            reference_lock: None,
            metrics,
            metadata_cache: MetadataCache::new((config.metadata_cache_mb as u64) * 1024 * 1024),
            bucket_policies: crate::bucket_policy::BucketPolicyRegistry::new(
                config.buckets.clone(),
                config.max_delta_ratio,
            )
            .with_reserved_bucket(config.config_sync_bucket.as_deref()),
            bucket_usage: None,
            spool,
            range_spools,
        }
    }

    /// Attach the per-instance usage counter (builder; called once at startup
    /// after the usage DB is opened). The handle survives engine rebuilds by
    /// being re-attached.
    pub fn with_bucket_usage(
        mut self,
        usage: Option<Arc<crate::bucket_usage::BucketUsage>>,
    ) -> Self {
        self.bucket_usage = usage;
        self
    }

    /// Attach the cross-instance reference lock (builder; re-attached on engine
    /// rebuild, mirroring `with_bucket_usage`). `None` keeps single-instance
    /// behavior — the in-process `prefix_locks` mutex is the only lock and no S3
    /// round-trip is paid.
    pub fn with_reference_lock(
        mut self,
        lock: Option<Arc<dyn crate::coordination::ReferenceLock>>,
    ) -> Self {
        self.reference_lock = lock;
        self
    }
}

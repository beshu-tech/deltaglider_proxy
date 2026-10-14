// SPDX-License-Identifier: BUSL-1.1

//! Engine construction: backend build, per-backend encryption wrapping, builders.

use super::*;
use crate::storage::DynStorageBackend;

impl DynEngine {
    /// Create a new engine with the appropriate backend based on configuration.
    /// Pass `metrics` to enable Prometheus instrumentation (None disables it).
    ///
    /// When `config.backends` is non-empty, constructs a `RoutingBackend` that
    /// routes calls to the correct underlying backend per bucket. Otherwise,
    /// uses the legacy single-backend path from `config.backend`.
    pub async fn new(config: &Config, metrics: Option<Arc<Metrics>>) -> Result<Self, StorageError> {
        Self::new_sharing(config, metrics, Arc::default()).await
    }

    /// [`Self::new`], with the encryption-policy slots of `policies` (the
    /// registry of the engine this one replaces, `encryption_policies()`).
    /// A raw backend whose definition and own encryption did not change
    /// gets the SAME slot as in the old engine, so a copy that still runs
    /// on the old engine (a replication or lifecycle run, a multipart
    /// upload) writes with the per-bucket policy that this engine
    /// publishes ([`DeltaGliderEngine::publish_encryption`], called when
    /// the engine is installed). Review D: each engine had its own slot,
    /// and a long run kept writing an overridden bucket in its old form.
    pub async fn new_sharing(
        config: &Config,
        metrics: Option<Arc<Metrics>>,
        policies: Arc<crate::storage::EncryptionPolicies>,
    ) -> Result<Self, StorageError> {
        let mut published = Vec::new();
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
        // Per-bucket overrides (issue #116), handed to the wrapper of the
        // raw backend that stores each bucket, under its REAL bucket name.
        let overrides = config.bucket_encryption_overrides();
        let on = |backend: &str| -> Vec<crate::config::BucketOverride<'_>> {
            overrides
                .iter()
                .filter(|o| o.backend == backend)
                .cloned()
                .collect()
        };
        let storage: Box<DynStorageBackend<'static>> = if config.backends.is_empty() {
            // Singleton backend path. Synthetic name "default" matches
            // what `apply_backend_encryption_env` uses for this entry.
            let cfg = resolve_encryption_config(
                "default",
                &config.backend_encryption,
                &on("default"),
                &mut KeyIdCollisionCheck::new(),
            )?;
            refuse_native_overrides_off_s3("default", &config.backend, &on("default"))?;
            let slot = policies.slot(
                &policy_slot_key("default", &config.backend, &config.backend_encryption),
                cfg,
                &mut published,
            );
            let raw = build_raw_backend_native(
                "default",
                &config.backend,
                native_policy(&config.backend_encryption, &slot),
            )
            .await?;
            DynStorageBackend::new_box(crate::storage::EncryptingBackend::new(raw, slot))
        } else {
            // Multi-backend routing. Each named entry is constructed
            // raw (with native-SSE config already baked in), wrapped
            // with its own proxy-AES config if any, then handed to
            // the router.
            let mut backends = std::collections::HashMap::new();
            let mut kid_collisions = KeyIdCollisionCheck::new();
            for named in &config.backends {
                let cfg = resolve_encryption_config(
                    &named.name,
                    &named.encryption,
                    &on(&named.name),
                    &mut kid_collisions,
                )?;
                refuse_native_overrides_off_s3(&named.name, &named.backend, &on(&named.name))?;
                let slot = policies.slot(
                    &policy_slot_key(&named.name, &named.backend, &named.encryption),
                    cfg,
                    &mut published,
                );
                let raw = build_raw_backend_native(
                    &named.name,
                    &named.backend,
                    native_policy(&named.encryption, &slot),
                )
                .await?;
                let wrapped =
                    DynStorageBackend::new_box(crate::storage::EncryptingBackend::new(raw, slot));
                backends.insert(named.name.clone(), Arc::new(wrapped));
            }
            let default_name = config.default_backend_name();

            let registry = crate::bucket_policy::BucketPolicyRegistry::new(
                config.buckets.clone(),
                config.max_delta_ratio,
            );
            let routes = registry.routing_table();

            DynStorageBackend::new_box(crate::storage::RoutingBackend::new(
                backends,
                routes,
                default_name,
            )?)
        };

        let mut engine = Self::new_with_backend(Arc::new(storage), config, metrics);
        engine.encryption = EngineEncryption {
            policies,
            published,
        };
        Ok(engine)
    }
}

/// The key of a raw backend's policy slot: its name AND a fingerprint of
/// its definition and its own encryption. A change of those builds a new
/// raw backend with its own slot (the old engine keeps the old backend
/// policy while it drains, as before); a change of the bucket overrides
/// alone keeps the slot, so it is shared.
fn policy_slot_key(
    name: &str,
    backend: &BackendConfig,
    enc: &crate::config::BackendEncryptionConfig,
) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(serde_json::to_vec(&(backend, enc)).unwrap_or_default());
    format!("{name}\0{}", hex::encode(&h.finalize()[..16]))
}

/// The encryption-policy slots an engine uses, and the configs it
/// publishes into them when it is installed.
#[derive(Default)]
pub struct EngineEncryption {
    policies: Arc<crate::storage::EncryptionPolicies>,
    published: Vec<crate::storage::PolicySlot>,
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// The policy-slot registry, to build the engine that replaces this one
    /// with [`DynEngine::new_sharing`].
    pub fn encryption_policies(&self) -> Arc<crate::storage::EncryptionPolicies> {
        self.encryption.policies.clone()
    }

    /// Store this engine's encryption configs into its slots, so every
    /// engine that shares a slot (the one this replaces, still draining
    /// its runs) writes with them from now on, and drop the slots this
    /// engine does not use. Called when the engine is installed (also on
    /// a rollback, which installs the previous engine again). Infallible.
    pub fn publish_encryption(&self) {
        self.encryption.policies.publish(&self.encryption.published);
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

/// The native SSE policy of one raw S3 backend: the backend's own mode,
/// and each bucket override's mode read from the shared policy slot per
/// request (issue #116). The slot is the one the encrypting wrapper reads,
/// so the SSE headers and the proxy-AES decision of a write come from one
/// published config, also on an engine that drains after an apply.
fn native_policy(
    enc: &crate::config::BackendEncryptionConfig,
    slot: &Arc<arc_swap::ArcSwap<crate::storage::EncryptionConfig>>,
) -> crate::storage::NativeEncryptionPolicy {
    let slot = slot.clone();
    crate::storage::NativeEncryptionPolicy::with_overrides(
        native_encryption_for(enc),
        Arc::new(move |bucket: &str| slot.load().native_for(bucket)),
    )
}

/// A native SSE bucket override on a filesystem backend would store
/// plaintext: refuse the engine (`Config::check_fatal` refuses the config
/// first; this is the last line).
fn refuse_native_overrides_off_s3(
    backend_name: &str,
    backend: &BackendConfig,
    buckets: &[crate::config::BucketOverride<'_>],
) -> Result<(), StorageError> {
    if matches!(backend, BackendConfig::S3 { .. }) {
        return Ok(());
    }
    match buckets.iter().find(|o| o.config.is_native()) {
        Some(o) => Err(StorageError::Encryption(format!(
            "bucket '{}' uses native S3 encryption ({}) but backend '{backend_name}' is a              filesystem backend: native modes need S3",
            o.bucket,
            o.config.mode_tag()
        ))),
        None => Ok(()),
    }
}

/// Build ONE storage backend from a `BackendConfig` variant + its
/// encryption config, with no per-bucket SSE (a raw read-only client).
/// Native SSE modes are baked into the S3 client here; proxy-AES
/// encryption is layered on top by `wrap_backend_with_encryption`.
/// Filesystem backends ignore native modes (rejected at `Config::check`
/// time).
pub(crate) async fn build_raw_backend(
    name: &str,
    cfg: &BackendConfig,
    enc: &crate::config::BackendEncryptionConfig,
) -> Result<Box<DynStorageBackend<'static>>, StorageError> {
    build_raw_backend_native(name, cfg, native_encryption_for(enc).into()).await
}

/// [`build_raw_backend`] with a native SSE policy.
async fn build_raw_backend_native(
    name: &str,
    cfg: &BackendConfig,
    native: crate::storage::NativeEncryptionPolicy,
) -> Result<Box<DynStorageBackend<'static>>, StorageError> {
    match cfg {
        BackendConfig::Filesystem { path } => Ok(DynStorageBackend::new_box(
            FilesystemBackend::new(path.clone()).await?,
        )),
        BackendConfig::S3 { .. } => Ok(DynStorageBackend::new_box(
            S3Backend::new(cfg, native)
                .await?
                .with_health_name(name, cfg),
        )),
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
                    "{} declares key_id='{}' but a prior backend or bucket uses the SAME \
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
///   * `Aes256GcmProxy` → proxy key + key_id, write_mode Encrypt; with
///     no key, write_mode Refuse (every write fails, reads work).
///   * `SseKms` / `SseS3` → primary key None, write_mode PassThrough.
///     Inner S3Backend does the encryption (Step 4); wrapper stays
///     in the stack for read-side sniffer defense + legacy shim.
///   * `None` → no key, write_mode Encrypt (vacuous; encrypt_if_enabled
///     short-circuits when key is None).
///   * `legacy_key` / `legacy_key_id` (Step 5) → populated on the
///     wrapper config when the YAML carries them. Used by the
///     shim-aware read path to decrypt proxy-AES objects while the
///     backend is running in native or no-key mode.
#[cfg(test)]
pub(super) fn wrap_backend_with_encryption(
    backend_name: &str,
    inner: Box<DynStorageBackend<'static>>,
    enc: &crate::config::BackendEncryptionConfig,
    buckets: &[crate::config::BucketOverride<'_>],
    collisions: &mut KeyIdCollisionCheck,
) -> Result<Box<DynStorageBackend<'static>>, StorageError> {
    let cfg = resolve_encryption_config(backend_name, enc, buckets, collisions)?;
    Ok(DynStorageBackend::new_box(
        crate::storage::EncryptingBackend::new(
            inner,
            Arc::new(arc_swap::ArcSwap::new(Arc::new(cfg))),
        ),
    ))
}

/// The wrapper config of one raw backend: its own key, write mode and
/// legacy key, the per-bucket write policies and the keyring.
pub(super) fn resolve_encryption_config(
    backend_name: &str,
    enc: &crate::config::BackendEncryptionConfig,
    buckets: &[crate::config::BucketOverride<'_>],
    collisions: &mut KeyIdCollisionCheck,
) -> Result<crate::storage::EncryptionConfig, StorageError> {
    use crate::config::BackendEncryptionConfig as E;
    // THE key resolution (config::resolve_site_keys): the same ids and
    // the same parse as the config rules.
    let site =
        crate::config::resolve_site_keys(crate::config::KeyScope::Backend(backend_name), enc);
    // Resolve primary (key, key_id) + pick the write_mode.
    let (primary_key, primary_kid, write_mode): (
        Option<crate::storage::EncryptionKey>,
        Option<String>,
        crate::storage::WriteMode,
    ) = match enc {
        E::Aes256GcmProxy { key: Some(_), .. } => {
            let crate::config::SiteKey {
                key: parsed,
                key_id: kid,
            } = site
                .primary
                .expect("a proxy key resolves to a primary slot")
                .map_err(StorageError::Encryption)?;
            collisions.record(&format!("backend '{backend_name}'"), &kid, &parsed.0)?;
            tracing::info!(
                "backend '{}' encryption: ENABLED (AES-256-GCM proxy, key_id={})",
                backend_name,
                kid
            );
            let env_name = env_name_for_backend(backend_name);
            if crate::config::process_env(&env_name).is_none() {
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
        // Fail closed (issue #116): a backend set to encrypt never stores
        // plaintext. Writes are refused; reads keep working, so a missing
        // env var on upgrade does not take the stored data offline.
        E::Aes256GcmProxy { key: None, .. } => {
            tracing::error!(
                "backend '{}' has encryption mode aes256-gcm-proxy but no key is \
                 configured: every write to this backend is REFUSED (the proxy never \
                 stores plaintext on it). Reads keep working. Set encryption.key or {}.",
                backend_name,
                env_name_for_backend(backend_name)
            );
            (None, None, crate::storage::WriteMode::Refuse)
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
    let (legacy_key_opt, legacy_kid_opt) = shim_of(site.legacy)?;
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

    let (bucket_keys, keyring) = resolve_bucket_keys(backend_name, buckets, collisions)?;
    Ok(crate::storage::EncryptionConfig {
        key: primary_key,
        key_id: primary_kid,
        write_mode,
        legacy_key: legacy_key_opt,
        legacy_key_id: legacy_kid_opt,
        buckets: bucket_keys,
        keyring,
    })
}

/// The per-bucket write policies and the keyring of one raw backend
/// (issue #116): each override's write key under its REAL bucket name,
/// and every override key and legacy key by its explicit id. A bucket key
/// id joins the process-wide collision check, so one id names one key.
/// `Config::check_fatal` refuses the shapes this rejects (a missing id);
/// the errors here are the last line.
#[allow(clippy::type_complexity)]
pub(super) fn resolve_bucket_keys(
    backend_name: &str,
    buckets: &[crate::config::BucketOverride<'_>],
    collisions: &mut KeyIdCollisionCheck,
) -> Result<
    (
        std::collections::HashMap<String, crate::storage::BucketKeys>,
        std::collections::HashMap<String, crate::storage::EncryptionKey>,
    ),
    StorageError,
> {
    use crate::config::BackendEncryptionConfig as E;
    let mut policies = std::collections::HashMap::new();
    let mut keyring = std::collections::HashMap::new();
    for o in buckets {
        let label = format!("bucket '{}'", o.bucket);
        // THE key resolution, with the bucket's rules (explicit ids only).
        let site =
            crate::config::resolve_site_keys(crate::config::KeyScope::Bucket(o.bucket), o.config);
        let mut keys = crate::storage::BucketKeys::default();
        match o.config {
            E::Aes256GcmProxy { key: Some(_), .. } => {
                let crate::config::SiteKey { key, key_id: kid } = site
                    .primary
                    .expect("a proxy key resolves to a primary slot")
                    .map_err(StorageError::Encryption)?;
                collisions.record(&label, &kid, &key.0)?;
                keyring.insert(kid.clone(), key.clone());
                keys.key = Some(key);
                keys.key_id = Some(kid.clone());
                tracing::info!(
                    "backend '{backend_name}': {label} (real bucket '{}') encrypts with its own \
                     key (AES-256-GCM proxy, key_id={kid})",
                    o.real_bucket
                );
            }
            E::Aes256GcmProxy { key: None, .. } => {
                tracing::error!(
                    "{label} has encryption mode aes256-gcm-proxy but no key: every write to \
                     it is REFUSED (the proxy never stores plaintext there). Reads keep working."
                );
                keys.write_mode = crate::storage::WriteMode::Refuse;
            }
            E::None { .. } => {}
            // Native SSE (issue #116 P3): the S3 backend sets the headers
            // per request from `keys.native` (below); the wrapper stores
            // the bytes as they are, so nothing is encrypted twice.
            E::SseKms { .. } | E::SseS3 { .. } => {
                keys.write_mode = crate::storage::WriteMode::PassThrough;
                tracing::info!(
                    "backend '{backend_name}': {label} (real bucket '{}') uses native {}",
                    o.real_bucket,
                    o.config.mode_tag()
                );
            }
        }
        // Every override names its native mode, `None` included: an
        // override that is not native gets no SSE headers on a backend
        // whose own mode is native.
        keys.native = native_encryption_for(o.config);
        if let Some(slot) = site.legacy {
            let crate::config::SiteKey { key, key_id: kid } =
                slot.map_err(StorageError::Encryption)?;
            collisions.record(&label, &kid, &key.0)?;
            keyring.insert(kid.clone(), key);
            keys.legacy_key_id = Some(kid);
        }
        policies.insert(o.real_bucket.clone(), keys);
    }
    Ok((policies, keyring))
}

/// The decrypt-only shim of one resolved legacy slot: BOTH halves or
/// neither.
fn shim_of(
    slot: Option<Result<crate::config::SiteKey, String>>,
) -> Result<(Option<crate::storage::EncryptionKey>, Option<String>), StorageError> {
    match slot {
        Some(slot) => {
            let k = slot.map_err(StorageError::Encryption)?;
            Ok((Some(k.key), Some(k.key_id)))
        }
        None => Ok((None, None)),
    }
}

/// The legacy key pair of a backend's encryption config, as the wrapper
/// resolves it (`config::resolve_site_keys`; a missing id is derived from
/// `{backend_name}::legacy`, so a rotation-shaped transition still gets a
/// legacy id distinct from the primary's).
#[cfg(test)]
pub(super) fn resolve_legacy_shim(
    backend_name: &str,
    enc: &crate::config::BackendEncryptionConfig,
) -> Result<(Option<crate::storage::EncryptionKey>, Option<String>), StorageError> {
    shim_of(
        crate::config::resolve_site_keys(crate::config::KeyScope::Backend(backend_name), enc)
            .legacy,
    )
}

/// The legacy (decrypt-only) key id of a backend, as the wrapper resolves
/// it (`config::resolve_site_keys`); `None` when no legacy key is
/// configured or it does not parse.
pub(crate) fn effective_legacy_key_id(
    backend_name: &str,
    enc: &crate::config::BackendEncryptionConfig,
) -> Option<String> {
    crate::config::resolve_site_keys(crate::config::KeyScope::Backend(backend_name), enc)
        .legacy_id()
        .map(str::to_string)
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
        // How many xdelta3 subprocesses run in parallel (DGP_CODEC_CONCURRENCY;
        // default 4 per CPU core, at least 16).
        let codec_concurrency = config.effective_codec_concurrency();
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
            codec: Arc::new(
                DeltaCodec::new(config.max_object_size as usize).with_timeouts(config.tuning.codec),
            ),
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
            tuning: config.tuning.clone(),
            encryption: EngineEncryption::default(),
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
        lock: Option<Arc<crate::coordination::DynReferenceLock<'static>>>,
    ) -> Self {
        self.reference_lock = lock;
        self
    }
}

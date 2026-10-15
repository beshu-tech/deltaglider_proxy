// SPDX-License-Identifier: BUSL-1.1

//! Transparent encryption-at-rest wrapper for any StorageBackend.
//!
//! `EncryptingBackend<B>` wraps a storage backend and encrypts all object data
//! with AES-256-GCM before writing, decrypting on read. Metadata is NOT encrypted.
//!
//! # Two wire formats
//!
//! **`aes-256-gcm-v1`** (single-shot, original) — used for `put_reference`,
//! `put_delta`, `put_passthrough`. These bodies are bounded by
//! `max_object_size` (default 100 MiB) so whole-blob encryption is fine.
//!
//! ```text
//! [12-byte IV] [ciphertext + 16-byte GCM tag]
//! ```
//! Overhead: 28 bytes per object.
//!
//! **`aes-256-gcm-chunked-v1`** (chunked, streaming reads) — used ONLY for
//! `put_passthrough_chunked`. The format exists so the read path can
//! decrypt chunk-by-chunk with bounded peak memory and do O(1) range reads
//! on large objects. The WRITE path is not fully streaming in this
//! release — the wrapper buffers all encrypted frames before handing them
//! to the inner backend's chunked PUT. Peak write memory ≈ ciphertext size.
//! The engine's `max_object_size` ceiling (default 100 MiB) keeps this
//! bounded; if operators raise it, they should budget RAM accordingly.
//!
//! ```text
//! [4-byte magic "DGE1"] [12-byte base_iv]
//! | [4-byte u32 LE frame_len] [ciphertext + 16-byte GCM tag]    # chunk 0
//! | [4-byte u32 LE frame_len] [ciphertext + 16-byte GCM tag]    # chunk 1
//! | ...
//! | [4-byte u32 LE frame_len] [ciphertext + 16-byte GCM tag]    # chunk N (final)
//! ```
//!
//! Each chunk's nonce = `base_iv XOR (chunk_index as big-endian u96)` — unique
//! for 2^32 chunks (256 TiB at 64 KiB each). The AAD for chunk `i` is 16 bytes:
//! `"DGE1" || chunk_index_le_u32 || final_flag_u8 || 0x00 0x00 0x00`, binding
//! the index (foils reordering) and the final flag (foils truncation — the
//! former last-chunk's `final_flag=0` AAD wouldn't match after a truncation).
//!
//! Every non-final chunk is exactly `4 + 64 * 1024 + 16 = 65556` wire bytes,
//! which lets range reads compute chunk offsets in O(1) without scanning
//! the frame-length prefixes.
//!
//! # Detection
//!
//! Objects with `dg-encrypted: aes-256-gcm-v1` → single-shot decrypt.
//! Objects with `dg-encrypted: aes-256-gcm-chunked-v1` → chunked decrypt.
//! Objects without the marker → returned as-is (backward compatible).

//!
//! # Layout
//!
//! `format.rs` holds the formats (key, markers, v1 blob, chunked framing and
//! decoder); this file holds the wrapper backend; `tests.rs` the tests.

#[cfg(test)]
mod bucket_tests;
mod format;
#[cfg(test)]
mod tests;
// Format v2 (issue #120). Wired into the read and write paths in a later
// commit; until then only its tests and the fuzz entry use it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod v2;

pub use format::*;

use super::io_to_storage_error;
use super::list_size_cache::{ListedSize, ResolveListed};
use super::traits::{
    BulkListing, ByteStream, DelegatedListResult, LiteScanResult, MultipartUpload, ReferenceSource,
    StorageBackend, StorageError, StoredObject, UploadedPart,
};
use crate::deltaglider::spool::SpoolBudget;
use crate::types::FileMetadata;
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use arc_swap::ArcSwap;
use bytes::Bytes;
use futures::stream::BoxStream;
use rand::RngCore;
use std::sync::Arc;

/// Drop the at-rest encryption markers from an object's user-metadata so a
/// SUBSEQUENT write decides them fresh: re-stamped iff that write actually
/// encrypts, absent iff it writes plaintext (PassThrough shim / no key). This
/// is the single home for the marker-key set — a stale marker on a plaintext
/// (or re-encrypted) body makes reads pick the wrong key and hard-fail. Used by
/// the re-encrypt job (maintenance) AND the delta-passthrough ship (transfer);
/// both live ABOVE this module, so it belongs here where the consts do.
pub(crate) fn strip_encryption_markers(
    user_metadata: &mut std::collections::HashMap<String, String>,
) {
    user_metadata.remove(ENCRYPTION_MARKER_KEY);
    user_metadata.remove(ENCRYPTION_KEY_ID_KEY);
}

/// The wrapper OWNS its markers: a write stores only its own decision. Any
/// copy the caller carries (a client's `x-amz-meta-dg-encrypted`, a sync
/// tool replaying GET metadata into a PUT, a marker read from another
/// object) is dropped first — stored verbatim, it made the body unreadable.
fn without_markers(metadata: &FileMetadata) -> FileMetadata {
    let mut meta = metadata.clone();
    strip_encryption_markers(&mut meta.user_metadata);
    meta
}

/// For a metadata-only rewrite (body unchanged): `metadata` with the markers
/// of the stored object `raw`, never the caller's own.
fn with_markers_of(metadata: &FileMetadata, raw: &FileMetadata) -> FileMetadata {
    let mut meta = without_markers(metadata);
    for key in [ENCRYPTION_MARKER_KEY, ENCRYPTION_KEY_ID_KEY] {
        if let Some(v) = raw.user_metadata.get(key) {
            meta.user_metadata.insert(key.to_string(), v.clone());
        }
    }
    meta
}

/// A spool refusal is back-pressure: `Throttled` reaches the client as a
/// retryable 503 SlowDown.
fn spool_error(e: std::io::Error) -> StorageError {
    if e.kind() == crate::deltaglider::spool::CONTENDED {
        StorageError::Throttled(e.to_string())
    } else {
        io_to_storage_error(e)
    }
}

/// Controls what the wrapper does on writes. Reads always decrypt
/// tagged objects regardless of this flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WriteMode {
    /// Default: encrypt writes when `key` is Some, pass through
    /// when None.
    #[default]
    Encrypt,
    /// Writes always skip encryption — even when a `key` is present.
    /// Used by the decrypt-only shim during a proxy-AES → native-SSE
    /// mode transition: the wrapper keeps the legacy proxy key so
    /// old objects still decrypt on read, but new writes go through
    /// unencrypted (the inner `S3Backend` is already configured with
    /// native SSE and encrypts server-side on its own).
    PassThrough,
    /// The backend's mode encrypts (`aes256-gcm-proxy`) but no key is
    /// configured. Every object write fails with
    /// [`StorageError::Encryption`]: a backend that the operator set to
    /// encrypt never stores plaintext (issue #116). Reads keep working
    /// (plaintext objects, and the legacy key when one is set).
    Refuse,
}

/// The error of a write under [`WriteMode::Refuse`].
pub(crate) const NO_KEY_WRITE_REFUSAL: &str = "this backend is in aes256-gcm-proxy mode but no \
     key is configured, so the proxy refuses the write instead of storing plaintext. Set \
     encryption.key (or the DGP_*_ENCRYPTION_KEY env var) for the backend, or change its mode";

/// Hot-reloadable encryption configuration.
///
/// `key` is the AES-256 master key used to encrypt/decrypt object
/// bodies on this backend. `key_id` is a stable, non-secret
/// identifier stamped on each written object as the
/// `dg-encryption-key-id` metadata field so read paths can detect
/// cross-backend key mismatch (see [`ENCRYPTION_KEY_ID_KEY`]).
///
/// `legacy_key` + `legacy_key_id` hold the PREVIOUS key after a mode
/// transition. When a read's `dg-encryption-key-id` matches the
/// legacy id, the wrapper decrypts with `legacy_key` instead of
/// `key`. This is the decrypt-only-shim affordance during
/// proxy-AES → native-SSE migrations: operators keep reading old
/// objects without having to rewrite them all up front.
#[derive(Default)]
pub struct EncryptionConfig {
    pub key: Option<EncryptionKey>,
    /// Stable id paired with `key`. Required when `key` is Some so
    /// reads can detect mismatch; the engine resolver derives it
    /// automatically from `SHA-256(backend_name || key)` when the
    /// YAML doesn't pin one explicitly.
    pub key_id: Option<String>,
    /// Write-path policy. `Encrypt` (default) follows the key:
    /// encrypt when Some, passthrough when None. `PassThrough`
    /// forces passthrough regardless of key presence — used by the
    /// decrypt-only shim.
    pub write_mode: WriteMode,
    /// Decrypt-only-shim: the PREVIOUS key after a mode transition.
    /// Consulted on reads when the object's stamped id doesn't match
    /// `key_id` but DOES match `legacy_key_id`.
    pub legacy_key: Option<EncryptionKey>,
    /// Id paired with `legacy_key`. Same shape rules as `key_id`.
    pub legacy_key_id: Option<String>,
    /// Per-bucket write policy (issue #116), indexed by the REAL bucket
    /// name that this wrapper receives (the router has already mapped a
    /// virtual name or an alias to it). A bucket that is not here writes
    /// with the backend's own `key` / `write_mode` above.
    pub buckets: std::collections::HashMap<String, BucketKeys>,
    /// Every other key on this raw backend (the bucket overrides' keys and
    /// legacy keys), indexed by key id. A read picks the key by the id
    /// stamped on the object, so a bucket reads its history under the
    /// backend's key, a rotated key, or a removed override.
    pub keyring: std::collections::HashMap<String, EncryptionKey>,
}

/// One raw backend's policy slot and the config an engine publishes into it.
pub type PolicySlot = (
    String,
    Arc<ArcSwap<EncryptionConfig>>,
    Arc<EncryptionConfig>,
);

/// The encryption-policy slots of the raw backends, shared by an engine
/// and the engine that replaces it (issue #116 review D). An engine
/// rebuild (a config apply) builds new wrappers; without a shared slot, a
/// copy that still runs on the old engine (a replication or lifecycle
/// run, a multipart upload) kept writing a bucket in the form it had
/// before the apply. The new engine publishes its configs into the shared
/// slots when it is installed, so every engine of the lineage writes with
/// the current per-bucket policy.
#[derive(Default)]
pub struct EncryptionPolicies {
    slots: parking_lot::Mutex<std::collections::HashMap<String, Arc<ArcSwap<EncryptionConfig>>>>,
    /// The witness gate that every wrapper of the lineage consults.
    witness: Arc<WitnessGate>,
}

impl EncryptionPolicies {
    /// The witness gate of every engine that shares these slots.
    pub fn witness_gate(&self) -> Arc<WitnessGate> {
        self.witness.clone()
    }

    /// The slot of `key` and remember `cfg` for [`Self::publish`]. A new
    /// slot starts with `cfg` (no engine reads it yet); an existing one
    /// keeps the value that the running engine uses until the publish.
    pub fn slot(
        &self,
        key: &str,
        cfg: EncryptionConfig,
        published: &mut Vec<PolicySlot>,
    ) -> Arc<ArcSwap<EncryptionConfig>> {
        let cfg = Arc::new(cfg);
        let slot = self
            .slots
            .lock()
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(ArcSwap::new(cfg.clone())))
            .clone();
        published.push((key.to_string(), slot.clone(), cfg));
        slot
    }

    /// Store each config into its slot, and forget the slots that are not
    /// in `published` (an engine that still holds one keeps its `Arc`).
    pub fn publish(&self, published: &[PolicySlot]) {
        for (_, slot, cfg) in published {
            slot.store(cfg.clone());
        }
        self.slots
            .lock()
            .retain(|k, _| published.iter().any(|(p, _, _)| p == k));
    }
}

/// The write policy of one bucket with its own `encryption` override.
#[derive(Clone, Default)]
pub struct BucketKeys {
    /// The key new writes use (`None` with `WriteMode::Encrypt` = plaintext).
    pub key: Option<EncryptionKey>,
    pub key_id: Option<String>,
    pub write_mode: WriteMode,
    /// The bucket's decrypt-only key id, if any (its key is in the keyring).
    pub legacy_key_id: Option<String>,
    /// The native SSE headers of the bucket's writes (an S3 backend reads
    /// them per request through [`EncryptionConfig::native_for`]).
    /// `None` for a proxy-AES or plaintext override.
    pub native: crate::storage::NativeEncryptionConfig,
}

impl EncryptionConfig {
    /// The native SSE mode of a bucket with its own override, or `None`
    /// when the bucket follows the backend.
    pub fn native_for(&self, bucket: &str) -> Option<crate::storage::NativeEncryptionConfig> {
        self.buckets.get(bucket).map(|b| b.native.clone())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Chunked-format primitives
// ─────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────
// Streaming decoder
// ─────────────────────────────────────────────────────────────────────

/// A whole stream in one buffer.
async fn collect_stream(stream: ByteStream) -> Result<Vec<u8>, StorageError> {
    use futures::TryStreamExt;
    let parts: Vec<Bytes> = stream.try_collect().await?;
    let mut buf = Vec::with_capacity(parts.iter().map(|b| b.len()).sum());
    for p in parts {
        buf.extend_from_slice(&p);
    }
    Ok(buf)
}

/// `take` bytes of `stream` after its first `skip` bytes.
fn slice_stream(stream: ByteStream, skip: u64, take: u64) -> ByteStream {
    use futures::StreamExt;
    let (mut skip, mut left) = (skip, take);
    Box::pin(
        stream
            .map(move |chunk| {
                chunk.map(|mut b| {
                    let s = skip.min(b.len() as u64) as usize;
                    skip -= s as u64;
                    let b = b.split_off(s);
                    let t = left.min(b.len() as u64) as usize;
                    left -= t as u64;
                    b.slice(..t)
                })
            })
            .filter(|r| futures::future::ready(r.as_ref().map_or(true, |b| !b.is_empty()))),
    )
}

/// Split the `[magic][base_iv]` header off the front of a chunked object's
/// wire stream: the parsed `base_iv`, and the stream of the frames that
/// follow it.
async fn split_chunked_header(
    mut stream: ByteStream,
) -> Result<([u8; IV_LEN], ByteStream), StorageError> {
    use futures::StreamExt;
    let mut head: Vec<u8> = Vec::with_capacity(CHUNK_HEADER_LEN);
    while head.len() < CHUNK_HEADER_LEN {
        match stream.next().await {
            Some(chunk) => head.extend_from_slice(&chunk?),
            None => {
                return Err(StorageError::Encryption(format!(
                    "stream ended before encryption header (got {} of {} bytes)",
                    head.len(),
                    CHUNK_HEADER_LEN
                )))
            }
        }
    }
    if head[..4] != CHUNK_MAGIC {
        return Err(StorageError::Encryption(format!(
            "bad chunked-encryption magic: {:02x?}",
            &head[..4]
        )));
    }
    let mut base_iv = [0u8; IV_LEN];
    base_iv.copy_from_slice(&head[4..CHUNK_HEADER_LEN]);
    let rest = Bytes::from(head.split_off(CHUNK_HEADER_LEN));
    let frames = futures::stream::once(async move { Ok(rest) })
        .filter(|b: &Result<Bytes, StorageError>| {
            futures::future::ready(b.as_ref().map_or(true, |b| !b.is_empty()))
        })
        .chain(stream);
    Ok((base_iv, Box::pin(frames)))
}

/// The stored buckets whose writes this node refuses (the multi-instance
/// encryption witness, issue #116 P3). Indexed by `(raw backend, real
/// bucket)`; the bucket `*` stands for every bucket on the backend that has
/// no entry of its own. Inactive (refuses nothing) on a single instance.
#[derive(Default)]
pub struct WitnessGate {
    state: ArcSwap<WitnessGateState>,
    /// Unix ms of the last successful sync; 0 = never.
    last_ok_ms: std::sync::atomic::AtomicI64,
    /// How long a sync verdict holds; 0 = gate inactive.
    ttl_ms: std::sync::atomic::AtomicI64,
}

/// What the witness decided at its last judgement.
#[derive(Default, Clone, Debug)]
pub struct WitnessGateState {
    /// Storages this node refuses, with the refusal message.
    pub refused: std::collections::HashMap<(String, String), String>,
    /// Storages the witness covers: when the last successful sync is older
    /// than the TTL, writes to them are refused (a stale verdict must not
    /// let this node write a form that the others may have changed).
    pub witnessed: std::collections::HashSet<(String, String)>,
}

/// The bucket name that stands for a backend's buckets with no entry.
pub const WITNESS_BACKEND_DEFAULT: &str = "*";

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl WitnessGate {
    /// Turn the gate on with a verdict lifetime of `ttl`. Until the first
    /// successful sync, every witnessed storage is refused (fail closed).
    pub fn activate(&self, ttl: std::time::Duration) {
        use std::sync::atomic::Ordering;
        self.ttl_ms
            .store((ttl.as_millis() as i64).max(1), Ordering::Relaxed);
    }

    /// Replace the judgement.
    pub fn set(&self, state: WitnessGateState) {
        self.state.store(Arc::new(state));
    }

    /// Record a successful sync (the verdict is fresh from now).
    pub fn synced(&self) {
        self.synced_at(unix_ms());
    }

    /// Record a successful sync that STARTED at `started_ms` (unix ms): the
    /// verdict's age counts from there, so a slow sync does not stretch it.
    pub fn synced_at(&self, started_ms: i64) {
        self.last_ok_ms
            .store(started_ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Pure lookup of `(backend, bucket)` in `map`, with the backend's `*`
    /// entry standing for a bucket that has none of its own.
    fn lookup<'a, V>(
        map: impl Fn(&(String, String)) -> Option<&'a V>,
        has_own: impl Fn(&(String, String)) -> bool,
        backend: &str,
        bucket: &str,
    ) -> Option<&'a V> {
        let own = (backend.to_string(), bucket.to_string());
        if has_own(&own) {
            return map(&own);
        }
        map(&(backend.to_string(), WITNESS_BACKEND_DEFAULT.to_string()))
    }

    /// The refusal of a write to `bucket` on `backend`, if any.
    pub fn refusal(&self, backend: &str, bucket: &str) -> Option<String> {
        use std::sync::atomic::Ordering;
        let ttl = self.ttl_ms.load(Ordering::Relaxed);
        if ttl == 0 {
            return None;
        }
        let st = self.state.load();
        let key = |b: &str| (backend.to_string(), b.to_string());
        let own = key(bucket);
        let witnessed_own = st.witnessed.contains(&own);
        let has_own = |k: &(String, String)| st.refused.contains_key(k) || st.witnessed.contains(k);
        if let Some(msg) = Self::lookup(|k| st.refused.get(k), has_own, backend, bucket) {
            return Some(msg.clone());
        }
        let witnessed = witnessed_own || st.witnessed.contains(&key(WITNESS_BACKEND_DEFAULT));
        let age = unix_ms() - self.last_ok_ms.load(Ordering::Relaxed);
        (witnessed && age > ttl).then(|| {
            format!(
                "bucket '{bucket}' on backend '{backend}': this instance cannot confirm that \
                 its encryption of the bucket agrees with the other instances (no successful \
                 check of the coordination bucket for {}s), so it refuses writes to the bucket \
                 until a check succeeds. Reads keep working",
                age / 1000
            )
        })
    }
}

/// Transparent encryption wrapper around any `StorageBackend`.
pub struct EncryptingBackend<B: StorageBackend> {
    inner: B,
    config: Arc<ArcSwap<EncryptionConfig>>,
    /// The raw backend's name and the witness gate (multi-instance).
    witness: Option<(String, Arc<WitnessGate>)>,
}

impl<B: StorageBackend> EncryptingBackend<B> {
    pub fn new(inner: B, config: Arc<ArcSwap<EncryptionConfig>>) -> Self {
        Self {
            inner,
            config,
            witness: None,
        }
    }

    /// THE witness check (issue #116 P3): a node whose encryption of this
    /// bucket disagrees with the other instances (or cannot confirm that it
    /// agrees) never writes it, also not a metadata rewrite or a folder
    /// marker (an S3 self-copy carries this node's SSE headers). Every
    /// mutating method calls it first (source guard
    /// `every_write_method_checks_the_witness`).
    fn check_witness(&self, bucket: &str) -> Result<(), StorageError> {
        match self
            .witness
            .as_ref()
            .and_then(|(name, gate)| gate.refusal(name, bucket))
        {
            Some(msg) => Err(StorageError::WritesRefused(msg)),
            None => Ok(()),
        }
    }

    /// Refuse the writes that the witness gate names for raw backend `name`.
    pub fn with_witness(mut self, name: &str, gate: Arc<WitnessGate>) -> Self {
        self.witness = Some((name.to_string(), gate));
        self
    }

    /// THE write-side decision: the (key, key_id) to encrypt a write with,
    /// or `None` to write plaintext. `None` under `WriteMode::PassThrough`
    /// even when a key is present (the decrypt-only shim for proxy-AES →
    /// native-SSE transitions: the inner backend encrypts natively). One
    /// `ArcSwap` load, so a concurrent hot-reload cannot pair one config's
    /// key with another config's key_id. Every write path goes through here.
    /// Encrypt `source_path` into the spool file `ct` (chunked wire format,
    /// 64 KiB windows, on a blocking thread), then store `ct` through the
    /// inner backend.
    #[allow(clippy::too_many_arguments)]
    async fn encrypt_file_and_put(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        source_path: &std::path::Path,
        ct: &crate::deltaglider::spool::Spool,
        (key, key_id): (EncryptionKey, Option<String>),
        metadata: &FileMetadata,
        spool: SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        let src = source_path.to_path_buf();
        let dest = ct.path().to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
            use std::io::{Read, Write};
            let mut input = std::fs::File::open(&src).map_err(io_to_storage_error)?;
            let mut out = std::fs::File::create(&dest).map_err(io_to_storage_error)?;
            let (mut framer, header) = ChunkFramer::with_random_iv();
            out.write_all(&header).map_err(io_to_storage_error)?;

            // Read the source in 64 KiB windows; ChunkFramer owns the
            // is_final/index framing so this path just feeds windows and
            // writes the frames it hands back.
            let mut buf = vec![0u8; CHUNK_PLAINTEXT_SIZE];
            loop {
                // Fill a full window (short read only at EOF).
                let mut filled = 0usize;
                while filled < CHUNK_PLAINTEXT_SIZE {
                    let n = input
                        .read(&mut buf[filled..])
                        .map_err(io_to_storage_error)?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                if filled == 0 {
                    break; // EOF, no more windows
                }
                if let Some(frame) = framer.push_window(&key, buf[..filled].to_vec())? {
                    out.write_all(&frame).map_err(io_to_storage_error)?;
                }
                if filled < CHUNK_PLAINTEXT_SIZE {
                    break; // short read = EOF
                }
            }
            // The final frame (or an empty final frame for a zero-byte object).
            out.write_all(&framer.finish(&key)?)
                .map_err(io_to_storage_error)?;
            out.flush().map_err(io_to_storage_error)
        })
        .await
        .map_err(|e| StorageError::Other(format!("encrypt-to-file task join: {e}")))??;

        let mut meta = without_markers(metadata);
        mark_chunked_encrypted(&mut meta, key_id.as_deref());
        self.inner
            .put_passthrough_file(
                bucket,
                prefix,
                filename,
                ct.path(),
                &meta,
                spool.holding(ct),
            )
            .await
    }

    /// THE write-side decision for one REAL bucket: the bucket's own
    /// override when it has one (issue #116), else the backend's. `Err`
    /// under [`WriteMode::Refuse`]: the write must fail, never fall back
    /// to plaintext.
    fn write_key(
        &self,
        bucket: &str,
    ) -> Result<Option<(EncryptionKey, Option<String>)>, StorageError> {
        self.check_witness(bucket)?;
        let cfg = self.config.load();
        let (mode, key, key_id) = match cfg.buckets.get(bucket) {
            Some(b) => (b.write_mode, &b.key, &b.key_id),
            None => (cfg.write_mode, &cfg.key, &cfg.key_id),
        };
        match mode {
            WriteMode::PassThrough => Ok(None),
            WriteMode::Refuse if cfg.buckets.contains_key(bucket) => {
                Err(StorageError::Encryption(format!(
                    "bucket '{bucket}' is in aes256-gcm-proxy mode but no key is configured, \
                     so the proxy refuses the write instead of storing plaintext. Set the \
                     bucket's encryption.key, or change its mode"
                )))
            }
            WriteMode::Refuse => Err(StorageError::Encryption(NO_KEY_WRITE_REFUSAL.to_string())),
            WriteMode::Encrypt => Ok(key.clone().map(|key| (key, key_id.clone()))),
        }
    }

    /// True when a write to `bucket` stores its bytes as they are (no
    /// key, or native SSE). False when it encrypts or is refused.
    fn writes_plaintext(&self, bucket: &str) -> bool {
        matches!(self.write_key(bucket), Ok(None))
    }

    /// True when this wrapper holds any key (primary or decrypt-only
    /// legacy), so the backend may hold proxy-encrypted bodies.
    /// A range of a passthrough object on a backend with a key, and the
    /// object's raw stored metadata, without a metadata HEAD (P3.5).
    ///
    /// The first read guesses the chunked format (what every large or
    /// streamed write stores) and is a ranged GET from wire byte 0: its
    /// response carries the metadata with the encryption markers. A range
    /// that starts in chunk 0 (`bytes=0-`, a video player's first read)
    /// reads the header and the frames it needs in that one GET: the
    /// header and chunk 0 are contiguous. A range further in reads only
    /// the 16-byte header first, then the frames. When the metadata names
    /// another form, the guessed bytes still serve: a v1 (single-shot)
    /// object reads only its rest (none when the guess held it all); a
    /// plaintext object serves a chunk-0 range from them, and a later
    /// range reads its bytes, with the magic check done on the guessed
    /// header (no 4-byte probe).
    async fn keyed_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<(ByteStream, u64, FileMetadata), StorageError> {
        let (first_chunk, _) = chunk_index_for_plaintext_offset(start);
        let guess_end = if first_chunk == 0 {
            // Through the frame that holds `end` (or past the object's
            // end: the backend clamps). The size is not known yet.
            let last_chunk = (end / CHUNK_PLAINTEXT_SIZE as u64).min(u64::from(u32::MAX));
            (CHUNK_HEADER_LEN as u64)
                .saturating_add((last_chunk + 1).saturating_mul(CHUNK_FRAME_WIRE_LEN as u64))
                .saturating_sub(1)
                .min(u64::MAX - 1)
        } else {
            CHUNK_HEADER_LEN as u64 - 1
        };
        let (stream, got, meta) = self
            .inner
            .open_passthrough_range(bucket, prefix, filename, 0, guess_end)
            .await?;
        // The backend clamped the guess: the response holds the whole object.
        let whole = got < guess_end + 1;
        if is_encrypted(&meta) && !is_chunked_encrypted(&meta) {
            // v1 single-shot (bounded by max_object_size): decrypt the
            // whole body. The guessed bytes are its start; the rest, if
            // any, is one more ranged GET.
            let mut wire = collect_stream(stream).await?;
            if !whole {
                match self
                    .pinned_range(bucket, prefix, filename, guess_end + 1, u64::MAX - 1, &meta)
                    .await
                {
                    Ok((rest, _)) => wire.extend_from_slice(&collect_stream(rest).await?),
                    Err(StorageError::InvalidRange(_)) => {}
                    Err(e) => return Err(e),
                }
            }
            let plain = self.decrypt_if_needed(wire, &meta)?;
            let (s, e) = super::clamp_range(start, end, plain.len() as u64)?;
            let slice = Bytes::from(plain[s as usize..=e as usize].to_vec());
            let len = slice.len() as u64;
            return Ok((
                Box::pin(futures::stream::once(async { Ok(slice) })),
                len,
                meta,
            ));
        }
        if !is_chunked_encrypted(&meta) {
            // Plaintext per its metadata. The guessed bytes start at 0, so
            // they show the chunked magic of a ciphertext whose markers were
            // stripped: refuse it, never serve ciphertext as plaintext.
            if first_chunk == 0 {
                // The guess holds the range: through `end`, or to the
                // object's end when the backend clamped it.
                let object_end = if whole { got.checked_sub(1) } else { Some(end) };
                let Some(effective_end) = object_end.map(|o| o.min(end)).filter(|e| *e >= start)
                else {
                    return Err(StorageError::InvalidRange(format!(
                        "range start {start} is past the end of {prefix}/{filename}"
                    )));
                };
                let len = effective_end - start + 1;
                return Ok((
                    slice_stream(sniff_dge1_magic(stream), start, len),
                    len,
                    meta,
                ));
            }
            let head = collect_stream(stream).await?;
            if head.starts_with(&CHUNK_MAGIC) {
                return Err(stripped_marker_error());
            }
            let (stream, len) = self
                .pinned_range(bucket, prefix, filename, start, end, &meta)
                .await?;
            return Ok((stream, len, meta));
        }
        let key = self.pick_decrypt_key(stamped_key_id(&meta))?;
        let final_idx = final_chunk_index_for_plaintext_size(meta.file_size);
        let (start, effective_end) = super::clamp_range(start, end, meta.file_size)?;
        let plaintext_len = effective_end - start + 1;
        let (base_iv, frames) = split_chunked_header(stream).await?;
        if first_chunk == 0 {
            let plain = chunked_decrypt_stream_from_chunk(
                frames,
                key,
                base_iv,
                0,
                final_idx,
                start,
                Some(plaintext_len),
            );
            return Ok((plain, plaintext_len, meta));
        }
        drop(frames);
        let (plain, len) = self
            .chunked_frames_range(
                (bucket, prefix, filename),
                (start, effective_end),
                (key, base_iv, final_idx),
                &meta,
            )
            .await?;
        Ok((plain, len, meta))
    }

    /// A follow-up read of `start..=end` of the stored (raw) object whose
    /// first read gave the metadata `pin`. Its response carries the stored
    /// metadata too: when it shows another version (a write replaced the
    /// object between the two requests), the read fails with a retryable
    /// 503 SlowDown, so the bytes of one version never go out with the
    /// metadata (the ETag, the format) of another. Every follow-up read of
    /// this wrapper goes through here.
    async fn pinned_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
        pin: &FileMetadata,
    ) -> Result<(ByteStream, u64), StorageError> {
        let (stream, len, again) = self
            .inner
            .open_passthrough_range(bucket, prefix, filename, start, end)
            .await?;
        if !super::traits::same_stored_version(pin, &again) {
            return Err(super::traits::object_changed_while_read(
                bucket,
                &format!("{prefix}/{filename}"),
            ));
        }
        Ok((stream, len))
    }

    /// The plaintext `start..=end` (clamped already) of a chunked object
    /// whose header is known: one ranged GET of the frames that hold it,
    /// pinned to the version whose metadata is `pin`.
    async fn chunked_frames_range(
        &self,
        (bucket, prefix, filename): (&str, &str, &str),
        (start, effective_end): (u64, u64),
        (key, base_iv, final_idx): (EncryptionKey, [u8; IV_LEN], u32),
        pin: &FileMetadata,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
        let (first_chunk, _) = chunk_index_for_plaintext_offset(start);
        let (last_chunk, _) = chunk_index_for_plaintext_offset(effective_end);
        let wire_start = wire_offset_of_chunk(first_chunk);
        let wire_end = if last_chunk < final_idx {
            wire_offset_of_chunk(last_chunk) + CHUNK_FRAME_WIRE_LEN as u64 - 1
        } else {
            // Last chunk IS the object's final chunk; it may be
            // shorter than a full frame, so ask for EOF. The
            // `u64::MAX - 1` sentinel works across both backends:
            // S3 interprets it per RFC 7233 (clamp to resource
            // length), filesystem `File::take` limits on actual
            // EOF.
            u64::MAX - 1
        };
        let (ct_stream, _) = self
            .pinned_range(bucket, prefix, filename, wire_start, wire_end, pin)
            .await?;
        // Skip any plaintext bytes before `start` within the
        // first fetched chunk. E.g. for start=70000 and chunk
        // size 65536, first_chunk=1 (starts at plaintext 65536)
        // and we skip 70000 - 65536 = 4464 bytes of its
        // plaintext. The preceding full chunks (index 0) are
        // never fetched or decrypted.
        let skip_bytes = start - (first_chunk as u64) * (CHUNK_PLAINTEXT_SIZE as u64);
        let plaintext_len = effective_end - start + 1;
        let plain = chunked_decrypt_stream_from_chunk(
            ct_stream,
            key,
            base_iv,
            first_chunk,
            final_idx,
            skip_bytes,
            Some(plaintext_len),
        );
        Ok((plain, plaintext_len))
    }

    /// True when `bucket` may hold proxy-encrypted bodies: the backend's
    /// own key or legacy key, or the bucket's override key or legacy key.
    /// (A key of ANOTHER bucket on this backend can still decrypt an
    /// object that was moved here at the storage level: the read path
    /// sees its marker and takes the keyed route; only its listed size
    /// may then be the ciphertext size.)
    fn has_any_key(&self, bucket: &str) -> bool {
        let cfg = self.config.load();
        cfg.key.is_some()
            || cfg.legacy_key.is_some()
            || cfg
                .buckets
                .get(bucket)
                .is_some_and(|b| b.key.is_some() || b.legacy_key_id.is_some())
    }

    /// True when this wrapper encrypts `bucket`'s object bodies in-process.
    /// Gates the streaming multipart path OFF — whole-object GCM framing
    /// doesn't map onto independent S3 parts — so those copies fall back to
    /// the buffered/chunked path.
    fn actively_encrypts(&self, bucket: &str) -> bool {
        !self.writes_plaintext(bucket)
    }

    /// The stored body: the ciphertext, or `data` itself (shared, not
    /// copied) when `bucket` does not encrypt.
    fn encrypt_if_enabled(
        &self,
        bucket: &str,
        data: &bytes::Bytes,
        metadata: &mut FileMetadata,
    ) -> Result<bytes::Bytes, StorageError> {
        match self.write_key(bucket)? {
            Some((key, key_id)) => {
                let encrypted = encrypt(&key, data)?;
                mark_encrypted(metadata, key_id.as_deref());
                Ok(bytes::Bytes::from(encrypted))
            }
            None => Ok(data.clone()),
        }
    }

    /// Pick the (key, key_id) pair that should decrypt this object.
    /// Returns `(key, Some(key_id))` for the matching current or
    /// legacy key, or the current key with no-id-match for legacy
    /// (pre-Step-3) objects. Returns an error on mismatch AND no
    /// legacy fallback — same semantics as `check_key_id_match`
    /// plus the shim overlay.
    fn pick_decrypt_key(&self, object_kid: Option<&str>) -> Result<EncryptionKey, StorageError> {
        // Snapshot under a single ArcSwap load so a concurrent
        // hot-reload can't split the decision.
        let cfg = self.config.load();
        let primary_key = cfg.key.clone();
        let primary_kid = cfg.key_id.clone();
        let legacy = match (cfg.legacy_key.clone(), cfg.legacy_key_id.clone()) {
            (Some(k), Some(kid)) => Some((k, kid)),
            _ => None,
        };
        // Every other key on this raw backend (issue #116).
        let ring = object_kid.and_then(|id| cfg.keyring.get(id).cloned());
        let has_ring = !cfg.keyring.is_empty();
        drop(cfg); // release the guard early

        match object_kid {
            Some(obj_id) => {
                // Prefer primary when ids match.
                if let Some(pid) = primary_kid.as_deref() {
                    if pid == obj_id {
                        return primary_key.ok_or_else(|| {
                            StorageError::Encryption(
                                "object is encrypted but no key is configured".into(),
                            )
                        });
                    }
                }
                // Fall back to legacy if present and matching.
                if let Some((lk, lid)) = legacy {
                    if lid == obj_id {
                        return Ok(lk);
                    }
                }
                // A bucket override's key or legacy key on this backend.
                if let Some(key) = ring {
                    return Ok(key);
                }
                // Neither primary nor legacy matches. Split the
                // error text by root cause — an "actually no key
                // at all" state looks like "<unset>" vs X on the
                // current backend and is a completely different
                // operational fix (restore the key; or configure the
                // backend's encryption mode correctly) from a
                // "rotated-without-shim" mismatch.
                //
                // H6: the old error text cited "rotated without
                // `legacy_key`" in all three sub-cases, which misled
                // operators whose actual problem was "the backend
                // was wrongly flipped to mode: none + no shim".
                let cfg_has_primary = primary_kid.is_some() || primary_key.is_some();
                if !cfg_has_primary && !has_ring {
                    return Err(StorageError::Encryption(format!(
                        "object was encrypted with key id '{obj_id}', but this backend has \
                         NO encryption key configured. The object can't be decrypted until \
                         the key is restored. Set `encryption.key` in YAML or the \
                         DGP_*_ENCRYPTION_KEY env var on this backend. If the object shouldn't \
                         be here (e.g. bucket routed to the wrong backend), fix routing \
                         instead."
                    )));
                }
                let cfg_id = primary_kid.as_deref().unwrap_or("<unset>");
                Err(StorageError::Encryption(format!(
                    "object was encrypted with key id '{obj_id}', but this backend is \
                     configured with key id '{cfg_id}' (no legacy-shim match either). This \
                     usually means: (a) the key was rotated without `legacy_key` set — \
                     restore the old key alongside the new one to read historical objects; \
                     (b) this bucket is routed to the wrong backend; (c) two backends \
                     share physical storage with different keys. Refusing to run AEAD — \
                     the underlying auth failure would be opaque."
                )))
            }
            None => {
                // Legacy object (no stamp). Primary key wins. If
                // primary has no key, we return the same error as
                // pre-shim behaviour — the caller surfaces "no key
                // configured" when needed.
                primary_key.ok_or_else(|| {
                    StorageError::Encryption("object is encrypted but no key is configured".into())
                })
            }
        }
    }

    fn decrypt_if_needed(
        &self,
        data: Vec<u8>,
        metadata: &FileMetadata,
    ) -> Result<Vec<u8>, StorageError> {
        if is_encrypted(metadata) {
            let key = self.pick_decrypt_key(stamped_key_id(metadata))?;
            decrypt(&key, &data)
        } else {
            Ok(data)
        }
    }
}

/// Short-circuit check: if the object carries a stamped `dg-encryption-
/// key-id` AND the wrapper is configured with a `key_id`, they must
/// match. Returns a SPECIFIC error on mismatch — the AEAD auth failure
/// that would otherwise surface gives an opaque "decryption failed"
/// message that doesn't tell operators whether they rotated the key,
/// routed a bucket to the wrong backend, or accidentally pointed two
/// backends at the same physical bucket with different keys.
///
/// Returns `Ok(())` in three legal cases:
///   * both ids present and equal (happy path).
///   * object has no id (legacy / pre-Step-3 object).
///   * wrapper has no id (mode:none wrapper reading an encrypted
///     object; the outer `no key configured` error still fires).
pub fn check_key_id_match(
    object_kid: Option<&str>,
    configured_kid: Option<&str>,
) -> Result<(), StorageError> {
    match (object_kid, configured_kid) {
        (Some(obj), Some(cfg)) if obj != cfg => Err(StorageError::Encryption(format!(
            "object was encrypted with key id '{obj}', but this backend is configured \
             with key id '{cfg}'. This usually means: (a) the key was rotated (unsupported \
             in this release — restore the old key alongside the new one to read historical \
             objects); (b) this bucket is routed to the wrong backend; (c) two backends \
             share physical storage with different keys. Refusing to run AEAD — the \
             underlying auth failure would be opaque."
        ))),
        _ => Ok(()),
    }
}

// Generate the full StorageBackend impl. Encrypt/decrypt methods are hand-written;
// all other methods delegate to self.inner unchanged.
impl<B: StorageBackend + Send + Sync> StorageBackend for EncryptingBackend<B> {
    // ── Encrypt on write ──

    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: bytes::Bytes,
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.check_witness(bucket)?;
        let mut meta = without_markers(metadata);
        let enc = self.encrypt_if_enabled(bucket, &data, &mut meta)?;
        self.inner
            .put_reference(bucket, prefix, enc, &meta, proof)
            .await
    }

    /// The stored bytes move as they are, so the copy is valid only when
    /// this wrapper would store them the same way: plaintext with no write
    /// key, or encrypted under the write key's own id. Otherwise
    /// `Unsupported` (the caller re-encrypts through the proxy). The
    /// destination gets the markers of the copied bytes, never the
    /// caller's.
    async fn copy_within(
        &self,
        variant: super::traits::ObjectVariant,
        src: super::traits::ObjectAt<'_>,
        dst: super::traits::ObjectAt<'_>,
        source: &FileMetadata,
        metadata: &FileMetadata,
    ) -> Result<super::traits::CopyOutcome, StorageError> {
        self.check_witness(dst.bucket)?;
        // The destination bucket's own write policy (issue #116).
        let write_key_id = self.write_key(dst.bucket)?.map(|(_, id)| id);
        let same_form = match (&write_key_id, is_encrypted(source)) {
            (None, false) => true,
            (Some(Some(id)), true) => stamped_key_id(source) == Some(id.as_str()),
            _ => false,
        };
        if !same_form {
            return Ok(super::traits::CopyOutcome::Unsupported);
        }
        let meta = with_markers_of(metadata, source);
        self.inner
            .copy_within(variant, src, dst, source, &meta)
            .await
    }

    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: bytes::Bytes,
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.check_witness(bucket)?;
        let mut meta = without_markers(metadata);
        let enc = self.encrypt_if_enabled(bucket, &data, &mut meta)?;
        self.inner
            .put_delta(bucket, prefix, filename, enc, &meta, proof)
            .await
    }

    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: bytes::Bytes,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.check_witness(bucket)?;
        let mut meta = without_markers(metadata);
        let enc = self.encrypt_if_enabled(bucket, &data, &mut meta)?;
        self.inner
            .put_passthrough(bucket, prefix, filename, enc, &meta)
            .await
    }

    /// BOUNDED-MEMORY file store: encrypt the source file in 64 KiB windows
    /// into the chunked wire format, streamed to a spool file (peak ≈ one
    /// window + one frame, ~130 KiB — never the whole object), then hand THAT
    /// file to the inner backend (which streams it to disk/S3). This is the
    /// re-encrypt / large-passthrough path; the trait default
    /// `tokio::fs::read`s the whole file and single-shot-encrypts (100
    /// MiB-capped, O(object) RAM). The ciphertext file is in the spool dir
    /// and holds spool budget. PassThrough / no-key → delegate to inner.
    async fn put_passthrough_file(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
        spool: SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        self.check_witness(bucket)?;
        let Some((key, key_id)) = self.write_key(bucket)? else {
            return self
                .inner
                .put_passthrough_file(
                    bucket,
                    prefix,
                    filename,
                    source_path,
                    &without_markers(metadata),
                    spool,
                )
                .await;
        };
        let plain_len = tokio::fs::metadata(source_path)
            .await
            .map_err(io_to_storage_error)?
            .len();
        let ct = spool
            .file(chunked_wire_len_bound(plain_len))
            .await
            .map_err(spool_error)?;
        self.encrypt_file_and_put(
            bucket,
            prefix,
            filename,
            source_path,
            &ct,
            (key, key_id),
            metadata,
            spool,
        )
        .await
    }

    /// BOUNDED-MEMORY relayed-parts store: concatenate the ordered part files
    /// into ONE spool file (streamed, `std::io::copy` — never a whole `Vec`),
    /// then encrypt it windowed like `put_passthrough_file`. The trait default
    /// assembles every part into one in-RAM `Vec` (O(object)); with the raised
    /// passthrough ceiling that OOMs a multi-GiB encrypted multipart upload.
    /// PassThrough / no-key → delegate.
    async fn put_passthrough_parts(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        part_paths: &[std::path::PathBuf],
        metadata: &FileMetadata,
        spool: SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        self.check_witness(bucket)?;
        let Some(write_key) = self.write_key(bucket)? else {
            return self
                .inner
                .put_passthrough_parts(
                    bucket,
                    prefix,
                    filename,
                    part_paths,
                    &without_markers(metadata),
                    spool,
                )
                .await;
        };
        let mut plain_len = 0u64;
        for p in part_paths {
            plain_len += tokio::fs::metadata(p)
                .await
                .map_err(io_to_storage_error)?
                .len();
        }
        let joined = spool.file(plain_len).await.map_err(spool_error)?;
        let ct = spool
            .file(chunked_wire_len_bound(plain_len))
            .await
            .map_err(spool_error)?;
        let parts = part_paths.to_vec();
        let joined_path = joined.path().to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
            let mut out = std::fs::File::create(&joined_path).map_err(io_to_storage_error)?;
            for p in &parts {
                let mut f = std::fs::File::open(p).map_err(io_to_storage_error)?;
                std::io::copy(&mut f, &mut out).map_err(io_to_storage_error)?;
            }
            std::io::Write::flush(&mut out).map_err(io_to_storage_error)
        })
        .await
        .map_err(|e| StorageError::Other(format!("relay-join task: {e}")))??;
        self.encrypt_file_and_put(
            bucket,
            prefix,
            filename,
            joined.path(),
            &ct,
            write_key,
            metadata,
            spool,
        )
        .await
    }

    async fn file_put_spool_bytes(&self, bucket: &str, bytes: u64, parts: bool) -> u64 {
        if self.writes_plaintext(bucket) {
            return self.inner.file_put_spool_bytes(bucket, bytes, parts).await;
        }
        // The ciphertext file, plus the joined plaintext for parts.
        let wire = chunked_wire_len_bound(bytes);
        let joined = if parts { bytes } else { 0 };
        wire + joined + self.inner.file_put_spool_bytes(bucket, wire, false).await
    }

    // put_passthrough_chunked: re-slices incoming chunks into 64 KiB
    // plaintext windows, encrypts each into a framed ciphertext chunk,
    // and forwards a new `Vec<Bytes>` (header + all frames) to the
    // inner backend's chunked PUT. No whole-object buffer in memory —
    // the peak allocation is one 64 KiB plaintext window + one frame
    // (~130 KiB) at a time.
    //
    // When encryption is off, delegates to inner's chunked impl
    // directly — no copying.
    async fn put_passthrough_chunked(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        chunks: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.check_witness(bucket)?;
        let Some((key, key_id)) = self.write_key(bucket)? else {
            return self
                .inner
                .put_passthrough_chunked(
                    bucket,
                    prefix,
                    filename,
                    chunks,
                    &without_markers(metadata),
                )
                .await;
        };

        // Random per-object base IV (each chunk's nonce derives from it +
        // the chunk index); the wire-format header goes out first.
        let (mut framer, header) = ChunkFramer::with_random_iv();
        let mut out_frames: Vec<Bytes> = Vec::with_capacity(chunks.len() + 4);
        out_frames.push(Bytes::from(header));

        // Re-slice incoming chunks into exactly CHUNK_PLAINTEXT_SIZE windows
        // (multipart parts arrive ~5 MiB, so one Bytes splits into ~80 windows)
        // and drive them through ChunkFramer — which owns the is_final/index
        // invariant and the zero-byte / sub-window / boundary tail cases that
        // used to be a 5-branch hand-rolled block here.
        let mut pt_window: Vec<u8> = Vec::with_capacity(CHUNK_PLAINTEXT_SIZE);
        for incoming in chunks {
            let mut remaining: &[u8] = incoming.as_ref();
            while !remaining.is_empty() {
                let space = CHUNK_PLAINTEXT_SIZE - pt_window.len();
                let take = std::cmp::min(space, remaining.len());
                pt_window.extend_from_slice(&remaining[..take]);
                remaining = &remaining[take..];
                if pt_window.len() == CHUNK_PLAINTEXT_SIZE {
                    if let Some(frame) = framer.push_window(&key, std::mem::take(&mut pt_window))? {
                        out_frames.push(Bytes::from(frame));
                    }
                    pt_window = Vec::with_capacity(CHUNK_PLAINTEXT_SIZE);
                }
            }
        }
        // Feed a non-empty tail (sub-window remainder) as the last window, then
        // let the framer emit the final frame. An all-empty input → finish()
        // emits the single empty final frame (zero-byte object).
        if !pt_window.is_empty() {
            if let Some(frame) = framer.push_window(&key, pt_window)? {
                out_frames.push(Bytes::from(frame));
            }
        }
        out_frames.push(Bytes::from(framer.finish(&key)?));

        let mut meta = without_markers(metadata);
        mark_chunked_encrypted(&mut meta, key_id.as_deref());
        self.inner
            .put_passthrough_chunked(bucket, prefix, filename, &out_frames, &meta)
            .await
    }

    // ── Decrypt on read ──

    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> {
        let (stream, _) = self
            .open_object(bucket, prefix, StoredObject::Reference)
            .await?;
        collect_stream(stream).await
    }

    // NOTE: get_reference_to_file deliberately uses the trait DEFAULT (get_reference
    // + write) here. AES-GCM decryption is a whole-buffer operation in the current
    // design, so a streaming hardlink/stream-to-file would hand xdelta3 ciphertext.
    // The default decrypts to RAM then writes plaintext to the spool — correct, but
    // bounded by reference size for encrypted backends. Streaming decryption is a
    // separate future optimisation (chunked GCM / per-block nonce).

    /// A plaintext reference passes through (the filesystem keeps its
    /// hardlink). An encrypted one is decrypted whole, as `get_reference`
    /// does, and its size is the decrypted length.
    async fn open_reference_source(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<ReferenceSource, StorageError> {
        let source = self.inner.open_reference_source(bucket, prefix).await?;
        if !is_encrypted(&source.metadata) {
            return Ok(source);
        }
        let meta = source.metadata.clone();
        let data = source.read_all().await?;
        let plain = self.decrypt_if_needed(data, &meta)?;
        let size = plain.len() as u64;
        let stream: ByteStream = Box::pin(futures::stream::once(async { Ok(Bytes::from(plain)) }));
        Ok(ReferenceSource::stream(stream, size, meta))
    }

    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let (stream, _) = self
            .open_object(bucket, prefix, StoredObject::Delta(filename))
            .await?;
        collect_stream(stream).await
    }

    /// One blob, plaintext, for both wire formats: a chunked object runs
    /// through the chunked decoder (a single-shot decrypt would read its
    /// `DGE1` header as an IV and fail).
    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let (stream, _) = self
            .open_object(bucket, prefix, StoredObject::Passthrough(filename))
            .await?;
        collect_stream(stream).await
    }

    async fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        let (stream, _) = self
            .open_object(bucket, prefix, StoredObject::Passthrough(filename))
            .await?;
        Ok(stream)
    }

    /// The inner read carries the raw stored metadata, markers included, so
    /// the format decision needs no HEAD of its own (storage-7). Returns
    /// that raw metadata, as `get_*_metadata` does.
    async fn open_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<(ByteStream, FileMetadata), StorageError> {
        let (stream, meta) = self.inner.open_object(bucket, prefix, object).await?;
        let plain: ByteStream = match object {
            // References and deltas are single-shot or plaintext.
            StoredObject::Reference | StoredObject::Delta(_) => {
                if is_encrypted(&meta) {
                    let data = collect_stream(stream).await?;
                    let plain = self.decrypt_if_needed(data, &meta)?;
                    Box::pin(futures::stream::once(async { Ok(Bytes::from(plain)) }))
                } else {
                    stream
                }
            }
            // Chunked: decrypt frame by frame, no whole-object buffer (a
            // 5 GiB download stays at ~130 KiB peak in the decoder). The
            // shim-aware key choice names the fix on a key-id mismatch.
            StoredObject::Passthrough(_) if is_chunked_encrypted(&meta) => {
                let key = self.pick_decrypt_key(stamped_key_id(&meta))?;
                let final_idx = final_chunk_index_for_plaintext_size(meta.file_size);
                chunked_decrypt_stream(stream, key, final_idx, 0, None)
            }
            // Single-shot (bounded by max_object_size): decrypt whole.
            StoredObject::Passthrough(_) if is_encrypted(&meta) => {
                let data = collect_stream(stream).await?;
                let plain = self.decrypt_if_needed(data, &meta)?;
                Box::pin(futures::stream::once(async { Ok(Bytes::from(plain)) }))
            }
            // Plaintext per metadata: refuse a body that starts with the
            // chunked `DGE1` magic (the xattr got stripped in a
            // backup/restore and the body is still ciphertext). Plaintext
            // that starts with those 4 bytes is a 1-in-2^32 case.
            StoredObject::Passthrough(_) => Box::pin(sniff_dge1_magic(stream)),
        };
        Ok((plain, meta))
    }

    // === Multipart upload (Phase B) ===
    //
    // Forward to the inner backend ONLY when this wrapper isn't actively
    // encrypting. When it IS (proxy-AES), the transfer layer never calls
    // these (gated off by `multipart_storage_label`); the explicit error
    // is defence-in-depth in case a future caller skips the gate.

    async fn create_multipart_upload(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
    ) -> Result<MultipartUpload, StorageError> {
        self.check_witness(bucket)?;
        // A refused write names its cause, not "multipart unsupported".
        self.write_key(bucket)?;
        if self.actively_encrypts(bucket) {
            return Err(StorageError::Other(
                "multipart upload unsupported on a proxy-AES-encrypting bucket".to_string(),
            ));
        }
        self.inner
            .create_multipart_upload(bucket, prefix, filename, &without_markers(metadata))
            .await
    }

    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        part_number: i32,
        data: Bytes,
    ) -> Result<UploadedPart, StorageError> {
        self.check_witness(&upload.bucket)?;
        self.inner
            .upload_part(upload, prefix, filename, part_number, data)
            .await
    }

    async fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        parts: &[UploadedPart],
        assembled: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<String, StorageError> {
        self.check_witness(&upload.bucket)?;
        // The bucket's write policy NOW. A config apply can change it
        // between create and complete: the new engine publishes its policy
        // into the slot that this (older) wrapper shares
        // (`EncryptionPolicies`), and a multipart upload completes on the
        // engine it started on. A bucket that encrypts never
        // stores the parts as plaintext: buffered parts are encrypted like
        // any chunked write; native parts already sit on the backend in
        // plaintext, so the upload is refused.
        if self.write_key(&upload.bucket)?.is_some() {
            if upload.native {
                return Err(StorageError::Encryption(format!(
                    "bucket '{}' encrypts at rest, but this multipart upload stored its \
                     parts natively (unencrypted): the encryption of the bucket changed \
                     during the upload. Start the upload again",
                    upload.bucket
                )));
            }
            self.put_passthrough_chunked(&upload.bucket, prefix, filename, assembled, metadata)
                .await?;
            return Ok(format!("\"{}\"", metadata.md5));
        }
        self.inner
            .complete_multipart_upload(
                upload,
                prefix,
                filename,
                parts,
                assembled,
                &without_markers(metadata),
            )
            .await
    }

    async fn abort_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        self.inner
            .abort_multipart_upload(upload, prefix, filename)
            .await
    }

    fn multipart_storage_label(&self, bucket: &str) -> &'static str {
        if self.actively_encrypts(bucket) {
            return "aes256-gcm-proxy";
        }
        self.inner.multipart_storage_label(bucket)
    }

    fn supports_native_multipart(&self, bucket: &str) -> bool {
        // Proxy-AES rewrites each part's bytes, so the passthrough streaming
        // multipart path can't be used regardless of the inner backend.
        if self.actively_encrypts(bucket) {
            return false;
        }
        self.inner.supports_native_multipart(bucket)
    }

    fn lite_list_carries_logical_facts(&self, bucket: &str) -> bool {
        // When actively encrypting, the lite list reports CIPHERTEXT size/etag —
        // parity must HEAD for plaintext logical facts.
        if self.actively_encrypts(bucket) {
            return false;
        }
        self.inner.lite_list_carries_logical_facts(bucket)
    }

    async fn get_passthrough_stream_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
        let (stream, len, _) = self
            .open_passthrough_range(bucket, prefix, filename, start, end)
            .await?;
        Ok((stream, len))
    }

    /// Without a key, objects here are plaintext: the inner backend's one
    /// read gives the range and the metadata, with the same stripped-marker
    /// checks as a range read. With a key, `keyed_range` takes the
    /// metadata from its first ranged GET, never from a HEAD.
    async fn open_passthrough_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<(ByteStream, u64, FileMetadata), StorageError> {
        if self.has_any_key(bucket) {
            return self.keyed_range(bucket, prefix, filename, start, end).await;
        }
        let (stream, len, meta) = self
            .inner
            .open_passthrough_range(bucket, prefix, filename, start, end)
            .await?;
        if is_chunked_encrypted(&meta) || is_encrypted(&meta) {
            // Encrypted, and no key to read it: the key choice names why,
            // with no more requests. A key that a reload added since the
            // check above reads the object the keyed way.
            drop(stream);
            self.pick_decrypt_key(stamped_key_id(&meta))?;
            return self.keyed_range(bucket, prefix, filename, start, end).await;
        }
        if start == 0 {
            return Ok((sniff_dge1_magic(stream), len, meta));
        }
        Ok((stream, len, meta))
    }

    // ── Pass-through (no encryption) ──

    async fn create_bucket(&self, b: &str) -> Result<(), StorageError> {
        self.inner.create_bucket(b).await
    }
    async fn ensure_declared_bucket(&self, b: &str) -> Result<(), StorageError> {
        self.inner.ensure_declared_bucket(b).await
    }
    async fn delete_bucket(&self, b: &str) -> Result<(), StorageError> {
        self.inner.delete_bucket(b).await
    }
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_buckets().await
    }
    async fn list_buckets_with_dates(
        &self,
    ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
        self.inner.list_buckets_with_dates().await
    }
    async fn head_bucket(&self, b: &str) -> Result<bool, StorageError> {
        self.inner.head_bucket(b).await
    }
    async fn has_reference(&self, b: &str, p: &str) -> Result<bool, StorageError> {
        self.inner.has_reference(b, p).await
    }
    async fn get_reference_metadata(&self, b: &str, p: &str) -> Result<FileMetadata, StorageError> {
        self.inner.get_reference_metadata(b, p).await
    }
    async fn get_delta_metadata(
        &self,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.inner.get_delta_metadata(b, p, f).await
    }
    async fn get_passthrough_metadata(
        &self,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.inner.get_passthrough_metadata(b, p, f).await
    }
    async fn get_passthrough_metadata_for_rewrite(
        &self,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<(FileMetadata, super::RewriteBasis), StorageError> {
        // As `get_passthrough_metadata`: the stored metadata as it is.
        self.inner
            .get_passthrough_metadata_for_rewrite(b, p, f)
            .await
    }
    async fn put_passthrough_metadata(
        &self,
        b: &str,
        p: &str,
        f: &str,
        m: &FileMetadata,
        basis: &super::RewriteBasis,
    ) -> Result<(), StorageError> {
        self.check_witness(b)?;
        // Metadata itself is not encrypted, but the RAW object carries the
        // encryption markers, and a caller-supplied FileMetadata built from a
        // decrypted read may lack them. Re-assert the markers from the raw
        // object so a metadata-only rewrite can NEVER strip what makes the
        // object decryptable on read.
        // The markers come from the raw object ONLY: the caller's copy is
        // dropped, so a rewrite can neither strip nor plant one.
        // Fail CLOSED: without the stored markers the rewrite would drop
        // them and make an encrypted body unreadable.
        // A caller's earlier read of the stored object gives them without
        // a HEAD (the rewrite is fenced on that read where the backend can).
        let raw = match &basis.stored {
            Some(raw) => raw.clone(),
            None => self.inner.get_passthrough_metadata(b, p, f).await?,
        };
        let meta = with_markers_of(m, &raw);
        self.inner
            .put_passthrough_metadata(b, p, f, &meta, basis)
            .await
    }
    async fn put_reference_metadata(
        &self,
        b: &str,
        p: &str,
        m: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.check_witness(b)?;
        // Same rule as put_passthrough_metadata: the body is not rewritten,
        // so its markers stay those of the stored reference.
        let raw = self.inner.get_reference_metadata(b, p).await?;
        let meta = with_markers_of(m, &raw);
        self.inner.put_reference_metadata(b, p, &meta, proof).await
    }
    async fn delete_reference(
        &self,
        b: &str,
        p: &str,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.inner.delete_reference(b, p, proof).await
    }
    async fn flush_pending(&self) -> Result<(), StorageError> {
        self.inner.flush_pending().await
    }
    async fn reference_fence(
        &self,
        b: &str,
        p: &str,
    ) -> Result<super::traits::RefFence, StorageError> {
        self.inner.reference_fence(b, p).await
    }
    /// Metadata is stored as-is (see `get_reference_metadata`).
    async fn reference_fence_with_metadata(
        &self,
        b: &str,
        p: &str,
    ) -> Result<(super::traits::RefFence, Option<FileMetadata>), StorageError> {
        self.inner.reference_fence_with_metadata(b, p).await
    }
    /// The same transformation as the unfenced writes (encrypt the body, keep
    /// the stored markers on a metadata-only write), then the inner fenced
    /// write: the fence is on the stored object, which is what the inner
    /// backend observes.
    async fn write_reference_fenced(
        &self,
        b: &str,
        p: &str,
        op: super::traits::RefWrite<'_>,
        fence: &super::traits::RefFence,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<super::traits::RefFence, StorageError> {
        self.check_witness(b)?;
        use super::traits::RefWrite;
        match op {
            RefWrite::Put { data, metadata } => {
                let mut meta = without_markers(metadata);
                let enc = self.encrypt_if_enabled(b, data, &mut meta)?;
                self.inner
                    .write_reference_fenced(
                        b,
                        p,
                        RefWrite::Put {
                            data: &enc,
                            metadata: &meta,
                        },
                        fence,
                        proof,
                    )
                    .await
            }
            // Like the unfenced default of `put_reference_from_file`: read the
            // file, then encrypt it as a Put.
            RefWrite::PutFile { path, metadata } => {
                let data = bytes::Bytes::from(tokio::fs::read(path).await?);
                let mut meta = without_markers(metadata);
                let enc = self.encrypt_if_enabled(b, &data, &mut meta)?;
                self.inner
                    .write_reference_fenced(
                        b,
                        p,
                        RefWrite::Put {
                            data: &enc,
                            metadata: &meta,
                        },
                        fence,
                        proof,
                    )
                    .await
            }
            RefWrite::Metadata { metadata } => {
                let raw = self.inner.get_reference_metadata(b, p).await?;
                let meta = with_markers_of(metadata, &raw);
                self.inner
                    .write_reference_fenced(
                        b,
                        p,
                        RefWrite::Metadata { metadata: &meta },
                        fence,
                        proof,
                    )
                    .await
            }
            RefWrite::Delete => {
                self.inner
                    .write_reference_fenced(b, p, RefWrite::Delete, fence, proof)
                    .await
            }
        }
    }
    async fn delete_delta(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
        self.inner.delete_delta(b, p, f).await
    }
    async fn delete_passthrough(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
        // The inner backend drops the listing facts of the ciphertext.
        self.inner.delete_passthrough(b, p, f).await
    }
    // The version is the stored ciphertext's: an overwrite changes it too.
    async fn variant_version(
        &self,
        b: &str,
        p: &str,
        f: &str,
        v: crate::storage::ObjectVariant,
    ) -> Result<Option<String>, StorageError> {
        self.inner.variant_version(b, p, f, v).await
    }
    async fn delete_variant_if(
        &self,
        b: &str,
        p: &str,
        f: &str,
        v: crate::storage::ObjectVariant,
        version: &str,
    ) -> Result<bool, StorageError> {
        self.inner.delete_variant_if(b, p, f, v, version).await
    }
    async fn scan_deltaspace(&self, b: &str, p: &str) -> Result<Vec<FileMetadata>, StorageError> {
        self.inner.scan_deltaspace(b, p).await
    }
    async fn scan_deltaspace_lite(&self, b: &str, p: &str) -> Result<LiteScanResult, StorageError> {
        self.inner.scan_deltaspace_lite(b, p).await
    }
    async fn holds_only_reference(&self, b: &str, p: &str) -> Result<bool, StorageError> {
        self.inner.holds_only_reference(b, p).await
    }
    async fn list_deltaspaces(&self, b: &str) -> Result<Vec<String>, StorageError> {
        self.inner.list_deltaspaces(b).await
    }
    async fn list_reference_prefixes(&self, b: &str, s: &str) -> Result<Vec<String>, StorageError> {
        self.inner.list_reference_prefixes(b, s).await
    }
    async fn total_size(&self, b: Option<&str>) -> Result<u64, StorageError> {
        self.inner.total_size(b).await
    }
    fn write_refusal(&self, bucket: &str) -> Option<String> {
        match self.check_witness(bucket) {
            Err(StorageError::WritesRefused(m)) => Some(m),
            _ => None,
        }
    }
    async fn put_directory_marker(&self, b: &str, k: &str) -> Result<(), StorageError> {
        self.check_witness(b)?;
        self.inner.put_directory_marker(b, k).await
    }
    async fn bulk_list_objects(
        &self,
        b: &str,
        p: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        self.inner.bulk_list_objects(b, p).await
    }
    async fn enrich_list_metadata(
        &self,
        b: &str,
        o: Vec<(String, FileMetadata)>,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        self.inner.enrich_list_metadata(b, o).await
    }
    async fn bulk_list_objects_with_baselines(
        &self,
        b: &str,
        p: &str,
        start_after: Option<&str>,
        max_listed: Option<usize>,
    ) -> Result<BulkListing, StorageError> {
        self.inner
            .bulk_list_objects_with_baselines(b, p, start_after, max_listed)
            .await
    }
    async fn resolve_listed_sizes(
        &self,
        b: &str,
        objects: &mut [(String, FileMetadata)],
        how: ResolveListed,
    ) -> Vec<ListedSize> {
        // With a key (current or legacy), a passthrough entry may be
        // ciphertext: its facts are looked up like a delta's.
        let mut sizes = self
            .inner
            .resolve_listed_sizes(
                b,
                objects,
                ResolveListed {
                    passthrough_may_differ: how.passthrough_may_differ || self.has_any_key(b),
                    ..how
                },
            )
            .await;
        // An inner listing of ciphertext (S3) reports the ciphertext size of a
        // passthrough object: without a cache hit, only that is known. A
        // decrypt-only key (write mode passthrough, or a legacy key) still
        // reads ciphertext written before.
        if self.has_any_key(b) && !self.inner.lite_list_carries_logical_facts(b) {
            downgrade_ciphertext_listings(objects, &mut sizes);
        }
        // A listed delta may be ciphertext whenever a key exists: its
        // listed `.delta` size is not the delta size a HEAD shows.
        if self.has_any_key(b) && !self.inner.lite_list_carries_logical_facts(b) {
            mark_encrypted_deltas(objects, &mut sizes);
        }
        sizes
    }
    async fn list_objects_delegated(
        &self,
        b: &str,
        p: &str,
        d: Option<&str>,
        m: u32,
        t: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        self.inner.list_objects_delegated(b, p, d, m, t).await
    }
}

/// On a backend whose listing reports ciphertext, a passthrough entry the
/// listing-size cache did not resolve carries the ciphertext size: mark it
/// `StoredOnly`. Directory markers stay `Listed`. Pure; unit-tested.
fn mark_encrypted_deltas(objects: &[(String, FileMetadata)], sizes: &mut [ListedSize]) {
    for ((_, meta), size) in objects.iter().zip(sizes.iter_mut()) {
        if *size == ListedSize::Cached && meta.is_delta() {
            *size = ListedSize::CachedEncryptedDelta;
        }
    }
}

fn downgrade_ciphertext_listings(objects: &[(String, FileMetadata)], sizes: &mut [ListedSize]) {
    for ((key, _), size) in objects.iter().zip(sizes.iter_mut()) {
        if *size == ListedSize::Listed && !key.ends_with('/') {
            *size = ListedSize::StoredOnly;
        }
    }
}

// SPDX-License-Identifier: BUSL-1.1

//! The multi-instance encryption witness (issue #116 P3).
//!
//! The YAML config is per node, so two nodes can disagree on how one bucket
//! is encrypted (a node that lacks a bucket's `encryption` block, or its
//! key). The witness makes such a node refuse WRITES to that bucket: two
//! nodes never write one bucket in two forms.
//!
//! ## The manifest
//!
//! `_dgp/encryption/intent.json` in the coordination bucket holds, for each
//! stored bucket (raw backend + real bucket name; the bucket `*` stands for
//! every bucket of the backend with no entry of its own), the published
//! write encryption `{mode, key_id, kcv}`, who set it and when. Never key
//! material: the key id is the public id that every object carries, and
//! the key check value is a truncated HMAC of the key. Written with the CAS
//! rules of the rest of coordination: create with `If-None-Match: *`,
//! replace with `If-Match: <etag>`, re-read and merge on a lost race.
//!
//! ## Who changes an entry (owner decision: no automatic takeover)
//!
//! 1. **Seeding**, create-only: each node adds every storage that it
//!    ENCRYPTS (its bucket blocks, the buckets it names that inherit an
//!    encrypting backend, each encrypting backend's `*`) when the manifest
//!    lacks it. Never plaintext, never a refusal (a proxy mode, no key).
//! 2. **A config apply** that CHANGES a storage's encryption on a node
//!    publishes that storage (per changed storage). A downgrade needs the
//!    apply's confirmation; a confirmed subject whose intent is plaintext
//!    is published also when the config did not change.
//! 3. **`witness publish`** (`POST /_/api/admin/encryption-witness/publish`,
//!    CLI `deltaglider_proxy witness publish <subject>...`): the operator
//!    publishes this node's current encryption of named storages, e.g.
//!    after a restart rollout (a ConfigMap, an env key). Plaintext needs the
//!    downgrade confirmation. Audited (`bucket_encryption_witness_publish`).
//!
//! Every other disagreement fails CLOSED: a node whose encryption of a
//! witnessed storage differs from the published one refuses writes to it
//! (`503 SlowDown` naming the mismatch and the publish command; reads keep
//! working). A node that takes the verdict from the backend's `*` entry
//! (a bucket block with no entry of its own) is judged against it too.
//!
//! **Freshness**: a verdict holds two polls, measured from the START of the
//! sync that produced it; then the node refuses writes to every witnessed
//! storage until a sync succeeds. At boot the gate starts closed for them.
//! Residual: a write admitted before the gate closes (a large upload) can
//! finish after it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::{Config, WriteEncryption};
use crate::storage::{WitnessGate, WitnessGateState, WITNESS_BACKEND_DEFAULT};

/// The manifest object.
pub const MANIFEST_KEY: &str = "_dgp/encryption/intent.json";
/// Default poll interval (`DGP_ENCRYPTION_WITNESS_POLL_SECS`).
pub const DEFAULT_POLL_SECS: u64 = 30;

/// A stored bucket: `(raw backend, real bucket)`; `*` = the backend's
/// buckets with no entry of their own.
pub type Storage = (String, String);

/// One published intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    #[serde(flatten)]
    pub intent: WriteEncryption,
    /// The node that set it.
    #[serde(default)]
    pub set_by: String,
    /// When, in unix ms on the S3 server clock (estimated by the writer
    /// from the `Date` of its last read).
    #[serde(default)]
    pub set_at: i64,
}

/// The manifest: backend → real bucket → published intent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub storages: BTreeMap<String, BTreeMap<String, Published>>,
}

impl Manifest {
    pub fn get(&self, s: &Storage) -> Option<&Published> {
        self.storages.get(&s.0).and_then(|b| b.get(&s.1))
    }

    pub fn set(&mut self, s: &Storage, intent: WriteEncryption, by: &str, at: i64) {
        self.version = 1;
        self.storages.entry(s.0.clone()).or_default().insert(
            s.1.clone(),
            Published {
                intent,
                set_by: by.to_string(),
                set_at: at,
            },
        );
    }

    pub fn remove(&mut self, s: &Storage) {
        if let Some(b) = self.storages.get_mut(&s.0) {
            b.remove(&s.1);
            if b.is_empty() {
                self.storages.remove(&s.0);
            }
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = Storage> + '_ {
        self.storages
            .iter()
            .flat_map(|(b, m)| m.keys().map(move |r| (b.clone(), r.clone())))
    }
}

/// A storage where this node and the manifest disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mismatch {
    pub backend: String,
    pub bucket: String,
    /// This node's write encryption.
    pub local: WriteEncryption,
    /// The published one.
    pub published: WriteEncryption,
    pub set_by: String,
}

impl Mismatch {
    /// The refusal of a write (the client sees it in the 503 SlowDown).
    pub fn refusal(&self) -> String {
        let which = if self.bucket == WITNESS_BACKEND_DEFAULT {
            format!(
                "the buckets on backend '{}' without an entry of their own",
                self.backend
            )
        } else {
            format!("bucket '{}' on backend '{}'", self.bucket, self.backend)
        };
        let keys_differ =
            self.local.mode == self.published.mode && self.local.key_id == self.published.key_id;
        format!(
            "{which}: this instance writes with {}, but the encryption that the instances share \
             (coordination bucket) is {}{}, set by '{}'. This instance refuses writes until they \
             agree: apply the same config here, or, if this instance has the encryption that \
             you want, run `deltaglider_proxy witness publish {}` (POST \
             /_/api/admin/encryption-witness/publish). Reads keep working",
            self.local.describe(),
            self.published.describe(),
            if keys_differ {
                " (the same key id names another key)"
            } else {
                ""
            },
            self.set_by,
            self.subject()
        )
    }

    /// The `witness publish` subject that publishes this storage.
    pub fn subject(&self) -> String {
        if self.bucket == WITNESS_BACKEND_DEFAULT {
            format!("backend:{}", self.backend)
        } else {
            self.bucket.clone()
        }
    }
}

// ── Pure decisions ──────────────────────────────────────────────────────

/// This node's write encryption of `s`, or `None` when the node does not
/// define the backend (it cannot write there).
pub fn intent_of(cfg: &Config, s: &Storage) -> Option<WriteEncryption> {
    cfg.encryption_for_storage(&s.0, &s.1)
        .map(|e| e.write_encryption())
}

/// The storages that this node gives their own `encryption` block.
pub fn override_storages(cfg: &Config) -> BTreeSet<Storage> {
    cfg.bucket_encryption_overrides()
        .into_iter()
        .filter(|o| {
            !o.bucket
                .starts_with(crate::maintenance::migrate::TRANSIENT_PREFIX)
        })
        .map(|o| (o.backend, o.real_bucket))
        .collect()
}

/// Every storage that this node's config fixes: each named bucket's
/// storage, and each backend's `*`.
fn config_storages(cfg: &Config) -> BTreeSet<Storage> {
    let mut out: BTreeSet<Storage> = cfg
        .buckets
        .keys()
        .filter(|b| !b.starts_with(crate::maintenance::migrate::TRANSIENT_PREFIX))
        .filter_map(|b| cfg.bucket_storage(b))
        .collect();
    let backends: Vec<String> = if cfg.backends.is_empty() {
        vec!["default".to_string()]
    } else {
        cfg.backends.iter().map(|b| b.name.clone()).collect()
    };
    out.extend(
        backends
            .into_iter()
            .map(|b| (b, WITNESS_BACKEND_DEFAULT.to_string())),
    );
    out
}

/// The storages this node seeds (create-only): every storage of its config
/// that it ENCRYPTS with a usable key. Plaintext and a refusal (a proxy mode
/// with no key) are never seeded.
pub fn seeds(cfg: &Config) -> Vec<(Storage, WriteEncryption)> {
    config_storages(cfg)
        .into_iter()
        .filter_map(|s| intent_of(cfg, &s).map(|i| (s, i)))
        .filter(|(_, i)| i.encrypts() && !i.refuses_writes())
        .collect()
}

/// The storages this node witnesses: the manifest's that it can write, its
/// seeds and its own overrides.
pub fn witnessed(cfg: &Config, manifest: &Manifest) -> BTreeSet<Storage> {
    let mut out: BTreeSet<Storage> = manifest
        .keys()
        .filter(|s| intent_of(cfg, s).is_some())
        .collect();
    out.extend(seeds(cfg).into_iter().map(|(s, _)| s));
    out.extend(override_storages(cfg));
    out
}

/// Every manifest entry that this node can write and disagrees with, and
/// every storage of this node with no entry of its own that disagrees with
/// its backend's `*` entry (the gate judges such a bucket by `*`, so the
/// verdict must too: a bucket block `mode: none` on a backend that the
/// others encrypt is a mismatch, never an agreement).
pub fn mismatches(cfg: &Config, manifest: &Manifest) -> Vec<Mismatch> {
    let judged = |s: Storage, p: &Published| {
        let local = intent_of(cfg, &s)?;
        (local != p.intent).then(|| Mismatch {
            backend: s.0.clone(),
            bucket: s.1.clone(),
            local,
            published: p.intent.clone(),
            set_by: p.set_by.clone(),
        })
    };
    let mut out: Vec<Mismatch> = manifest
        .keys()
        .filter_map(|s| {
            let p = manifest.get(&s)?.clone();
            judged(s, &p)
        })
        .collect();
    let mut own: BTreeSet<Storage> = override_storages(cfg);
    own.extend(config_storages(cfg));
    for s in own {
        if s.1 == WITNESS_BACKEND_DEFAULT || manifest.get(&s).is_some() {
            continue;
        }
        let star = (s.0.clone(), WITNESS_BACKEND_DEFAULT.to_string());
        if let Some(p) = manifest.get(&star).cloned() {
            out.extend(judged(s, &p));
        }
    }
    out
}

/// The gate state of a judgement.
pub fn gate_state(cfg: &Config, manifest: &Manifest, found: &[Mismatch]) -> WitnessGateState {
    WitnessGateState {
        refused: found
            .iter()
            .map(|m| ((m.backend.clone(), m.bucket.clone()), m.refusal()))
            .collect(),
        witnessed: witnessed(cfg, manifest).into_iter().collect(),
    }
}

/// The storages whose write encryption a change from `old` to `new`
/// changed on this node, among `candidates`. What an apply publishes.
pub fn changed_storages(
    old: &Config,
    new: &Config,
    candidates: &BTreeSet<Storage>,
) -> Vec<(Storage, WriteEncryption)> {
    candidates
        .iter()
        .filter_map(|s| {
            let after = intent_of(new, s)?;
            (intent_of(old, s).as_ref() != Some(&after)).then(|| (s.clone(), after))
        })
        .collect()
}

/// The storages that a list of subjects names (downgrade confirmations,
/// `witness publish`): a bucket name (its storage), `backend:<name>` (every
/// candidate storage on it), and `route:default` (the default backend's
/// `*`).
pub fn confirmed_storages(
    cfg: &Config,
    candidates: &BTreeSet<Storage>,
    confirmed: &[String],
) -> BTreeSet<Storage> {
    let named: BTreeSet<Storage> = confirmed
        .iter()
        .filter_map(|c| cfg.bucket_storage(c))
        .collect();
    candidates
        .iter()
        .filter(|s| {
            named.contains(*s)
                || confirmed.iter().any(|c| *c == format!("backend:{}", s.0))
                || (s.1 == WITNESS_BACKEND_DEFAULT
                    && s.0 == cfg.default_backend_name()
                    && confirmed
                        .iter()
                        .any(|c| c == crate::config::DEFAULT_ROUTE_SUBJECT))
        })
        .cloned()
        .collect()
}

/// PURE: what an apply publishes (see `EncryptionWitness::publish_changes`).
pub fn changes_to_publish(
    old: &Config,
    new: &Config,
    cached: &Manifest,
    confirmed: &[String],
) -> Vec<(Storage, WriteEncryption)> {
    let mut candidates: BTreeSet<Storage> = cached.keys().collect();
    candidates.extend(config_storages(old));
    candidates.extend(config_storages(new));
    candidates.extend(override_storages(old));
    candidates.extend(override_storages(new));
    let mut publish = changed_storages(old, new, &candidates);
    for s in confirmed_storages(new, &candidates, confirmed) {
        // A confirmation publishes a downgrade, never an arbitrary
        // intent over the one that the others write with.
        if let Some(i) = intent_of(new, &s).filter(|i| !i.encrypts()) {
            if !publish.iter().any(|(p, _)| *p == s) {
                publish.push((s, i));
            }
        }
    }
    // A plaintext storage that nothing witnesses (no entry of its own
    // and no `*` of its backend) needs no entry.
    publish.retain(|(s, i)| {
        i.encrypts()
            || cached.get(s).is_some()
            || cached
                .get(&(s.0.clone(), WITNESS_BACKEND_DEFAULT.to_string()))
                .is_some()
    });
    publish
}

/// The storages of this node that a publish may name: the manifest's that
/// it can write, and every storage of its config.
fn publish_candidates(cfg: &Config, manifest: &Manifest) -> BTreeSet<Storage> {
    let mut out: BTreeSet<Storage> = manifest
        .keys()
        .filter(|s| intent_of(cfg, s).is_some())
        .collect();
    out.extend(config_storages(cfg));
    out.extend(override_storages(cfg));
    out
}

/// Why an operator publish is refused before any write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishRefusal {
    /// No storage of this node matches a subject.
    Unknown(Vec<String>),
    /// A storage would be published as plaintext without its confirmation.
    Downgrade(Vec<String>),
}

impl std::fmt::Display for PublishRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(s) => write!(
                f,
                "no storage of this instance matches {s:?}: name a configured bucket, \
                 `backend:<name>` or `route:default`"
            ),
            Self::Downgrade(s) => write!(
                f,
                "this instance stores {s:?} in plaintext: publishing that is a downgrade. \
                 Send the request again with the header `X-DGP-Confirm-Encryption-Downgrade: \
                 <subject>` (`witness publish --confirm-encryption-downgrade <subject>`)"
            ),
        }
    }
}

/// PURE plan of an operator publish: every storage the subjects name, with
/// this node's intent; refused when a subject names nothing or a plaintext
/// intent lacks its confirmation.
pub fn plan_publish(
    cfg: &Config,
    manifest: &Manifest,
    subjects: &[String],
    confirmed: &[String],
) -> Result<Vec<(Storage, WriteEncryption)>, PublishRefusal> {
    let candidates = publish_candidates(cfg, manifest);
    let unknown: Vec<String> = subjects
        .iter()
        .filter(|sub| confirmed_storages(cfg, &candidates, std::slice::from_ref(*sub)).is_empty())
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(PublishRefusal::Unknown(unknown));
    }
    let plan: Vec<(Storage, WriteEncryption)> = confirmed_storages(cfg, &candidates, subjects)
        .into_iter()
        .filter_map(|s| intent_of(cfg, &s).map(|i| (s, i)))
        .collect();
    let unconfirmed: Vec<String> = plan
        .iter()
        .filter(|(s, i)| {
            !i.encrypts()
                && confirmed_storages(cfg, &BTreeSet::from([s.clone()]), confirmed).is_empty()
        })
        .map(|(s, _)| format!("{}/{}", s.0, s.1))
        .collect();
    if !unconfirmed.is_empty() {
        return Err(PublishRefusal::Downgrade(unconfirmed));
    }
    Ok(plan)
}

/// Wall time: only for the stamps written into the manifest. Ages are
/// judged on [`WitnessGate::now_ms`].
fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── The witness ─────────────────────────────────────────────────────────

/// The witness verdict. `/_/ready` shows `status` only; the admin API
/// (`GET /_/api/admin/encryption-witness`) shows all of it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WitnessStatus {
    /// `agree`, `mismatch`, `unknown` (the coordination bucket did not
    /// answer; the gate refuses the witnessed buckets once the last good
    /// verdict is older than two polls), or `unavailable` (the witness has
    /// no client: the witnessed buckets refuse writes).
    pub status: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mismatches: Vec<Mismatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The witness of one node.
pub struct EncryptionWitness {
    /// `None`: the coordination client could not be built (`unavailable`;
    /// the gate stays closed for the witnessed storages, judged at each
    /// apply).
    client: Option<aws_sdk_s3::Client>,
    bucket: String,
    node_id: String,
    gate: Arc<WitnessGate>,
    /// The last manifest read, to judge a config change at once.
    manifest: parking_lot::Mutex<Manifest>,
    /// S3 server clock minus local clock, in ms, from the last read.
    server_offset_ms: std::sync::atomic::AtomicI64,
    status: arc_swap::ArcSwap<WitnessStatus>,
    /// One sync at a time (a poll, an apply, a publish).
    running: tokio::sync::Mutex<()>,
    /// The last manifest read, saved next to the config DB: a node that
    /// starts while the coordination bucket does not answer judges against
    /// it (it holds no key material).
    cache_path: Option<std::path::PathBuf>,
    /// The publishes of an apply that failed: retried at each sync while
    /// they are still this node's intent.
    pending: parking_lot::Mutex<Vec<(Storage, WriteEncryption)>>,
    /// What the last apply's publish replaced: `(storage, previous entry,
    /// published intent)`. A rollback of that apply puts it back.
    last_publish: parking_lot::Mutex<Vec<(Storage, Option<Published>, WriteEncryption)>>,
}

/// The file of the saved manifest, in the config-DB directory.
pub const MANIFEST_CACHE_FILE: &str = "encryption-witness.json";

/// Write `manifest` to `path`: a 0600 temp file renamed over it.
fn save_manifest(path: &std::path::Path, manifest: &Manifest) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(&serde_json::to_vec(manifest).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// The saved manifest at `path`, if one parses.
fn load_manifest(path: &std::path::Path) -> Option<Manifest> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// How many times a CAS write re-reads and merges after a lost race.
const CAS_ATTEMPTS: usize = 8;

/// The status error of a witness with no client.
const UNAVAILABLE: &str = "the coordination client could not be built";

impl EncryptionWitness {
    /// A witness whose gate starts CLOSED for the storages that `cfg`
    /// witnesses, until the first sync succeeds. `client: None` = the
    /// coordination client could not be built: the status is `unavailable`
    /// and the gate stays closed (each apply judges the new config).
    ///
    /// `cache_dir`: where the last good manifest is saved. A saved manifest
    /// is judged at once: until the first sync succeeds, every storage in
    /// it is refused as well (a node with a lagging plaintext config that
    /// starts during a coordination outage seeds nothing of its own).
    pub fn new(
        client: Option<aws_sdk_s3::Client>,
        bucket: String,
        node_id: String,
        poll: std::time::Duration,
        gate: Arc<WitnessGate>,
        cfg: &Config,
        cache_dir: Option<&std::path::Path>,
    ) -> Self {
        Self::close_gate(&gate, cfg, poll);
        let cache_path = cache_dir.map(|d| d.join(MANIFEST_CACHE_FILE));
        let cached = cache_path
            .as_deref()
            .and_then(load_manifest)
            .unwrap_or_default();
        let found = mismatches(cfg, &cached);
        gate.set(gate_state(cfg, &cached, &found));
        let unavailable = client.is_none();
        Self {
            client,
            bucket,
            node_id,
            gate,
            cache_path,
            pending: Default::default(),
            last_publish: Default::default(),
            manifest: parking_lot::Mutex::new(cached),
            server_offset_ms: Default::default(),
            status: arc_swap::ArcSwap::from_pointee(WitnessStatus {
                status: if unavailable {
                    "unavailable"
                } else {
                    "unknown"
                },
                error: unavailable.then(|| UNAVAILABLE.to_string()),
                ..Default::default()
            }),
            running: tokio::sync::Mutex::new(()),
        }
    }

    /// Turn `gate` on with nothing synced: every storage that `cfg`
    /// witnesses is refused until a sync succeeds. A verdict holds two
    /// polls from the start of the sync that made it.
    pub fn close_gate(gate: &WitnessGate, cfg: &Config, poll: std::time::Duration) {
        gate.activate(poll.max(std::time::Duration::from_secs(1)) * 2);
        gate.set(gate_state(cfg, &Manifest::default(), &[]));
    }

    /// The last verdict.
    pub fn status(&self) -> WitnessStatus {
        (**self.status.load()).clone()
    }

    fn client(&self) -> Result<&aws_sdk_s3::Client, String> {
        self.client.as_ref().ok_or_else(|| UNAVAILABLE.to_string())
    }

    /// Now, in unix ms on the S3 server clock (estimated).
    fn server_now(&self) -> i64 {
        unix_ms()
            + self
                .server_offset_ms
                .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Judge `cfg` against the last manifest read, with no I/O: the gate
    /// follows a config change at once.
    pub fn judge(&self, cfg: &Config) {
        let manifest = self.manifest.lock().clone();
        let found = mismatches(cfg, &manifest);
        self.gate.set(gate_state(cfg, &manifest, &found));
        let error = self.status.load().error.clone();
        self.publish_status(found, error);
    }

    fn publish_status(&self, found: Vec<Mismatch>, error: Option<String>) {
        let status = match (&error, found.is_empty()) {
            (Some(_), _) if self.client.is_none() => "unavailable",
            (Some(_), _) => "unknown",
            (None, true) => "agree",
            (None, false) => "mismatch",
        };
        self.status.store(Arc::new(WitnessStatus {
            status,
            mismatches: found,
            error,
        }));
    }

    async fn read_manifest(&self) -> Result<(Manifest, Option<String>), String> {
        use super::cas::sdk_error_signal;
        let client = self.client()?;
        let date = super::server_clock::ServerDate::default();
        let sent_at = unix_ms();
        let got = client
            .get_object()
            .bucket(&self.bucket)
            .key(MANIFEST_KEY)
            .customize()
            .interceptor(date.clone())
            .send()
            .await;
        if let Some(server_secs) = date.get() {
            self.server_offset_ms.store(
                server_secs * 1000 - sent_at,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        match got {
            Ok(out) => {
                let etag = out.e_tag().map(str::to_string);
                let body = out
                    .body
                    .collect()
                    .await
                    .map_err(|_| format!("reading {MANIFEST_KEY} broke off"))?
                    .into_bytes();
                let manifest = serde_json::from_slice(&body)
                    .map_err(|e| format!("{MANIFEST_KEY} is not a valid manifest: {e}"))?;
                Ok((manifest, etag))
            }
            // Only a missing OBJECT is an empty manifest. A missing
            // coordination BUCKET (NoSuchBucket) is a failure: read as "no
            // manifest", a node with a lagging plaintext config would
            // witness nothing and write plaintext.
            Err(e) if e.as_service_error().is_some_and(|se| se.is_no_such_key()) => {
                Ok((Manifest::default(), None))
            }
            Err(e) => Err(format!("GET {MANIFEST_KEY}: {}", sdk_error_signal(&e))),
        }
    }

    /// Read, change with `edit`, write back with CAS; re-read and edit
    /// again on a lost race. `edit` returns false when nothing changes.
    async fn update_manifest(
        &self,
        mut edit: impl FnMut(&mut Manifest, i64) -> bool,
    ) -> Result<Manifest, String> {
        let client = self.client()?;
        for _ in 0..CAS_ATTEMPTS {
            let (mut manifest, etag) = self.read_manifest().await?;
            if !edit(&mut manifest, self.server_now()) {
                return Ok(manifest);
            }
            let body = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
            match super::cas::conditional_put_etag(
                client,
                &self.bucket,
                MANIFEST_KEY,
                body.into(),
                etag.as_deref(),
            )
            .await
            {
                Ok(Some(_)) => return Ok(manifest),
                Ok(None) => continue, // a peer wrote first: merge again
                Err(detail) => {
                    // The detail is an SDK Debug dump: logs only.
                    tracing::debug!("encryption witness: PUT {MANIFEST_KEY}: {detail}");
                    return Err(format!("the conditional PUT of {MANIFEST_KEY} failed"));
                }
            }
        }
        Err(format!(
            "{MANIFEST_KEY}: {CAS_ATTEMPTS} conditional writes lost to peers"
        ))
    }

    /// One full sync against `cfg`: seed, then judge and set the gate.
    pub async fn sync(&self, cfg: &Config) -> Result<(), String> {
        let _one = self.running.lock().await;
        self.sync_locked(cfg).await
    }

    async fn sync_locked(&self, cfg: &Config) -> Result<(), String> {
        // The verdict holds from the START of the sync: a slow sync cannot
        // stretch the window in which a stale verdict lets writes through.
        let started = WitnessGate::now_ms();
        let result = self.sync_inner(cfg).await;
        match &result {
            Ok(()) => self.gate.synced_at(started),
            Err(e) => {
                // The last verdict, judged against the current config; the
                // gate refuses the witnessed storages once it is stale.
                let manifest = self.manifest.lock().clone();
                let found = mismatches(cfg, &manifest);
                self.gate.set(gate_state(cfg, &manifest, &found));
                self.publish_status(found, Some(e.clone()));
            }
        }
        result
    }

    async fn sync_inner(&self, cfg: &Config) -> Result<(), String> {
        let label = self.node_id.clone();
        // The publishes of a failed apply, while they are still this
        // node's intent: retried before anything else.
        let pending: Vec<(Storage, WriteEncryption)> = std::mem::take(&mut *self.pending.lock())
            .into_iter()
            .filter(|(s, i)| intent_of(cfg, s).as_ref() == Some(i))
            .collect();
        if !pending.is_empty() {
            if let Err(e) = self.write_entries(&pending).await {
                self.pending.lock().extend(pending);
                return Err(e);
            }
            tracing::info!(
                "encryption witness: published {} change(s) that an apply could not",
                pending.len()
            );
        }
        let read = self.read_manifest().await?.0;
        let missing: Vec<(Storage, WriteEncryption)> = seeds(cfg)
            .into_iter()
            .filter(|(s, _)| read.get(s).is_none())
            .collect();
        let manifest = if missing.is_empty() {
            read
        } else {
            self.update_manifest(|m, now| {
                let mut changed = false;
                for (s, i) in &missing {
                    if m.get(s).is_none() {
                        m.set(s, i.clone(), &label, now);
                        changed = true;
                    }
                }
                changed
            })
            .await?
        };
        *self.manifest.lock() = manifest.clone();
        if let Some(path) = &self.cache_path {
            if let Err(e) = save_manifest(path, &manifest) {
                tracing::warn!(
                    "encryption witness: could not save the manifest to {}: {e}",
                    path.display()
                );
            }
        }
        let found = mismatches(cfg, &manifest);
        // Log a mismatch when it starts, not at every poll.
        let before = self.status.load();
        for m in found.iter().filter(|m| !before.mismatches.contains(m)) {
            tracing::error!("encryption witness: {}", m.refusal());
        }
        self.gate.set(gate_state(cfg, &manifest, &found));
        self.publish_status(found, None);
        Ok(())
    }

    /// After a config apply on this node: publish this node's write
    /// encryption for every storage whose encryption the apply CHANGED,
    /// and the PLAINTEXT intent of every storage that the request confirmed
    /// as a downgrade (`confirmed`, the `X-DGP-Confirm-Encryption-Downgrade`
    /// subjects; also when the config did not change, to finish a downgrade
    /// that the nodes got by restart). Then sync. The gate follows `new` at
    /// once, before any I/O.
    pub async fn publish_changes(
        &self,
        old: &Config,
        new: &Config,
        confirmed: &[String],
    ) -> Result<(), String> {
        self.judge(new);
        let _one = self.running.lock().await;
        let cached = self.manifest.lock().clone();
        let publish = changes_to_publish(old, new, &cached, confirmed);
        if self.client.is_none() {
            return Err(UNAVAILABLE.to_string());
        }
        self.last_publish.lock().clear();
        if !publish.is_empty() {
            match self.write_entries(&publish).await {
                Ok(previous) => {
                    *self.last_publish.lock() = previous
                        .into_iter()
                        .zip(publish.iter().map(|(_, i)| i.clone()))
                        .map(|((s, prev), i)| (s, prev, i))
                        .collect();
                }
                Err(e) => {
                    // Kept and retried at each sync (the poller), so the
                    // node does not refuse its own change forever.
                    let mut pending = self.pending.lock();
                    pending.retain(|(s, _)| !publish.iter().any(|(p, _)| p == s));
                    pending.extend(publish.iter().cloned());
                    drop(pending);
                    self.judge(new);
                    return Err(e);
                }
            }
            for (s, i) in &publish {
                tracing::info!(
                    "encryption witness: published {} for {}/{}",
                    i.describe(),
                    s.0,
                    s.1
                );
            }
        }
        self.sync_locked(new).await
    }

    /// Write `entries`; returns each storage's entry before the write (in
    /// the order of `entries`).
    async fn write_entries(
        &self,
        entries: &[(Storage, WriteEncryption)],
    ) -> Result<Vec<(Storage, Option<Published>)>, String> {
        let label = self.node_id.clone();
        let mut previous = Vec::new();
        let m = self
            .update_manifest(|m, now| {
                previous = entries
                    .iter()
                    .map(|(s, _)| (s.clone(), m.get(s).cloned()))
                    .collect();
                let mut edited = false;
                for (s, i) in entries {
                    if m.get(s).map(|p| &p.intent) != Some(i) {
                        m.set(s, i.clone(), &label, now);
                        edited = true;
                    }
                }
                edited
            })
            .await?;
        *self.manifest.lock() = m;
        Ok(previous)
    }

    /// The rollback of the last apply (a failed create, a failed persist):
    /// put back the entries that its publish replaced, where they still
    /// hold what it published. Never publishes this node's intent (that
    /// would publish a downgrade without its confirmation), then syncs.
    pub async fn restore_last_publish(&self, cfg: &Config) -> Result<(), String> {
        self.judge(cfg);
        let _one = self.running.lock().await;
        let undo = std::mem::take(&mut *self.last_publish.lock());
        self.pending
            .lock()
            .retain(|(s, _)| !undo.iter().any(|(u, _, _)| u == s));
        if self.client.is_none() {
            return Err(UNAVAILABLE.to_string());
        }
        if !undo.is_empty() {
            let m = self
                .update_manifest(|m, _| {
                    let mut edited = false;
                    for (s, prev, published) in &undo {
                        if m.get(s).map(|p| &p.intent) != Some(published) {
                            continue; // changed since: not ours to undo
                        }
                        match prev {
                            Some(p) => {
                                m.set(s, p.intent.clone(), &p.set_by, p.set_at);
                            }
                            None => m.remove(s),
                        }
                        edited = true;
                    }
                    edited
                })
                .await?;
            *self.manifest.lock() = m;
        }
        self.sync_locked(cfg).await
    }

    /// The operator's publish (`witness publish`): this node's current
    /// encryption of the storages that `subjects` name becomes the shared
    /// one. Plaintext needs its downgrade confirmation (`confirmed`). Each
    /// published storage writes the audit entry
    /// `bucket_encryption_witness_publish`. Returns the published storages.
    pub async fn publish_subjects(
        &self,
        cfg: &Config,
        subjects: &[String],
        confirmed: &[String],
        headers: &axum::http::HeaderMap,
    ) -> Result<Vec<(Storage, WriteEncryption)>, PublishError> {
        let _one = self.running.lock().await;
        let manifest = self.read_manifest().await.map_err(PublishError::Failed)?.0;
        let plan =
            plan_publish(cfg, &manifest, subjects, confirmed).map_err(PublishError::Refused)?;
        let before = manifest;
        self.write_entries(&plan)
            .await
            .map_err(PublishError::Failed)?;
        self.pending
            .lock()
            .retain(|(s, _)| !plan.iter().any(|(p, _)| p == s));
        for (s, i) in &plan {
            let was = before
                .get(s)
                .map(|p| p.intent.describe())
                .unwrap_or_else(|| "no entry".to_string());
            crate::audit::audit_log(
                "bucket_encryption_witness_publish",
                "admin",
                &format!("{}/{}: {was} -> {}", s.0, s.1, i.describe()),
                headers,
                &s.1,
                "",
            );
        }
        self.sync_locked(cfg).await.map_err(PublishError::Failed)?;
        Ok(plan)
    }

    /// Poll every `interval`, judging the live config each time.
    pub fn spawn_poller(
        self: Arc<Self>,
        config: crate::config::SharedConfig,
        interval: std::time::Duration,
    ) {
        if self.client.is_none() {
            return; // nothing to poll: applies keep judging
        }
        crate::background::spawn_supervised("encryption-witness-poller", move || {
            let (this, config) = (self.clone(), config.clone());
            async move {
                let mut tick = tokio::time::interval(interval);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                tick.tick().await; // the boot ran the first sync
                loop {
                    tick.tick().await;
                    if crate::shutdown::is_shutting_down() {
                        return;
                    }
                    let cfg = config.read().await.clone();
                    if let Err(e) = this.sync(&cfg).await {
                        tracing::warn!(
                            "encryption witness: sync failed ({e}); the last verdict holds for \
                             two polls, then the witnessed buckets refuse writes"
                        );
                    }
                }
            }
        });
    }
}

/// Why an operator publish failed.
#[derive(Debug)]
pub enum PublishError {
    /// Refused before any write (400).
    Refused(PublishRefusal),
    /// The coordination bucket did not answer (503).
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const KA: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn cfg_with(local_enc: &str, releases: &str) -> Config {
        Config::from_yaml_str(&format!(
            "storage:\n  backends:\n    - name: local-disk\n      type: filesystem\n      path: /tmp/dgp-witness-a\n{local_enc}  default_backend: local-disk\n  buckets:\n    releases: {releases}\n    downloads: {{ backend: local-disk }}\n"
        ))
        .unwrap()
    }

    fn cfg(releases: &str) -> Config {
        cfg_with("", releases)
    }

    fn with_key() -> Config {
        cfg(&format!(
            "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KA}\", key_id: rel-a }} }}"
        ))
    }

    fn rel() -> Storage {
        ("local-disk".into(), "releases".into())
    }

    fn star() -> Storage {
        ("local-disk".into(), WITNESS_BACKEND_DEFAULT.into())
    }

    fn aes(id: &str) -> WriteEncryption {
        WriteEncryption {
            mode: "aes256-gcm-proxy".into(),
            key_id: Some(id.into()),
            kcv: Some("00".into()),
        }
    }

    fn none() -> WriteEncryption {
        WriteEncryption {
            mode: "none".into(),
            key_id: None,
            kcv: None,
        }
    }

    #[test]
    fn a_node_seeds_what_it_encrypts_and_compares_every_entry() {
        let a = with_key();
        let seeded: Vec<Storage> = seeds(&a).into_iter().map(|(s, _)| s).collect();
        assert_eq!(seeded, vec![rel()], "plaintext storages are never seeded");
        let b = cfg("{ backend: local-disk }");
        assert!(seeds(&b).is_empty());
        let mut m = Manifest::default();
        let ia = intent_of(&a, &rel()).unwrap();
        m.set(&rel(), ia.clone(), "node-a", 1);
        assert!(mismatches(&a, &m).is_empty());
        let found = mismatches(&b, &m);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].local.clone(), found[0].published.clone()),
            (none(), ia)
        );
        let msg = found[0].refusal();
        assert!(
            msg.contains("bucket 'releases'") && msg.contains("key id 'rel-a'"),
            "{msg}"
        );
        assert!(!msg.contains(KA));
        // A storage on a backend this node does not define is not judged.
        m.set(&("aws-dr".into(), "x".into()), aes("x"), "node-c", 1);
        assert_eq!(mismatches(&b, &m).len(), 1);
    }

    /// Review of P3 (probe-verified): a node that boots with `mode: none`
    /// on a bucket that the others encrypt by INHERITANCE was the first
    /// writer of its entry, so the encrypting nodes went mismatch. Now the
    /// inheriting nodes seed it, and plaintext is never seeded.
    #[test]
    fn a_bucket_that_inherits_an_encrypting_backend_is_seeded() {
        let backend_aes = format!(
            "      encryption: {{ mode: aes256-gcm-proxy, key: \"{KA}\", key_id: local-a }}\n"
        );
        let inheriting = cfg_with(&backend_aes, "{ backend: local-disk }");
        let seeded: BTreeSet<Storage> = seeds(&inheriting).into_iter().map(|(s, _)| s).collect();
        assert!(
            seeded.contains(&rel()) && seeded.contains(&star()),
            "{seeded:?}"
        );
        let downgrading = cfg_with(
            &backend_aes,
            "{ backend: local-disk, encryption: { mode: none } }",
        );
        assert!(!seeds(&downgrading).iter().any(|(s, _)| *s == rel()));
        let mut m = Manifest::default();
        for (s, i) in seeds(&inheriting) {
            m.set(&s, i, "node-a", 1);
        }
        let found = mismatches(&downgrading, &m);
        assert!(found.iter().any(|f| f.bucket == "releases"), "{found:?}");
        // A node with no key for a proxy mode never seeds its refusal.
        let keyless =
            cfg("{ backend: local-disk, encryption: { mode: aes256-gcm-proxy, key_id: rel-a } }");
        assert!(seeds(&keyless).is_empty());
    }

    #[test]
    fn an_apply_publishes_what_it_changed_and_what_it_confirmed() {
        let (a, b) = (cfg("{ backend: local-disk }"), with_key());
        let cands = BTreeSet::from([rel(), ("local-disk".into(), "downloads".into())]);
        assert_eq!(changed_storages(&a, &b, &cands).len(), 1);
        assert_eq!(changed_storages(&b, &a, &cands)[0].1, none());
        assert!(changed_storages(&b, &b, &cands).is_empty());
        let all = BTreeSet::from([rel(), star()]);
        assert_eq!(
            confirmed_storages(&a, &all, &["releases".into()]),
            BTreeSet::from([rel()])
        );
        assert_eq!(
            confirmed_storages(&a, &all, &["backend:local-disk".into()]),
            all
        );
        assert_eq!(
            confirmed_storages(&a, &all, &["route:default".into()]),
            BTreeSet::from([star()])
        );
        assert!(confirmed_storages(&a, &all, &[]).is_empty());
    }

    fn backend_aes() -> String {
        format!("      encryption: {{ mode: aes256-gcm-proxy, key: \"{KA}\", key_id: local-a }}\n")
    }

    /// Second review, finding 1 (probe-verified): a bucket encrypted only
    /// through the backend's `*` entry was downgraded by a node that booted
    /// with `mode: none` for it: `mismatches` compared only storages with an
    /// entry of their own, so both nodes agreed and the node wrote
    /// plaintext. Such a storage is judged against `*`, as the gate does.
    #[test]
    fn a_bucket_block_with_no_entry_is_judged_against_the_backends_star() {
        let mut m = Manifest::default();
        m.set(&star(), aes("local-a"), "node-a", 1);
        let downgrading = cfg_with(
            &backend_aes(),
            "{ backend: local-disk, encryption: { mode: none } }",
        );
        let found = mismatches(&downgrading, &m);
        assert!(
            found
                .iter()
                .any(|f| f.bucket == "releases" && f.local == none()),
            "{found:?}"
        );
        let gate = WitnessGate::default();
        gate.activate(std::time::Duration::from_secs(60));
        gate.synced();
        gate.set(gate_state(&downgrading, &m, &found));
        assert!(gate.refusal("local-disk", "releases").is_some());
    }

    /// Second review, finding 4 (probe-verified): a lagging node with the
    /// downgrade header and an empty patch republished its OLD key over the
    /// rotation. A confirmation publishes plaintext intents only.
    #[test]
    fn a_confirmation_publishes_plaintext_only() {
        let lagging = with_key(); // rel-a
        let mut m = Manifest::default();
        m.set(&rel(), aes("rel-c"), "node-a", 1);
        let confirmed = vec!["releases".to_string()];
        assert!(changes_to_publish(&lagging, &lagging, &m, &confirmed).is_empty());
        let plain = cfg("{ backend: local-disk, encryption: { mode: none } }");
        assert_eq!(
            changes_to_publish(&plain, &plain, &m, &confirmed),
            vec![(rel(), none())]
        );
        assert!(changes_to_publish(&plain, &plain, &m, &[]).is_empty());
    }

    /// The operator's `witness publish`: a subject must name a storage of
    /// this node, and plaintext needs its downgrade confirmation.
    #[test]
    fn an_operator_publish_names_storages_and_confirms_plaintext() {
        let m = Manifest::default();
        let a = with_key();
        let plan = plan_publish(&a, &m, &["releases".into()], &[]).unwrap();
        assert_eq!(plan, vec![(rel(), intent_of(&a, &rel()).unwrap())]);
        assert!(matches!(
            plan_publish(&a, &m, &["nope".into()], &[]),
            Err(PublishRefusal::Unknown(_))
        ));
        let plain = cfg("{ backend: local-disk, encryption: { mode: none } }");
        assert!(matches!(
            plan_publish(&plain, &m, &["releases".into()], &[]),
            Err(PublishRefusal::Downgrade(_))
        ));
        assert!(plan_publish(&plain, &m, &["releases".into()], &["releases".into()]).is_ok());
        let all = plan_publish(
            &a,
            &m,
            &["backend:local-disk".into()],
            &["backend:local-disk".into()],
        )
        .unwrap();
        assert!(all.iter().any(|(s, _)| *s == star()), "{all:?}");
    }

    /// Finding 6: a verdict ages from the START of its sync.
    #[test]
    fn a_verdict_ages_from_the_start_of_its_sync() {
        let gate = WitnessGate::default();
        let cfg = with_key();
        EncryptionWitness::close_gate(&gate, &cfg, std::time::Duration::from_secs(1));
        gate.synced_at(WitnessGate::now_ms() - 2_500); // a sync that took 2.5 s
        assert!(
            gate.refusal("local-disk", "releases").is_some(),
            "older than 2 polls"
        );
        gate.synced_at(WitnessGate::now_ms() - 500);
        assert!(gate.refusal("local-disk", "releases").is_none());
    }

    #[test]
    fn the_manifest_never_holds_key_material() {
        let mut m = Manifest::default();
        for (s, i) in seeds(&with_key()) {
            m.set(&s, i, "node-a", 1);
        }
        let text = serde_json::to_string(&m).unwrap();
        assert!(!text.contains(KA), "{text}");
        assert!(text.contains("\"key_id\":\"rel-a\""), "{text}");
        assert!(text.contains("\"kcv\":"), "{text}");
        let back: Manifest = serde_json::from_str(&text).unwrap();
        assert_eq!(back, m);
    }

    /// The gate: a mismatch refuses, the backend's `*` covers a bucket with
    /// no entry, a stale verdict refuses witnessed storages only, and the
    /// gate starts closed for them.
    #[test]
    fn the_gate_refuses_mismatches_and_stale_witnessed_storages() {
        let gate = WitnessGate::default();
        assert_eq!(gate.refusal("local-disk", "releases"), None, "inactive");
        let cfg = with_key();
        EncryptionWitness::close_gate(&gate, &cfg, std::time::Duration::from_secs(30));
        assert!(
            gate.refusal("local-disk", "releases").is_some(),
            "closed until a sync"
        );
        assert_eq!(
            gate.refusal("local-disk", "downloads"),
            None,
            "not witnessed"
        );
        gate.synced();
        assert_eq!(gate.refusal("local-disk", "releases"), None);
        let mut m = Manifest::default();
        m.set(&star(), aes("local-a"), "node-a", 1);
        m.set(&rel(), intent_of(&cfg, &rel()).unwrap(), "node-a", 1);
        let found = mismatches(&cfg, &m);
        gate.set(gate_state(&cfg, &m, &found));
        assert!(
            gate.refusal("local-disk", "downloads").is_some(),
            "the backend's *"
        );
        assert!(gate.refusal("local-disk", "anything").is_some());
        assert_eq!(
            gate.refusal("local-disk", "releases"),
            None,
            "its own entry agrees"
        );
    }
}

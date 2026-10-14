// SPDX-License-Identifier: BUSL-1.1

//! The multi-instance encryption witness (issue #116 P3).
//!
//! The YAML config is per node, so two nodes can disagree on how one bucket
//! is encrypted (a node that lacks a bucket's `encryption` block, or its
//! key). The witness makes such a node refuse WRITES to that bucket: two
//! nodes never write one bucket in two forms.
//!
//! ## Objects in the coordination bucket
//!
//! * `_dgp/encryption/intent.json` — THE manifest: for each stored bucket
//!   (raw backend + real bucket name; the bucket `*` stands for every
//!   bucket of the backend with no entry of its own) the published write
//!   encryption `{mode, key_id, kcv}`, the node that set it and when
//!   (`set_at`, on the S3 server clock). Never key material: the key id is
//!   the public id that every object already carries, and the key check
//!   value is a truncated HMAC of the key. Written with the CAS rules of
//!   the rest of coordination: create with `If-None-Match: *`, replace with
//!   `If-Match: <etag>`, re-read and merge on a lost race.
//! * `_dgp/encryption/nodes/<node id>-<boot id>.json` — each node's
//!   heartbeat: its own write encryption for every witnessed storage.
//!   Liveness is judged on the S3 server clock (the GET's `Date` minus the
//!   object's `Last-Modified`, against the body's `ttl_secs`).
//!
//! ## Who sets the intent
//!
//! * **Seeding** (boot and every poll): each node adds, create-only, every
//!   storage that it ENCRYPTS (its bucket blocks, the buckets it names that
//!   inherit an encrypting backend, and each encrypting backend's `*`).
//!   Plaintext is never seeded, so a node that boots with `mode: none` for
//!   a bucket that others encrypt is a mismatch, never a first writer.
//! * **Compare** (boot and every poll): a node whose write encryption for a
//!   witnessed storage differs from the manifest refuses writes to it
//!   (`503 SlowDown` naming the mismatch; reads keep working).
//! * **A config apply** on a node publishes that node's write encryption
//!   for every storage whose encryption the apply changed on the node (the
//!   transition gated a downgrade with its confirmation), and for every
//!   storage that the request confirmed as a downgrade (the supported path
//!   to finish a downgrade that the nodes got by restart).
//! * **Takeover**: a mismatched node publishes its own encryption only when
//!   no live node holds the published one, its config is NEWER than the
//!   published entry (its config file changed after `set_at`, so a lagging
//!   node that restarts with its old file never wins), its own state is not
//!   a refusal (a missing key), and it is not a downgrade. Every takeover
//!   is audited.
//! * **Freshness**: a node whose last successful sync is older than two
//!   polls refuses writes to every witnessed storage (it stops before its
//!   heartbeat lapses after three polls and a peer may take over). At boot
//!   the gate starts closed for the witnessed storages until a sync
//!   succeeds.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::{Config, WriteEncryption};
use crate::storage::{WitnessGate, WitnessGateState, WITNESS_BACKEND_DEFAULT};

/// The manifest object.
pub const MANIFEST_KEY: &str = "_dgp/encryption/intent.json";
/// The prefix of the node heartbeat objects.
pub const NODES_PREFIX: &str = "_dgp/encryption/nodes/";
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

    pub fn keys(&self) -> impl Iterator<Item = Storage> + '_ {
        self.storages
            .iter()
            .flat_map(|(b, m)| m.keys().map(move |r| (b.clone(), r.clone())))
    }
}

/// One node's heartbeat.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub node: String,
    pub boot_id: String,
    pub ttl_secs: i64,
    /// backend → real bucket → this node's write encryption.
    #[serde(default)]
    pub storages: BTreeMap<String, BTreeMap<String, WriteEncryption>>,
}

impl NodeRecord {
    fn get(&self, s: &Storage) -> Option<&WriteEncryption> {
        self.storages.get(&s.0).and_then(|b| b.get(&s.1))
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
             (coordination bucket) is {}{}, set by '{}'. This instance refuses writes until its \
             config agrees: apply the same config on every instance. Reads keep working",
            self.local.describe(),
            self.published.describe(),
            if keys_differ {
                " (the same key id names another key)"
            } else {
                ""
            },
            self.set_by
        )
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

/// Every manifest entry that this node can write and disagrees with.
pub fn mismatches(cfg: &Config, manifest: &Manifest) -> Vec<Mismatch> {
    manifest
        .keys()
        .filter_map(|s| {
            let local = intent_of(cfg, &s)?;
            let p = manifest.get(&s)?;
            (local != p.intent).then(|| Mismatch {
                backend: s.0.clone(),
                bucket: s.1.clone(),
                local,
                published: p.intent.clone(),
                set_by: p.set_by.clone(),
            })
        })
        .collect()
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

/// The storages that the downgrade confirmations of a request name: a
/// bucket name (its storage), `backend:<name>` (every storage on it), and
/// `route:default` (the default backend's `*`).
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

/// What a takeover is judged on.
#[derive(Debug, Clone, Copy)]
pub struct TakeoverFacts<'a> {
    pub local: &'a WriteEncryption,
    pub published: &'a Published,
    /// A live node writes with the published encryption.
    pub live_holder: bool,
    /// When this node's config last changed (its config file, or its last
    /// apply), in unix ms on the S3 server clock. `None`: unknown.
    pub config_changed_at: Option<i64>,
}

/// May a mismatched node publish its own intent over the published one?
/// Only when no live node holds the published intent, this node's config
/// changed AFTER the entry was set (a lagging node, restarted with the
/// file it had, never wins), this node does not refuse writes itself (a
/// missing key), and it is not a downgrade (encrypting → plaintext).
pub fn takeover_allowed(f: TakeoverFacts<'_>) -> bool {
    !f.live_holder
        && !f.local.refuses_writes()
        && !(f.published.intent.encrypts() && !f.local.encrypts())
        && f.config_changed_at.is_some_and(|t| t > f.published.set_at)
}

/// Is a heartbeat live? `age` is its server-clock age; an unknown age
/// counts as live (never take over on a guess).
pub fn heartbeat_live(age: Option<i64>, ttl_secs: i64) -> bool {
    age.is_none_or(|a| a <= ttl_secs)
}

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
    /// `agree`, `mismatch`, or `unknown` (the coordination bucket did not
    /// answer; the gate refuses the witnessed buckets once the last good
    /// verdict is older than two polls).
    pub status: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mismatches: Vec<Mismatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The witness of one node.
pub struct EncryptionWitness {
    client: aws_sdk_s3::Client,
    bucket: String,
    node_id: String,
    boot_id: String,
    previous_boot_id: Option<String>,
    ttl_secs: i64,
    gate: Arc<WitnessGate>,
    /// The last manifest read, to judge a config change at once.
    manifest: parking_lot::Mutex<Manifest>,
    /// S3 server clock minus local clock, in ms, from the last read.
    server_offset_ms: std::sync::atomic::AtomicI64,
    /// When this node's config last changed, local unix ms.
    config_changed_at: std::sync::atomic::AtomicI64,
    status: arc_swap::ArcSwap<WitnessStatus>,
    /// One sync at a time (a poll and an apply).
    running: tokio::sync::Mutex<()>,
}

/// How many times a CAS write re-reads and merges after a lost race.
const CAS_ATTEMPTS: usize = 8;

impl EncryptionWitness {
    /// A witness whose gate starts CLOSED for the storages that `cfg`
    /// witnesses, until the first sync succeeds. `config_changed_at`: the
    /// config file's mtime (local unix ms), `None` when there is no file.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: aws_sdk_s3::Client,
        bucket: String,
        node_id: String,
        boot: crate::coordination::BootIds,
        poll: std::time::Duration,
        gate: Arc<WitnessGate>,
        cfg: &Config,
        config_changed_at: Option<i64>,
    ) -> Self {
        Self::close_gate(&gate, cfg, poll);
        Self {
            client,
            bucket,
            node_id,
            boot_id: boot.current,
            previous_boot_id: boot.previous,
            // Three missed polls before a node counts as gone.
            ttl_secs: (poll.as_secs().max(1) * 3) as i64,
            gate,
            manifest: Default::default(),
            server_offset_ms: Default::default(),
            config_changed_at: std::sync::atomic::AtomicI64::new(
                config_changed_at.unwrap_or(i64::MIN),
            ),
            status: arc_swap::ArcSwap::from_pointee(WitnessStatus {
                status: "unknown",
                ..Default::default()
            }),
            running: tokio::sync::Mutex::new(()),
        }
    }

    /// Turn `gate` on with nothing synced: every storage that `cfg`
    /// witnesses is refused until a sync succeeds. A verdict holds for two
    /// polls, one less than a heartbeat: this node stops before a peer may
    /// take over its storages. Also the fail-closed state of a node whose
    /// witness cannot be built.
    pub fn close_gate(gate: &WitnessGate, cfg: &Config, poll: std::time::Duration) {
        gate.activate(poll.max(std::time::Duration::from_secs(1)) * 2);
        gate.set(gate_state(cfg, &Manifest::default(), &[]));
    }

    /// The last verdict.
    pub fn status(&self) -> WitnessStatus {
        (**self.status.load()).clone()
    }

    fn node_label(&self) -> String {
        format!("{}-{}", self.node_id, self.boot_id)
    }

    fn record_key(&self, boot_id: &str) -> String {
        format!("{NODES_PREFIX}{}-{boot_id}.json", self.node_id)
    }

    /// Now, in unix ms on the S3 server clock (estimated).
    fn server_now(&self) -> i64 {
        unix_ms()
            + self
                .server_offset_ms
                .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// When this node's config last changed, on the server clock.
    fn config_changed_at_server(&self) -> Option<i64> {
        let local = self
            .config_changed_at
            .load(std::sync::atomic::Ordering::Relaxed);
        (local != i64::MIN).then(|| {
            local
                + self
                    .server_offset_ms
                    .load(std::sync::atomic::Ordering::Relaxed)
        })
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
        use super::cas::{is_object_absent, sdk_error_signal};
        let date = super::server_clock::ServerDate::default();
        let sent_at = unix_ms();
        let got = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(MANIFEST_KEY)
            .customize()
            .interceptor(date.clone())
            .send()
            .await;
        if let Some(server_secs) = date.get() {
            // The Date header has 1 s resolution: the offset is good to
            // about a second, which the takeover rule allows for.
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
            Err(e) if is_object_absent(&sdk_error_signal(&e)) => Ok((Manifest::default(), None)),
            Err(e) => Err(format!("GET {MANIFEST_KEY}: {}", sdk_error_signal(&e))),
        }
    }

    /// Read, change with `edit`, write back with CAS; re-read and edit
    /// again on a lost race. `edit` returns false when nothing changes.
    async fn update_manifest(
        &self,
        mut edit: impl FnMut(&mut Manifest, i64) -> bool,
    ) -> Result<Manifest, String> {
        for _ in 0..CAS_ATTEMPTS {
            let (mut manifest, etag) = self.read_manifest().await?;
            if !edit(&mut manifest, self.server_now()) {
                return Ok(manifest);
            }
            let body = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
            match super::cas::conditional_put_etag(
                &self.client,
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

    /// The live heartbeats of the other nodes (dead ones far past their TTL
    /// are removed on the way).
    async fn live_peers(&self) -> Result<Vec<NodeRecord>, String> {
        use super::cas::sdk_error_signal;
        let mut keys = Vec::new();
        let mut token = None;
        loop {
            let page = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(NODES_PREFIX)
                .set_continuation_token(token)
                .send()
                .await
                .map_err(|e| format!("LIST {NODES_PREFIX}: {}", sdk_error_signal(&e)))?;
            keys.extend(
                page.contents()
                    .iter()
                    .filter_map(|o| o.key().map(str::to_string)),
            );
            match page.next_continuation_token() {
                Some(t) if page.is_truncated() == Some(true) => token = Some(t.to_string()),
                _ => break,
            }
        }
        let own = self.record_key(&self.boot_id);
        let mut live = Vec::new();
        for key in keys.into_iter().filter(|k| *k != own) {
            let Ok((out, age)) =
                super::server_clock::get_with_server_age(&self.client, &self.bucket, &key).await
            else {
                continue; // gone between the LIST and the GET
            };
            let Ok(body) = out.body.collect().await else {
                continue;
            };
            let Ok(record) = serde_json::from_slice::<NodeRecord>(&body.into_bytes()) else {
                continue;
            };
            if heartbeat_live(age, record.ttl_secs) {
                live.push(record);
            } else if age.is_some_and(|a| a > record.ttl_secs.saturating_mul(10)) {
                // Long dead: housekeeping, best effort.
                let _ = self
                    .client
                    .delete_object()
                    .bucket(&self.bucket)
                    .key(&key)
                    .send()
                    .await;
            }
        }
        Ok(live)
    }

    async fn heartbeat(&self, cfg: &Config, manifest: &Manifest) -> Result<(), String> {
        let mut record = NodeRecord {
            node: self.node_id.clone(),
            boot_id: self.boot_id.clone(),
            ttl_secs: self.ttl_secs,
            storages: BTreeMap::new(),
        };
        for s in witnessed(cfg, manifest) {
            if let Some(i) = intent_of(cfg, &s) {
                record.storages.entry(s.0).or_default().insert(s.1, i);
            }
        }
        let body = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(self.record_key(&self.boot_id))
            .content_type("application/json")
            .body(body.into())
            .send()
            .await
            .map_err(|e| format!("PUT node heartbeat: {}", super::cas::sdk_error_signal(&e)))?;
        Ok(())
    }

    /// One full sync against `cfg`: seed, take over storages whose writers
    /// are gone, heartbeat, and set the gate.
    pub async fn sync(&self, cfg: &Config) -> Result<(), String> {
        let _one = self.running.lock().await;
        self.sync_locked(cfg).await
    }

    async fn sync_locked(&self, cfg: &Config) -> Result<(), String> {
        let result = self.sync_inner(cfg).await;
        if let Err(e) = &result {
            // The last verdict, judged against the current config; the
            // gate refuses the witnessed storages once it is stale.
            let manifest = self.manifest.lock().clone();
            let found = mismatches(cfg, &manifest);
            self.gate.set(gate_state(cfg, &manifest, &found));
            self.publish_status(found, Some(e.clone()));
        }
        result
    }

    async fn sync_inner(&self, cfg: &Config) -> Result<(), String> {
        let label = self.node_label();
        let read = self.read_manifest().await?.0;
        let missing: Vec<(Storage, WriteEncryption)> = seeds(cfg)
            .into_iter()
            .filter(|(s, _)| read.get(s).is_none())
            .collect();
        let mut manifest = if missing.is_empty() {
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
        let found = mismatches(cfg, &manifest);
        if !found.is_empty() {
            let peers = self.live_peers().await?;
            let changed_at = self.config_changed_at_server();
            let takeovers: Vec<(Storage, Published, WriteEncryption)> = found
                .iter()
                .filter_map(|m| {
                    let s = (m.backend.clone(), m.bucket.clone());
                    let published = manifest.get(&s)?.clone();
                    let held = peers.iter().any(|p| p.get(&s) == Some(&m.published));
                    takeover_allowed(TakeoverFacts {
                        local: &m.local,
                        published: &published,
                        live_holder: held,
                        config_changed_at: changed_at,
                    })
                    .then(|| (s, published, m.local.clone()))
                })
                .collect();
            if !takeovers.is_empty() {
                manifest = self
                    .update_manifest(|m, now| {
                        let mut changed = false;
                        for (s, seen, mine) in &takeovers {
                            // Only over the entry that we judged.
                            if m.get(s) == Some(seen) {
                                m.set(s, mine.clone(), &label, now);
                                changed = true;
                            }
                        }
                        changed
                    })
                    .await?;
                for (s, seen, mine) in &takeovers {
                    if manifest.get(s).map(|p| &p.intent) != Some(mine) {
                        continue; // a peer changed it first
                    }
                    let target = format!(
                        "{}/{}: {} -> {} (no live instance writes the old encryption)",
                        s.0,
                        s.1,
                        seen.intent.describe(),
                        mine.describe()
                    );
                    tracing::warn!("encryption witness: takeover of {target}");
                    crate::audit::audit_log(
                        "bucket_encryption_witness_takeover",
                        "background",
                        &target,
                        &axum::http::HeaderMap::new(),
                        &s.1,
                        "",
                    );
                }
            }
        }
        *self.manifest.lock() = manifest.clone();
        let found = mismatches(cfg, &manifest);
        // Log a mismatch when it starts, not at every poll.
        let before = self.status.load();
        for m in found.iter().filter(|m| !before.mismatches.contains(m)) {
            tracing::error!("encryption witness: {}", m.refusal());
        }
        self.gate.set(gate_state(cfg, &manifest, &found));
        if !witnessed(cfg, &manifest).is_empty() {
            self.heartbeat(cfg, &manifest).await?;
        }
        self.gate.synced();
        self.publish_status(found, None);
        Ok(())
    }

    /// After a config apply on this node: publish this node's write
    /// encryption for every witnessed storage whose encryption the apply
    /// changed, and for every storage that the request confirmed as a
    /// downgrade (`confirmed`, the `X-DGP-Confirm-Encryption-Downgrade`
    /// subjects), then sync. The gate follows `new` at once, before any I/O.
    pub async fn publish_changes(
        &self,
        old: &Config,
        new: &Config,
        confirmed: &[String],
    ) -> Result<(), String> {
        self.config_changed_at
            .store(unix_ms(), std::sync::atomic::Ordering::Relaxed);
        self.judge(new);
        let label = self.node_label();
        let _one = self.running.lock().await;
        let cached = self.manifest.lock().clone();
        let mut candidates: BTreeSet<Storage> = cached.keys().collect();
        candidates.extend(config_storages(old));
        candidates.extend(config_storages(new));
        let mut publish = changed_storages(old, new, &candidates);
        for s in confirmed_storages(new, &candidates, confirmed) {
            if let Some(i) = intent_of(new, &s) {
                if !publish.iter().any(|(p, _)| *p == s) {
                    publish.push((s, i));
                }
            }
        }
        // A plaintext storage that nobody witnesses needs no entry.
        publish.retain(|(s, i)| i.encrypts() || cached.get(s).is_some());
        if !publish.is_empty() {
            let published = self
                .update_manifest(|m, now| {
                    let mut edited = false;
                    for (s, i) in &publish {
                        if m.get(s).map(|p| &p.intent) != Some(i) {
                            m.set(s, i.clone(), &label, now);
                            edited = true;
                        }
                    }
                    edited
                })
                .await;
            match published {
                Ok(m) => *self.manifest.lock() = m,
                Err(e) => {
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

    /// Remove the heartbeat of this node's previous process (a restart).
    pub async fn forget_previous_boot(&self) {
        if let Some(prev) = &self.previous_boot_id {
            let _ = self
                .client
                .delete_object()
                .bucket(&self.bucket)
                .key(self.record_key(prev))
                .send()
                .await;
        }
    }

    /// Poll every `interval`, judging the live config each time.
    pub fn spawn_poller(
        self: Arc<Self>,
        config: crate::config::SharedConfig,
        interval: std::time::Duration,
    ) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // the boot ran the first sync
            loop {
                tick.tick().await;
                if crate::shutdown::is_shutting_down() {
                    return;
                }
                let cfg = config.read().await.clone();
                if let Err(e) = self.sync(&cfg).await {
                    tracing::warn!(
                        "encryption witness: sync failed ({e}); the last verdict holds for \
                         two polls, then the witnessed buckets refuse writes"
                    );
                }
            }
        });
    }
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

    fn published(i: WriteEncryption, at: i64) -> Published {
        Published {
            intent: i,
            set_by: "node-a".into(),
            set_at: at,
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

    /// Review of P3 (probe-verified): a lagging node reverted a rotation
    /// when the up-to-date node restarted, and a node with a missing key
    /// could take over. A takeover needs a config NEWER than the entry,
    /// a usable key, no live holder, and no downgrade.
    #[test]
    fn takeover_needs_a_newer_config_a_key_and_no_live_holder() {
        let p = published(aes("a"), 1_000);
        fn facts<'a>(
            local: &'a WriteEncryption,
            p: &'a Published,
            held: bool,
            at: Option<i64>,
        ) -> TakeoverFacts<'a> {
            TakeoverFacts {
                local,
                published: p,
                live_holder: held,
                config_changed_at: at,
            }
        }
        let newer = Some(2_000);
        assert!(takeover_allowed(facts(&aes("b"), &p, false, newer)));
        assert!(
            !takeover_allowed(facts(&aes("b"), &p, true, newer)),
            "a live holder"
        );
        assert!(
            !takeover_allowed(facts(&aes("b"), &p, false, Some(999))),
            "a lagging config"
        );
        assert!(
            !takeover_allowed(facts(&aes("b"), &p, false, None)),
            "no config time"
        );
        assert!(
            !takeover_allowed(facts(&none(), &p, false, newer)),
            "never a downgrade"
        );
        let refusing = WriteEncryption {
            mode: "aes256-gcm-proxy".into(),
            key_id: None,
            kcv: None,
        };
        assert!(
            !takeover_allowed(facts(&refusing, &p, false, newer)),
            "a missing key"
        );
        let p0 = published(none(), 1_000);
        assert!(takeover_allowed(TakeoverFacts {
            local: &aes("a"),
            published: &p0,
            live_holder: false,
            config_changed_at: newer,
        }));
    }

    #[test]
    fn heartbeat_liveness_is_judged_on_the_server_clock() {
        assert!(heartbeat_live(Some(90), 90));
        assert!(!heartbeat_live(Some(91), 90));
        assert!(heartbeat_live(None, 90), "an unknown age is live");
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

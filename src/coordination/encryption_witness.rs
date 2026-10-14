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
//!   (raw backend + real bucket name) the published write encryption, as
//!   `{mode, key_id}` and the node that set it. Never key material: the
//!   key id is the public id that every object already carries. Written
//!   with the CAS rules of the rest of coordination: create with
//!   `If-None-Match: *`, replace with `If-Match: <etag>`, re-read and
//!   merge on a lost race.
//! * `_dgp/encryption/nodes/<node id>-<boot id>.json` — each node's
//!   heartbeat: its own write encryption for every witnessed storage.
//!   Liveness is judged on the S3 server clock (the GET's `Date` minus the
//!   object's `Last-Modified`, against the body's `ttl_secs`), so node
//!   clock skew does not matter.
//!
//! ## Who sets the intent
//!
//! * **Boot and every poll** compare: a node whose write encryption for a
//!   witnessed storage differs from the manifest refuses writes to it
//!   (`503 SlowDown` naming the mismatch; reads keep working). A storage
//!   that this node gives its own `encryption` block and that the manifest
//!   lacks is ADDED (create-only: the first writer wins).
//! * **A config apply** on a node publishes that node's write encryption
//!   for every storage whose encryption the apply changed on the node: the
//!   apply is the operator's intent. The other nodes then refuse writes to
//!   it until they apply the same config (a rolling apply converges).
//! * **Takeover**: a mismatched node publishes its own encryption when NO
//!   live node holds the published one (its writers are gone: a
//!   restart-based rollout where every node restarted with the new file),
//!   but never as a downgrade (encrypting → plaintext needs an apply with
//!   its confirmation).
//!
//! An unreadable manifest (the coordination bucket does not answer) keeps
//! the last verdict; `/_/ready` reports `unknown`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::{Config, WriteEncryption};
use crate::storage::WitnessGate;

/// The manifest object.
pub const MANIFEST_KEY: &str = "_dgp/encryption/intent.json";
/// The prefix of the node heartbeat objects.
pub const NODES_PREFIX: &str = "_dgp/encryption/nodes/";
/// Default poll interval (`DGP_ENCRYPTION_WITNESS_POLL_SECS`).
pub const DEFAULT_POLL_SECS: u64 = 30;

/// A stored bucket: `(raw backend, real bucket)`.
pub type Storage = (String, String);

/// One published intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    #[serde(flatten)]
    pub intent: WriteEncryption,
    /// The node that set it.
    #[serde(default)]
    pub set_by: String,
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

    pub fn set(&mut self, s: &Storage, intent: WriteEncryption, by: &str) {
        self.version = 1;
        self.storages.entry(s.0.clone()).or_default().insert(
            s.1.clone(),
            Published {
                intent,
                set_by: by.to_string(),
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
        format!(
            "bucket '{}' on backend '{}': this instance writes it with {}, but the encryption \
             that the instances share (coordination bucket) is {}, set by '{}'. This instance \
             refuses writes to the bucket until its config agrees: apply the same config on \
             every instance. Reads keep working",
            self.bucket,
            self.backend,
            self.local.describe(),
            self.published.describe(),
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

/// The storages this node witnesses: the manifest's that it can write,
/// and its own overrides.
pub fn witnessed(cfg: &Config, manifest: &Manifest) -> BTreeSet<Storage> {
    let mut out: BTreeSet<Storage> = manifest
        .keys()
        .filter(|s| intent_of(cfg, s).is_some())
        .collect();
    out.extend(override_storages(cfg));
    out
}

/// The node's own override storages that the manifest lacks, with their
/// intent: what a boot or a poll adds (create-only).
pub fn missing_entries(cfg: &Config, manifest: &Manifest) -> Vec<(Storage, WriteEncryption)> {
    override_storages(cfg)
        .into_iter()
        .filter(|s| manifest.get(s).is_none())
        .filter_map(|s| intent_of(cfg, &s).map(|i| (s, i)))
        .collect()
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

/// May a mismatched node publish its own intent over the published one?
/// Only when no live node holds the published intent, and never as a
/// downgrade (encrypting → plaintext).
pub fn takeover_allowed(
    local: &WriteEncryption,
    published: &WriteEncryption,
    live_holder: bool,
) -> bool {
    !live_holder && !(published.encrypts() && !local.encrypts())
}

/// Is a heartbeat live? `age` is its server-clock age; an unknown age
/// counts as live (never take over on a guess).
pub fn heartbeat_live(age: Option<i64>, ttl_secs: i64) -> bool {
    age.is_none_or(|a| a <= ttl_secs)
}

// ── The witness ─────────────────────────────────────────────────────────

/// The witness verdict, for `/_/ready`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WitnessStatus {
    /// `agree`, `mismatch`, or `unknown` (the coordination bucket did not
    /// answer; the last verdict holds).
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
    status: arc_swap::ArcSwap<WitnessStatus>,
    /// One sync at a time (a poll and an apply).
    running: tokio::sync::Mutex<()>,
}

/// How many times a CAS write re-reads and merges after a lost race.
const CAS_ATTEMPTS: usize = 8;

impl EncryptionWitness {
    pub fn new(
        client: aws_sdk_s3::Client,
        bucket: String,
        node_id: String,
        boot: crate::coordination::BootIds,
        poll: std::time::Duration,
        gate: Arc<WitnessGate>,
    ) -> Self {
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
            status: arc_swap::ArcSwap::from_pointee(WitnessStatus {
                status: "unknown",
                ..Default::default()
            }),
            running: tokio::sync::Mutex::new(()),
        }
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

    /// Judge `cfg` against the last manifest read, with no I/O: the gate
    /// follows a config change at once (fail closed until the next sync).
    pub fn judge(&self, cfg: &Config) {
        let manifest = self.manifest.lock().clone();
        self.publish_verdict(mismatches(cfg, &manifest), None);
    }

    fn publish_verdict(&self, found: Vec<Mismatch>, error: Option<String>) {
        let refused = found
            .iter()
            .map(|m| ((m.backend.clone(), m.bucket.clone()), m.refusal()))
            .collect();
        self.gate.set(refused);
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
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(MANIFEST_KEY)
            .send()
            .await
        {
            Ok(out) => {
                let etag = out.e_tag().map(str::to_string);
                let body = out
                    .body
                    .collect()
                    .await
                    .map_err(|e| format!("read {MANIFEST_KEY}: {e}"))?
                    .into_bytes();
                let manifest = serde_json::from_slice(&body)
                    .map_err(|e| format!("parse {MANIFEST_KEY}: {e}"))?;
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
        mut edit: impl FnMut(&mut Manifest) -> bool,
    ) -> Result<Manifest, String> {
        for _ in 0..CAS_ATTEMPTS {
            let (mut manifest, etag) = self.read_manifest().await?;
            if !edit(&mut manifest) {
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
            .await?
            {
                Some(_) => return Ok(manifest),
                None => continue, // a peer wrote first: merge again
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

    /// One full sync against `cfg`: add this node's missing entries, take
    /// over storages whose writers are gone, heartbeat, and set the gate.
    pub async fn sync(&self, cfg: &Config) -> Result<(), String> {
        let _one = self.running.lock().await;
        let result = self.sync_inner(cfg).await;
        if let Err(e) = &result {
            // Keep the last verdict, judged against the current config.
            let manifest = self.manifest.lock().clone();
            self.publish_verdict(mismatches(cfg, &manifest), Some(e.clone()));
        }
        result
    }

    async fn sync_inner(&self, cfg: &Config) -> Result<(), String> {
        let label = self.node_label();
        let read = self.read_manifest().await?.0;
        let missing = missing_entries(cfg, &read);
        let mut manifest = if missing.is_empty() {
            read
        } else {
            self.update_manifest(|m| {
                let mut changed = false;
                for (s, i) in &missing {
                    if m.get(s).is_none() {
                        m.set(s, i.clone(), &label);
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
            let takeovers: Vec<(Storage, WriteEncryption, WriteEncryption)> = found
                .iter()
                .filter_map(|m| {
                    let s = (m.backend.clone(), m.bucket.clone());
                    let held = peers.iter().any(|p| p.get(&s) == Some(&m.published));
                    takeover_allowed(&m.local, &m.published, held)
                        .then(|| (s, m.published.clone(), m.local.clone()))
                })
                .collect();
            if !takeovers.is_empty() {
                manifest = self
                    .update_manifest(|m| {
                        let mut changed = false;
                        for (s, seen, mine) in &takeovers {
                            // Only over the intent that we judged.
                            if m.get(s).map(|p| &p.intent) == Some(seen) {
                                m.set(s, mine.clone(), &label);
                                changed = true;
                            }
                        }
                        changed
                    })
                    .await?;
                for (s, seen, mine) in &takeovers {
                    tracing::warn!(
                        "encryption witness: bucket '{}' on backend '{}': no live instance \
                         writes it with {} any more; this instance published {}",
                        s.1,
                        s.0,
                        seen.describe(),
                        mine.describe()
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
        self.publish_verdict(found, None);
        if !witnessed(cfg, &manifest).is_empty() {
            self.heartbeat(cfg, &manifest).await?;
        }
        Ok(())
    }

    /// After a config apply on this node: publish this node's write
    /// encryption for every witnessed storage whose encryption the apply
    /// changed, then sync. The gate follows `new` at once, before any I/O.
    pub async fn publish_changes(&self, old: &Config, new: &Config) -> Result<(), String> {
        self.judge(new);
        let label = self.node_label();
        let cached = self.manifest.lock().clone();
        let mut candidates: BTreeSet<Storage> = cached.keys().collect();
        candidates.extend(override_storages(old));
        candidates.extend(override_storages(new));
        let changed = changed_storages(old, new, &candidates);
        if !changed.is_empty() {
            let _one = self.running.lock().await;
            let published = self
                .update_manifest(|m| {
                    let mut edited = false;
                    for (s, i) in &changed {
                        // Only an entry that differs is written.
                        if m.get(s).map(|p| &p.intent) != Some(i) {
                            m.set(s, i.clone(), &label);
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
            for (s, i) in &changed {
                tracing::info!(
                    "encryption witness: published {} for bucket '{}' on backend '{}'",
                    i.describe(),
                    s.1,
                    s.0
                );
            }
        }
        self.sync(new).await
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
                    tracing::warn!("encryption witness: sync failed ({e}); the last verdict holds");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KA: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn cfg(releases: &str) -> Config {
        Config::from_yaml_str(&format!(
            "storage:\n  backends:\n    - name: local-disk\n      type: filesystem\n      path: /tmp/dgp-witness-a\n  default_backend: local-disk\n  buckets:\n    releases: {releases}\n    downloads: {{ backend: local-disk }}\n"
        ))
        .unwrap()
    }

    fn with_key() -> Config {
        cfg(&format!(
            "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KA}\", key_id: rel-a }} }}"
        ))
    }

    fn rel() -> Storage {
        ("local-disk".into(), "releases".into())
    }

    fn aes(id: &str) -> WriteEncryption {
        WriteEncryption {
            mode: "aes256-gcm-proxy".into(),
            key_id: Some(id.into()),
        }
    }

    fn none() -> WriteEncryption {
        WriteEncryption {
            mode: "none".into(),
            key_id: None,
        }
    }

    #[test]
    fn a_node_adds_its_overrides_and_compares_every_entry() {
        let a = with_key();
        let b = cfg("{ backend: local-disk }");
        let mut m = Manifest::default();
        assert_eq!(missing_entries(&a, &m), vec![(rel(), aes("rel-a"))]);
        // B has no override: it adds nothing, and has nothing to compare.
        assert!(missing_entries(&b, &m).is_empty());
        m.set(&rel(), aes("rel-a"), "node-a");
        assert!(mismatches(&a, &m).is_empty());
        let found = mismatches(&b, &m);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].local.clone(), found[0].published.clone()),
            (none(), aes("rel-a"))
        );
        let msg = found[0].refusal();
        assert!(
            msg.contains("bucket 'releases'") && msg.contains("key id 'rel-a'"),
            "{msg}"
        );
        assert!(!msg.contains(KA));
        // A storage on a backend this node does not define is not judged.
        m.set(&("aws-dr".into(), "x".into()), aes("x"), "node-c");
        assert_eq!(mismatches(&b, &m).len(), 1);
        assert_eq!(witnessed(&b, &m), BTreeSet::from([rel()]));
    }

    #[test]
    fn an_apply_publishes_only_what_it_changed() {
        let (a, b) = (cfg("{ backend: local-disk }"), with_key());
        let cands = BTreeSet::from([rel(), ("local-disk".into(), "downloads".into())]);
        assert_eq!(
            changed_storages(&a, &b, &cands),
            vec![(rel(), aes("rel-a"))]
        );
        assert_eq!(changed_storages(&b, &a, &cands), vec![(rel(), none())]);
        assert!(changed_storages(&b, &b, &cands).is_empty());
    }

    #[test]
    fn takeover_needs_no_live_holder_and_never_downgrades() {
        assert!(takeover_allowed(&aes("b"), &aes("a"), false));
        assert!(!takeover_allowed(&aes("b"), &aes("a"), true));
        assert!(takeover_allowed(&aes("a"), &none(), false));
        assert!(
            !takeover_allowed(&none(), &aes("a"), false),
            "never a silent downgrade"
        );
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
        let a = with_key();
        for (s, i) in missing_entries(&a, &m) {
            m.set(&s, i, "node-a");
        }
        let text = serde_json::to_string(&m).unwrap();
        assert!(!text.contains(KA), "{text}");
        assert!(text.contains("\"key_id\":\"rel-a\""), "{text}");
        let back: Manifest = serde_json::from_str(&text).unwrap();
        assert_eq!(back, m);
    }
}

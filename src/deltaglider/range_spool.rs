// SPDX-License-Identifier: BUSL-1.1

//! Short-lived cache of VERIFIED delta reconstructions for range reads
//! (storage-11).
//!
//! A range GET of a large delta object reconstructs the whole object into a
//! spool file, checks its SHA-256, and serves one slice. A client that reads
//! the object in many ranges (a parallel downloader) paid one full decode per
//! range. This cache keeps the verified spool file for a short time, keyed by
//! `(bucket, key, content sha256)`, so the other ranges read the same file:
//!
//! - Only a spool that passed the integrity gate is inserted, so a range from
//!   the cache is as verified as a fresh decode.
//! - Single flight: a second range of the same key waits for the first
//!   decode instead of starting its own.
//! - The file keeps its spool-budget reservation while it is cached. The spool
//!   evicts idle entries first when another op finds the budget short
//!   ([`SpoolEvictor`]), and an entry expires after its TTL.
//! - An overwrite changes the content sha, so a new version never matches an
//!   old entry.
//!
//! The same cache keeps the verified REFERENCE of a deltaspace that a large
//! delta GET downloaded ([`RangeSpoolKey::reference`]): the other deltas of
//! the deltaspace decode against that file instead of downloading the
//! reference again. A full GET of a large delta shares its reconstruction
//! here too, so concurrent GETs of one object decode it once.
//!
//! KEPT reference files (issue #200). A reference that the storage reads as
//! plaintext from a remote backend ([`RangeSpoolKey::kept_reference`]) has
//! no TTL: it stays until the kept files pass their budget (least recently
//! used first) or another op needs the spool space. They live in ONE store
//! per spool directory ([`RangeSpoolCache::kept_store`]), which the engine
//! of a config apply shares with the engine it replaces. Each file has a
//! second name, `<spool>/references/<storage>.<deltaspace>.<sha256>` (a
//! hard link), that outlives the process. The next start takes these files
//! back ([`RangeSpoolCache::adopt`]) and hashes each again before its first
//! use. A delete or a rewrite of a reference drops its kept file
//! ([`RangeSpoolCache::forget_deltaspace`]), and a change of the encryption
//! settings drops them all ([`RangeSpoolCache::set_policy`]).

use crate::deltaglider::spool::{Spool, SpoolDir, SpoolEvictor};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Most cached reconstructions at once (kept reference files not counted).
pub(crate) const MAX_ENTRIES: usize = 16;

/// How long the kept store holds a reference that it cannot keep (one above
/// its whole budget, or one whose fill a delete or a purge overtook).
const UNKEPT_TTL: Duration = Duration::from_secs(60);

/// The settings of a closed store: no engine has them.
const CLOSED_POLICY: &str = "\0closed";

/// The file in the directory of the kept files that names the encryption
/// settings they were kept under.
const POLICY_FILE: &str = ".policy";

/// `(bucket, key, content sha256)`.
#[derive(Debug, Clone)]
pub(crate) struct RangeSpoolKey {
    pub bucket: String,
    pub key: String,
    pub sha256: String,
    /// A kept reference ([`Self::kept_reference`]): what the store looked
    /// like when the key was made. Not part of the key's identity.
    kept: Option<KeptTag>,
}

/// What a kept key saw when it was made. Its fill keeps the file only when
/// the store still looks the same.
#[derive(Debug, Clone)]
pub(crate) struct KeptTag {
    /// [`RangeSpoolCache::generation`].
    pub generation: u64,
    /// The encryption settings of the engine that made the key
    /// ([`RangeSpoolCache::set_policy`]).
    pub policy: Arc<str>,
}

impl PartialEq for RangeSpoolKey {
    fn eq(&self, other: &Self) -> bool {
        (&self.bucket, &self.key, &self.sha256, self.kept.is_some())
            == (
                &other.bucket,
                &other.key,
                &other.sha256,
                other.kept.is_some(),
            )
    }
}

impl Eq for RangeSpoolKey {}

impl std::hash::Hash for RangeSpoolKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (&self.bucket, &self.key, &self.sha256, self.kept.is_some()).hash(state);
    }
}

impl RangeSpoolKey {
    /// The reconstruction of the object `bucket/key` with content `sha256`.
    pub(crate) fn object(bucket: &str, key: String, sha256: String) -> Self {
        Self {
            bucket: bucket.to_string(),
            key,
            sha256,
            kept: None,
        }
    }

    /// The reference of a deltaspace (`deltaspace`: the engine's cache key
    /// of it) with content `sha256`. Its bucket is empty, so no object key
    /// can name it.
    pub(crate) fn reference(deltaspace: String, sha256: String) -> Self {
        Self {
            bucket: String::new(),
            key: deltaspace,
            sha256,
            kept: None,
        }
    }

    /// The reference of `deltaspace` in the storage `storage` (the engine's
    /// storage identity of the bucket) with content `sha256`, kept with no
    /// TTL and across restarts. `tag`: made BEFORE the caller asked whether
    /// the bucket's references may be kept, so a purge after that answer is
    /// seen.
    pub(crate) fn kept_reference(
        storage: &str,
        deltaspace: &str,
        sha256: String,
        tag: KeptTag,
    ) -> Self {
        Self {
            bucket: name_part(storage),
            key: name_part(deltaspace),
            sha256,
            kept: Some(tag),
        }
    }

    /// The name of the kept file of this key.
    fn file_name(&self) -> String {
        format!("{}.{}.{}", self.bucket, self.key, self.sha256)
    }

    /// Pure: the kept key that the file name `name` stands for.
    fn of_file_name(name: &str) -> Option<Self> {
        let mut parts = name.split('.');
        let (storage, deltaspace, sha256) = (parts.next()?, parts.next()?, parts.next()?);
        let ok = parts.next().is_none()
            && is_hex(storage, NAME_PART_LEN)
            && is_hex(deltaspace, NAME_PART_LEN)
            && is_hex(sha256, 64);
        ok.then(|| Self {
            bucket: storage.to_string(),
            key: deltaspace.to_string(),
            sha256: sha256.to_string(),
            kept: Some(KeptTag {
                generation: 0,
                policy: "".into(),
            }),
        })
    }
}

/// Hex digits of one name part of a kept file (128 bits of SHA-256).
const NAME_PART_LEN: usize = 32;

/// Pure: the name part of `text` in a kept file name. The name of a storage
/// or a deltaspace is not a safe file name, and it stays out of the listing
/// of the directory.
fn name_part(text: &str) -> String {
    use sha2::Digest as _;
    let mut hex = hex::encode(sha2::Sha256::digest(text.as_bytes()));
    hex.truncate(NAME_PART_LEN);
    hex
}

/// The state of a kept-reference store.
struct KeptState {
    /// `<spool>/references`.
    dir: PathBuf,
    /// Bytes of all kept files together (`config::limits`:
    /// `default_reference_cache`). A larger single file is not kept.
    max_bytes: AtomicU64,
    /// Rises with every purge: a fill that started before a purge keeps
    /// nothing.
    generation: AtomicU64,
    /// The encryption settings that the kept files belong to
    /// ([`POLICY_FILE`]); `None`: not known.
    policy: parking_lot::Mutex<Option<String>>,
    /// The disk level of the spool: nothing new is kept while it refuses
    /// writes.
    gate: Arc<crate::disk_watch::SpoolGate>,
}

/// The state of a kept reference file.
struct Kept {
    /// Its name in the directory of the kept files (`None`: the link failed).
    link: Option<PathBuf>,
    /// False for a file of an earlier run until its SHA-256 was checked.
    verified: bool,
}

struct Entry {
    spool: Arc<Spool>,
    created: Instant,
    used: Instant,
    bytes: u64,
    /// `None`: the entry expires at its TTL.
    kept: Option<Kept>,
}

impl Entry {
    /// Drop the entry and its kept name. The file goes, and its budget is
    /// released, once no reader holds it.
    fn discard(self) {
        if let Some(Kept {
            link: Some(link), ..
        }) = &self.kept
        {
            let _ = std::fs::remove_file(link);
        }
    }

    /// Is the file of a kept entry still there? Another process that starts
    /// on the same spool directory sweeps old names from it; the kept name
    /// is outside that sweep and gives the file its spool name back.
    fn alive(&self) -> bool {
        let path = self.spool.path();
        if path.exists() {
            return true;
        }
        match &self.kept {
            Some(Kept {
                link: Some(link), ..
            }) => std::fs::hard_link(link, path).is_ok(),
            _ => false,
        }
    }
}

/// Which entries an eviction takes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Entries with a TTL: the oldest.
    Timed,
    /// Kept reference files: the least recently used.
    Kept,
}

/// One key: the fill lock (single flight) and the cached spool.
#[derive(Default)]
struct Slot {
    entry: tokio::sync::Mutex<Option<Entry>>,
    /// A delete or a purge came while the slot was filling: the fill keeps
    /// nothing.
    forgotten: AtomicBool,
}

pub(crate) struct RangeSpoolCache {
    ttl: Duration,
    max_entries: usize,
    /// `Some`: this cache is a kept-reference store.
    kept: Option<KeptState>,
    /// The kept-reference store that the kept keys of this cache go to.
    store: Option<Arc<RangeSpoolCache>>,
    slots: parking_lot::Mutex<HashMap<RangeSpoolKey, Arc<Slot>>>,
    fills: AtomicU64,
    me: Weak<RangeSpoolCache>,
}

/// The live kept-reference stores, by directory.
static STORES: parking_lot::Mutex<Vec<(PathBuf, Weak<RangeSpoolCache>)>> =
    parking_lot::Mutex::new(Vec::new());

impl RangeSpoolCache {
    /// `ttl == 0` turns the cache off: every range decodes.
    #[cfg(test)]
    pub(crate) fn new(ttl: Duration, max_entries: usize) -> Arc<Self> {
        Self::build(ttl, max_entries, None, None)
    }

    /// The cache of one engine. Its kept references go to `store`; with the
    /// cache off (`ttl == 0`) or without a store, no reference is kept.
    pub(crate) fn for_engine(
        ttl: Duration,
        max_entries: usize,
        store: Option<Arc<RangeSpoolCache>>,
    ) -> Arc<Self> {
        Self::build(ttl, max_entries, None, store.filter(|_| !ttl.is_zero()))
    }

    /// THE kept-reference store of the directory `dir` in `spool`. One
    /// store per directory: the engine of a config apply gets the store of
    /// the engine it replaces, unchanged, so no file is taken back twice
    /// and an apply that fails changed nothing. Without a live store, a
    /// budget above 0 makes one, which takes back the files of an earlier
    /// run. The budget and the encryption settings of an engine take
    /// effect when it is installed ([`Self::install_kept`]).
    pub(crate) fn kept_store(dir: PathBuf, max_bytes: u64, spool: &SpoolDir) -> Option<Arc<Self>> {
        let mut stores = STORES.lock();
        stores.retain(|(_, store)| store.strong_count() > 0);
        let live = stores
            .iter()
            .find(|(d, _)| *d == dir)
            .and_then(|(_, store)| store.upgrade());
        if live.is_some() || max_bytes == 0 {
            return live;
        }
        let store = Self::new_store(UNKEPT_TTL, dir.clone(), max_bytes, spool);
        spool.register_evictor(Arc::downgrade(&store) as Weak<dyn SpoolEvictor>);
        stores.push((dir, Arc::downgrade(&store)));
        Some(store)
    }

    /// An engine with a budget of `max_bytes` for the kept files of `dir`
    /// and the encryption settings `policy` is the installed one now. A
    /// budget of 0 closes the live store and deletes every kept file.
    pub(crate) fn install_kept(dir: &Path, max_bytes: u64, policy: &str) {
        let live = STORES
            .lock()
            .iter()
            .find(|(d, _)| d == dir)
            .and_then(|(_, store)| store.upgrade());
        match live {
            Some(store) if max_bytes == 0 => store.close(),
            Some(store) => {
                if let Some(kept) = &store.kept {
                    kept.max_bytes.store(max_bytes, Ordering::SeqCst);
                }
                store.set_policy(policy);
                store.trim_kept(None);
            }
            None if max_bytes == 0 => {
                remove_kept_files(dir);
            }
            None => {}
        }
    }

    /// Keep nothing more and drop what is kept: the installed engine has
    /// no budget for kept files. An engine that still uses this store (a
    /// run that drains) matches no settings until the next install.
    fn close(&self) {
        let Some(kept) = &self.kept else { return };
        kept.max_bytes.store(0, Ordering::SeqCst);
        *kept.policy.lock() = Some(CLOSED_POLICY.to_string());
        self.purge();
    }

    /// A kept-reference store on `dir` that took back the files there.
    fn new_store(ttl: Duration, dir: PathBuf, max_bytes: u64, spool: &SpoolDir) -> Arc<Self> {
        let marker = dir.join(POLICY_FILE);
        let mut policy = None;
        if std::fs::metadata(&marker).is_ok_and(|m| m.len() <= 64 * 1024) {
            // small: one line per backend (a longer file reads as "not known").
            policy = std::fs::read_to_string(&marker).ok();
        }
        let kept = KeptState {
            dir,
            max_bytes: AtomicU64::new(max_bytes),
            generation: AtomicU64::new(0),
            policy: parking_lot::Mutex::new(policy),
            gate: spool.gate().clone(),
        };
        let store = Self::build(ttl, MAX_ENTRIES, Some(kept), None);
        store.adopt(spool);
        store
    }

    fn build(
        ttl: Duration,
        max_entries: usize,
        kept: Option<KeptState>,
        store: Option<Arc<RangeSpoolCache>>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            ttl,
            max_entries,
            kept,
            store,
            slots: Default::default(),
            fills: AtomicU64::new(0),
            me: me.clone(),
        })
    }

    /// Whether entries are kept at all (`ttl > 0`).
    pub(crate) fn caches(&self) -> bool {
        !self.ttl.is_zero()
    }

    /// The kept-reference store: this cache, or the one it sends kept keys to.
    fn kept_store_of(&self) -> Option<&RangeSpoolCache> {
        match &self.store {
            Some(store) => Some(store),
            None => self.kept.is_some().then_some(self),
        }
    }

    /// The tag of a kept key that an engine with the encryption settings
    /// `policy` makes now ([`RangeSpoolKey::kept_reference`]); `None`: this
    /// cache keeps no reference.
    pub(crate) fn kept_tag(&self, policy: &Arc<str>) -> Option<KeptTag> {
        let kept = self.kept_store_of()?.kept.as_ref()?;
        Some(KeptTag {
            generation: kept.generation.load(Ordering::SeqCst),
            policy: policy.clone(),
        })
    }

    /// How many times `fill` ran (decodes), for the tests.
    #[cfg(test)]
    pub(crate) fn fill_count(&self) -> u64 {
        self.fills.load(Ordering::SeqCst)
            + self
                .store
                .as_ref()
                .map_or(0, |s| s.fills.load(Ordering::SeqCst))
    }

    /// Drop the kept references of `deltaspace` in the storage `storage`:
    /// its reference was deleted or written again.
    pub(crate) fn forget_deltaspace(&self, storage: &str, deltaspace: &str) {
        let (storage, deltaspace) = (name_part(storage), name_part(deltaspace));
        self.forget(|k| k.bucket == storage && k.key == deltaspace);
    }

    /// The encryption settings that kept files may belong to are `policy`
    /// now. Kept files of other settings are all dropped: a bucket that
    /// encrypts now may have a plaintext reference among them. Returns
    /// whether the settings changed.
    pub(crate) fn set_policy(&self, policy: &str) -> bool {
        let Some(store) = self.kept_store_of() else {
            return false;
        };
        let Some(kept) = &store.kept else {
            return false;
        };
        {
            let mut current = kept.policy.lock();
            if current.as_deref() == Some(policy) {
                return false;
            }
            *current = Some(policy.to_string());
        }
        store.purge();
        if let Err(e) = std::fs::write(kept.dir.join(POLICY_FILE), policy) {
            tracing::warn!(
                "[reference cache] cannot write {} ({e}): the next start drops the kept files",
                kept.dir.join(POLICY_FILE).display()
            );
        }
        true
    }

    /// Drop every kept reference and every file of the directory. A fill
    /// that runs now keeps nothing.
    pub(crate) fn purge(&self) {
        let Some(store) = self.kept_store_of() else {
            return;
        };
        let Some(kept) = &store.kept else { return };
        kept.generation.fetch_add(1, Ordering::SeqCst);
        store.forget(|_| true);
        let dropped = remove_kept_files(&kept.dir);
        if dropped > 0 {
            tracing::info!(
                "[reference cache] dropped {dropped} file(s) in {}",
                kept.dir.display()
            );
        }
    }

    /// Drop the kept entries whose key `matches`. A slot that is filling
    /// is marked, so its fill keeps nothing.
    fn forget(&self, matches: impl Fn(&RangeSpoolKey) -> bool) {
        let Some(store) = self.kept_store_of() else {
            return;
        };
        let slots: Vec<Arc<Slot>> = store
            .slots
            .lock()
            .iter()
            .filter(|(k, _)| k.kept.is_some() && matches(k))
            .map(|(_, s)| s.clone())
            .collect();
        for slot in slots {
            let taken = match slot.entry.try_lock() {
                Ok(mut entry) => entry.take(),
                Err(_) => {
                    // In use (a fill, a check, a reader of the map): the
                    // fill keeps nothing, and the entry goes when the lock
                    // is free.
                    slot.forgotten.store(true, Ordering::SeqCst);
                    store.drop_when_free(slot.clone());
                    None
                }
            };
            if let Some(entry) = taken {
                entry.discard();
            }
        }
        store.prune_empty();
    }

    /// Drop the entry of a forgotten `slot` as soon as its lock is free.
    fn drop_when_free(&self, slot: Arc<Slot>) {
        let me = self.me.clone();
        let drop_it = async move {
            let taken = {
                let mut entry = slot.entry.lock().await;
                if slot.forgotten.swap(false, Ordering::SeqCst) {
                    entry.take()
                } else {
                    None
                }
            };
            if let Some(entry) = taken {
                entry.discard();
            }
            drop(slot);
            if let Some(cache) = me.upgrade() {
                cache.prune_empty();
            }
        };
        // Without a runtime the mark stays: the next fill keeps nothing.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(drop_it);
        }
    }

    /// Take back the reference files that an earlier run kept: reserve the
    /// space of each (newest first; a file past the budget, or with no
    /// spool room, is deleted) and enter it unverified. Its SHA-256 is
    /// checked before its first use.
    fn adopt(&self, spool: &SpoolDir) {
        let Some(kept) = &self.kept else { return };
        if let Err(e) = crate::deltaglider::spool::create_private_dir(&kept.dir) {
            tracing::warn!(
                "[reference cache] {} is not usable ({e}): reference files do not outlive this process",
                kept.dir.display()
            );
            return;
        }
        let Ok(entries) = std::fs::read_dir(&kept.dir) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, PathBuf, String, u64)> = entries
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok().filter(|m| m.is_file())?;
                let name = e.file_name().to_str()?.to_string();
                let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                Some((modified, e.path(), name, meta.len()))
            })
            .filter(|(_, _, name, _)| name != POLICY_FILE)
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        let max_bytes = kept.max_bytes.load(Ordering::SeqCst);
        let (mut count, mut total) = (0u64, 0u64);
        let now = Instant::now();
        for (_, path, name, len) in files {
            let key = RangeSpoolKey::of_file_name(&name)
                .filter(|_| total.saturating_add(len) <= max_bytes);
            // The temp name of the new reservation takes the file over.
            let taken = key.and_then(|key| {
                let file = spool.try_acquire(len).ok()?;
                std::fs::rename(&path, file.path()).ok()?;
                Some((key, file))
            });
            let Some((key, file)) = taken else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            let link = std::fs::hard_link(file.path(), &path)
                .is_ok()
                .then_some(path);
            let entry = Entry {
                spool: Arc::new(file),
                created: now,
                used: now,
                bytes: len,
                kept: Some(Kept {
                    link,
                    verified: false,
                }),
            };
            self.slots.lock().insert(
                key,
                Arc::new(Slot {
                    entry: tokio::sync::Mutex::new(Some(entry)),
                    forgotten: AtomicBool::new(false),
                }),
            );
            count += 1;
            total += len;
        }
        if count > 0 {
            tracing::info!(
                "[reference cache] took back {count} reference file(s), {} MiB, from {}",
                total / (1024 * 1024),
                kept.dir.display()
            );
        }
    }

    /// The cache that holds `key`: the kept-reference store for a kept key.
    fn home(&self, key: &RangeSpoolKey) -> &RangeSpoolCache {
        match (&self.store, &key.kept) {
            (Some(store), Some(_)) => store,
            _ => self,
        }
    }

    /// The cached spool of `key` if it is fresh and not being filled now:
    /// a hit that needs no fill, and so no reservation.
    pub(crate) fn peek(&self, key: &RangeSpoolKey) -> Option<Arc<Spool>> {
        let home = self.home(key);
        if home.ttl.is_zero() {
            return None;
        }
        let slot = home.slots.lock().get(key).cloned()?;
        let mut entry = slot.entry.try_lock().ok()?;
        let e = entry.as_mut()?;
        let fresh = match &e.kept {
            Some(kept) => kept.verified && e.alive(),
            None => e.created.elapsed() < home.ttl,
        };
        fresh.then(|| {
            e.used = Instant::now();
            e.spool.clone()
        })
    }

    /// The verified spool of `key`: the cached one while it is fresh, else
    /// the result of `fill` (which must verify it), cached. A concurrent
    /// caller of the same key waits for the running fill. A failed fill is
    /// not cached; the next caller runs its own.
    pub(crate) async fn get_or_fill<F, Fut, E>(
        &self,
        key: RangeSpoolKey,
        fill: F,
    ) -> Result<Arc<Spool>, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Spool, E>>,
    {
        self.home(&key).get_or_fill_here(key, fill).await
    }

    async fn get_or_fill_here<F, Fut, E>(
        &self,
        key: RangeSpoolKey,
        fill: F,
    ) -> Result<Arc<Spool>, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Spool, E>>,
    {
        if self.ttl.is_zero() {
            self.fills.fetch_add(1, Ordering::SeqCst);
            return fill().await.map(Arc::new);
        }
        let slot = self.slots.lock().entry(key.clone()).or_default().clone();
        let mut entry = slot.entry.lock().await;
        if let Some(e) = entry.as_mut() {
            let alive = e.kept.is_none() || e.alive();
            let fresh = match e.kept.as_mut() {
                Some(kept) => {
                    if alive && !kept.verified {
                        kept.verified =
                            sha256_of(e.spool.path()).await.as_deref() == Some(key.sha256.as_str());
                    }
                    alive && kept.verified
                }
                None => e.created.elapsed() < self.ttl,
            };
            if fresh {
                e.used = Instant::now();
                let spool = e.spool.clone();
                // A file that an earlier fill could not keep (the engine
                // of that fill was not the installed one) is kept now.
                let promoted = match e.kept {
                    None => self.keep_file(&key, &slot, spool.path(), e.bytes),
                    Some(_) => None,
                };
                if promoted.is_some() {
                    e.kept = promoted;
                    drop(entry);
                    self.trim_kept(Some(&key));
                }
                return Ok(spool);
            }
        }
        // Expired, damaged or empty: drop the old file before a new fill.
        if let Some(old) = entry.take() {
            old.discard();
        }
        self.fills.fetch_add(1, Ordering::SeqCst);
        let spool = Arc::new(fill().await?);
        let created = Instant::now();
        let bytes = std::fs::metadata(spool.path()).map_or(0, |m| m.len());
        let kept = self.keep_file(&key, &slot, spool.path(), bytes);
        let timed = kept.is_none();
        *entry = Some(Entry {
            spool: spool.clone(),
            created,
            used: created,
            bytes,
            kept,
        });
        drop(entry);
        if timed {
            self.schedule_expiry(key.clone(), created);
        } else {
            self.trim_kept(Some(&key));
        }
        // Over the bound: evict the oldest other entries.
        while self.timed_len() > self.max_entries {
            let Some(victim) = self.take_victim(Some(&key), Class::Timed) else {
                break;
            };
            victim.discard();
        }
        Ok(spool)
    }

    /// Keep the verified file `file` of `key` (`bytes` long, in `slot`) when
    /// [`Self::keeps`] says so: give it its kept name. A purge or a delete
    /// that came while the name was made takes the name back, so no name
    /// stays that a purge did not see.
    fn keep_file(&self, key: &RangeSpoolKey, slot: &Slot, file: &Path, bytes: u64) -> Option<Kept> {
        let kept = self.keeps(key, slot, bytes)?;
        let link = link_as(&kept.dir.join(key.file_name()), file);
        if self.keeps(key, slot, bytes).is_none() {
            if let Some(link) = &link {
                let _ = std::fs::remove_file(link);
            }
            return None;
        }
        Some(Kept {
            link,
            verified: true,
        })
    }

    /// The kept state when the new file of `key` (`bytes` long, filled in
    /// `slot`) has no TTL: a kept key, a file inside the budget, and no
    /// purge and no delete since the key was made.
    fn keeps(&self, key: &RangeSpoolKey, slot: &Slot, bytes: u64) -> Option<&KeptState> {
        let kept = self.kept.as_ref()?;
        let tag = key.kept.as_ref()?;
        let max_bytes = kept.max_bytes.load(Ordering::SeqCst);
        if max_bytes == 0 {
            return None;
        }
        if bytes > max_bytes {
            tracing::info!(
                "[reference cache] a reference of {} MiB is above the cache of {} MiB \
                 (DGP_REFERENCE_CACHE_MAX_BYTES): kept for {} s only",
                bytes / (1024 * 1024),
                max_bytes / (1024 * 1024),
                self.ttl.as_secs()
            );
            return None;
        }
        // The settings of the engine that made the key are the installed
        // ones (unknown settings match only an engine with no encryption
        // wrapper), no purge and no delete came since, and the disk of the
        // spool takes writes.
        let installed = kept.policy.lock().as_deref().unwrap_or("") == &*tag.policy;
        let current = installed
            && tag.generation == kept.generation.load(Ordering::SeqCst)
            && !slot.forgotten.load(Ordering::SeqCst)
            && kept.gate.refusal().is_none();
        current.then_some(kept)
    }

    /// Evict the least recently used kept files (not `keep`) while the kept
    /// files hold more than their budget.
    fn trim_kept(&self, keep: Option<&RangeSpoolKey>) {
        let Some(kept) = &self.kept else { return };
        while self.kept_bytes() > kept.max_bytes.load(Ordering::SeqCst) {
            let Some(victim) = self.take_victim(keep, Class::Kept) else {
                break;
            };
            victim.discard();
        }
    }

    /// Bytes of the kept files (a slot that is filling is not counted).
    fn kept_bytes(&self) -> u64 {
        self.slots
            .lock()
            .values()
            .filter_map(|s| {
                let entry = s.entry.try_lock().ok()?;
                entry.as_ref().filter(|e| e.kept.is_some()).map(|e| e.bytes)
            })
            .sum()
    }

    /// Cached entries (filled slots).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots
            .lock()
            .values()
            .filter(|s| s.entry.try_lock().map_or(true, |e| e.is_some()))
            .count()
    }

    /// Entries with a TTL (a slot that is filling counts).
    fn timed_len(&self) -> usize {
        self.slots
            .lock()
            .values()
            .filter(|s| {
                s.entry
                    .try_lock()
                    .map_or(true, |e| e.as_ref().is_some_and(|e| e.kept.is_none()))
            })
            .count()
    }

    /// Drop the entry of `key` at its TTL, unless it was refilled since.
    fn schedule_expiry(&self, key: RangeSpoolKey, created: Instant) {
        let me = self.me.clone();
        let ttl = self.ttl;
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            let Some(cache) = me.upgrade() else { return };
            let slot = cache.slots.lock().get(&key).cloned();
            let Some(slot) = slot else { return };
            let expired = {
                let mut entry = slot.entry.lock().await;
                match entry.as_ref() {
                    Some(e) if e.created == created && e.kept.is_none() => entry.take(),
                    _ => None,
                }
            };
            drop(expired);
            cache.prune_empty();
        });
    }

    /// Forget slots that hold nothing and that no caller uses.
    fn prune_empty(&self) {
        self.slots.lock().retain(|_, slot| {
            Arc::strong_count(slot) > 1 || slot.entry.try_lock().map_or(true, |e| e.is_some())
        });
    }

    /// Take one idle entry of `class` (not `keep`; a slot that is filling is
    /// skipped): the oldest with a TTL, or the least recently used kept
    /// file. The caller discards it outside every lock.
    fn take_victim(&self, keep: Option<&RangeSpoolKey>, class: Class) -> Option<Entry> {
        let slots: Vec<(RangeSpoolKey, Arc<Slot>)> = self
            .slots
            .lock()
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        let mut first: Option<(Instant, Arc<Slot>)> = None;
        for (k, slot) in slots {
            if keep == Some(&k) {
                continue;
            }
            let Ok(entry) = slot.entry.try_lock() else {
                continue;
            };
            let Some(e) = entry.as_ref() else { continue };
            let at = match (class, e.kept.is_some()) {
                (Class::Timed, false) => e.created,
                (Class::Kept, true) => e.used,
                _ => continue,
            };
            if first.as_ref().is_none_or(|(t, _)| at < *t) {
                first = Some((at, slot.clone()));
            }
        }
        let (_, slot) = first?;
        let taken = slot.entry.try_lock().ok()?.take();
        self.prune_empty();
        taken
    }
}

impl SpoolEvictor for RangeSpoolCache {
    /// Entries with a TTL go first, then the kept reference files.
    fn evict_one(&self) -> bool {
        let victim = self
            .take_victim(None, Class::Timed)
            .or_else(|| self.take_victim(None, Class::Kept));
        match victim {
            Some(entry) => {
                entry.discard();
                true
            }
            None => false,
        }
    }
}

/// Delete every kept file of `dir` (not [`POLICY_FILE`]). Returns the count.
/// Also for a start with the cache off: nothing of an earlier run stays.
pub(crate) fn remove_kept_files(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_name() != POLICY_FILE)
        .filter(|e| std::fs::remove_file(e.path()).is_ok())
        .count() as u64
}

/// Pure: is `text` `len` lowercase hex digits?
fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Give `file` the second name `link` (a hard link, no copy). `None`: the
/// file is kept for this process only.
fn link_as(link: &Path, file: &Path) -> Option<PathBuf> {
    // A name that an earlier file left: this verified file replaces it.
    let _ = std::fs::remove_file(link);
    match std::fs::hard_link(file, link) {
        Ok(()) => Some(link.to_path_buf()),
        Err(e) => {
            tracing::debug!("[reference cache] no link {}: {e}", link.display());
            None
        }
    }
}

/// The SHA-256 of the file at `path` (lowercase hex), read on a blocking
/// thread. `None` when the read failed.
async fn sha256_of(path: &Path) -> Option<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        crate::deltaglider::spool::for_each_chunk_blocking(&path, |c| hasher.update(c)).ok()?;
        Some(hex::encode(hasher.finalize()))
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deltaglider::spool::SpoolDir;

    fn key(k: &str) -> RangeSpoolKey {
        RangeSpoolKey::object("b", k.into(), "s".into())
    }

    const MIB: u64 = 1024 * 1024;

    #[tokio::test]
    async fn concurrent_callers_share_one_fill() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = RangeSpoolCache::new(Duration::from_secs(60), MAX_ENTRIES);
        let fill = || async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            sd.acquire(MIB).await
        };
        let got =
            futures::future::join_all((0..8).map(|_| cache.get_or_fill(key("a"), fill))).await;
        assert_eq!(cache.fill_count(), 1, "one decode for eight ranges");
        let first = got[0].as_ref().unwrap().path().to_path_buf();
        assert!(got.iter().all(|s| s.as_ref().unwrap().path() == first));
    }

    #[tokio::test]
    async fn a_failed_fill_is_not_cached_and_ttl_zero_never_caches() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = RangeSpoolCache::new(Duration::from_secs(60), MAX_ENTRIES);
        let failed: Result<_, std::io::Error> = cache
            .get_or_fill(key("a"), || async { Err(std::io::Error::other("bad sha")) })
            .await;
        assert!(failed.is_err());
        cache
            .get_or_fill(key("a"), || sd.acquire(MIB))
            .await
            .unwrap();
        assert_eq!(cache.fill_count(), 2);

        let off = RangeSpoolCache::new(Duration::ZERO, MAX_ENTRIES);
        for _ in 0..3 {
            off.get_or_fill(key("a"), || sd.acquire(MIB)).await.unwrap();
        }
        assert_eq!(off.fill_count(), 3);
        assert_eq!(sd.free_mib(), 63, "only the cached entry holds budget");
    }

    #[tokio::test]
    async fn entries_are_bounded_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = RangeSpoolCache::new(Duration::from_millis(200), 2);
        for k in ["a", "b", "c"] {
            cache.get_or_fill(key(k), || sd.acquire(MIB)).await.unwrap();
        }
        assert_eq!(cache.len(), 2, "the oldest entry is evicted");
        assert_eq!(sd.free_mib(), 62);
        // The TTL drops the rest and releases their budget.
        let deadline = Instant::now() + Duration::from_secs(5);
        while sd.free_mib() < 64 {
            assert!(Instant::now() < deadline, "entries did not expire");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(cache.len(), 0);
    }

    /// A spool acquire that finds the budget short evicts idle cached
    /// reconstructions first, instead of waiting for their TTL.
    #[tokio::test]
    async fn a_short_budget_evicts_cached_spools_first() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 4 * MIB).unwrap();
        let cache = RangeSpoolCache::new(Duration::from_secs(600), MAX_ENTRIES);
        sd.register_evictor(Arc::downgrade(&cache) as Weak<dyn SpoolEvictor>);
        for k in ["a", "b", "c"] {
            cache.get_or_fill(key(k), || sd.acquire(MIB)).await.unwrap();
        }
        assert_eq!(sd.free_mib(), 1);
        let big = tokio::time::timeout(Duration::from_secs(5), sd.acquire(3 * MIB))
            .await
            .expect("waited for budget that cached spools held");
        assert!(big.is_ok());
        assert_eq!(cache.len(), 1, "two entries went, the newest stays");
        // The no-wait path evicts too.
        drop(big);
        assert!(sd.try_acquire(4 * MIB).is_ok());
        assert_eq!(cache.len(), 0);
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(bytes))
    }

    fn mib_of(byte: u8, mib: u64) -> Vec<u8> {
        vec![byte; (mib * MIB) as usize]
    }

    /// A kept-reference store on the `references` directory of `sd`, as a
    /// start of the process makes it.
    fn store(sd: &SpoolDir, ttl: Duration, max_bytes: u64) -> Arc<RangeSpoolCache> {
        RangeSpoolCache::new_store(ttl, sd.dir().join("references"), max_bytes, sd)
    }

    /// The kept key of the reference with content `bytes` (its deltaspace is
    /// named after its first byte).
    fn kept_key(cache: &RangeSpoolCache, bytes: &[u8]) -> RangeSpoolKey {
        RangeSpoolKey::kept_reference(
            "storage",
            &format!("folder-{}", bytes[0]),
            sha(bytes),
            cache.kept_tag(&"".into()).unwrap(),
        )
    }

    /// The kept reference with content `bytes`, written by a fill when the
    /// cache does not hold it.
    async fn reference(cache: &RangeSpoolCache, sd: &SpoolDir, bytes: &[u8]) -> Arc<Spool> {
        cache
            .get_or_fill(kept_key(cache, bytes), || async {
                let file = sd.acquire(bytes.len() as u64).await?;
                std::fs::write(file.path(), bytes)?;
                Ok::<_, std::io::Error>(file)
            })
            .await
            .unwrap()
    }

    /// The kept file names in the directory, sorted.
    fn names(sd: &SpoolDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(sd.dir().join("references"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n != POLICY_FILE)
            .collect();
        names.sort();
        names
    }

    fn names_of(cache: &RangeSpoolCache, contents: &[&[u8]]) -> Vec<String> {
        let mut names: Vec<String> = contents
            .iter()
            .map(|b| kept_key(cache, b).file_name())
            .collect();
        names.sort();
        names
    }

    /// Issue #200: a kept reference has no TTL, and the kept files stay
    /// inside their byte budget, least recently used first.
    #[tokio::test]
    async fn a_kept_reference_has_no_ttl_and_the_least_recently_used_goes_first() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = store(&sd, Duration::from_millis(100), 2 * MIB);
        let (a, b, c) = (mib_of(1, 1), mib_of(2, 1), mib_of(3, 1));
        reference(&cache, &sd, &a).await;
        reference(&cache, &sd, &b).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(cache.len(), 2, "no TTL");
        // A read of `a` makes `b` the least recently used.
        assert!(cache.peek(&kept_key(&cache, &a)).is_some());
        reference(&cache, &sd, &c).await;
        assert_eq!(cache.fill_count(), 3);
        assert_eq!(
            names(&sd),
            names_of(&cache, &[&a, &c]),
            "b went, with its name"
        );
        assert_eq!(sd.free_mib(), 62);
        reference(&cache, &sd, &a).await;
        assert_eq!(cache.fill_count(), 3, "a hit");
    }

    /// A reference above the whole budget of the kept files keeps a TTL.
    #[tokio::test]
    async fn a_reference_above_the_kept_budget_expires() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = store(&sd, Duration::from_millis(100), MIB);
        drop(reference(&cache, &sd, &mib_of(1, 2)).await);
        assert!(names(&sd).is_empty());
        let deadline = Instant::now() + Duration::from_secs(5);
        while sd.free_mib() < 64 {
            assert!(Instant::now() < deadline, "the entry did not expire");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The kept files outlive the process: the next start takes them back
    /// inside its budget, reserves their space, and checks the SHA-256 of
    /// each before its first use. A damaged file is filled again.
    #[tokio::test]
    async fn kept_references_outlive_the_process_and_are_checked_again() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (mib_of(1, 1), mib_of(2, 1), mib_of(3, 1));
        let name_of_b = {
            let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
            let cache = store(&sd, Duration::from_secs(60), 8 * MIB);
            for bytes in [&a, &b, &c] {
                reference(&cache, &sd, bytes).await;
                // Distinct mtimes: the next start keeps the newest.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            kept_key(&cache, &b).file_name()
        };
        let refs = dir.path().join("references");
        assert_eq!(std::fs::read_dir(&refs).unwrap().count(), 3);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "the spool names went with the process"
        );
        // The process ended. One file is damaged, and one is not ours.
        std::fs::write(refs.join(&name_of_b), mib_of(9, 1)).unwrap();
        std::fs::write(refs.join("junk"), b"x").unwrap();

        // The next start has room for two files: the oldest (`a`) goes.
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = store(&sd, Duration::from_secs(60), 2 * MIB);
        assert_eq!(names(&sd), names_of(&cache, &[&b, &c]));
        assert_eq!(sd.free_mib(), 62, "the files hold their space again");
        assert!(
            cache.peek(&kept_key(&cache, &c)).is_none(),
            "not before its check"
        );
        let file = reference(&cache, &sd, &c).await;
        assert_eq!(cache.fill_count(), 0, "c is read from the disk");
        assert!(std::fs::read(file.path()).unwrap() == c);
        assert!(cache.peek(&kept_key(&cache, &c)).is_some());
        let file = reference(&cache, &sd, &b).await;
        assert_eq!(cache.fill_count(), 1, "the damaged file is filled again");
        assert!(std::fs::read(file.path()).unwrap() == b);
        assert!(std::fs::read(refs.join(&name_of_b)).unwrap() == b);
    }

    /// A file that no spool room is left for is deleted at the start: no
    /// kept file stays on the disk without a reservation.
    #[tokio::test]
    async fn a_file_with_no_spool_room_is_deleted_at_the_start() {
        let dir = tempfile::tempdir().unwrap();
        {
            let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
            let cache = store(&sd, Duration::from_secs(60), 8 * MIB);
            reference(&cache, &sd, &mib_of(1, 2)).await;
        }
        let sd = SpoolDir::new(dir.path().to_path_buf(), 4 * MIB).unwrap();
        let _held = sd.try_acquire(3 * MIB).unwrap();
        let cache = store(&sd, Duration::from_secs(60), 8 * MIB);
        assert_eq!(cache.len(), 0);
        assert!(names(&sd).is_empty());
    }

    /// A delete or a rewrite of a reference drops its kept file, and a fill
    /// that it overtakes keeps nothing.
    #[tokio::test]
    async fn a_forgotten_deltaspace_keeps_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = store(&sd, Duration::from_secs(60), 8 * MIB);
        let (a, b) = (mib_of(1, 1), mib_of(2, 1));
        reference(&cache, &sd, &a).await;
        reference(&cache, &sd, &b).await;
        cache.forget_deltaspace("storage", "folder-1");
        assert_eq!(names(&sd), names_of(&cache, &[&b]));
        assert_eq!(sd.free_mib(), 63);
        cache.forget_deltaspace("other-storage", "folder-2");
        assert_eq!(cache.len(), 1, "another storage has another reference");

        // The delete comes while the reference downloads.
        let (started, start) = tokio::sync::oneshot::channel();
        let (go, wait) = tokio::sync::oneshot::channel::<()>();
        let fill = cache.get_or_fill(kept_key(&cache, &a), || async {
            let file = sd.acquire(MIB).await?;
            std::fs::write(file.path(), &a)?;
            let _ = started.send(());
            let _ = wait.await;
            Ok::<_, std::io::Error>(file)
        });
        let forget = async {
            start.await.unwrap();
            cache.forget_deltaspace("storage", "folder-1");
            go.send(()).unwrap();
        };
        let (file, ()) = tokio::join!(fill, forget);
        assert!(std::fs::read(file.unwrap().path()).unwrap() == a);
        assert_eq!(names(&sd), names_of(&cache, &[&b]), "not kept");
        // The entry of the overtaken fill goes when its lock is free.
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.len() > 1 {
            assert!(Instant::now() < deadline, "the forgotten entry stayed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The kept reference with content `bytes`, asked by an engine whose
    /// encryption settings are `policy`.
    async fn reference_under(
        cache: &RangeSpoolCache,
        sd: &SpoolDir,
        bytes: &[u8],
        policy: &str,
    ) -> Arc<Spool> {
        let key = RangeSpoolKey::kept_reference(
            "storage",
            &format!("folder-{}", bytes[0]),
            sha(bytes),
            cache.kept_tag(&policy.into()).unwrap(),
        );
        cache
            .get_or_fill(key, || async {
                let file = sd.acquire(bytes.len() as u64).await?;
                std::fs::write(file.path(), bytes)?;
                Ok::<_, std::io::Error>(file)
            })
            .await
            .unwrap()
    }

    /// The kept files belong to the encryption settings of the installed
    /// engine: other settings drop them all, also across a restart, and an
    /// engine with other settings (one that an apply replaced, or one not
    /// installed yet) keeps nothing.
    #[tokio::test]
    async fn a_change_of_the_encryption_settings_drops_every_kept_file() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (mib_of(1, 1), mib_of(2, 1));
        {
            let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
            let cache = store(&sd, Duration::from_secs(60), 8 * MIB);
            // Settings that no engine installed: only an engine with no
            // encryption wrapper (the empty text) keeps.
            reference_under(&cache, &sd, &b, "plain").await;
            assert!(names(&sd).is_empty(), "not the installed settings");
            assert!(cache.set_policy("plain"), "the first settings");
            reference_under(&cache, &sd, &a, "plain").await;
            assert!(
                !cache.set_policy("plain"),
                "the same settings change nothing"
            );
            assert_eq!(names(&sd).len(), 1);
            // The apply: `b` encrypts now.
            assert!(cache.set_policy("b encrypts"));
            assert!(names(&sd).is_empty());
            // The engine that the apply replaced reads `b`: nothing is kept.
            cache.forget_deltaspace("storage", "folder-2");
            reference_under(&cache, &sd, &b, "plain").await;
            assert!(names(&sd).is_empty(), "made under the old settings");
            reference_under(&cache, &sd, &a, "b encrypts").await;
            assert_eq!(names(&sd).len(), 1);
        }
        // The next start under the same settings takes the file back; under
        // other settings (changed while the proxy was down) it drops it.
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = store(&sd, Duration::from_secs(60), 8 * MIB);
        assert!(!cache.set_policy("b encrypts"));
        assert_eq!(names(&sd).len(), 1);
        drop(cache);
        let cache = store(&sd, Duration::from_secs(60), 8 * MIB);
        assert!(cache.set_policy("a encrypts too"));
        assert!(names(&sd).is_empty());
        assert_eq!(cache.len(), 0);
        assert_eq!(sd.free_mib(), 64);
    }

    /// A key made before a purge keeps nothing, also when the settings are
    /// the same again (an apply and its rollback).
    #[tokio::test]
    async fn a_key_made_before_a_purge_keeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = store(&sd, Duration::from_secs(60), 8 * MIB);
        let a = mib_of(1, 1);
        let before = kept_key(&cache, &a);
        assert!(cache.set_policy("b encrypts"));
        assert!(cache.set_policy(""));
        cache
            .get_or_fill(before, || async {
                let file = sd.acquire(MIB).await?;
                std::fs::write(file.path(), &a)?;
                Ok::<_, std::io::Error>(file)
            })
            .await
            .unwrap();
        assert!(names(&sd).is_empty());
        // A key made now promotes the file that is there.
        reference(&cache, &sd, &a).await;
        assert_eq!(names(&sd).len(), 1);
        assert_eq!(cache.fill_count(), 1);
    }

    /// While the disk of the spool refuses writes, nothing new is kept.
    #[tokio::test]
    async fn nothing_is_kept_while_the_disk_level_refuses_writes() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = store(&sd, Duration::from_secs(60), 8 * MIB);
        sd.gate()
            .set(crate::disk_watch::Level::High, "the disk is full".into());
        reference(&cache, &sd, &mib_of(1, 1)).await;
        assert!(names(&sd).is_empty());
        sd.gate().set(crate::disk_watch::Level::Ok, String::new());
        reference(&cache, &sd, &mib_of(2, 1)).await;
        assert_eq!(names(&sd).len(), 1);
    }

    /// An engine with a budget of 0 is installed while an engine still
    /// uses the store (an apply that turns the cache off): what was kept
    /// goes, and the engine that drains keeps nothing more, also after a
    /// later engine opened the store again with other settings.
    #[tokio::test]
    async fn an_installed_budget_of_zero_closes_a_live_store() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let refs = dir.path().join("references");
        let live = RangeSpoolCache::kept_store(refs.clone(), 8 * MIB, &sd).unwrap();
        let old = RangeSpoolCache::for_engine(Duration::from_secs(60), 4, Some(live.clone()));
        let a = mib_of(1, 1);
        reference(&old, &sd, &a).await;
        assert_eq!(names(&sd).len(), 1);
        // The build of the next engine changes nothing (its apply may fail).
        let same = RangeSpoolCache::kept_store(refs.clone(), 0, &sd).unwrap();
        assert!(Arc::ptr_eq(&live, &same));
        assert_eq!(names(&sd).len(), 1);
        // Its install does.
        RangeSpoolCache::install_kept(&refs, 0, "");
        assert!(names(&sd).is_empty());
        assert_eq!(sd.free_mib(), 64);
        old.forget_deltaspace("storage", "folder-1");
        reference(&old, &sd, &a).await;
        assert!(names(&sd).is_empty(), "closed");
        // The cache is on again, with a key on the backend: the old engine
        // (settings "") still keeps nothing.
        RangeSpoolCache::install_kept(&refs, 8 * MIB, "keyed");
        old.forget_deltaspace("storage", "folder-1");
        reference(&old, &sd, &a).await;
        assert!(names(&sd).is_empty(), "not the installed settings");
        reference_under(&old, &sd, &a, "keyed").await;
        assert_eq!(names(&sd).len(), 1);
        // No live store and no budget: the files of an earlier run go.
        drop((old, live, same));
        RangeSpoolCache::install_kept(&refs, 0, "");
        assert!(names(&sd).is_empty());
    }

    /// Another process that starts on the same spool directory sweeps old
    /// names from it. The kept name gives the file its spool name back.
    #[tokio::test]
    async fn a_swept_spool_name_comes_back_from_the_kept_name() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = store(&sd, Duration::from_secs(60), 8 * MIB);
        let a = mib_of(1, 1);
        let path = reference(&cache, &sd, &a).await.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();
        let file = cache.peek(&kept_key(&cache, &a)).expect("still a hit");
        assert!(std::fs::read(file.path()).unwrap() == a);
        std::fs::remove_file(&path).unwrap();
        let file = reference(&cache, &sd, &a).await;
        assert!(std::fs::read(file.path()).unwrap() == a);
        assert_eq!(cache.fill_count(), 1, "no second download");
    }

    /// One store per directory: the engine of a config apply gets the store
    /// of the engine it replaces, with the new budget. The kept keys of an
    /// engine's cache go to the store; a budget of 0 makes no store.
    #[tokio::test]
    async fn engines_on_one_directory_share_one_kept_store() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let refs = dir.path().join("references");
        assert!(RangeSpoolCache::kept_store(refs.clone(), 0, &sd).is_none());
        assert!(!refs.exists());
        let first = RangeSpoolCache::kept_store(refs.clone(), 8 * MIB, &sd).unwrap();
        RangeSpoolCache::install_kept(&refs, 8 * MIB, "");
        let old = RangeSpoolCache::for_engine(Duration::from_secs(60), 4, Some(first.clone()));
        let (a, b) = (mib_of(1, 1), mib_of(2, 1));
        reference(&old, &sd, &a).await;
        reference(&old, &sd, &b).await;
        assert_eq!((old.len(), first.len()), (0, 2));
        // The apply: a smaller budget, the same store. The budget takes
        // effect when the engine is installed.
        let second = RangeSpoolCache::kept_store(refs.clone(), MIB, &sd).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.len(), 2, "the build changes nothing");
        RangeSpoolCache::install_kept(&refs, MIB, "");
        assert_eq!(first.len(), 1, "trimmed to the new budget");
        let new = RangeSpoolCache::for_engine(Duration::from_secs(60), 4, Some(second));
        reference(&new, &sd, &b).await;
        assert_eq!(
            new.fill_count(),
            2,
            "the new engine reads the old engine's file"
        );
        // An engine with the cache off keeps nothing.
        let off = RangeSpoolCache::for_engine(Duration::ZERO, 4, Some(first));
        assert!(off.kept_tag(&"".into()).is_none());
    }

    #[test]
    fn a_kept_file_name_stands_for_its_key() {
        let tag = || KeptTag {
            generation: 7,
            policy: "p".into(),
        };
        let key = RangeSpoolKey::kept_reference("backend\0bucket", "a/b", "c3".repeat(32), tag());
        let name = key.file_name();
        assert_eq!(RangeSpoolKey::of_file_name(&name), Some(key));
        assert!(!name.contains("bucket") && !name.contains('/'));
        let other = RangeSpoolKey::kept_reference("backend\0other", "a/b", "c3".repeat(32), tag());
        assert_ne!(other.file_name(), name, "another storage, another file");
        for bad in [
            "",
            "junk",
            POLICY_FILE,
            "../x",
            &"a".repeat(64),
            &name.to_uppercase(),
        ] {
            assert_eq!(RangeSpoolKey::of_file_name(bad), None, "{bad}");
        }
    }
}

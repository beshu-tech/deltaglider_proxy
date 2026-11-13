// SPDX-License-Identifier: BUSL-1.1

//! Per-bucket running usage counter — the O(1) "how big is this bucket".
//!
//! S3 has no protocol call for bucket size; the only primitive is an O(n)
//! `ListObjectsV2` sweep (see `status.rs::compute_stats`, capped at 1000
//! objects and therefore wrong for big buckets). Ceph/B2 show a precise
//! number instantly because the backend keeps a *running counter* updated on
//! every write/delete. DGP is the only layer that sees every mutation, so it
//! keeps the same counter here — and uniquely in LOGICAL (pre-delta) bytes,
//! which a backend counter can't report.
//!
//! Maintained inline at the engine `store()`/`delete()` choke point. An
//! explicit Refresh overwrites a bucket's row with a full-scan ground truth.
//!
//! ## Why its own DB file
//!
//! The encrypted config DB (`deltaglider_config.db`) is synced across
//! instances as a whole-file compare-and-swap blob (`config_db_sync.rs`).
//! A counter that increments concurrently on two instances does NOT compose
//! under whole-file last-writer-wins — it would corrupt or clobber IAM. So
//! the counter lives in a SEPARATE, never-synced file
//! (`deltaglider_usage.db`): per-instance, approximate across a fleet, and
//! reconciled by Refresh. No secrets here (just counts) → plain SQLite, no
//! SQLCipher, opens unconditionally even in open-mode dev.

use crate::deltaglider::savings::SavingsTotals;
use crate::types::{FileMetadata, StorageInfo};
use dashmap::DashMap;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

/// SCHEMA_VERSION for the usage DB — independent of the config DB's version.
const SCHEMA_VERSION: i32 = 1;

/// Path to the usage DB — beside the config DB (same dir-derivation rule).
pub fn bucket_usage_db_path() -> PathBuf {
    let db_dir = crate::config::process_env("DGP_CONFIG")
        .and_then(|p| Path::new(&p).parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    db_dir.join("deltaglider_usage.db")
}

/// One bucket's running totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketUsageRow {
    pub object_count: u64,
    pub logical_bytes: u64,
    pub stored_bytes: u64,
    /// Unix secs of the last full scan during which the bucket did not
    /// change (the counter is then exact, and the quota gate trusts it).
    /// `None`: never scanned, the last scan was busy, or a write since then
    /// changed the bucket by an unknown amount.
    pub last_scan_at: Option<i64>,
}

/// Savings % from a logical/stored pair — the ONE clamp (0..=99.99, "100%"
/// never reaches the UI). Mirrors `SavingsTotals::savings_percentage`; `None`
/// when nothing is measurable. Used by both the per-bucket counter endpoint and
/// the aggregate `/_/stats`.
pub fn savings_pct(logical_bytes: u64, stored_bytes: u64) -> Option<f64> {
    if logical_bytes == 0 {
        return None;
    }
    Some(((1.0 - stored_bytes as f64 / logical_bytes as f64) * 100.0).clamp(0.0, 99.99))
}

impl BucketUsageRow {
    /// This row's savings % (see [`savings_pct`]).
    pub fn savings_pct(&self) -> Option<f64> {
        savings_pct(self.logical_bytes, self.stored_bytes)
    }
}

/// The (count, logical, stored) delta a single object contributes, signed.
///
/// Mirrors [`SavingsTotals::accumulate`] EXACTLY so the inline counter and the
/// Refresh scan can never diverge by interpretation: a Reference is on-disk
/// bytes only (not user-visible → no count, no logical); a Delta stores its
/// `delta_size`; a Passthrough stores its `file_size`. `sign` is +1 on create,
/// -1 on delete.
pub fn usage_delta_for(meta: &FileMetadata, sign: i8) -> (i64, i64, i64) {
    let s = sign as i64;
    match &meta.storage_info {
        // reference.bin is internal: stored bytes only, never counted/logical.
        StorageInfo::Reference { .. } => (0, 0, s * meta.file_size as i64),
        StorageInfo::Delta { delta_size, .. } => {
            (s, s * meta.file_size as i64, s * *delta_size as i64)
        }
        StorageInfo::Passthrough => (s, s * meta.file_size as i64, s * meta.file_size as i64),
    }
}

/// The per-instance usage counter DB.
///
/// Writes never touch SQLite from the caller's thread. Each `apply_*` folds
/// its delta into a sharded in-process pending map (`DashMap` — per-bucket
/// shard lock, never a global lock), so the S3 PUT/DELETE path cannot block
/// on the DB (#85). A background flush (`spawn_periodic_blocking` in
/// `main.rs`) drains the pending map into SQLite; `overwrite_from_scan`
/// and reads synchronise against pending explicitly:
///
/// - `read`/`read_all` merge pending deltas OVER the stored row, so a read
///   is exact even before a flush.
/// - `overwrite_from_scan` (Refresh) replaces the row with the scan totals
///   plus the deltas journaled since `begin_scan`: stale deltas from before
///   the scan are superseded, writes during the scan are kept.
///
/// Durability is unchanged from the old write-through shape: this DB is a
/// best-effort derived counter (WAL + `synchronous=NORMAL`, failures are
/// warn-and-continue, drift is repairable by Refresh). A crash now loses at
/// most the un-flushed pending map — the same class of loss a power cut
/// already caused, just with a slightly larger window.
pub struct BucketUsage {
    conn: Mutex<Connection>,
    /// Un-flushed net deltas keyed by bucket, plus the journal of each
    /// Refresh scan that runs on the bucket.
    pending: DashMap<String, Pending>,
    /// Source of [`ScanTicket`] ids.
    next_scan: AtomicU64,
    /// Woken when a [`WriteWindow`] ends ([`Self::wait_windows_closed`]).
    window_closed: tokio::sync::Notify,
}

/// A signed (count, logical, stored) delta — the triple `apply_delta` takes.
type Triple = (i64, i64, i64);

fn add(t: &mut Triple, (c, l, s): Triple) {
    t.0 += c;
    t.1 += l;
    t.2 += s;
}

/// One bucket's entry in the pending map.
#[derive(Default)]
struct Pending {
    /// Not yet flushed to SQLite.
    delta: Triple,
    /// A write with an unknown net change skipped its delta: the row needs
    /// a Refresh. Not yet flushed (the flush clears `last_scan_at`).
    recount: bool,
    /// Per running scan (ticket id): every change since that scan started,
    /// flushed or not. One shard lock guards all fields, so a change can
    /// never reach one and miss the other.
    scans: Vec<(u64, Journal)>,
    /// Writes of this bucket between their first storage write and the
    /// record of their change ([`WriteWindow`]). In the same entry as the
    /// delta and the journals, so a scan reads it in the same critical
    /// section that cuts them.
    windows: u32,
}

impl Pending {
    /// Nothing in the entry: it can go.
    fn is_idle(&self) -> bool {
        self.scans.is_empty() && self.delta == (0, 0, 0) && !self.recount && self.windows == 0
    }
}

/// The changes of a bucket since one Refresh scan started.
#[derive(Debug, Default, Clone, Copy)]
struct Journal {
    delta: Triple,
    /// A write during the scan skipped its delta: the scan result cannot
    /// be stamped as a full count.
    recount: bool,
}

/// A Refresh scan in flight, from [`BucketUsage::begin_scan`]. Consumed by
/// [`BucketUsage::overwrite_from_scan`]; a dropped ticket (failed scan)
/// removes its journal.
pub struct ScanTicket<'a> {
    db: &'a BucketUsage,
    bucket: String,
    id: u64,
    consumed: bool,
}

/// THE rule for a scan result to be stamped as the trusted counter
/// (`last_scan_at`): the bucket did not change while the scan ran (the
/// journal is empty), AND no write sits between its storage write and the
/// record of its change when the scan ends (`writes_in_window`).
///
/// A write stores its object first and records its change a moment later.
/// A scan that lists in between sees the object, finds an empty journal, and
/// would be stamped; the writer's change then lands on top and the object
/// is counted twice (or, for a delete, the change lands after a scan that
/// no longer lists the object, and it is counted too few), with no later
/// scan to repair it.
///
/// Only that window matters. The window is the whole hold of the object
/// write lock or the deltaspace lock (a copy holds the lock of its
/// destination while it reads its source). A client write that is still
/// receiving its body holds no lock and has stored nothing, and a write
/// that has recorded its change is in the pending delta or the journal;
/// neither blocks a stamp. A write that
/// entered and left the window during the scan recorded its change during
/// the scan, so the journal has it. The count is read in the same critical
/// section that cuts the journal ([`BucketUsage::cut_scan`]), so a write
/// cannot record between the two.
pub(crate) fn scan_is_quiet(journal_is_empty: bool, writes_in_window: u32) -> bool {
    journal_is_empty && writes_in_window == 0
}

/// A write between its first storage write and the record of its change.
/// Made by [`BucketUsage::open_write_window`] BEFORE the storage write (the
/// object is visible to a listing from the write on), and dropped after the
/// record. The engine holds one inside every object write lock and every
/// deltaspace lock, which is where every storage write and its record sit.
/// It spans the whole hold of the lock, so it is wider than the span from
/// the storage write to the record: a copy holds its destination lock while
/// it reads its source, and counts for that time too.
#[must_use = "the window ends when the guard drops"]
pub struct WriteWindow {
    /// `None`: a bucket the counter skips.
    open: Option<(Arc<BucketUsage>, String)>,
}

impl Drop for WriteWindow {
    fn drop(&mut self) {
        let Some((db, bucket)) = self.open.take() else {
            return;
        };
        if let Some(mut p) = db.pending.get_mut(&bucket) {
            p.windows = p.windows.saturating_sub(1);
        }
        db.pending.remove_if(&bucket, |_, p| p.is_idle());
        db.window_closed.notify_waiters();
    }
}

/// A test hook at the two ends of the cut of a scan.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CutStep {
    BeforeCut,
    AfterCut,
}

#[cfg(test)]
thread_local! {
    /// Counter changes recorded on this thread with no write in a window.
    /// The engine tests assert that it stays 0 across the write paths
    /// (`every_counter_change_of_the_engine_is_recorded_in_a_window`).
    pub(crate) static UNWINDOWED_CHANGES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_unwindowed(windows: u32) {
    if windows == 0 {
        UNWINDOWED_CHANGES.with(|c| c.set(c.get() + 1));
    }
}

#[cfg(test)]
type CutHook = Box<dyn FnMut(CutStep)>;

#[cfg(test)]
thread_local! {
    static CUT_HOOK: std::cell::RefCell<Option<CutHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn cut_hook(step: CutStep) {
    // Taken out while it runs: a hook that scans again does not re-enter.
    if let Some(mut hook) = CUT_HOOK.with(|h| h.borrow_mut().take()) {
        hook(step);
        CUT_HOOK.with(|h| *h.borrow_mut() = Some(hook));
    }
}

impl Drop for ScanTicket<'_> {
    fn drop(&mut self) {
        if !self.consumed {
            self.db.end_scan(&self.bucket, self.id);
        }
    }
}

impl BucketUsage {
    /// Open (creating if absent) the usage DB at `path` and run migrations.
    ///
    /// PERF: every object PUT/DELETE applies a counter delta here, so this DB is
    /// on the hot S3 write path. With SQLite's defaults (`journal_mode=DELETE`,
    /// `synchronous=FULL`) each of those is a separate fsync'd commit — measured
    /// at ~270ms per object on a loaded host, which made a 1100-object prefix
    /// sweep take over 300s and trip the request timeout. WAL + `synchronous
    /// = NORMAL` removes the per-write fsync.
    ///
    /// This is the right durability trade for THIS DB specifically: it is a
    /// best-effort derived counter, not a source of truth. Every call site
    /// already treats a failure as non-fatal (warn-and-continue), and drift is
    /// repairable on demand by the usage scanner's Refresh. The worst case on a
    /// power cut is a few lost counter deltas that a Refresh reconciles — which
    /// is exactly what the existing drift-reconciliation path exists to fix.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = crate::sqlite_open::open(path)?;
        // Best-effort: a backend that refuses WAL (rare, e.g. some network FS)
        // still works correctly, just slower — never fail startup over a pragma.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        let db = Self {
            conn: Mutex::new(conn),
            pending: DashMap::new(),
            next_scan: AtomicU64::new(0),
            window_closed: tokio::sync::Notify::new(),
        };
        db.migrate()?;
        Ok(db)
    }

    /// In-memory instance for tests.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let conn = crate::sqlite_open::open_in_memory()?;
        let db = Self {
            conn: Mutex::new(conn),
            pending: DashMap::new(),
            next_scan: AtomicU64::new(0),
            window_closed: tokio::sync::Notify::new(),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap_or(0);
        if version < 1 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS bucket_usage (
                    bucket        TEXT PRIMARY KEY,
                    object_count  INTEGER NOT NULL DEFAULT 0,
                    logical_bytes INTEGER NOT NULL DEFAULT 0,
                    stored_bytes  INTEGER NOT NULL DEFAULT 0,
                    last_scan_at  INTEGER
                );",
            )?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// Apply a signed delta to a bucket's counters (upsert: creates the row on
    /// first flush). Counters are stored signed and clamped at 0 on read so a
    /// transient out-of-order delta can never surface a negative size.
    ///
    /// PERF (#85): folds into the pending map — a per-bucket shard lock, never
    /// a global mutex and never SQLite — so the S3 PUT/DELETE path cannot
    /// block. The background flush persists it (see `flush_pending`).
    pub fn apply_delta(&self, bucket: &str, d_count: i64, d_logical: i64, d_stored: i64) {
        if d_count == 0 && d_logical == 0 && d_stored == 0 {
            return;
        }
        let d = (d_count, d_logical, d_stored);
        let mut e = self.pending.entry(bucket.to_string()).or_default();
        #[cfg(test)]
        note_unwindowed(e.windows);
        add(&mut e.delta, d);
        for (_, journal) in e.scans.iter_mut() {
            add(&mut journal.delta, d);
        }
    }

    /// Mark a bucket's row for a Refresh: a write changed the bucket by an
    /// unknown amount (its prior-object read failed) and recorded no delta.
    /// The flush clears `last_scan_at`, so the row reads as not scanned: the
    /// UI asks for a Refresh and the quota gate stops trusting the row.
    /// Like `apply_delta`, never touches SQLite.
    pub fn mark_for_recount(&self, bucket: &str) {
        if is_transient_bucket(bucket) {
            return;
        }
        let mut e = self.pending.entry(bucket.to_string()).or_default();
        #[cfg(test)]
        note_unwindowed(e.windows);
        e.recount = true;
        for (_, journal) in e.scans.iter_mut() {
            journal.recount = true;
        }
    }

    /// Apply one object's create (+1) or delete (-1) via [`usage_delta_for`].
    pub fn apply_object(&self, bucket: &str, meta: &FileMetadata, sign: i8) {
        let (dc, dl, ds) = usage_delta_for(meta, sign);
        self.apply_delta(bucket, dc, dl, ds);
    }

    /// Drain the pending map into SQLite. Called by the background flush task
    /// (main.rs) and by [`Self::overwrite_from_scan`] before it replaces a
    /// row — the scan is ground truth, so stale deltas must land first and
    /// then be superseded, not lost or double-applied.
    ///
    /// Each drained bucket is one `INSERT ... ON CONFLICT DO UPDATE` under the
    /// connection mutex — the same statement the write-through path used, so
    /// the negative-detection WARN fires exactly as before. Failures are
    /// warn-and-drop per the best-effort contract... EXCEPT here the delta is
    /// re-queued (pushed back into pending) so a transient SQLite error does
    /// not silently lose counts a caller believed applied.
    pub fn flush_pending(&self) {
        // Hold the connection lock ACROSS the drain and the upserts. A
        // concurrent `read` takes the same lock first, so it can never
        // observe the interval where a delta has left `pending` but has not
        // yet landed in SQLite. Lock order is conn → pending everywhere
        // (`read`, `read_all`, this), so there is no inverse to deadlock on.
        let conn = self.conn.lock().unwrap();
        // Two-phase drain: a DashMap iterator holds each shard's READ lock, so
        // calling remove() on the same key inside the iteration deadlocks.
        // Collect keys first (locks released after the collect), then remove
        // each — a delta folded in between phases is simply flushed early,
        // which is harmless (flush is only the point deltas become durable).
        let keys: Vec<String> = self.pending.iter().map(|e| e.key().clone()).collect();
        let drained: Vec<(String, Triple, bool)> = keys
            .into_iter()
            .filter_map(|k| self.take_delta(&k).map(|(d, recount)| (k, d, recount)))
            .filter(|(_, d, recount)| *d != (0, 0, 0) || *recount)
            .collect();
        if drained.is_empty() {
            return;
        }
        let drained_len = drained.len();
        let mut flushed_ok = 0usize;
        for (bucket, delta, recount) in drained {
            match Self::flush_one(&conn, &bucket, delta, recount) {
                Ok(()) => flushed_ok += 1,
                Err(e) => {
                    // Re-queue: the caller's apply_delta already "succeeded"
                    // in-memory; dropping here would silently lose it.
                    warn!(
                        "bucket_usage: flush failed for '{}': {} (re-queued)",
                        bucket, e
                    );
                    // Not through apply_delta: the scan journals hold
                    // this delta already.
                    let mut e = self.pending.entry(bucket).or_default();
                    add(&mut e.delta, delta);
                    e.recount |= recount;
                }
            }
        }
        debug!(
            "bucket_usage: flush drained {} bucket delta(s), {} re-queued on failure, {} still pending",
            drained_len, drained_len - flushed_ok, self.pending.len()
        );
    }

    /// One drained bucket: the recount mark, then the delta upsert (with
    /// the negative-detection WARN). In this order a failure re-queues
    /// nothing that landed: the mark is idempotent, and the upsert is last.
    fn flush_one(
        conn: &Connection,
        bucket: &str,
        (d_count, d_logical, d_stored): Triple,
        recount: bool,
    ) -> Result<(), rusqlite::Error> {
        if recount {
            conn.execute(
                "UPDATE bucket_usage SET last_scan_at = NULL WHERE bucket = ?1",
                params![bucket],
            )?;
        }
        if (d_count, d_logical, d_stored) != (0, 0, 0) {
            let (oc, lb, sb) = conn.query_row(
                "INSERT INTO bucket_usage (bucket, object_count, logical_bytes, stored_bytes)
                     VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(bucket) DO UPDATE SET
                     object_count  = object_count  + excluded.object_count,
                     logical_bytes = logical_bytes + excluded.logical_bytes,
                     stored_bytes  = stored_bytes  + excluded.stored_bytes
                 RETURNING object_count, logical_bytes, stored_bytes",
                params![bucket, d_count, d_logical, d_stored],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )?;
            // A negative column is ALWAYS a real accounting bug (a
            // missed-count upstream), never a steady state — warn
            // loudly. The read path still clamps at 0 for display;
            // this surfaces the drift instead of hiding it.
            if oc < 0 || lb < 0 || sb < 0 {
                warn!(
                    "bucket_usage: counter for '{}' went negative (count={}, logical={}, stored={}) — \
                     an upstream mutation was miscounted; run Refresh to reconcile",
                    bucket, oc, lb, sb
                );
            }
        }
        Ok(())
    }

    /// Pending (un-flushed) net delta and recount mark for one bucket.
    fn pending_for(&self, bucket: &str) -> (Triple, bool) {
        self.pending
            .get(bucket)
            .map(|e| (e.delta, e.recount))
            .unwrap_or(((0, 0, 0), false))
    }

    /// Move one bucket's un-flushed delta and recount mark out; the entry
    /// stays while a scan journal needs it.
    fn take_delta(&self, bucket: &str) -> Option<(Triple, bool)> {
        let removed = self
            .pending
            .remove_if(bucket, |_, p| p.scans.is_empty() && p.windows == 0)
            .map(|(_, p)| (p.delta, p.recount));
        removed.or_else(|| {
            self.pending
                .get_mut(bucket)
                .map(|mut p| (std::mem::take(&mut p.delta), std::mem::take(&mut p.recount)))
        })
    }

    /// Remove one scan's journal and return it; drop the entry when nothing
    /// is left in it.
    fn end_scan(&self, bucket: &str, id: u64) -> Option<Journal> {
        let journal = {
            let mut p = self.pending.get_mut(bucket)?;
            let i = p.scans.iter().position(|(sid, _)| *sid == id)?;
            p.scans.swap_remove(i).1
        };
        self.pending.remove_if(bucket, |_, p| p.is_idle());
        Some(journal)
    }

    /// Start a Refresh scan of `bucket`: from now on every delta of the
    /// bucket is also journaled for this scan, so
    /// [`Self::overwrite_from_scan`] can keep the writes that land while
    /// the scan runs.
    pub fn begin_scan(&self, bucket: &str) -> ScanTicket<'_> {
        let id = self.next_scan.fetch_add(1, Ordering::Relaxed);
        self.pending
            .entry(bucket.to_string())
            .or_default()
            .scans
            .push((id, Journal::default()));
        ScanTicket {
            db: self,
            bucket: bucket.to_string(),
            id,
            consumed: false,
        }
    }

    /// The writes of `bucket` in their window now.
    pub(crate) fn writes_in_window(&self, bucket: &str) -> u32 {
        self.pending.get(bucket).map_or(0, |p| p.windows)
    }

    /// Wait until no write of `bucket` is in its window, at most `max`.
    /// A scan that ended inside a window is repeated after this, so that
    /// the repeat does not meet the same write.
    pub(crate) async fn wait_windows_closed(&self, bucket: &str, max: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + max;
        loop {
            let closed = self.window_closed.notified();
            tokio::pin!(closed);
            // Armed before the check: a window that ends in between wakes us.
            closed.as_mut().enable();
            if self.writes_in_window(bucket) == 0 {
                return;
            }
            if tokio::time::timeout_at(deadline, closed).await.is_err() {
                return;
            }
        }
    }

    /// A write of `bucket` is about to store an object: open its
    /// [`WriteWindow`]. Call it BEFORE the first storage write; drop the
    /// guard after the record of the change.
    pub fn open_write_window(self: &Arc<Self>, bucket: &str) -> WriteWindow {
        if is_transient_bucket(bucket) {
            return WriteWindow { open: None };
        }
        self.pending.entry(bucket.to_string()).or_default().windows += 1;
        WriteWindow {
            open: Some((Arc::clone(self), bucket.to_string())),
        }
    }

    /// End a scan, in ONE critical section of the bucket's entry: cut its
    /// journal, read the writes in their window, and (unless the scan is
    /// not going to replace the row) take the un-flushed delta. Returns
    /// the journal and whether the scan is quiet ([`scan_is_quiet`]).
    ///
    /// One section, because a writer's `apply_delta` takes this same lock,
    /// so a write records its change either wholly before the section or
    /// wholly after it:
    /// - before: its change is in the journal or in the delta that the
    ///   replace supersedes;
    /// - after: its change lands on the new row, and the write is still in
    ///   its window while the section reads it (the window ends after the
    ///   record), so a write that the scan may have listed makes the scan
    ///   not quiet.
    ///
    /// Read in two steps, a write that began after the listing could record
    /// between them, and its change would be dropped with the old delta.
    fn cut_scan(&self, bucket: &str, id: u64, quiet_only: bool) -> (Journal, bool) {
        let (journal, quiet) = match self.pending.get_mut(bucket) {
            None => (Journal::default(), true),
            Some(mut p) => {
                let journal = p
                    .scans
                    .iter()
                    .position(|(sid, _)| *sid == id)
                    .map(|i| p.scans.swap_remove(i).1)
                    .unwrap_or_default();
                let quiet =
                    scan_is_quiet(journal.delta == (0, 0, 0) && !journal.recount, p.windows);
                // A scan that will not replace the row keeps the delta.
                if !(quiet_only && !quiet) {
                    p.delta = (0, 0, 0);
                    p.recount = false;
                }
                (journal, quiet)
            }
        };
        self.pending.remove_if(bucket, |_, p| p.is_idle());
        (journal, quiet)
    }

    /// Read one bucket's counters (clamped at 0), merging any un-flushed
    /// pending delta. `None` only when the bucket has neither a stored row
    /// NOR pending activity.
    ///
    /// Holds the connection lock across the pending read: `flush_pending`
    /// takes the same lock before draining, so this ordering (conn → pending)
    /// makes it impossible to read a stored row that predates a flush and
    /// then miss the delta that flush just moved out of `pending` (#85
    /// review). No site takes `pending → conn`, so there is no deadlock.
    pub fn read(&self, bucket: &str) -> Result<Option<BucketUsageRow>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let stored = conn
            .query_row(
                "SELECT object_count, logical_bytes, stored_bytes, last_scan_at
                   FROM bucket_usage WHERE bucket = ?1",
                params![bucket],
                |r| Self::map_row_at(r, 0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let ((dc, dl, ds), recount) = self.pending_for(bucket);
        drop(conn);
        let Some(mut row) = stored else {
            if dc == 0 && dl == 0 && ds == 0 {
                return Ok(None);
            }
            return Ok(Some(BucketUsageRow {
                object_count: dc.max(0) as u64,
                logical_bytes: dl.max(0) as u64,
                stored_bytes: ds.max(0) as u64,
                last_scan_at: None,
            }));
        };
        // Saturating adds: the pending delta belongs to the stored row.
        row.object_count = row.object_count.saturating_add_signed(dc);
        row.logical_bytes = row.logical_bytes.saturating_add_signed(dl);
        row.stored_bytes = row.stored_bytes.saturating_add_signed(ds);
        if recount {
            row.last_scan_at = None;
        }
        Ok(Some(row))
    }

    /// Read every bucket's counters (for the aggregate `/_/stats`), merging
    /// un-flushed pending deltas over the stored rows. Holds the connection
    /// across the pending read for the same reason as [`Self::read`].
    pub fn read_all(&self) -> Result<Vec<(String, BucketUsageRow)>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT bucket, object_count, logical_bytes, stored_bytes, last_scan_at
               FROM bucket_usage",
        )?;
        let mut rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, Self::map_row_at(r, 1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        // Merge pending while still holding `conn` (see `read`).
        for (bucket, row) in rows.iter_mut() {
            let ((dc, dl, ds), recount) = self.pending_for(bucket);
            row.object_count = row.object_count.saturating_add_signed(dc);
            row.logical_bytes = row.logical_bytes.saturating_add_signed(dl);
            row.stored_bytes = row.stored_bytes.saturating_add_signed(ds);
            if recount {
                row.last_scan_at = None;
            }
        }
        // Buckets pending-only (no stored row yet — flush hasn't run): surface
        // them so a freshly-written bucket shows up immediately.
        for e in self.pending.iter() {
            let bucket = e.key();
            if rows.iter().any(|(b, _)| b == bucket) {
                continue;
            }
            let (dc, dl, ds) = e.delta;
            if dc == 0 && dl == 0 && ds == 0 {
                continue;
            }
            rows.push((
                bucket.clone(),
                BucketUsageRow {
                    object_count: dc.max(0) as u64,
                    logical_bytes: dl.max(0) as u64,
                    stored_bytes: ds.max(0) as u64,
                    last_scan_at: None,
                },
            ));
        }
        Ok(rows)
    }

    /// Overwrite a bucket's row with full-scan ground truth, and stamp
    /// `last_scan_at` only when the scan was quiet: no write changed the
    /// bucket while it ran (see below, and [`Self::mark_for_recount`]).
    ///
    /// The row becomes the scan totals PLUS every delta since the scan
    /// started (the ticket's journal): a PUT or DELETE that lands while the
    /// scan runs is kept, also when a periodic flush already moved it into
    /// the row that this REPLACE overwrites. The bucket's un-flushed delta
    /// is dropped: its part from before the scan is superseded, its part
    /// from during the scan is in the journal. A write whose key the scan
    /// passes after the write can count twice; a Refresh repairs it, which
    /// is better than losing writes on every Refresh of a busy bucket. So
    /// a busy scan leaves `last_scan_at` empty: its totals show, but the
    /// quota gate (`quota_used`) and the job progress total
    /// (`trusted_object_count`) do not trust them. Before, a busy Refresh
    /// stamped the row, and its double count held the bucket over its quota
    /// with no further scan.
    pub fn overwrite_from_scan(
        &self,
        ticket: ScanTicket<'_>,
        totals: &SavingsTotals,
        now: i64,
    ) -> Result<(), rusqlite::Error> {
        self.overwrite_from_scan_inner(ticket, totals, now, false)
            .map(|_| ())
    }

    /// `quiet_only`: write nothing (and keep the un-flushed delta) when the
    /// bucket changed while the scan ran. `Ok(false)`: nothing written.
    fn overwrite_from_scan_inner(
        &self,
        mut ticket: ScanTicket<'_>,
        totals: &SavingsTotals,
        now: i64,
        quiet_only: bool,
    ) -> Result<bool, rusqlite::Error> {
        // conn → pending, the lock order of flush and read: no flush can run
        // between the journal take and the REPLACE.
        let conn = self.conn.lock().unwrap();
        let bucket = std::mem::take(&mut ticket.bucket);
        #[cfg(test)]
        cut_hook(CutStep::BeforeCut);
        // Quiet: the bucket did not change while the scan ran, and no write
        // sits between its storage write and its record ([`scan_is_quiet`]).
        // Only a quiet scan stamps `last_scan_at`, the mark that the quota
        // gate trusts. The journal, the writes in their window and the
        // un-flushed delta are read in ONE critical section.
        let (journal, quiet) = self.cut_scan(&bucket, ticket.id, quiet_only);
        ticket.consumed = true;
        #[cfg(test)]
        cut_hook(CutStep::AfterCut);
        if quiet_only && !quiet {
            return Ok(false);
        }
        let (jc, jl, js) = journal.delta;
        let scanned_at = quiet.then_some(now);
        // object_count = user-visible only (delta + passthrough); logical =
        // original_bytes; stored = stored_bytes (incl references). Same
        // interpretation as usage_delta_for, so inline + scan agree.
        let object_count = (totals.delta_count + totals.passthrough_count) as i64;
        conn.execute(
            "INSERT OR REPLACE INTO bucket_usage
                (bucket, object_count, logical_bytes, stored_bytes, last_scan_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                bucket,
                object_count + jc,
                totals.original_bytes as i64 + jl,
                totals.stored_bytes as i64 + js,
                scanned_at
            ],
        )?;
        Ok(true)
    }

    /// [`Self::overwrite_from_scan`] for a scan that nobody repeats: the
    /// quota check's own scan of a bucket (`UsageScanner`, prefix "").
    ///
    /// A write that lands while the scan lists can count twice in
    /// `overwrite_from_scan` (the scan may list the key AND the journal
    /// adds its delta). An operator's Refresh repairs that. A quota scan
    /// stamps a row that the quota check then trusts with no further scan,
    /// so a double count would hold a bucket over its quota for good. This
    /// variant stamps only a QUIET scan, one with no change of the bucket
    /// while it ran; otherwise it writes nothing, and the next quota scan
    /// (after the scanner's TTL) tries again. `Ok(true)`: stamped.
    pub fn overwrite_from_quiet_scan(
        &self,
        ticket: ScanTicket<'_>,
        totals: &SavingsTotals,
        now: i64,
    ) -> Result<bool, rusqlite::Error> {
        self.overwrite_from_scan_inner(ticket, totals, now, true)
    }

    /// Forget a bucket (DeleteBucket): its row and its un-flushed changes
    /// go. `/_/stats` sums every row, and a bucket created again under the
    /// same name must start at zero, not scanned. A Refresh still running
    /// on the bucket keeps its journal.
    pub fn forget(&self, bucket: &str) -> Result<(), rusqlite::Error> {
        // conn → pending, the lock order of flush and read: no flush can
        // write the dropped delta back.
        let conn = self.conn.lock().unwrap();
        let _ = self.take_delta(bucket);
        self.pending
            .remove_if(bucket, |_, p| p.scans.is_empty() && p.windows == 0);
        conn.execute(
            "DELETE FROM bucket_usage WHERE bucket = ?1",
            params![bucket],
        )?;
        Ok(())
    }

    /// Map a row whose count/logical/stored/last_scan_at columns start at
    /// `base` (counters clamped at 0 — a negative is a bug, never a real size).
    /// `read` selects from offset 0; `read_all` puts `bucket` first so offset 1.
    fn map_row_at(r: &rusqlite::Row<'_>, base: usize) -> Result<BucketUsageRow, rusqlite::Error> {
        Ok(BucketUsageRow {
            object_count: r.get::<_, i64>(base)?.max(0) as u64,
            logical_bytes: r.get::<_, i64>(base + 1)?.max(0) as u64,
            stored_bytes: r.get::<_, i64>(base + 2)?.max(0) as u64,
            last_scan_at: r.get::<_, Option<i64>>(base + 3)?,
        })
    }
}

/// Transient internal buckets/routes the counter must IGNORE — migration
/// staging (`__dgmigrate_*`) writes/deletes real objects under throwaway bucket
/// names that are filtered out of every listing and torn down at flip. Counting
/// them would leak orphan rows that inflate the global `/_/stats` aggregate
/// forever. Matches the engine/maintenance convention (these prefixes are
/// already gated from creation and hidden from listings).
pub fn is_transient_bucket(bucket: &str) -> bool {
    bucket.starts_with("__dgmigrate_")
}

impl BucketUsage {
    /// The ONE counter mutation every write path goes through. Applies a single
    /// net delta: subtract `removed`'s contribution (the prior object on an
    /// overwrite, or the deleted object), add `added`'s (the new object), and
    /// adjust `stored_bytes` by `ref_bytes_delta` (a seeded reference is `+`, a
    /// reclaimed one `-`). Best-effort — log-and-drop so a counter hiccup never
    /// fails the S3 path (mirrors `enqueue_object_event`). Skips transient
    /// internal buckets (`__dgmigrate_*`).
    ///
    /// Folding the whole net delta here is deliberate: three hand-maintained
    /// copies of "subtract old, add new, adjust ref" is exactly how the
    /// overwrite over-count bug crept in originally.
    pub fn apply_net(
        &self,
        bucket: &str,
        removed: Option<&FileMetadata>,
        added: Option<&FileMetadata>,
        ref_bytes_delta: i64,
    ) {
        if is_transient_bucket(bucket) {
            return;
        }
        let (mut dc, mut dl, mut ds) = (0i64, 0i64, ref_bytes_delta);
        if let Some(m) = removed {
            let (c, l, s) = usage_delta_for(m, -1);
            dc += c;
            dl += l;
            ds += s;
        }
        if let Some(m) = added {
            let (c, l, s) = usage_delta_for(m, 1);
            dc += c;
            dl += l;
            ds += s;
        }
        if dc == 0 && dl == 0 && ds == 0 {
            return;
        }
        // apply_delta is infallible now — it only touches the pending map.
        self.apply_delta(bucket, dc, dl, ds);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FileMetadata;

    fn mk(size: u64, storage_info: StorageInfo) -> FileMetadata {
        FileMetadata::fallback(
            "k".into(),
            size,
            String::new(),
            chrono::Utc::now(),
            None,
            storage_info,
        )
    }
    fn meta_passthrough(size: u64) -> FileMetadata {
        mk(size, StorageInfo::Passthrough)
    }
    fn meta_delta(logical: u64, delta: u64) -> FileMetadata {
        mk(
            logical,
            StorageInfo::Delta {
                ref_path: "reference.bin".into(),
                ref_sha256: "x".into(),
                delta_size: delta,
                delta_cmd: "xdelta3".into(),
            },
        )
    }
    fn meta_reference(size: u64) -> FileMetadata {
        mk(
            size,
            StorageInfo::Reference {
                source_name: "k".into(),
            },
        )
    }

    // ── pure truth table ──────────────────────────────────────────────
    #[test]
    fn delta_for_passthrough() {
        assert_eq!(usage_delta_for(&meta_passthrough(100), 1), (1, 100, 100));
        assert_eq!(
            usage_delta_for(&meta_passthrough(100), -1),
            (-1, -100, -100)
        );
    }
    #[test]
    fn delta_for_delta() {
        // logical 1000, delta 30: counts as one object, 1000 logical, 30 stored.
        assert_eq!(usage_delta_for(&meta_delta(1000, 30), 1), (1, 1000, 30));
        assert_eq!(usage_delta_for(&meta_delta(1000, 30), -1), (-1, -1000, -30));
    }
    #[test]
    fn delta_for_reference() {
        // reference.bin: stored bytes only, never counted, never logical.
        assert_eq!(usage_delta_for(&meta_reference(8000), 1), (0, 0, 8000));
        assert_eq!(usage_delta_for(&meta_reference(8000), -1), (0, 0, -8000));
    }

    // ── store roundtrip ───────────────────────────────────────────────
    #[test]
    fn apply_read_and_clamp() {
        let db = BucketUsage::in_memory().unwrap();
        db.apply_object("b", &meta_delta(1000, 30), 1);
        db.apply_object("b", &meta_passthrough(100), 1);
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.object_count, 2);
        assert_eq!(row.logical_bytes, 1100);
        assert_eq!(row.stored_bytes, 130);
        assert_eq!(row.last_scan_at, None);

        // Over-delete cannot go negative on read.
        db.apply_object("b", &meta_delta(1000, 30), -1);
        db.apply_object("b", &meta_passthrough(100), -1);
        db.apply_object("b", &meta_passthrough(100), -1);
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.object_count, 0, "clamped at 0, not negative");
        assert_eq!(row.logical_bytes, 0);
    }

    /// The whole truth table of the stamp rule: a scan is trusted only when
    /// the journal is empty AND no write sits in its window at the end.
    #[test]
    fn a_scan_is_quiet_only_without_journal_and_without_writes_in_the_window() {
        for (journal_is_empty, writes, quiet) in [
            (true, 0, true),
            (true, 1, false),
            (true, 7, false),
            (false, 0, false),
            (false, 1, false),
        ] {
            assert_eq!(
                scan_is_quiet(journal_is_empty, writes),
                quiet,
                "journal empty: {journal_is_empty}, writes in their window: {writes}"
            );
        }
    }

    /// Run `f` as the hook of the next scan cut on this thread.
    fn with_cut_hook<R>(hook: impl FnMut(CutStep) + 'static, f: impl FnOnce() -> R) -> R {
        CUT_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        let out = f();
        CUT_HOOK.with(|h| *h.borrow_mut() = None);
        out
    }

    fn totals_of(metas: &[FileMetadata]) -> SavingsTotals {
        let mut totals = SavingsTotals::default();
        for m in metas {
            totals.accumulate(m);
        }
        totals
    }

    /// Stress: three writers store (S) and record (R) inside a window while
    /// a scanner lists and asks for a quiet stamp, over and over. After
    /// every stamp, with the writers parked, the row must equal what was
    /// stored: a stamp never loses a write and never counts one twice.
    /// Red when the cut reads the window count in a second step.
    #[test]
    fn a_stamp_is_exact_under_concurrent_writers() {
        use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
        const WRITERS: usize = 3;
        let db = Arc::new(BucketUsage::in_memory().unwrap());
        // Objects stored so far (what a listing sees).
        let visible = Arc::new(AtomicU64::new(0));
        let pause = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicUsize::new(0));
        let writers: Vec<_> = (0..WRITERS)
            .map(|_| {
                let (db, visible, pause, stop, parked) = (
                    db.clone(),
                    visible.clone(),
                    pause.clone(),
                    stop.clone(),
                    parked.clone(),
                );
                std::thread::spawn(move || {
                    let meta = meta_passthrough(10);
                    while !stop.load(SeqCst) {
                        if pause.load(SeqCst) {
                            parked.fetch_add(1, SeqCst);
                            while pause.load(SeqCst) && !stop.load(SeqCst) {
                                std::thread::yield_now();
                            }
                            parked.fetch_sub(1, SeqCst);
                            continue;
                        }
                        let window = db.open_write_window("b");
                        visible.fetch_add(1, SeqCst); // the storage write
                        db.apply_object("b", &meta, 1); // its record
                        drop(window);
                        std::thread::yield_now();
                    }
                })
            })
            .collect();
        let start = std::time::Instant::now();
        let (mut tries, mut stamps, mut wrong) = (0u32, 0u32, 0u32);
        while start.elapsed() < std::time::Duration::from_secs(2) && wrong == 0 {
            tries += 1;
            let ticket = db.begin_scan("b");
            let listed = visible.load(SeqCst);
            let totals = SavingsTotals {
                original_bytes: 10 * listed,
                stored_bytes: 10 * listed,
                passthrough_bytes: 10 * listed,
                passthrough_count: listed,
                ..Default::default()
            };
            if db.overwrite_from_quiet_scan(ticket, &totals, 5).unwrap() {
                stamps += 1;
                pause.store(true, SeqCst);
                while parked.load(SeqCst) < WRITERS {
                    std::thread::yield_now();
                }
                let row = db.read("b").unwrap().unwrap();
                if row.object_count != visible.load(SeqCst) {
                    wrong += 1;
                    eprintln!(
                        "stamped row {} != stored {}",
                        row.object_count,
                        visible.load(SeqCst)
                    );
                }
                pause.store(false, SeqCst);
            }
        }
        stop.store(true, SeqCst);
        for w in writers {
            w.join().unwrap();
        }
        assert_eq!(wrong, 0, "a stamped scan disagrees with what was stored");
        assert!(stamps > 0, "no scan was stamped in {tries} tries");
    }

    /// A window is a count in the bucket's entry: it blocks the stamp while
    /// open, a flush keeps it, and the entry goes with the last window.
    #[test]
    fn a_write_window_blocks_the_stamp_and_survives_a_flush() {
        let db = Arc::new(BucketUsage::in_memory().unwrap());
        let window = db.open_write_window("b");
        db.apply_object("b", &meta_passthrough(10), 1);
        db.flush_pending();
        assert_eq!(
            db.pending.get("b").map(|p| p.windows),
            Some(1),
            "the flush dropped the open window"
        );
        let two = db.open_write_window("b");
        assert_eq!(db.pending.get("b").map(|p| p.windows), Some(2));
        drop(two);
        let ticket = db.begin_scan("b");
        let totals = totals_of(&[meta_passthrough(10)]);
        assert!(
            !db.overwrite_from_quiet_scan(ticket, &totals, 5).unwrap(),
            "stamped with a write in its window"
        );
        drop(window);
        assert!(db.pending.is_empty(), "the last window leaves an entry");
        let ticket = db.begin_scan("b");
        assert!(db.overwrite_from_quiet_scan(ticket, &totals, 6).unwrap());
        assert!(
            db.open_write_window("__dgmigrate_x").open.is_none(),
            "a transient bucket has no window"
        );
    }

    /// A write that stores its object before the cut and records after it:
    /// the scan is not stamped (S2: the window is read in the cut, not
    /// before; here the write enters its window after the earliest point
    /// where a count could have been read).
    #[test]
    fn a_write_that_enters_its_window_before_the_cut_blocks_the_stamp() {
        let db = Arc::new(BucketUsage::in_memory().unwrap());
        let totals = totals_of(&[meta_passthrough(40)]); // the scan listed it
        let ticket = db.begin_scan("b");
        let writer = db.clone();
        let window: std::rc::Rc<std::cell::RefCell<Option<WriteWindow>>> = Default::default();
        let held = window.clone();
        let stamped = with_cut_hook(
            move |step| {
                if step == CutStep::BeforeCut {
                    *held.borrow_mut() = Some(writer.open_write_window("b"));
                }
            },
            || db.overwrite_from_quiet_scan(ticket, &totals, 9).unwrap(),
        );
        assert!(
            !stamped,
            "stamped a scan that ended inside a write's window"
        );
        // The writer records its change and leaves.
        db.apply_object("b", &meta_passthrough(40), 1);
        window.borrow_mut().take();
        let row = db.read("b").unwrap().unwrap();
        assert_eq!((row.object_count, row.logical_bytes), (1, 40), "{row:?}");
    }

    /// A write that starts after the listing and records right after the
    /// cut lands on the new row: the old delta goes with the cut, in the
    /// same critical section, not before or after it (S1: the windows
    /// between the cut and the take).
    #[test]
    fn a_write_that_records_right_after_the_cut_is_kept() {
        let db = Arc::new(BucketUsage::in_memory().unwrap());
        db.apply_object("b", &meta_passthrough(100), 1);
        let totals = totals_of(&[meta_passthrough(100)]);
        let ticket = db.begin_scan("b");
        let writer = db.clone();
        let stamped = with_cut_hook(
            move |step| {
                if step == CutStep::AfterCut {
                    // Not listed by the scan, recorded and finished.
                    let w = writer.open_write_window("b");
                    writer.apply_object("b", &meta_passthrough(7), 1);
                    drop(w);
                }
            },
            || db.overwrite_from_quiet_scan(ticket, &totals, 9).unwrap(),
        );
        assert!(stamped, "the scan was quiet at its cut");
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(
            (row.object_count, row.logical_bytes, row.last_scan_at),
            (2, 107, Some(9)),
            "the late write was dropped or counted twice: {row:?}"
        );
    }

    /// The same write, recorded just BEFORE the cut and after the scan
    /// began: the journal has it, the scan is not stamped, the row is exact.
    #[test]
    fn a_write_that_records_right_before_the_cut_is_in_the_journal() {
        let db = Arc::new(BucketUsage::in_memory().unwrap());
        let totals = totals_of(&[meta_passthrough(100), meta_passthrough(7)]);
        let ticket = db.begin_scan("b");
        let writer = db.clone();
        let stamped = with_cut_hook(
            move |step| {
                if step == CutStep::BeforeCut {
                    let w = writer.open_write_window("b");
                    writer.apply_object("b", &meta_passthrough(7), 1);
                    drop(w);
                }
            },
            || db.overwrite_from_quiet_scan(ticket, &totals, 9).unwrap(),
        );
        assert!(!stamped);
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.last_scan_at, None, "{row:?}");
    }

    #[test]
    fn read_missing_is_none() {
        let db = BucketUsage::in_memory().unwrap();
        assert_eq!(db.read("nope").unwrap(), None);
    }

    #[test]
    fn overwrite_from_scan_sets_truth_and_timestamp() {
        let db = BucketUsage::in_memory().unwrap();
        // drift the inline counter first
        db.apply_object("b", &meta_passthrough(5), 1);
        let mut totals = SavingsTotals::default();
        totals.accumulate(&meta_delta(1000, 30));
        totals.accumulate(&meta_passthrough(100));
        totals.accumulate(&meta_reference(8000));
        db.overwrite_from_scan(db.begin_scan("b"), &totals, 1234)
            .unwrap();
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(
            row.object_count, 2,
            "delta + passthrough, reference excluded"
        );
        assert_eq!(row.logical_bytes, 1100);
        assert_eq!(row.stored_bytes, 30 + 100 + 8000);
        assert_eq!(row.last_scan_at, Some(1234));
    }

    #[test]
    fn read_all_returns_every_bucket() {
        let db = BucketUsage::in_memory().unwrap();
        db.apply_object("a", &meta_passthrough(10), 1);
        db.apply_object("b", &meta_passthrough(20), 1);
        let mut all = db.read_all().unwrap();
        all.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "a");
        assert_eq!(all[0].1.logical_bytes, 10);
        assert_eq!(all[1].1.logical_bytes, 20);
    }

    // ── pending/flush semantics (#85) ────────────────────────────────
    #[test]
    fn read_merges_pending_before_flush() {
        let db = BucketUsage::in_memory().unwrap();
        // Pending-only bucket: visible BEFORE any flush ran.
        db.apply_object("fresh", &meta_passthrough(42), 1);
        assert_eq!(db.pending.len(), 1, "delta folded into pending, not SQL");
        let row = db.read("fresh").unwrap().unwrap();
        assert_eq!(row.object_count, 1);
        assert_eq!(row.logical_bytes, 42);
        assert_eq!(row.last_scan_at, None);
    }

    #[test]
    fn flush_persists_and_clears_pending() {
        let db = BucketUsage::in_memory().unwrap();
        db.apply_object("b", &meta_delta(1000, 30), 1);
        db.apply_object("b", &meta_passthrough(100), 1);
        db.flush_pending();
        assert!(db.pending.is_empty(), "flush drained the pending map");
        // Read straight from SQL now — same numbers.
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.object_count, 2);
        assert_eq!(row.logical_bytes, 1100);
        assert_eq!(row.stored_bytes, 130);
        // Deltas AFTER a flush merge on top of the flushed row.
        db.apply_object("b", &meta_passthrough(5), 1);
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.object_count, 3);
        assert_eq!(row.logical_bytes, 1105);
    }

    #[test]
    fn read_all_surfaces_pending_only_buckets() {
        let db = BucketUsage::in_memory().unwrap();
        // One flushed, one pending-only.
        db.apply_object("flushed", &meta_passthrough(10), 1);
        db.flush_pending();
        db.apply_object("pending", &meta_passthrough(20), 1);
        let mut all = db.read_all().unwrap();
        all.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "flushed");
        assert_eq!(all[1].0, "pending");
        assert_eq!(all[1].1.logical_bytes, 20);
    }

    #[test]
    fn scan_overwrite_supersedes_pending_deltas() {
        let db = BucketUsage::in_memory().unwrap();
        // Stale inline drift + a scan that says otherwise: ground truth wins,
        // and nothing double-applies after the REPLACE.
        db.apply_object("b", &meta_passthrough(5), 1);
        let mut totals = SavingsTotals::default();
        totals.accumulate(&meta_delta(1000, 30));
        totals.accumulate(&meta_passthrough(100));
        db.overwrite_from_scan(db.begin_scan("b"), &totals, 99)
            .unwrap();
        assert!(
            db.pending.is_empty(),
            "scan must flush pending so stale deltas cannot double-apply later"
        );
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.object_count, 2);
        assert_eq!(row.logical_bytes, 1100);
        assert_eq!(row.stored_bytes, 130);
        assert_eq!(row.last_scan_at, Some(99));
    }

    /// A write that reaches storage before a scan lists, but records its
    /// delta after the scan started, is in the listing AND in the journal.
    /// `overwrite_from_scan` (an operator's Refresh, which they can repeat)
    /// counts it twice, for display, but does not stamp the row: the quota
    /// gate trusts only a stamped row, and a double count must not hold a
    /// bucket over its quota (review 2d: the Refresh stamped it). The quota
    /// scan's `overwrite_from_quiet_scan` writes nothing instead. One rule
    /// for both: only a quiet scan stamps.
    #[test]
    fn a_quiet_scan_never_stamps_a_double_count() {
        let mut totals = SavingsTotals::default();
        totals.accumulate(&meta_passthrough(100));
        totals.accumulate(&meta_passthrough(50)); // the late write, listed

        let db = BucketUsage::in_memory().unwrap();
        db.apply_object("b", &meta_passthrough(100), 1);
        db.flush_pending();
        let ticket = db.begin_scan("b");
        db.apply_object("b", &meta_passthrough(50), 1); // its delta, late
        db.overwrite_from_scan(ticket, &totals, 7).unwrap();
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(
            (row.logical_bytes, row.last_scan_at),
            (200, None),
            "a busy Refresh shows its totals but is not trusted"
        );

        let db = BucketUsage::in_memory().unwrap();
        db.apply_object("b", &meta_passthrough(100), 1);
        db.flush_pending();
        let ticket = db.begin_scan("b");
        db.apply_object("b", &meta_passthrough(50), 1);
        assert!(!db.overwrite_from_quiet_scan(ticket, &totals, 7).unwrap());
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(
            (row.logical_bytes, row.last_scan_at),
            (150, None),
            "{row:?}"
        );
        db.flush_pending();
        assert_eq!(db.read("b").unwrap().unwrap().logical_bytes, 150);

        // A quiet scan stamps.
        let ticket = db.begin_scan("b");
        assert!(db.overwrite_from_quiet_scan(ticket, &totals, 8).unwrap());
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(
            (row.logical_bytes, row.last_scan_at),
            (150, Some(8)),
            "{row:?}"
        );
    }

    /// H14b: a PUT or DELETE that lands while the Refresh scan runs is
    /// kept on top of the scan result, also when a periodic flush moves it
    /// into SQLite before the scan ends. A delta from BEFORE the scan is
    /// still superseded.
    #[test]
    fn scan_keeps_deltas_that_arrive_during_the_scan() {
        let db = BucketUsage::in_memory().unwrap();
        db.apply_object("b", &meta_passthrough(5), 1); // before: superseded
        let ticket = db.begin_scan("b");
        db.apply_object("b", &meta_passthrough(7), 1); // during, flushed
        db.flush_pending();
        db.apply_object("b", &meta_passthrough(11), 1); // during, pending
        db.apply_object("other", &meta_passthrough(3), 1);
        let mut totals = SavingsTotals::default();
        totals.accumulate(&meta_passthrough(100));
        db.overwrite_from_scan(ticket, &totals, 42).unwrap();
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.object_count, 3, "scan (1) + two writes during it");
        assert_eq!(row.logical_bytes, 100 + 7 + 11);
        assert_eq!(row.last_scan_at, None, "writes ran during the scan");
        // Nothing double-applies at the next flush; other buckets keep theirs.
        db.flush_pending();
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.logical_bytes, 118);
        assert_eq!(db.read("other").unwrap().unwrap().logical_bytes, 3);
        // A write after the overwrite counts once, as usual.
        db.apply_object("b", &meta_passthrough(1), 1);
        db.flush_pending();
        assert_eq!(db.read("b").unwrap().unwrap().logical_bytes, 119);
    }

    /// Two overlapping scans of one bucket: each keeps the deltas since
    /// its own start. An abandoned scan (dropped ticket) leaves nothing.
    #[test]
    fn overlapping_and_abandoned_scans() {
        let db = BucketUsage::in_memory().unwrap();
        let first = db.begin_scan("b");
        db.apply_object("b", &meta_passthrough(1), 1);
        let second = db.begin_scan("b");
        db.apply_object("b", &meta_passthrough(2), 1);
        let abandoned = db.begin_scan("b");
        drop(abandoned);
        let mut totals = SavingsTotals::default();
        totals.accumulate(&meta_passthrough(10));
        db.overwrite_from_scan(first, &totals, 1).unwrap();
        assert_eq!(db.read("b").unwrap().unwrap().logical_bytes, 13);
        db.overwrite_from_scan(second, &totals, 2).unwrap();
        assert_eq!(db.read("b").unwrap().unwrap().logical_bytes, 12);
        assert!(db.pending.is_empty(), "no scan journal left behind");
    }

    #[test]
    fn concurrent_apply_then_flush_is_lossless() {
        let db = BucketUsage::in_memory().unwrap();
        db.apply_object("b", &meta_passthrough(1), 1);
        std::thread::scope(|s| {
            let db = &db;
            for _t in 0..4u8 {
                s.spawn(move || {
                    for _ in 0..250 {
                        db.apply_object("b", &meta_passthrough(1), 1);
                    }
                });
            }
        });
        db.flush_pending();
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.object_count, 1 + 4 * 250);
    }

    // ── recount marks and forget ─────────────────────────────────────
    /// A write with an unknown net change marks the row: read and flush
    /// both show it as not scanned until the next Refresh stamps it.
    #[test]
    fn a_recount_mark_clears_the_scan_stamp() {
        let db = BucketUsage::in_memory().unwrap();
        let mut totals = SavingsTotals::default();
        totals.accumulate(&meta_passthrough(10));
        db.overwrite_from_scan(db.begin_scan("b"), &totals, 7)
            .unwrap();
        db.mark_for_recount("b");
        assert_eq!(db.read("b").unwrap().unwrap().last_scan_at, None);
        assert_eq!(db.read_all().unwrap()[0].1.last_scan_at, None);
        db.flush_pending();
        assert!(db.pending.is_empty());
        let row = db.read("b").unwrap().unwrap();
        assert_eq!((row.object_count, row.last_scan_at), (1, None));
        db.overwrite_from_scan(db.begin_scan("b"), &totals, 8)
            .unwrap();
        assert_eq!(db.read("b").unwrap().unwrap().last_scan_at, Some(8));
        // Migration staging is never counted, so never marked.
        db.mark_for_recount("__dgmigrate_x__src");
        assert!(db.pending.is_empty());
    }

    /// A mark that arrives while a Refresh scans the bucket: the scan may
    /// have missed that write's change, so its result is not stamped.
    #[test]
    fn a_recount_mark_during_a_scan_leaves_the_row_unstamped() {
        let db = BucketUsage::in_memory().unwrap();
        db.mark_for_recount("b"); // before the scan: superseded
        let ticket = db.begin_scan("b");
        let mut totals = SavingsTotals::default();
        totals.accumulate(&meta_passthrough(10));
        let clean = db.begin_scan("b");
        db.overwrite_from_scan(clean, &totals, 1).unwrap();
        assert_eq!(db.read("b").unwrap().unwrap().last_scan_at, Some(1));
        db.mark_for_recount("b");
        db.flush_pending();
        db.overwrite_from_scan(ticket, &totals, 2).unwrap();
        assert_eq!(db.read("b").unwrap().unwrap().last_scan_at, None);
        assert!(db.pending.is_empty(), "no journal left behind");
    }

    /// DeleteBucket: the row and the un-flushed delta go, other buckets
    /// keep theirs, and a later write starts from zero.
    #[test]
    fn forget_drops_the_row_and_its_pending_delta() {
        let db = BucketUsage::in_memory().unwrap();
        db.apply_object("b", &meta_passthrough(10), 1);
        db.apply_object("other", &meta_passthrough(3), 1);
        db.flush_pending();
        db.apply_object("b", &meta_passthrough(5), 1);
        db.mark_for_recount("b");
        db.forget("b").unwrap();
        assert_eq!(db.read("b").unwrap(), None);
        db.flush_pending();
        assert_eq!(db.read("b").unwrap(), None, "the flush wrote it back");
        assert_eq!(db.read_all().unwrap().len(), 1);
        db.apply_object("b", &meta_passthrough(1), 1);
        db.flush_pending();
        assert_eq!(db.read("b").unwrap().unwrap().logical_bytes, 1);
    }
}

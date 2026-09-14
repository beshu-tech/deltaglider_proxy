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
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::warn;

/// SCHEMA_VERSION for the usage DB — independent of the config DB's version.
const SCHEMA_VERSION: i32 = 1;

/// Path to the usage DB — beside the config DB (same dir-derivation rule).
pub fn bucket_usage_db_path() -> PathBuf {
    let db_dir = std::env::var("DGP_CONFIG")
        .ok()
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
    /// Unix secs of the last authoritative full scan; `None` = never scanned.
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
/// The `rusqlite::Connection` is not `Sync`, but the engine that holds this is
/// shared across threads — so the connection sits behind a `std::sync::Mutex`.
/// Critical sections are a single SQL statement (sub-millisecond), and all call
/// sites are synchronous (no `await` held across the lock), so a blocking mutex
/// is the right, cheap choice.
pub struct BucketUsage {
    conn: Mutex<Connection>,
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
        let conn = Connection::open(path)?;
        // Best-effort: a backend that refuses WAL (rare, e.g. some network FS)
        // still works correctly, just slower — never fail startup over a pragma.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// In-memory instance for tests.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Mutex::new(conn),
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
    /// first touch). Counters are stored signed and clamped at 0 on read so a
    /// transient out-of-order delta can never surface a negative size.
    pub fn apply_delta(
        &self,
        bucket: &str,
        d_count: i64,
        d_logical: i64,
        d_stored: i64,
    ) -> Result<(), rusqlite::Error> {
        // RETURNING the post-update row so we can detect a column going negative
        // — that is ALWAYS a real accounting bug (a missed-count somewhere
        // upstream), never a steady state, so warn loudly. The read path still
        // clamps at 0 for display; this surfaces the drift instead of hiding it.
        let (oc, lb, sb): (i64, i64, i64) = self.conn.lock().unwrap().query_row(
            "INSERT INTO bucket_usage (bucket, object_count, logical_bytes, stored_bytes)
                 VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(bucket) DO UPDATE SET
                 object_count  = object_count  + excluded.object_count,
                 logical_bytes = logical_bytes + excluded.logical_bytes,
                 stored_bytes  = stored_bytes  + excluded.stored_bytes
             RETURNING object_count, logical_bytes, stored_bytes",
            params![bucket, d_count, d_logical, d_stored],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if oc < 0 || lb < 0 || sb < 0 {
            warn!(
                "bucket_usage: counter for '{}' went negative (count={}, logical={}, stored={}) — \
                 an upstream mutation was miscounted; run Refresh to reconcile",
                bucket, oc, lb, sb
            );
        }
        Ok(())
    }

    /// Apply one object's create (+1) or delete (-1) via [`usage_delta_for`].
    pub fn apply_object(
        &self,
        bucket: &str,
        meta: &FileMetadata,
        sign: i8,
    ) -> Result<(), rusqlite::Error> {
        let (dc, dl, ds) = usage_delta_for(meta, sign);
        self.apply_delta(bucket, dc, dl, ds)
    }

    /// Read one bucket's counters (clamped at 0). `None` if never touched.
    pub fn read(&self, bucket: &str) -> Result<Option<BucketUsageRow>, rusqlite::Error> {
        self.conn
            .lock()
            .unwrap()
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
            })
    }

    /// Read every bucket's counters (for the aggregate `/_/stats`).
    pub fn read_all(&self) -> Result<Vec<(String, BucketUsageRow)>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT bucket, object_count, logical_bytes, stored_bytes, last_scan_at
               FROM bucket_usage",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, Self::map_row_at(r, 1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Overwrite a bucket's row with full-scan ground truth + stamp `last_scan_at`.
    pub fn overwrite_from_scan(
        &self,
        bucket: &str,
        totals: &SavingsTotals,
        now: i64,
    ) -> Result<(), rusqlite::Error> {
        // object_count = user-visible only (delta + passthrough); logical =
        // original_bytes; stored = stored_bytes (incl references). Same
        // interpretation as usage_delta_for, so inline + scan agree.
        let object_count = totals.delta_count + totals.passthrough_count;
        self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO bucket_usage
                (bucket, object_count, logical_bytes, stored_bytes, last_scan_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                bucket,
                object_count as i64,
                totals.original_bytes as i64,
                totals.stored_bytes as i64,
                now
            ],
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
        if let Err(e) = self.apply_delta(bucket, dc, dl, ds) {
            warn!(
                "bucket_usage: failed to apply net delta for {}: {}",
                bucket, e
            );
        }
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
        db.apply_object("b", &meta_delta(1000, 30), 1).unwrap();
        db.apply_object("b", &meta_passthrough(100), 1).unwrap();
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.object_count, 2);
        assert_eq!(row.logical_bytes, 1100);
        assert_eq!(row.stored_bytes, 130);
        assert_eq!(row.last_scan_at, None);

        // Over-delete cannot go negative on read.
        db.apply_object("b", &meta_delta(1000, 30), -1).unwrap();
        db.apply_object("b", &meta_passthrough(100), -1).unwrap();
        db.apply_object("b", &meta_passthrough(100), -1).unwrap();
        let row = db.read("b").unwrap().unwrap();
        assert_eq!(row.object_count, 0, "clamped at 0, not negative");
        assert_eq!(row.logical_bytes, 0);
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
        db.apply_object("b", &meta_passthrough(5), 1).unwrap();
        let mut totals = SavingsTotals::default();
        totals.accumulate(&meta_delta(1000, 30));
        totals.accumulate(&meta_passthrough(100));
        totals.accumulate(&meta_reference(8000));
        db.overwrite_from_scan("b", &totals, 1234).unwrap();
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
        db.apply_object("a", &meta_passthrough(10), 1).unwrap();
        db.apply_object("b", &meta_passthrough(20), 1).unwrap();
        let mut all = db.read_all().unwrap();
        all.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "a");
        assert_eq!(all[0].1.logical_bytes, 10);
        assert_eq!(all[1].1.logical_bytes, 20);
    }
}

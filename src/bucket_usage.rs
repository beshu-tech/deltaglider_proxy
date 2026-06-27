// SPDX-License-Identifier: GPL-3.0-only

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
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
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

/// Best-effort apply: log-and-drop on error so a counter hiccup never fails
/// the S3 path (mirrors `enqueue_object_event`'s contract). Skips transient
/// internal buckets.
pub fn record_object(
    usage: &Option<std::sync::Arc<BucketUsage>>,
    bucket: &str,
    meta: &FileMetadata,
    sign: i8,
) {
    let Some(u) = usage else { return };
    record_object_into(u, bucket, meta, sign);
}

/// Like [`record_object`] but against an already-unwrapped handle (the engine
/// holds `&BucketUsage` directly when applying a multi-step net delta).
pub fn record_object_into(u: &BucketUsage, bucket: &str, meta: &FileMetadata, sign: i8) {
    if is_transient_bucket(bucket) {
        return;
    }
    if let Err(e) = u.apply_object(bucket, meta, sign) {
        warn!(
            "bucket_usage: failed to apply object delta for {}: {}",
            bucket, e
        );
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

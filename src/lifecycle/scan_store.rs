// SPDX-License-Identifier: BUSL-1.1

//! The persisted lifecycle scan cache (`lifecycle_scan_cache` in the
//! node-local jobs DB): the logical metadata of each listed object, kept
//! under the stored identity that a lite listing shows (ETag, size and
//! LastModified). A restart or a config apply empties the in-memory cache
//! (`scan_cache`); this table answers the first scan after it, so an
//! unchanged object costs no HEAD.
//!
//! Jobs DB only: the table is never in the config DB, and the config DB
//! is never adopted into it.

use crate::config_db::{ConfigDbError, JobsDb};
use rusqlite::{params, OptionalExtension};

/// One cached object: its stored identity and its metadata (JSON).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanRow {
    pub key: String,
    pub etag: String,
    pub size: u64,
    pub last_modified_ms: i64,
    pub meta_json: String,
}

impl JobsDb {
    /// The cached rows of `keys` in `bucket`, each with the time a scan
    /// last saw it.
    pub(crate) fn lifecycle_scan_get(
        &self,
        bucket: &str,
        keys: &[&str],
    ) -> Result<Vec<(ScanRow, i64)>, ConfigDbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT etag, size, last_modified_ms, meta_json, seen_at
             FROM lifecycle_scan_cache WHERE bucket = ?1 AND key = ?2",
        )?;
        let mut out = Vec::new();
        for key in keys {
            let row = stmt
                .query_row(params![bucket, key], |r| {
                    Ok((
                        ScanRow {
                            key: key.to_string(),
                            etag: r.get(0)?,
                            size: r.get::<_, i64>(1)?.max(0) as u64,
                            last_modified_ms: r.get(2)?,
                            meta_json: r.get(3)?,
                        },
                        r.get::<_, i64>(4)?,
                    ))
                })
                .optional()?;
            out.extend(row);
        }
        Ok(out)
    }

    /// One page of a scan, in ONE transaction: write the rows it read
    /// (`upserts`), and mark `seen` the keys it found that a scan has not
    /// seen for a while.
    pub(crate) fn lifecycle_scan_record(
        &self,
        bucket: &str,
        upserts: &[ScanRow],
        seen: &[&str],
        now: i64,
    ) -> Result<(), ConfigDbError> {
        if upserts.is_empty() && seen.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut put = tx.prepare_cached(
                "INSERT INTO lifecycle_scan_cache
                     (bucket, key, etag, size, last_modified_ms, meta_json, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (bucket, key) DO UPDATE SET
                     etag = excluded.etag, size = excluded.size,
                     last_modified_ms = excluded.last_modified_ms,
                     meta_json = excluded.meta_json, seen_at = excluded.seen_at",
            )?;
            for r in upserts {
                put.execute(params![
                    bucket,
                    r.key,
                    r.etag,
                    r.size as i64,
                    r.last_modified_ms,
                    r.meta_json,
                    now
                ])?;
            }
            let mut touch = tx.prepare_cached(
                "UPDATE lifecycle_scan_cache SET seen_at = ?3 WHERE bucket = ?1 AND key = ?2",
            )?;
            for key in seen {
                touch.execute(params![bucket, key, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop the rows no scan saw since `now - idle_secs` (a rule removed or
    /// narrowed, objects deleted), then the least recently seen rows past
    /// `max_rows`. Returns the rows deleted.
    pub(crate) fn lifecycle_scan_prune(
        &self,
        now: i64,
        idle_secs: i64,
        max_rows: u64,
    ) -> Result<usize, ConfigDbError> {
        let idle = self.conn.execute(
            "DELETE FROM lifecycle_scan_cache WHERE seen_at < ?1",
            params![now - idle_secs],
        )?;
        let rows: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM lifecycle_scan_cache", [], |r| {
                    r.get(0)
                })?;
        let over = rows.saturating_sub(max_rows as i64);
        let capped = if over > 0 {
            self.conn.execute(
                "DELETE FROM lifecycle_scan_cache WHERE rowid IN
                     (SELECT rowid FROM lifecycle_scan_cache ORDER BY seen_at LIMIT ?1)",
                params![over],
            )?
        } else {
            0
        };
        Ok(idle + capped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, etag: &str) -> ScanRow {
        ScanRow {
            key: key.into(),
            etag: etag.into(),
            size: 7,
            last_modified_ms: 1_000,
            meta_json: "{}".into(),
        }
    }

    #[test]
    fn rows_round_trip_and_are_scoped_by_bucket() {
        let db = JobsDb::in_memory("k").unwrap();
        db.lifecycle_scan_record("b", &[row("a", "e1"), row("c", "e3")], &[], 10)
            .unwrap();
        let got = db.lifecycle_scan_get("b", &["a", "x", "c"]).unwrap();
        assert_eq!(got, vec![(row("a", "e1"), 10), (row("c", "e3"), 10)]);
        assert!(db.lifecycle_scan_get("other", &["a"]).unwrap().is_empty());
        // An upsert replaces the row; a seen key gets the new time.
        db.lifecycle_scan_record("b", &[row("a", "e2")], &["c"], 20)
            .unwrap();
        let got = db.lifecycle_scan_get("b", &["a", "c"]).unwrap();
        assert_eq!(got, vec![(row("a", "e2"), 20), (row("c", "e3"), 20)]);
    }

    #[test]
    fn prune_drops_idle_rows_then_the_oldest_past_the_cap() {
        let db = JobsDb::in_memory("k").unwrap();
        db.lifecycle_scan_record("b", &[row("old", "e")], &[], 1)
            .unwrap();
        for (i, k) in ["k1", "k2", "k3"].iter().enumerate() {
            db.lifecycle_scan_record("b", &[row(k, "e")], &[], 100 + i as i64)
                .unwrap();
        }
        assert_eq!(db.lifecycle_scan_prune(200, 150, 2).unwrap(), 2);
        let left: Vec<String> = db
            .lifecycle_scan_get("b", &["old", "k1", "k2", "k3"])
            .unwrap()
            .into_iter()
            .map(|(r, _)| r.key)
            .collect();
        assert_eq!(left, ["k2", "k3"]);
    }
}

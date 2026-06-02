// SPDX-License-Identifier: GPL-3.0-only

//! Durable object-event outbox.
//!
//! Request handlers only append facts after successful mutations. Delivery
//! happens from a background dispatcher that claims due rows and never blocks
//! the S3 request path.

use crate::config_db::{ConfigDb, ConfigDbError};
use rusqlite::{params, types::Type, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const STATUS_PENDING: &str = "pending";
pub const STATUS_IN_PROGRESS: &str = "in_progress";
pub const STATUS_DELIVERED: &str = "delivered";
pub const STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutboxSort {
    Id,
    OccurredAt,
    CreatedAt,
    NextAttemptAt,
    DeliveredAt,
    Attempts,
    Status,
    Kind,
    Bucket,
    Key,
}

impl EventOutboxSort {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "id" => Some(Self::Id),
            "occurred_at" => Some(Self::OccurredAt),
            "created_at" => Some(Self::CreatedAt),
            "next_attempt_at" => Some(Self::NextAttemptAt),
            "delivered_at" => Some(Self::DeliveredAt),
            "attempts" => Some(Self::Attempts),
            "status" => Some(Self::Status),
            "kind" => Some(Self::Kind),
            "bucket" => Some(Self::Bucket),
            "key" | "object_key" => Some(Self::Key),
            _ => None,
        }
    }

    fn column(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::OccurredAt => "occurred_at",
            Self::CreatedAt => "created_at",
            Self::NextAttemptAt => "next_attempt_at",
            Self::DeliveredAt => "delivered_at",
            Self::Attempts => "attempts",
            Self::Status => "status",
            Self::Kind => "kind",
            Self::Bucket => "bucket",
            Self::Key => "object_key",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutboxSortOrder {
    Asc,
    Desc,
}

impl EventOutboxSortOrder {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "asc" | "ascend" => Some(Self::Asc),
            "desc" | "descend" => Some(Self::Desc),
            _ => None,
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventOutboxListQuery<'a> {
    pub status: Option<&'a str>,
    pub limit: u32,
    pub offset: u32,
    pub sort: EventOutboxSort,
    pub order: EventOutboxSortOrder,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventOutboxPage {
    pub rows: Vec<EventOutboxRecord>,
    pub total: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    ObjectCreated,
    ObjectDeleted,
    ObjectCopied,
    ReplicationObjectCopied,
    LifecycleExpired,
    LifecycleTransitioned,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ObjectCreated => "ObjectCreated",
            Self::ObjectDeleted => "ObjectDeleted",
            Self::ObjectCopied => "ObjectCopied",
            Self::ReplicationObjectCopied => "ReplicationObjectCopied",
            Self::LifecycleExpired => "LifecycleExpired",
            Self::LifecycleTransitioned => "LifecycleTransitioned",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventSource {
    S3Api,
    Replication,
    Lifecycle,
}

impl EventSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::S3Api => "s3_api",
            Self::Replication => "replication",
            Self::Lifecycle => "lifecycle",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    pub kind: EventKind,
    pub bucket: String,
    pub key: String,
    pub source: EventSource,
    pub occurred_at: i64,
    pub payload: Value,
}

impl NewEvent {
    pub fn new(
        kind: EventKind,
        bucket: impl Into<String>,
        key: impl Into<String>,
        source: EventSource,
        occurred_at: i64,
        payload: Value,
    ) -> Self {
        Self {
            kind,
            bucket: bucket.into(),
            key: key.into(),
            source,
            occurred_at,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventOutboxRecord {
    pub id: i64,
    pub kind: String,
    pub bucket: String,
    pub key: String,
    pub source: String,
    pub occurred_at: i64,
    pub payload: Value,
    pub status: String,
    pub attempts: i64,
    pub next_attempt_at: Option<i64>,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<i64>,
    pub delivered_at: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventOutboxStatusCounts {
    pub pending: i64,
    pub in_progress: i64,
    pub delivered: i64,
    pub failed: i64,
}

pub fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl ConfigDb {
    pub fn event_outbox_insert(&self, event: &NewEvent) -> Result<i64, ConfigDbError> {
        let ids = self.event_outbox_insert_many(std::slice::from_ref(event))?;
        ids.first()
            .copied()
            .ok_or_else(|| ConfigDbError::Other("event outbox insert returned no row id".into()))
    }

    pub fn event_outbox_insert_many(&self, events: &[NewEvent]) -> Result<Vec<i64>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO event_outbox
                (kind, bucket, object_key, source, occurred_at, payload_json, status)
             VALUES (?, ?, ?, ?, ?, ?, 'pending')",
        )?;
        let mut ids = Vec::with_capacity(events.len());
        for event in events {
            let payload_json = serde_json::to_string(&event.payload)
                .map_err(|e| ConfigDbError::Other(e.to_string()))?;
            let id = stmt.insert(params![
                event.kind.as_str(),
                event.bucket,
                event.key,
                event.source.as_str(),
                event.occurred_at,
                payload_json
            ])?;
            ids.push(id);
        }
        Ok(ids)
    }

    pub fn event_outbox_recent(&self, limit: u32) -> Result<Vec<EventOutboxRecord>, ConfigDbError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, bucket, object_key, source, occurred_at,
                    payload_json, status, attempts, next_attempt_at,
                    claimed_by, claimed_at, delivered_at, last_error, created_at
               FROM event_outbox
              ORDER BY occurred_at DESC, id DESC
              LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], event_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn event_outbox_list(
        &self,
        query: EventOutboxListQuery<'_>,
    ) -> Result<EventOutboxPage, ConfigDbError> {
        if query.limit == 0 {
            return Ok(EventOutboxPage {
                rows: Vec::new(),
                total: self.event_outbox_count(query.status)?,
            });
        }
        if let Some(status) = query.status {
            validate_status(status)?;
        }

        let column = query.sort.column();
        let order = query.order.sql();
        let sql = match query.status {
            Some(_) => format!(
                "SELECT id, kind, bucket, object_key, source, occurred_at,
                        payload_json, status, attempts, next_attempt_at,
                        claimed_by, claimed_at, delivered_at, last_error, created_at
                   FROM event_outbox
                  WHERE status = ?
                  ORDER BY {column} {order}, id {order}
                  LIMIT ? OFFSET ?"
            ),
            None => format!(
                "SELECT id, kind, bucket, object_key, source, occurred_at,
                        payload_json, status, attempts, next_attempt_at,
                        claimed_by, claimed_at, delivered_at, last_error, created_at
                   FROM event_outbox
                  ORDER BY {column} {order}, id {order}
                  LIMIT ? OFFSET ?"
            ),
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = match query.status {
            Some(status) => stmt
                .query_map(
                    params![status, query.limit as i64, query.offset as i64],
                    event_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?,
            None => stmt
                .query_map(
                    params![query.limit as i64, query.offset as i64],
                    event_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?,
        };

        Ok(EventOutboxPage {
            rows,
            total: self.event_outbox_count(query.status)?,
        })
    }

    pub fn event_outbox_recent_by_status(
        &self,
        status: &str,
        limit: u32,
    ) -> Result<Vec<EventOutboxRecord>, ConfigDbError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        validate_status(status)?;
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, bucket, object_key, source, occurred_at,
                    payload_json, status, attempts, next_attempt_at,
                    claimed_by, claimed_at, delivered_at, last_error, created_at
               FROM event_outbox
              WHERE status = ?
              ORDER BY occurred_at DESC, id DESC
              LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![status, limit as i64], event_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn event_outbox_count(&self, status: Option<&str>) -> Result<i64, ConfigDbError> {
        if let Some(status) = status {
            validate_status(status)?;
            return Ok(self.conn.query_row(
                "SELECT count(*) FROM event_outbox WHERE status = ?",
                params![status],
                |row| row.get(0),
            )?);
        }
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM event_outbox", [], |row| row.get(0))?)
    }

    pub fn event_outbox_status_counts(&self) -> Result<EventOutboxStatusCounts, ConfigDbError> {
        let mut counts = EventOutboxStatusCounts {
            pending: 0,
            in_progress: 0,
            delivered: 0,
            failed: 0,
        };
        let mut stmt = self
            .conn
            .prepare("SELECT status, count(*) FROM event_outbox GROUP BY status")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            match status.as_str() {
                STATUS_PENDING => counts.pending = count,
                STATUS_IN_PROGRESS => counts.in_progress = count,
                STATUS_DELIVERED => counts.delivered = count,
                STATUS_FAILED => counts.failed = count,
                _ => {}
            }
        }
        Ok(counts)
    }

    pub fn event_outbox_claim_due(
        &self,
        claimant: &str,
        now: i64,
        stale_after_secs: i64,
        limit: u32,
    ) -> Result<Vec<EventOutboxRecord>, ConfigDbError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let stale_claimed_at = now.saturating_sub(stale_after_secs.max(1));
        let ids = {
            let mut stmt = self.conn.prepare(
                "SELECT id
                   FROM event_outbox
                  WHERE (
                            status = 'pending'
                        AND (next_attempt_at IS NULL OR next_attempt_at <= ?)
                        )
                     OR (
                            status = 'in_progress'
                        AND claimed_at IS NOT NULL
                        AND claimed_at <= ?
                        )
                  ORDER BY occurred_at ASC, id ASC
                  LIMIT ?",
            )?;
            let rows = stmt.query_map(params![now, stale_claimed_at, limit as i64], |row| {
                row.get::<_, i64>(0)
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let mut claimed = Vec::with_capacity(ids.len());
        for id in ids {
            let updated = self.conn.execute(
                "UPDATE event_outbox
                    SET status = 'in_progress',
                        attempts = attempts + 1,
                        claimed_by = ?,
                        claimed_at = ?,
                        last_error = NULL
                  WHERE id = ?
                    AND (
                            (
                                status = 'pending'
                            AND (next_attempt_at IS NULL OR next_attempt_at <= ?)
                            )
                         OR (
                                status = 'in_progress'
                            AND claimed_at IS NOT NULL
                            AND claimed_at <= ?
                            )
                    )",
                params![claimant, now, id, now, stale_claimed_at],
            )?;
            if updated > 0 {
                if let Some(row) = self.event_outbox_load(id)? {
                    claimed.push(row);
                }
            }
        }
        Ok(claimed)
    }

    pub fn event_outbox_mark_delivered(&self, id: i64, now: i64) -> Result<bool, ConfigDbError> {
        let updated = self.conn.execute(
            "UPDATE event_outbox
                SET status = 'delivered',
                    delivered_at = ?,
                    claimed_by = NULL,
                    claimed_at = NULL,
                    next_attempt_at = NULL,
                    last_error = NULL
              WHERE id = ?",
            params![now, id],
        )?;
        Ok(updated > 0)
    }

    pub fn event_outbox_mark_failed(
        &self,
        id: i64,
        error: &str,
        next_attempt_at: Option<i64>,
    ) -> Result<bool, ConfigDbError> {
        let status = if next_attempt_at.is_some() {
            STATUS_PENDING
        } else {
            STATUS_FAILED
        };
        let updated = self.conn.execute(
            "UPDATE event_outbox
                SET status = ?,
                    next_attempt_at = ?,
                    claimed_by = NULL,
                    claimed_at = NULL,
                    last_error = ?
              WHERE id = ?",
            params![status, next_attempt_at, error, id],
        )?;
        Ok(updated > 0)
    }

    pub fn event_outbox_requeue_failed(&self, id: i64, now: i64) -> Result<bool, ConfigDbError> {
        let updated = self.conn.execute(
            "UPDATE event_outbox
                SET status = 'pending',
                    next_attempt_at = ?,
                    claimed_by = NULL,
                    claimed_at = NULL,
                    delivered_at = NULL,
                    last_error = NULL
              WHERE id = ?
                AND status = 'failed'",
            params![now, id],
        )?;
        Ok(updated > 0)
    }

    pub fn event_outbox_requeue_failed_many(
        &self,
        ids: &[i64],
        now: i64,
    ) -> Result<usize, ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        let mut updated = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE event_outbox
                    SET status = 'pending',
                        next_attempt_at = ?,
                        claimed_by = NULL,
                        claimed_at = NULL,
                        delivered_at = NULL,
                        last_error = NULL
                  WHERE id = ?
                    AND status = 'failed'",
            )?;
            for id in ids {
                updated += stmt.execute(params![now, id])?;
            }
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Delete up to `limit` delivered rows older than `before`. `min_keep_id`
    /// is the prune FLOOR — only rows with `id <= min_keep_id` are eligible, so
    /// the pruner never removes a row a slower listener (event-driven
    /// replication) hasn't consumed yet. Pass `i64::MAX` for no floor (e.g. when
    /// no listeners exist).
    pub fn event_outbox_prune_delivered_before(
        &self,
        before: i64,
        limit: u32,
        min_keep_id: i64,
    ) -> Result<usize, ConfigDbError> {
        if limit == 0 {
            return Ok(0);
        }
        let deleted = self.conn.execute(
            "DELETE FROM event_outbox
              WHERE id IN (
                    SELECT id
                      FROM event_outbox
                     WHERE status = 'delivered'
                       AND delivered_at IS NOT NULL
                       AND delivered_at < ?
                       AND id <= ?
                     ORDER BY delivered_at ASC, id ASC
                     LIMIT ?
              )",
            params![before, min_keep_id, limit as i64],
        )?;
        Ok(deleted)
    }

    /// Delete delivered rows beyond `max_delivered_rows` (newest kept), bounded
    /// by `limit` per call. `min_keep_id` is the prune FLOOR (see
    /// [`Self::event_outbox_prune_delivered_before`]) — only rows at or below it
    /// are eligible, so a slower listener's unconsumed rows survive.
    pub fn event_outbox_prune_delivered_over_count(
        &self,
        max_delivered_rows: u32,
        limit: u32,
        min_keep_id: i64,
    ) -> Result<usize, ConfigDbError> {
        if limit == 0 {
            return Ok(0);
        }
        let deleted = self.conn.execute(
            "DELETE FROM event_outbox
              WHERE id IN (
                    SELECT id
                      FROM (
                            SELECT id
                              FROM event_outbox
                             WHERE status = 'delivered'
                               AND id <= ?
                             ORDER BY COALESCE(delivered_at, occurred_at) DESC, id DESC
                             LIMIT -1 OFFSET ?
                      )
                     ORDER BY id ASC
                     LIMIT ?
              )",
            params![min_keep_id, max_delivered_rows as i64, limit as i64],
        )?;
        Ok(deleted)
    }

    fn event_outbox_load(&self, id: i64) -> Result<Option<EventOutboxRecord>, ConfigDbError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, kind, bucket, object_key, source, occurred_at,
                        payload_json, status, attempts, next_attempt_at,
                        claimed_by, claimed_at, delivered_at, last_error, created_at
                   FROM event_outbox
                  WHERE id = ?",
                params![id],
                event_from_row,
            )
            .optional()?;
        Ok(row)
    }

    // === Per-listener cursor API (event-driven replication, schema v12) ===
    //
    // The outbox is append-only. Each listener (webhook delivery, replication)
    // reads `WHERE id > cursor` and tracks its own high-water `last_event_id`
    // here — it does NOT touch the global `status`/`delivered_at`, so listeners
    // never contend. Ordering is by autoincrement `id` (true arrival order).

    /// Fetch up to `limit` outbox records with `id > after_id`, oldest first.
    /// This is the listener drain query — `id ASC` is the contract compaction
    /// and cursor advancement rely on.
    pub fn event_outbox_since(
        &self,
        after_id: i64,
        limit: u32,
    ) -> Result<Vec<EventOutboxRecord>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, bucket, object_key, source, occurred_at,
                    payload_json, status, attempts, next_attempt_at,
                    claimed_by, claimed_at, delivered_at, last_error, created_at
               FROM event_outbox
              WHERE id > ?
              ORDER BY id ASC
              LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![after_id, limit as i64], event_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The listener's current high-water `last_event_id` (0 if it has never
    /// recorded a cursor).
    pub fn listener_cursor_load(&self, listener: &str) -> Result<i64, ConfigDbError> {
        let cursor = self
            .conn
            .query_row(
                "SELECT last_event_id FROM listener_cursors WHERE listener_name = ?",
                params![listener],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(cursor.unwrap_or(0))
    }

    /// Advance the listener cursor to `new_id`. Monotonic: the stored value
    /// never regresses (`MAX(existing, new)`), so a reordered or duplicate
    /// advance can't move the high-water mark backwards.
    pub fn listener_cursor_advance(
        &self,
        listener: &str,
        new_id: i64,
        now: i64,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT INTO listener_cursors (listener_name, last_event_id, updated_at)
                  VALUES (?, ?, ?)
             ON CONFLICT(listener_name) DO UPDATE SET
                  last_event_id = MAX(last_event_id, excluded.last_event_id),
                  updated_at    = excluded.updated_at",
            params![listener, new_id, now],
        )?;
        Ok(())
    }

    /// Load the full cursor row (for surfacing "last event applied at" in the
    /// admin UI). `None` if the listener has no cursor yet.
    pub fn listener_cursor_load_full(
        &self,
        listener: &str,
    ) -> Result<Option<ListenerCursor>, ConfigDbError> {
        let row = self
            .conn
            .query_row(
                "SELECT listener_name, last_event_id, updated_at
                   FROM listener_cursors WHERE listener_name = ?",
                params![listener],
                |row| {
                    Ok(ListenerCursor {
                        listener_name: row.get(0)?,
                        last_event_id: row.get(1)?,
                        updated_at: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// The smallest `last_event_id` across ALL listener cursors, or `None` when
    /// there are no cursors. The webhook delivered-row pruner clamps its delete
    /// floor to this so it never removes a row a slower listener (replication)
    /// hasn't consumed yet.
    pub fn event_outbox_min_listener_cursor(&self) -> Result<Option<i64>, ConfigDbError> {
        let min = self
            .conn
            .query_row(
                "SELECT MIN(last_event_id) FROM listener_cursors",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(min)
    }

    /// The smallest `last_event_id` across listener cursors that are still
    /// ACTIVE — i.e. whose `updated_at` is within `max_age_secs` of `now`.
    ///
    /// A stuck or dead consumer (replication disabled mid-flight, a permanently
    /// failing rule wedging the watermark, an instance that died holding the
    /// consumer lease) would otherwise pin the pruner's delete floor forever and
    /// let the outbox grow without bound. Ignoring stale cursors lets the pruner
    /// reclaim space once a listener stops making progress; the cost is that a
    /// listener idle longer than `max_age_secs` may have already-delivered rows
    /// pruned out from under it — acceptable because a consumer that far behind
    /// relies on the slow full reconcile to self-heal anyway.
    ///
    /// `None` when no cursor is active (→ no floor; pruner may delete any
    /// delivered row).
    pub fn event_outbox_min_active_listener_cursor(
        &self,
        now: i64,
        max_age_secs: i64,
    ) -> Result<Option<i64>, ConfigDbError> {
        let cutoff = now.saturating_sub(max_age_secs);
        let min = self
            .conn
            .query_row(
                "SELECT MIN(last_event_id) FROM listener_cursors WHERE updated_at >= ?",
                params![cutoff],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(min)
    }
}

/// A listener's cursor row — its high-water `last_event_id` over the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerCursor {
    pub listener_name: String,
    pub last_event_id: i64,
    pub updated_at: i64,
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventOutboxRecord> {
    let payload_json: String = row.get(6)?;
    let payload = serde_json::from_str(&payload_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(6, Type::Text, Box::new(e)))?;
    Ok(EventOutboxRecord {
        id: row.get(0)?,
        kind: row.get(1)?,
        bucket: row.get(2)?,
        key: row.get(3)?,
        source: row.get(4)?,
        occurred_at: row.get(5)?,
        payload,
        status: row.get(7)?,
        attempts: row.get(8)?,
        next_attempt_at: row.get(9)?,
        claimed_by: row.get(10)?,
        claimed_at: row.get(11)?,
        delivered_at: row.get(12)?,
        last_error: row.get(13)?,
        created_at: row.get(14)?,
    })
}

fn validate_status(status: &str) -> Result<(), ConfigDbError> {
    if matches!(
        status,
        STATUS_PENDING | STATUS_IN_PROGRESS | STATUS_DELIVERED | STATUS_FAILED
    ) {
        Ok(())
    } else {
        Err(ConfigDbError::Other(format!(
            "invalid event_outbox status: {status}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event_at(ts: i64, key: &str) -> NewEvent {
        NewEvent::new(
            EventKind::ObjectCreated,
            "bucket",
            key,
            EventSource::S3Api,
            ts,
            json!({ "size": 123 }),
        )
    }

    #[test]
    fn migration_creates_outbox_table() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let version: i32 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert!(version >= 9);

        let count: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'event_outbox'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn insert_and_recent_preserve_payload_order() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let first = db.event_outbox_insert(&event_at(10, "a")).unwrap();
        let second = db.event_outbox_insert(&event_at(20, "b")).unwrap();
        assert!(second > first);

        let rows = db.event_outbox_recent(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "b");
        assert_eq!(rows[0].status, STATUS_PENDING);
        assert_eq!(rows[0].payload, json!({ "size": 123 }));
        assert_eq!(rows[1].key, "a");
    }

    #[test]
    fn claim_due_marks_rows_and_skips_future_retries() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let due = db.event_outbox_insert(&event_at(10, "due")).unwrap();
        let future = db.event_outbox_insert(&event_at(11, "future")).unwrap();
        db.event_outbox_mark_failed(future, "try later", Some(500))
            .unwrap();

        let claimed = db.event_outbox_claim_due("worker-a", 100, 30, 10).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, due);
        assert_eq!(claimed[0].status, STATUS_IN_PROGRESS);
        assert_eq!(claimed[0].attempts, 1);
        assert_eq!(claimed[0].claimed_by.as_deref(), Some("worker-a"));

        let none = db.event_outbox_claim_due("worker-b", 120, 30, 10).unwrap();
        assert!(none.is_empty());

        let stolen = db.event_outbox_claim_due("worker-b", 200, 30, 10).unwrap();
        assert_eq!(stolen.len(), 1);
        assert_eq!(stolen[0].id, due);
        assert_eq!(stolen[0].attempts, 2);
        assert_eq!(stolen[0].claimed_by.as_deref(), Some("worker-b"));
    }

    #[test]
    fn mark_delivered_and_prune_removes_only_old_delivered_rows() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let old = db.event_outbox_insert(&event_at(10, "old")).unwrap();
        let new = db.event_outbox_insert(&event_at(20, "new")).unwrap();
        let pending = db.event_outbox_insert(&event_at(30, "pending")).unwrap();

        assert!(db.event_outbox_mark_delivered(old, 100).unwrap());
        assert!(db.event_outbox_mark_delivered(new, 300).unwrap());

        let deleted = db
            .event_outbox_prune_delivered_before(200, 100, i64::MAX)
            .unwrap();
        assert_eq!(deleted, 1);

        let rows = db.event_outbox_recent(10).unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert!(!ids.contains(&old));
        assert!(ids.contains(&new));
        assert!(ids.contains(&pending));
    }

    #[test]
    fn list_supports_status_pagination_and_sorting() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let first = db.event_outbox_insert(&event_at(10, "a")).unwrap();
        let second = db.event_outbox_insert(&event_at(30, "b")).unwrap();
        let third = db.event_outbox_insert(&event_at(20, "c")).unwrap();
        db.event_outbox_mark_failed(second, "dead", None).unwrap();

        let page = db
            .event_outbox_list(EventOutboxListQuery {
                status: None,
                limit: 2,
                offset: 1,
                sort: EventOutboxSort::OccurredAt,
                order: EventOutboxSortOrder::Desc,
            })
            .unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(
            page.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![third, first]
        );

        let failed = db
            .event_outbox_list(EventOutboxListQuery {
                status: Some(STATUS_FAILED),
                limit: 10,
                offset: 0,
                sort: EventOutboxSort::Id,
                order: EventOutboxSortOrder::Asc,
            })
            .unwrap();
        assert_eq!(failed.total, 1);
        assert_eq!(failed.rows[0].id, second);
    }

    #[test]
    fn prune_delivered_over_count_keeps_newest_delivered_rows() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let old = db.event_outbox_insert(&event_at(10, "old")).unwrap();
        let mid = db.event_outbox_insert(&event_at(20, "mid")).unwrap();
        let new = db.event_outbox_insert(&event_at(30, "new")).unwrap();
        let failed = db.event_outbox_insert(&event_at(40, "failed")).unwrap();

        db.event_outbox_mark_delivered(old, 100).unwrap();
        db.event_outbox_mark_delivered(mid, 200).unwrap();
        db.event_outbox_mark_delivered(new, 300).unwrap();
        db.event_outbox_mark_failed(failed, "dead", None).unwrap();

        let deleted = db
            .event_outbox_prune_delivered_over_count(2, 100, i64::MAX)
            .unwrap();
        assert_eq!(deleted, 1);

        let rows = db.event_outbox_recent(10).unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert!(!ids.contains(&old));
        assert!(ids.contains(&mid));
        assert!(ids.contains(&new));
        assert!(ids.contains(&failed));
    }

    #[test]
    fn requeue_failed_preserves_attempt_count_and_clears_failure_state() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let id = db.event_outbox_insert(&event_at(10, "dead")).unwrap();

        let claimed = db.event_outbox_claim_due("worker-a", 100, 30, 10).unwrap();
        assert_eq!(claimed[0].attempts, 1);
        db.event_outbox_mark_failed(id, "webhook exhausted", None)
            .unwrap();

        assert!(db.event_outbox_requeue_failed(id, 200).unwrap());

        let row = db.event_outbox_load(id).unwrap().unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert_eq!(row.attempts, 1);
        assert_eq!(row.next_attempt_at, Some(200));
        assert_eq!(row.claimed_by, None);
        assert_eq!(row.claimed_at, None);
        assert_eq!(row.delivered_at, None);
        assert_eq!(row.last_error, None);
    }

    #[test]
    fn requeue_failed_many_only_touches_dead_rows() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let failed = db.event_outbox_insert(&event_at(10, "failed")).unwrap();
        let pending = db.event_outbox_insert(&event_at(20, "pending")).unwrap();
        let delivered = db.event_outbox_insert(&event_at(30, "delivered")).unwrap();

        db.event_outbox_mark_failed(failed, "dead", None).unwrap();
        db.event_outbox_mark_delivered(delivered, 150).unwrap();

        let updated = db
            .event_outbox_requeue_failed_many(&[failed, pending, delivered, 9999], 250)
            .unwrap();
        assert_eq!(updated, 1);

        let failed_row = db.event_outbox_load(failed).unwrap().unwrap();
        let pending_row = db.event_outbox_load(pending).unwrap().unwrap();
        let delivered_row = db.event_outbox_load(delivered).unwrap().unwrap();
        assert_eq!(failed_row.status, STATUS_PENDING);
        assert_eq!(failed_row.next_attempt_at, Some(250));
        assert_eq!(pending_row.status, STATUS_PENDING);
        assert_eq!(pending_row.next_attempt_at, None);
        assert_eq!(delivered_row.status, STATUS_DELIVERED);
    }

    #[test]
    fn listener_cursor_load_advance_monotonic() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        // Absent cursor reads as 0.
        assert_eq!(db.listener_cursor_load("replication").unwrap(), 0);
        db.listener_cursor_advance("replication", 5, 100).unwrap();
        assert_eq!(db.listener_cursor_load("replication").unwrap(), 5);
        // Monotonic: a lower advance does NOT regress the high-water mark.
        db.listener_cursor_advance("replication", 3, 110).unwrap();
        assert_eq!(db.listener_cursor_load("replication").unwrap(), 5);
        db.listener_cursor_advance("replication", 9, 120).unwrap();
        assert_eq!(db.listener_cursor_load("replication").unwrap(), 9);
        // Distinct listeners are independent.
        assert_eq!(db.listener_cursor_load("webhook").unwrap(), 0);
        let full = db
            .listener_cursor_load_full("replication")
            .unwrap()
            .unwrap();
        assert_eq!(full.last_event_id, 9);
        assert_eq!(full.updated_at, 120);
    }

    #[test]
    fn min_active_listener_cursor_ignores_stale_cursors() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        // Two listeners: webhook fresh at id=20 (updated_at=1000), replication
        // stale at id=5 (updated_at=100).
        db.listener_cursor_advance("webhook", 20, 1000).unwrap();
        db.listener_cursor_advance("replication", 5, 100).unwrap();
        // Plain min sees the stale replication cursor → pins the floor at 5.
        assert_eq!(db.event_outbox_min_listener_cursor().unwrap(), Some(5));
        // Active variant at now=1000 with a 60s staleness window: replication
        // (updated_at=100) is stale and ignored; only webhook (1000) counts.
        assert_eq!(
            db.event_outbox_min_active_listener_cursor(1000, 60)
                .unwrap(),
            Some(20)
        );
        // A wide window includes both again.
        assert_eq!(
            db.event_outbox_min_active_listener_cursor(1000, 100_000)
                .unwrap(),
            Some(5)
        );
        // When ALL cursors are stale, there's no floor (None) → pruner is free.
        assert_eq!(
            db.event_outbox_min_active_listener_cursor(100_000, 60)
                .unwrap(),
            None
        );
    }

    #[test]
    fn event_outbox_since_returns_id_ordered_window() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let a = db.event_outbox_insert(&event_at(10, "a")).unwrap();
        let b = db.event_outbox_insert(&event_at(20, "b")).unwrap();
        let c = db.event_outbox_insert(&event_at(30, "c")).unwrap();
        // Everything after `a` → b, c in id order.
        let rows = db.event_outbox_since(a, 100).unwrap();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![b, c]);
        // Cursor at the end → empty.
        assert!(db.event_outbox_since(c, 100).unwrap().is_empty());
        // Limit honored.
        assert_eq!(db.event_outbox_since(0, 1).unwrap().len(), 1);
    }

    #[test]
    fn prune_respects_listener_cursor_floor() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let a = db.event_outbox_insert(&event_at(10, "a")).unwrap();
        let b = db.event_outbox_insert(&event_at(20, "b")).unwrap();
        // Both delivered long ago.
        db.event_outbox_mark_delivered(a, 1).unwrap();
        db.event_outbox_mark_delivered(b, 1).unwrap();
        // Prune with floor = a (the listener has only consumed up to `a`): only
        // `a` is eligible; `b` survives because the listener hasn't passed it.
        let deleted = db
            .event_outbox_prune_delivered_before(1_000_000, 100, a)
            .unwrap();
        assert_eq!(deleted, 1);
        assert!(db.event_outbox_load(a).unwrap().is_none());
        assert!(db.event_outbox_load(b).unwrap().is_some());
    }
}

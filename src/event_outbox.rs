//! Durable object-event outbox.
//!
//! This is only the persistence foundation. Network delivery (webhooks,
//! queues, etc.) should live in a future dispatcher that claims rows from
//! `event_outbox`; request handlers only append facts after successful
//! mutations.

use crate::config_db::{ConfigDb, ConfigDbError};
use rusqlite::{params, types::Type, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const STATUS_PENDING: &str = "pending";
pub const STATUS_IN_PROGRESS: &str = "in_progress";
pub const STATUS_DELIVERED: &str = "delivered";
pub const STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    ObjectCreated,
    ObjectDeleted,
    ObjectCopied,
    ReplicationObjectCopied,
    LifecycleExpired,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ObjectCreated => "ObjectCreated",
            Self::ObjectDeleted => "ObjectDeleted",
            Self::ObjectCopied => "ObjectCopied",
            Self::ReplicationObjectCopied => "ReplicationObjectCopied",
            Self::LifecycleExpired => "LifecycleExpired",
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

#[derive(Debug, Clone, PartialEq)]
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
                  WHERE id = ?",
                params![claimant, now, id],
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

    pub fn event_outbox_prune_delivered_before(
        &self,
        before: i64,
        limit: u32,
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
                     ORDER BY delivered_at ASC, id ASC
                     LIMIT ?
              )",
            params![before, limit as i64],
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
    fn migration_creates_v9_outbox() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let version: i32 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 9);

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

        let deleted = db.event_outbox_prune_delivered_before(200, 100).unwrap();
        assert_eq!(deleted, 1);

        let rows = db.event_outbox_recent(10).unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert!(!ids.contains(&old));
        assert!(ids.contains(&new));
        assert!(ids.contains(&pending));
    }
}

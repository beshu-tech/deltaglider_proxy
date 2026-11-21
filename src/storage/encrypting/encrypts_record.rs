// SPDX-License-Identifier: BUSL-1.1

//! The proxy's own record of the buckets that it encrypted (`config_db::node_facts`).
//! Written when the proxy sees proof; read by the boot. Never waits for the jobs DB.

use crate::config_db::JobsDb;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

type SharedJobsDb = Arc<tokio::sync::Mutex<JobsDb>>;

#[derive(Default)]
pub struct EncryptsRecord {
    db: OnceLock<SharedJobsDb>,
    /// Identities already written: the jobs DB is touched once per bucket per process.
    noted: parking_lot::Mutex<HashSet<String>>,
    /// Identities whose write waits for the jobs DB lock.
    deferred: parking_lot::Mutex<HashSet<String>>,
    /// When each identity was last tried: a write that keeps failing is tried once a minute.
    tried: parking_lot::Mutex<HashMap<String, Instant>>,
}

/// Pause between two attempts to write the record of one bucket.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Pure: may the write of `identity` be tried at `now`? Records the attempt; one entry per bucket.
fn try_due(tried: &mut HashMap<String, Instant>, identity: &str, now: Instant) -> bool {
    match tried.get(identity) {
        Some(last) if now.saturating_duration_since(*last) < RETRY_AFTER => false,
        _ => {
            tried.insert(identity.to_string(), now);
            true
        }
    }
}

impl EncryptsRecord {
    /// Install the node's jobs DB (once; a second call is ignored).
    pub fn install(&self, db: SharedJobsDb) {
        let _ = self.db.set(db);
    }

    fn write(&self, db: &JobsDb, identity: &str, why: &str) {
        match db.record_encrypts(identity, why) {
            Ok(()) => {
                self.noted.lock().insert(identity.to_string());
                self.tried.lock().remove(identity);
            }
            Err(e) => tracing::warn!(
                "encryption: the record that '{}' encrypts could not be written: {e}",
                identity.replace('\0', "/")
            ),
        }
    }

    /// Note that the storage `identity` encrypts. Never waits for the jobs DB (a caller may
    /// hold it across a storage request): a busy DB gets the row from a task.
    pub async fn note(self: &Arc<Self>, identity: &str, why: &str) {
        self.note_at(identity, why, Instant::now()).await
    }

    /// [`Self::note`] at the time `now` (tests move the clock).
    async fn note_at(self: &Arc<Self>, identity: &str, why: &str, now: Instant) {
        if self.noted.lock().contains(identity) {
            return;
        }
        if !try_due(&mut self.tried.lock(), identity, now) {
            return;
        }
        let Some(db) = self.db.get() else { return };
        if let Ok(guard) = db.try_lock() {
            self.write(&guard, identity, why);
            return;
        }
        if !self.deferred.lock().insert(identity.to_string()) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.deferred.lock().remove(identity);
            return;
        };
        let (me, db, identity, why) = (
            self.clone(),
            db.clone(),
            identity.to_string(),
            why.to_string(),
        );
        runtime.spawn(async move {
            me.write(&*db.lock().await, &identity, &why);
            me.deferred.lock().remove(&identity);
        });
    }

    /// Does the record say that `identity` encrypts? `Err`: no jobs DB, or
    /// it could not be read (the caller refuses, never reads "no").
    pub async fn recorded(&self, identity: &str) -> Result<bool, String> {
        let db = self
            .db
            .get()
            .ok_or_else(|| "this instance has no jobs database".to_string())?;
        db.lock()
            .await
            .encrypts_recorded(identity)
            .map_err(|e| e.to_string())
    }

    /// Drop the record of a deleted bucket (never waits, as [`Self::note`]).
    pub async fn forget(self: &Arc<Self>, identity: &str) {
        self.noted.lock().remove(identity);
        self.deferred.lock().remove(identity);
        self.tried.lock().remove(identity);
        let Some(db) = self.db.get() else { return };
        let drop_row = |db: &JobsDb, identity: &str| {
            if let Err(e) = db.forget_encrypts(identity) {
                tracing::warn!(
                    "encryption: the record of '{}' could not be dropped: {e}",
                    identity.replace('\0', "/")
                );
            }
        };
        if let Ok(guard) = db.try_lock() {
            drop_row(&guard, identity);
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (db, identity) = (db.clone(), identity.to_string());
        runtime.spawn(async move { drop_row(&*db.lock().await, &identity) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed() -> (Arc<EncryptsRecord>, SharedJobsDb) {
        let db = Arc::new(tokio::sync::Mutex::new(JobsDb::in_memory("k").unwrap()));
        let record = Arc::new(EncryptsRecord::default());
        record.install(db.clone());
        (record, db)
    }

    #[tokio::test]
    async fn a_note_is_readable_and_survives_in_the_db() {
        let (record, db) = installed();
        assert_eq!(record.recorded("a\0b").await, Ok(false));
        record.note("a\0b", "sealed").await;
        assert_eq!(record.recorded("a\0b").await, Ok(true));
        assert!(db.lock().await.encrypts_recorded("a\0b").unwrap());
        record.forget("a\0b").await;
        assert_eq!(record.recorded("a\0b").await, Ok(false));
        // After a forget the next write notes the bucket again.
        record.note("a\0b", "sealed").await;
        assert_eq!(record.recorded("a\0b").await, Ok(true));
    }

    /// A record that cannot be written (the jobs DB is read-only or full) costs one attempt
    /// and one warning per bucket per minute, however many reads of sealed objects come.
    #[tokio::test]
    async fn a_failing_record_write_is_tried_once_per_bucket_per_minute() {
        let (record, db) = installed();
        db.lock()
            .await
            .durable(|c| Ok(c.execute("DROP TABLE encrypts_record", [])?))
            .unwrap();
        let log = crate::test_log_capture::capture("warn");
        let warnings = || log.text().matches("could not be written").count();
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        for secs in [0, 1, 10, 59] {
            record.note_at("a\0b", "sealed", at(secs)).await;
        }
        assert_eq!(warnings(), 1);
        // Another bucket has its own minute.
        record.note_at("c\0d", "sealed", at(5)).await;
        assert_eq!(warnings(), 2);
        record.note_at("a\0b", "sealed", at(61)).await;
        assert_eq!(warnings(), 3);
        record.note_at("a\0b", "sealed", at(62)).await;
        assert_eq!(warnings(), 3);
    }

    #[tokio::test]
    async fn without_a_jobs_db_the_record_cannot_answer() {
        let record = Arc::new(EncryptsRecord::default());
        record.note("a\0b", "sealed").await;
        assert!(record.recorded("a\0b").await.is_err());
    }

    /// The note and the forget never wait for the jobs DB lock, so they cannot
    /// deadlock the storage path; the row follows when the DB is free.
    #[tokio::test]
    async fn a_note_never_waits_for_the_jobs_db() {
        let (record, db) = installed();
        let held = db.lock().await;
        let quick = std::time::Duration::from_millis(500);
        tokio::time::timeout(quick, record.note("a\0b", "sealed"))
            .await
            .expect("note waited for the jobs DB lock");
        tokio::time::timeout(quick, record.forget("c\0d"))
            .await
            .expect("forget waited for the jobs DB lock");
        drop(held);
        for _ in 0..100 {
            if record.recorded("a\0b").await == Ok(true) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the deferred note was never written");
    }
}

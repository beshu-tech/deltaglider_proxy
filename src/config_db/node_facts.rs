// SPDX-License-Identifier: BUSL-1.1

//! Node-local facts that the proxy keeps outside the storage and the synced config DB
//! (the jobs DB, never synced): the `encrypts_record` and the per-target `sync_floors`.

use super::jobs_db::JobsDb;
use super::ConfigDbError;

impl JobsDb {
    /// The last generation this node confirmed for the sync `target`
    /// (`0`: none). Another target has its own floor.
    pub fn sync_floor(&self, target: &str) -> Result<i64, ConfigDbError> {
        match self.conn.query_row(
            "SELECT generation FROM sync_floors WHERE target = ?1",
            [target],
            |r| r.get::<_, i64>(0),
        ) {
            Ok(g) => Ok(g),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    /// Raise the floor of `target` to `generation`; it never goes down here.
    pub fn raise_sync_floor(&self, target: &str, generation: i64) -> Result<(), ConfigDbError> {
        if generation <= self.sync_floor(target)? {
            return Ok(());
        }
        self.durable(|conn| {
            conn.execute(
                "INSERT INTO sync_floors (target, generation) VALUES (?1, ?2) \
                 ON CONFLICT(target) DO UPDATE SET generation = MAX(generation, excluded.generation)",
                rusqlite::params![target, generation],
            )?;
            Ok(())
        })
    }

    /// Forget the floor of `target`: the operator's explicit reset.
    pub fn reset_sync_floor(&self, target: &str) -> Result<(), ConfigDbError> {
        self.durable(|conn| {
            conn.execute("DELETE FROM sync_floors WHERE target = ?1", [target])?;
            Ok(())
        })
    }

    /// Does the record say that the storage `identity` encrypts?
    pub fn encrypts_recorded(&self, identity: &str) -> Result<bool, ConfigDbError> {
        Ok(self.conn.query_row(
            "SELECT count(*) > 0 FROM encrypts_record WHERE identity = ?1",
            [identity],
            |r| r.get::<_, bool>(0),
        )?)
    }

    /// Record that the storage `identity` encrypts, and why (`why` is a
    /// short label for the first event that made the record). Idempotent.
    pub fn record_encrypts(&self, identity: &str, why: &str) -> Result<(), ConfigDbError> {
        if self.encrypts_recorded(identity)? {
            return Ok(());
        }
        self.durable(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO encrypts_record (identity, noted_at, why) \
                 VALUES (?1, strftime('%s','now'), ?2)",
                [identity, why],
            )?;
            Ok(())
        })
    }

    /// Drop the record of `identity` (its bucket was deleted).
    pub fn forget_encrypts(&self, identity: &str) -> Result<(), ConfigDbError> {
        self.durable(|conn| {
            conn.execute(
                "DELETE FROM encrypts_record WHERE identity = ?1",
                [identity],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> JobsDb {
        JobsDb::in_memory("k").unwrap()
    }

    #[test]
    fn the_sync_floor_only_goes_up_and_belongs_to_one_target() {
        let db = db();
        assert_eq!(db.sync_floor("a").unwrap(), 0);
        db.raise_sync_floor("a", 7).unwrap();
        db.raise_sync_floor("a", 3).unwrap();
        assert_eq!(db.sync_floor("a").unwrap(), 7);
        db.raise_sync_floor("a", 9).unwrap();
        assert_eq!(db.sync_floor("a").unwrap(), 9);
        assert_eq!(db.sync_floor("b").unwrap(), 0, "another target");
        db.raise_sync_floor("b", 2).unwrap();
        db.reset_sync_floor("a").unwrap();
        assert_eq!(db.sync_floor("a").unwrap(), 0);
        assert_eq!(db.sync_floor("b").unwrap(), 2, "a reset is per target");
    }

    #[test]
    fn the_encrypts_record_is_per_identity_and_can_be_forgotten() {
        let db = db();
        let a = "local-disk\0releases";
        assert!(!db.encrypts_recorded(a).unwrap());
        db.record_encrypts(a, "sealed").unwrap();
        db.record_encrypts(a, "again").unwrap();
        assert!(db.encrypts_recorded(a).unwrap());
        assert!(!db.encrypts_recorded("aws-dr\0releases").unwrap());
        db.forget_encrypts(a).unwrap();
        assert!(!db.encrypts_recorded(a).unwrap());
    }
}

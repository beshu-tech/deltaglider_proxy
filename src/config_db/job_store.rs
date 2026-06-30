// SPDX-License-Identifier: GPL-3.0-only

//! Shared job-machinery primitives for the three job subsystems
//! (replication, lifecycle, maintenance).
//!
//! Before this module each subsystem carried its own copy of the same
//! four SQL shapes: leader-lease acquire/renew, failure-ring pruning, and
//! the boot-time zombie-run scan. The copies had drifted in subtle ways
//! (renew-expiry boundary, ring ordering) — this module is the single
//! canonical implementation; the per-subsystem stores are thin
//! delegations binding their table/key names.
//!
//! ## Canonical semantics decisions
//!
//! * **Renew uses the conservative `>=`-expiry guard** (lifecycle's
//!   historical behavior): a worker whose lease has ALREADY lapsed must
//!   not resurrect it — another instance may have legitimately stolen
//!   the lease in the gap. Replication previously allowed a lapsed owner
//!   to renew; adopting the guard can only stop a runner earlier, never
//!   double-run.
//! * **Failure rings prune by `id DESC`** (insertion order). The old
//!   `occurred_at DESC` ordering had same-second ties, making which rows
//!   survive non-deterministic.
//!
//! Table/column names cannot be bound as `?` parameters, so every
//! function gates them through [`is_safe_sql_ident`] — all call sites
//! pass hardcoded literals; a non-literal fails loudly instead of
//! risking injection.

use rusqlite::{params, Connection, ToSql};

use super::{is_safe_sql_ident, ConfigDbError};

fn check_idents(idents: &[&str]) -> Result<(), ConfigDbError> {
    for ident in idents {
        if !is_safe_sql_ident(ident) {
            return Err(ConfigDbError::Other(format!(
                "refusing to interpolate unsafe SQL identifier: {ident:?}"
            )));
        }
    }
    Ok(())
}

/// Take the leader lease for `key` when it is free or expired.
/// Returns true when this owner now holds the lease.
///
/// Boundary tiling with [`renew_leader_lease`]: renew succeeds while
/// `expires_at >= now`, steal requires `expires_at < now` — the two
/// predicates partition the timeline, so the exact expiry instant can
/// never be both renewable by the owner AND stealable by a rival.
pub(crate) fn try_acquire_leader_lease(
    conn: &Connection,
    table: &str,
    key_col: &str,
    key: &dyn ToSql,
    owner: &str,
    now: i64,
    ttl_secs: i64,
) -> Result<bool, ConfigDbError> {
    check_idents(&[table, key_col])?;
    let expires_at = now.saturating_add(ttl_secs.max(1));
    let n = conn.execute(
        &format!(
            "UPDATE {table}
                SET leader_instance_id = ?,
                    leader_expires_at  = ?
              WHERE {key_col} = ?
                AND (
                    leader_instance_id IS NULL
                    OR leader_expires_at IS NULL
                    OR leader_expires_at < ?
                )"
        ),
        params![owner, expires_at, key, now],
    )?;
    Ok(n > 0)
}

/// Renew a lease this owner still holds. Fails (false) when the owner
/// doesn't match OR the lease already lapsed (`leader_expires_at < now`)
/// — a lapsed worker must stop, never resurrect.
pub(crate) fn renew_leader_lease(
    conn: &Connection,
    table: &str,
    key_col: &str,
    key: &dyn ToSql,
    owner: &str,
    now: i64,
    ttl_secs: i64,
) -> Result<bool, ConfigDbError> {
    check_idents(&[table, key_col])?;
    let expires_at = now.saturating_add(ttl_secs.max(1));
    let n = conn.execute(
        &format!(
            "UPDATE {table}
                SET leader_expires_at = ?
              WHERE {key_col} = ?
                AND leader_instance_id = ?
                AND (
                    leader_expires_at IS NULL
                    OR leader_expires_at >= ?
                )"
        ),
        params![expires_at, key, owner, now],
    )?;
    Ok(n > 0)
}

/// Release a lease this owner holds (no-op for other owners).
pub(crate) fn release_leader_lease(
    conn: &Connection,
    table: &str,
    key_col: &str,
    key: &dyn ToSql,
    owner: &str,
) -> Result<bool, ConfigDbError> {
    check_idents(&[table, key_col])?;
    let n = conn.execute(
        &format!(
            "UPDATE {table}
                SET leader_instance_id = NULL,
                    leader_expires_at  = NULL
              WHERE {key_col} = ?
                AND leader_instance_id = ?"
        ),
        params![key, owner],
    )?;
    Ok(n > 0)
}

/// Clear every expired lease in `table` (boot reconcile).
pub(crate) fn clear_stale_leases(
    conn: &Connection,
    table: &str,
    now: i64,
) -> Result<usize, ConfigDbError> {
    check_idents(&[table])?;
    let n = conn.execute(
        &format!(
            "UPDATE {table}
                SET leader_instance_id = NULL,
                    leader_expires_at  = NULL
              WHERE leader_expires_at IS NOT NULL AND leader_expires_at < ?"
        ),
        params![now],
    )?;
    Ok(n)
}

/// Bound a failure ring: keep the newest `max_retained` rows (by
/// insertion order) for `key`, delete the rest.
pub(crate) fn prune_failure_ring(
    conn: &Connection,
    table: &str,
    key_col: &str,
    key: &dyn ToSql,
    max_retained: u32,
) -> Result<(), ConfigDbError> {
    check_idents(&[table, key_col])?;
    conn.execute(
        &format!(
            "DELETE FROM {table}
              WHERE {key_col} = ?1
                AND id NOT IN (
                    SELECT id FROM {table}
                    WHERE {key_col} = ?2
                    ORDER BY id DESC
                    LIMIT ?3
                )"
        ),
        params![key, key, max_retained],
    )?;
    Ok(())
}

/// Boot-time zombie scan: run-history rows left `running` by a dead
/// process. Returns `(id, name, started_at)` triples; the caller applies
/// its subsystem's policy (mark failed / re-queue / stamp state).
/// Lease-aware: a `running` run is only a zombie if the rule's leader lease (on
/// `state_table`, keyed by `rule_name`) has LAPSED — never resurrect a run whose
/// lease a live peer still holds. Mirrors `maintenance_requeue_abandoned`'s
/// `leader_expires_at < now` guard. Harmless under today's node-local
/// coordination tables, but removes the asymmetry so a future shared config DB
/// can't fail/resurrect a peer's live run. `now` is the lapse cutoff.
pub(crate) fn find_zombie_runs(
    conn: &Connection,
    run_table: &str,
    state_table: &str,
    now: i64,
) -> Result<Vec<(i64, String, i64)>, ConfigDbError> {
    check_idents(&[run_table, state_table])?;
    let mut stmt = conn.prepare(&format!(
        "SELECT h.id, h.rule_name, h.started_at
           FROM {run_table} h
           LEFT JOIN {state_table} s ON s.rule_name = h.rule_name
          WHERE h.status IN ('running', 'cancelling')
            AND (s.leader_instance_id IS NULL
                 OR s.leader_expires_at IS NULL
                 OR s.leader_expires_at < ?)"
    ))?;
    let rows = stmt
        .query_map(params![now], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Flip one zombie run-history row to `failed`.
pub(crate) fn mark_run_failed(
    conn: &Connection,
    run_table: &str,
    run_id: i64,
    now: i64,
) -> Result<(), ConfigDbError> {
    check_idents(&[run_table])?;
    conn.execute(
        &format!("UPDATE {run_table} SET status = 'failed', finished_at = ? WHERE id = ?"),
        params![now, run_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal table carrying the lease columns + a failure ring table.
    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE jobs (
                name TEXT PRIMARY KEY,
                leader_instance_id TEXT,
                leader_expires_at INTEGER
            );
            INSERT INTO jobs (name) VALUES ('a');
            CREATE TABLE fails (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                rule_name TEXT NOT NULL,
                error TEXT NOT NULL
            );
            CREATE TABLE runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                rule_name TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                finished_at INTEGER,
                status TEXT NOT NULL
            );
            CREATE TABLE run_state (
                rule_name TEXT PRIMARY KEY,
                leader_instance_id TEXT,
                leader_expires_at INTEGER
            );",
        )
        .unwrap();
        c
    }

    fn lease(c: &Connection) -> (Option<String>, Option<i64>) {
        c.query_row(
            "SELECT leader_instance_id, leader_expires_at FROM jobs WHERE name='a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn acquire_truth_table() {
        let c = conn();
        // free → acquired
        assert!(try_acquire_leader_lease(&c, "jobs", "name", &"a", "w1", 100, 60).unwrap());
        assert_eq!(lease(&c), (Some("w1".into()), Some(160)));
        // held & unexpired by another → refused
        assert!(!try_acquire_leader_lease(&c, "jobs", "name", &"a", "w2", 120, 60).unwrap());
        // AT expiry (now == expires_at) the OWNER still renews — not stealable
        // (renew/steal predicates tile; see the doc comment).
        assert!(!try_acquire_leader_lease(&c, "jobs", "name", &"a", "w2", 160, 60).unwrap());
        // strictly past expiry → stealable
        assert!(try_acquire_leader_lease(&c, "jobs", "name", &"a", "w2", 161, 60).unwrap());
        assert_eq!(lease(&c).0.as_deref(), Some("w2"));
        // same-owner re-acquire while held: refused (not expired) — callers
        // use renew for that.
        assert!(!try_acquire_leader_lease(&c, "jobs", "name", &"a", "w2", 161, 60).unwrap());
        // unknown key → false
        assert!(!try_acquire_leader_lease(&c, "jobs", "name", &"zz", "w1", 0, 60).unwrap());
    }

    #[test]
    fn renew_truth_table() {
        let c = conn();
        try_acquire_leader_lease(&c, "jobs", "name", &"a", "w1", 100, 60).unwrap();
        // other owner → refused
        assert!(!renew_leader_lease(&c, "jobs", "name", &"a", "w2", 110, 60).unwrap());
        // owner, before expiry → renewed
        assert!(renew_leader_lease(&c, "jobs", "name", &"a", "w1", 110, 60).unwrap());
        assert_eq!(lease(&c).1, Some(170));
        // owner, AT expiry (now == expires_at) → still renewable (>= guard)
        assert!(renew_leader_lease(&c, "jobs", "name", &"a", "w1", 170, 60).unwrap());
        // owner, AFTER expiry → refused (lapsed leases never resurrect)
        assert!(!renew_leader_lease(&c, "jobs", "name", &"a", "w1", 231, 60).unwrap());
    }

    #[test]
    fn release_only_for_owner() {
        let c = conn();
        try_acquire_leader_lease(&c, "jobs", "name", &"a", "w1", 100, 60).unwrap();
        assert!(!release_leader_lease(&c, "jobs", "name", &"a", "w2").unwrap());
        assert!(release_leader_lease(&c, "jobs", "name", &"a", "w1").unwrap());
        assert_eq!(lease(&c), (None, None));
    }

    #[test]
    fn clear_stale_only_clears_expired() {
        let c = conn();
        c.execute("INSERT INTO jobs (name) VALUES ('b')", [])
            .unwrap();
        try_acquire_leader_lease(&c, "jobs", "name", &"a", "w1", 100, 60).unwrap(); // exp 160
        try_acquire_leader_lease(&c, "jobs", "name", &"b", "w2", 100, 600).unwrap(); // exp 700
        assert_eq!(clear_stale_leases(&c, "jobs", 200).unwrap(), 1);
        assert_eq!(lease(&c), (None, None));
    }

    #[test]
    fn failure_ring_keeps_newest_by_id() {
        let c = conn();
        for i in 0..10 {
            c.execute(
                "INSERT INTO fails (rule_name, error) VALUES ('r', ?)",
                params![format!("e{i}")],
            )
            .unwrap();
        }
        // another rule's rows are untouched
        c.execute(
            "INSERT INTO fails (rule_name, error) VALUES ('other', 'keep')",
            [],
        )
        .unwrap();
        prune_failure_ring(&c, "fails", "rule_name", &"r", 3).unwrap();
        let kept: Vec<String> = c
            .prepare("SELECT error FROM fails WHERE rule_name='r' ORDER BY id DESC")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(kept, vec!["e9", "e8", "e7"]);
        let other: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM fails WHERE rule_name='other'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(other, 1);
    }

    #[test]
    fn zombie_scan_and_mark() {
        let c = conn();
        c.execute_batch(
            "INSERT INTO runs (rule_name, started_at, status) VALUES ('r1', 10, 'running');
             INSERT INTO runs (rule_name, started_at, status) VALUES ('r2', 20, 'succeeded');",
        )
        .unwrap();
        // No lease row for r1 → LEFT JOIN yields NULL lease → it's a zombie.
        let zombies = find_zombie_runs(&c, "runs", "run_state", 100).unwrap();
        assert_eq!(zombies, vec![(1, "r1".to_string(), 10)]);

        // But if a LIVE peer lease exists (expires in the future), r1 is NOT a
        // zombie — never resurrect a run a peer still holds.
        c.execute(
            "INSERT INTO run_state (rule_name, leader_instance_id, leader_expires_at)
             VALUES ('r1', 'peer-b', 9999)",
            [],
        )
        .unwrap();
        assert!(
            find_zombie_runs(&c, "runs", "run_state", 100)
                .unwrap()
                .is_empty(),
            "a live peer lease protects the running row"
        );
        // Once the lease lapses (now > expiry), it's a zombie again.
        let after = find_zombie_runs(&c, "runs", "run_state", 10000).unwrap();
        assert_eq!(after, vec![(1, "r1".to_string(), 10)]);

        mark_run_failed(&c, "runs", 1, 99).unwrap();
        let (status, fin): (String, i64) = c
            .query_row("SELECT status, finished_at FROM runs WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((status.as_str(), fin), ("failed", 99));

        // A 'cancelling' row left by a crash mid-kill is ALSO a zombie — else it
        // stays 'cancelling' forever and the GUI polls it indefinitely (K3).
        c.execute(
            "INSERT INTO runs (rule_name, started_at, status) VALUES ('r3', 30, 'cancelling')",
            [],
        )
        .unwrap();
        let z = find_zombie_runs(&c, "runs", "run_state", 100).unwrap();
        assert!(
            z.iter().any(|(_, n, _)| n == "r3"),
            "a leaderless 'cancelling' row must be reconciled, got {z:?}"
        );
    }

    #[test]
    fn unsafe_identifiers_are_rejected_everywhere() {
        let c = conn();
        let bad = "jobs; DROP TABLE jobs";
        assert!(try_acquire_leader_lease(&c, bad, "name", &"a", "w", 0, 1).is_err());
        assert!(renew_leader_lease(&c, "jobs", bad, &"a", "w", 0, 1).is_err());
        assert!(release_leader_lease(&c, bad, "name", &"a", "w").is_err());
        assert!(clear_stale_leases(&c, bad, 0).is_err());
        assert!(prune_failure_ring(&c, bad, "rule_name", &"r", 1).is_err());
        assert!(find_zombie_runs(&c, bad, "run_state", 0).is_err());
        assert!(find_zombie_runs(&c, "runs", bad, 0).is_err());
        assert!(mark_run_failed(&c, bad, 1, 0).is_err());
    }
}

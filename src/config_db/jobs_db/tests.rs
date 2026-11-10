// SPDX-License-Identifier: BUSL-1.1

use super::*;
use crate::config_db::FallbackKind;
use rusqlite::types::Value;

const KEY: &str = "jobs-db-test-key-0123456789abcdef0123456789";

/// The IAM tables of the config DB (synced; `iam_merge` reads them).
const SYNCED_TABLES: &[&str] = &[
    "users",
    "permissions",
    "groups",
    "group_members",
    "group_permissions",
    "auth_providers",
    "group_mapping_rules",
    "external_identities",
    "session_revocations",
    "sync_state",
];

fn tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// `(name, type, notnull, default, pk)` of every column of `table`.
fn shape(conn: &Connection, table: &str) -> Vec<(String, String, i64, Option<String>, i64)> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    stmt.query_map([], |r| {
        Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn indexes(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 \
             AND name NOT LIKE 'sqlite_autoindex%' ORDER BY name",
        )
        .unwrap();
    stmt.query_map([table], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn fks(conn: &Connection, table: &str) -> Vec<(String, String, String, String)> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA foreign_key_list({table})"))
        .unwrap();
    let mut v: Vec<_> = stmt
        .query_map([], |r| Ok((r.get(2)?, r.get(3)?, r.get(4)?, r.get(6)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    v.sort();
    v
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// The rows of `table` without the jobs DB's own columns
/// (`JOBS_ONLY_COLUMNS`), so that both databases compare.
fn rows(conn: &Connection, table: &str) -> Vec<Vec<Value>> {
    let list = shape(conn, table)
        .into_iter()
        .map(|c| c.0)
        .filter(|c| {
            !super::JOBS_ONLY_COLUMNS
                .iter()
                .any(|(t, col, _)| *t == table && c == col)
        })
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn
        .prepare(&format!("SELECT {list} FROM {table} ORDER BY rowid"))
        .unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| (0..n).map(|i| r.get::<_, Value>(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn seq(conn: &Connection, table: &str) -> Option<i64> {
    conn.query_row(
        "SELECT seq FROM sqlite_sequence WHERE name = ?1",
        [table],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

/// What a release before the split leaves in its config DB: at least one
/// row in every node-local table, written straight to the config DB the way
/// the old `impl ConfigDb` stores wrote them. The outbox is pruned, so its
/// AUTOINCREMENT sequence (50) is above its last row (49) and the listener
/// cursor (50) points past every row.
fn seed_old_release_job_rows(db: &ConfigDb) {
    db.conn
        .execute_batch(
            "INSERT INTO replication_state (rule_name, next_due_at, last_status, continuation_token)
                 VALUES ('r1', 10, 'idle', 'tok-r1');
             INSERT INTO replication_run_history (rule_name, started_at, status, reconstructed)
                 VALUES ('r1', 1, 'done', 3);
             INSERT INTO replication_failures (rule_name, run_id, occurred_at, source_key, dest_key, error_message)
                 VALUES ('r1', 1, 1, 's', 'd', 'boom');
             INSERT INTO replication_object_failures (rule_name, source_key, consecutive_failures)
                 VALUES ('r1', 's', 2);
             INSERT INTO replication_parity_objects (rule_name, side, dest_key, size, updated_at, created_at)
                 VALUES ('r1', 'source', 'k', 5, 1, 7);
             INSERT INTO replication_parity (rule_name, status, progress_total) VALUES ('r1', 'done', 9);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 50)
                 INSERT INTO event_outbox (kind, bucket, object_key, source, occurred_at)
                 SELECT 'object.put', 'b', 'k' || i, 's3', i FROM n;
             DELETE FROM event_outbox WHERE id <= 45 OR id = 50;
             INSERT INTO event_deliveries (outbox_id, endpoint_id, status, updated_at)
                 VALUES (46, 'ep', 'delivered', 1);
             INSERT INTO listener_cursors (listener_name, last_event_id) VALUES ('replication', 50);
             INSERT INTO lifecycle_state (rule_name, next_due_at, last_status, cursor_scope)
                 VALUES ('l1', 1, 'idle', 'b|');
             INSERT INTO lifecycle_run_history (rule_name, started_at, status) VALUES ('l1', 1, 'done');
             INSERT INTO lifecycle_failures (rule_name, run_id, occurred_at, bucket, object_key, error_message)
                 VALUES ('l1', 1, 1, 'b', 'k', 'boom');
             INSERT INTO maintenance_jobs (bucket, status, created_at, updated_at, params)
                 VALUES ('b', 'running', 1, 1, '{}');
             INSERT INTO maintenance_failures (job_id, object_key, error, created_at)
                 VALUES (1, 'k', 'boom', 1);",
        )
        .unwrap();
}

fn job_rows(conn: &Connection) -> Vec<(String, Vec<Vec<Value>>)> {
    NODE_LOCAL_TABLES
        .iter()
        .map(|t| (t.to_string(), rows(conn, t)))
        .collect()
}

struct Node {
    _dir: tempfile::TempDir,
    config_path: PathBuf,
    jobs_path: PathBuf,
}

fn node() -> Node {
    let dir = tempfile::tempdir().unwrap();
    Node {
        config_path: dir.path().join("deltaglider_config.db"),
        jobs_path: dir.path().join("deltaglider_jobs.db"),
        _dir: dir,
    }
}

/// A config DB as an older release leaves it: IAM and job rows in one file.
fn old_release_config_db(path: &Path) -> ConfigDb {
    let db = ConfigDb::open_or_create(path, KEY).unwrap();
    db.create_user("alice", "AKALICE00001", "secret0000001", true, &[])
        .unwrap();
    seed_old_release_job_rows(&db);
    db
}

/// Every table of the config DB is classified, and the jobs DB has the
/// same shape for each node-local table. A new table in the config DB fails
/// this until it is put on one side.
#[test]
fn jobs_tables_match_the_config_db() {
    let config = ConfigDb::in_memory(KEY).unwrap();
    let jobs = JobsDb::in_memory(KEY).unwrap();
    let mut classified: Vec<String> = SYNCED_TABLES
        .iter()
        .chain(NODE_LOCAL_TABLES)
        .map(|s| s.to_string())
        .collect();
    classified.sort();
    assert_eq!(
        tables(&config.conn),
        classified,
        "unclassified config DB table"
    );
    let mut expected: Vec<String> = NODE_LOCAL_TABLES
        .iter()
        .chain(JOBS_ONLY_TABLES)
        .map(|s| s.to_string())
        .chain(["jobs_meta".to_string()])
        .collect();
    expected.sort();
    assert_eq!(tables(&jobs.conn), expected);
    for t in NODE_LOCAL_TABLES {
        // The jobs DB's own columns come last, and only it has them.
        let mut jobs_shape = shape(&jobs.conn, t);
        jobs_shape.retain(|c| {
            !super::JOBS_ONLY_COLUMNS
                .iter()
                .any(|(table, column, _)| table == t && c.0 == *column)
        });
        assert_eq!(shape(&config.conn, t), jobs_shape, "{t}: columns");
        assert_eq!(
            fks(&config.conn, t),
            fks(&jobs.conn, t),
            "{t}: foreign keys"
        );
        let have = indexes(&jobs.conn, t);
        for i in indexes(&config.conn, t) {
            // The jobs DB replaced it: a delete job does not hold its bucket.
            let i = if i == "idx_maint_active_bucket" {
                "idx_maint_active_bucket_v2".to_string()
            } else {
                i
            };
            assert!(have.contains(&i), "{t}: index {i} missing from the jobs DB");
        }
    }
}

#[test]
fn the_move_keeps_every_row_and_empties_the_config_db() {
    let n = node();
    let config = old_release_config_db(&n.config_path);
    let before = job_rows(&config.conn);
    for (t, r) in &before {
        assert!(!r.is_empty(), "the seed misses {t}");
    }
    let outbox_seq = seq(&config.conn, "event_outbox").unwrap();
    assert_eq!(outbox_seq, 50);

    let jobs = JobsDb::open_live(&n.jobs_path, &ConfigDbKeys::primary_only(KEY)).unwrap();
    let report = jobs.adopt_from(&config, KEY).unwrap();
    assert!(!report.leftovers);
    assert_eq!(job_rows(&jobs.conn), before, "rows differ after the move");
    for t in NODE_LOCAL_TABLES {
        assert_eq!(
            count(&config.conn, t),
            0,
            "{t} still has rows in the config DB"
        );
    }
    assert!(jobs.adopted().unwrap());
    // IAM is untouched.
    assert_eq!(config.load_users().unwrap()[0].name, "alice");
    // Ids keep growing past the config DB's sequence: the next event is
    // above the listener cursor (50), so the consumer sees it.
    assert_eq!(seq(&jobs.conn, "event_outbox"), Some(50));
    let event = crate::event_outbox::NewEvent::new(
        crate::event_outbox::EventKind::ObjectCreated,
        "b",
        "next",
        crate::event_outbox::EventSource::S3Api,
        51,
        serde_json::json!({}),
    );
    assert_eq!(jobs.event_outbox_insert(&event).unwrap(), 51);
    // A second boot does nothing.
    let again = jobs.adopt_from(&config, KEY).unwrap();
    assert_eq!(again, AdoptReport::default());
}

/// A crash after the copy committed, before the delete in the config DB:
/// the next boot finds `adopted = 1` and only deletes the leftovers.
#[test]
fn a_crash_between_the_copy_and_the_delete_heals_on_the_next_boot() {
    let n = node();
    let config = old_release_config_db(&n.config_path);
    let before = job_rows(&config.conn);
    {
        let jobs = JobsDb::open_or_create(&n.jobs_path, KEY).unwrap();
        jobs.copy_from_config(&n.config_path, KEY).unwrap();
        // crash: the config DB still has its rows
    }
    assert_eq!(job_rows(&config.conn), before);
    let jobs = JobsDb::open_live(&n.jobs_path, &ConfigDbKeys::primary_only(KEY)).unwrap();
    let report = jobs.adopt_from(&config, KEY).unwrap();
    assert!(report.leftovers);
    assert!(report.copied.is_empty(), "copied twice: {report:?}");
    assert_eq!(job_rows(&jobs.conn), before, "lost or duplicated rows");
    for t in NODE_LOCAL_TABLES {
        assert_eq!(count(&config.conn, t), 0, "{t}");
    }
}

/// A failure inside the copy transaction (here: the last table) rolls the
/// whole copy back; the next boot copies again from scratch.
#[test]
fn a_failed_copy_leaves_both_files_as_they_were() {
    let n = node();
    let config = old_release_config_db(&n.config_path);
    let before = job_rows(&config.conn);
    {
        let jobs = JobsDb::open_or_create(&n.jobs_path, KEY).unwrap();
        jobs.conn
            .execute_batch(
                "CREATE TEMP TRIGGER boom BEFORE INSERT ON main.maintenance_failures
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let err = jobs.adopt_from(&config, KEY).unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        assert!(!jobs.adopted().unwrap());
        for t in NODE_LOCAL_TABLES {
            assert_eq!(count(&jobs.conn, t), 0, "{t}: a partial copy stayed");
        }
    }
    assert_eq!(job_rows(&config.conn), before, "the config DB changed");
    let jobs = JobsDb::open_live(&n.jobs_path, &ConfigDbKeys::primary_only(KEY)).unwrap();
    jobs.adopt_from(&config, KEY).unwrap();
    assert_eq!(job_rows(&jobs.conn), before);
}

/// After a downgrade, an older binary writes job rows into the config DB
/// again. Upgrading once more keeps the jobs DB and drops those rows (their
/// ids collide with the jobs DB's).
#[test]
fn rows_written_after_a_downgrade_are_dropped_on_the_next_upgrade() {
    let n = node();
    let config = old_release_config_db(&n.config_path);
    let jobs = JobsDb::open_live(&n.jobs_path, &ConfigDbKeys::primary_only(KEY)).unwrap();
    jobs.adopt_from(&config, KEY).unwrap();
    let moved = job_rows(&jobs.conn);
    config
        .conn
        .execute_batch(
            "INSERT INTO listener_cursors (listener_name, last_event_id) VALUES ('old', 3);",
        )
        .unwrap();
    let report = jobs.adopt_from(&config, KEY).unwrap();
    assert!(report.leftovers);
    assert_eq!(report.cleared, vec![("listener_cursors".to_string(), 1)]);
    assert_eq!(job_rows(&jobs.conn), moved);
    assert_eq!(count(&config.conn, "listener_cursors"), 0);
}

/// Measure: the synced blob (the config DB file) before and after the move,
/// on a node with 10k parity rows and 10k outbox rows.
#[test]
fn the_synced_blob_shrinks_to_the_iam_rows() {
    let n = node();
    let config = ConfigDb::open_or_create(&n.config_path, KEY).unwrap();
    for i in 0..20 {
        config
            .create_user(
                &format!("user{i}"),
                &format!("AKUSER{i:06}"),
                "secret0000001",
                true,
                &[],
            )
            .unwrap();
    }
    config
        .conn
        .execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000)
             INSERT INTO replication_parity_objects
                 (rule_name, side, dest_key, sha256, size, etag, stored_etag, updated_at, created_at)
             SELECT 'mirror', 'source', 'releases/v1.' || i || '/app-linux-amd64.tar.gz',
                    printf('%064x', i), 1048576 + i, printf('\"%032x\"', i),
                    printf('\"%032x\"', i * 7), 1700000000, 1700000000000 FROM n;
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000)
             INSERT INTO event_outbox (kind, bucket, object_key, source, occurred_at, payload_json)
             SELECT 'object.put', 'releases', 'releases/v1.' || i || '/app-linux-amd64.tar.gz',
                    's3', 1700000000 + i,
                    '{\"size\":1048576,\"etag\":\"' || printf('%032x', i) ||
                    '\",\"storage\":\"delta\",\"user\":\"ci-uploader\",\"content_type\":\"application/gzip\"}'
             FROM n;",
        )
        .unwrap();
    let size = |p: &Path| std::fs::metadata(p).unwrap().len();
    let before = size(&n.config_path);
    let jobs = JobsDb::open_live(&n.jobs_path, &ConfigDbKeys::primary_only(KEY)).unwrap();
    jobs.adopt_from(&config, KEY).unwrap();
    let after = size(&n.config_path);
    let empty = {
        let p = n.config_path.with_file_name("iam-only.db");
        let db = ConfigDb::open_or_create(&p, KEY).unwrap();
        for i in 0..20 {
            db.create_user(
                &format!("user{i}"),
                &format!("AKUSER{i:06}"),
                "secret0000001",
                true,
                &[],
            )
            .unwrap();
        }
        size(&p)
    };
    // What the empty node-local tables cost in the blob (kept for older
    // peers and downgrades): the same file with the tables dropped.
    let dropped = {
        let p = n.config_path.with_file_name("dropped.db");
        std::fs::copy(&n.config_path, &p).unwrap();
        let db = ConfigDb::open_or_create(&p, KEY).unwrap();
        for t in NODE_LOCAL_TABLES.iter().rev() {
            db.conn.execute_batch(&format!("DROP TABLE {t};")).unwrap();
        }
        db.conn.execute_batch("VACUUM;").unwrap();
        size(&p)
    };
    eprintln!(
        "synced blob: {before} bytes before the move, {after} bytes after \
         (a fresh config DB with the same IAM rows: {empty} bytes; without the \
         empty job tables: {dropped} bytes); jobs DB: {} bytes",
        size(&n.jobs_path)
    );
    assert!(after * 20 < before, "{before} -> {after}");
    assert_eq!(count(&jobs.conn, "replication_parity_objects"), 10_000);
    assert_eq!(count(&jobs.conn, "event_outbox"), 10_000);
}

/// Mixed version, old -> new: a peer on a release before the split uploads
/// a copy WITH job rows. The new node merges its IAM; the job rows of the
/// copy reach neither the live config DB nor the jobs DB.
#[test]
fn a_new_node_merges_an_old_peer_copy_and_ignores_its_job_rows() {
    let n = node();
    let config = ConfigDb::open_or_create(&n.config_path, KEY).unwrap();
    config
        .create_user("local", "AKLOCAL00001", "secret0000001", true, &[])
        .unwrap();
    let jobs = JobsDb::open_live(&n.jobs_path, &ConfigDbKeys::primary_only(KEY)).unwrap();
    jobs.adopt_from(&config, KEY).unwrap();
    jobs.listener_cursor_advance("replication", 4242, 1)
        .unwrap();
    let jobs_before = job_rows(&jobs.conn);

    let peer_path = n.config_path.with_file_name("peer.db");
    drop(old_release_config_db(&peer_path));

    config.merge_iam_from(&peer_path, None, KEY).unwrap();
    let mut names: Vec<String> = config
        .load_users()
        .unwrap()
        .into_iter()
        .map(|u| u.name)
        .collect();
    names.sort();
    assert_eq!(names, ["alice", "local"]);
    for t in NODE_LOCAL_TABLES {
        assert_eq!(count(&config.conn, t), 0, "{t}: peer job rows imported");
    }
    assert_eq!(job_rows(&jobs.conn), jobs_before);
    assert_eq!(jobs.listener_cursor_load("replication").unwrap(), 4242);
    // A later boot does not pick them up either: the move reads only the
    // live config DB, which has none.
    let report = jobs.adopt_from(&config, KEY).unwrap();
    assert_eq!(report, AdoptReport::default());
}

/// Mixed version, new -> old: the merge code is the same in both releases
/// (this change does not touch `iam_merge`). A node that still keeps its
/// job rows in the config DB merges a new copy (empty job tables, and a
/// copy without those tables at all) and keeps its own job rows.
#[test]
fn an_old_node_merges_a_new_copy_and_keeps_its_job_rows() {
    let n = node();
    // The new copy: a moved config DB.
    let new_copy = {
        let config = old_release_config_db(&n.config_path);
        config
            .create_user("bob", "AKBOB0000001", "secret0000001", true, &[])
            .unwrap();
        let jobs = JobsDb::open_live(&n.jobs_path, &ConfigDbKeys::primary_only(KEY)).unwrap();
        jobs.adopt_from(&config, KEY).unwrap();
        n.config_path.clone()
    };
    let dropped_copy = n.config_path.with_file_name("dropped.db");
    std::fs::copy(&new_copy, &dropped_copy).unwrap();
    {
        let db = ConfigDb::open_or_create(&dropped_copy, KEY).unwrap();
        for t in NODE_LOCAL_TABLES.iter().rev() {
            db.conn.execute_batch(&format!("DROP TABLE {t};")).unwrap();
        }
    }
    for remote in [&new_copy, &dropped_copy] {
        let old_path = n.config_path.with_file_name("old-node.db");
        let _ = std::fs::remove_file(&old_path);
        let old = ConfigDb::open_or_create(&old_path, KEY).unwrap();
        seed_old_release_job_rows(&old);
        let before = job_rows(&old.conn);
        old.merge_iam_from(remote, None, KEY).unwrap();
        let mut names: Vec<String> = old
            .load_users()
            .unwrap()
            .into_iter()
            .map(|u| u.name)
            .collect();
        names.sort();
        assert_eq!(names, ["alice", "bob"], "{}", remote.display());
        assert_eq!(job_rows(&old.conn), before, "{}", remote.display());
    }
}

/// A rotation (`DGP_CONFIG_DB_KEY_PREVIOUS`) moves the jobs DB to the new
/// key on the same boot as the config DB.
#[test]
fn a_key_rotation_rekeys_both_files() {
    let n = node();
    const OLD: &str = "old-config-db-key-0123456789abcdef01234567";
    let config = old_release_config_db(&n.config_path);
    drop(config);
    // Keyed with OLD before the rotation.
    {
        let config = ConfigDb::open_with_keys(&n.config_path, &ConfigDbKeys::primary_only(KEY))
            .unwrap()
            .0;
        let jobs = JobsDb::open_live(&n.jobs_path, &ConfigDbKeys::primary_only(KEY)).unwrap();
        jobs.adopt_from(&config, KEY).unwrap();
    }
    rekey_file_for_test(&n.config_path, KEY, OLD);
    rekey_file_for_test(&n.jobs_path, KEY, OLD);
    let keys = ConfigDbKeys::primary_only(KEY).with_fallback(FallbackKind::PreviousKey, OLD);
    let (config, how) = ConfigDb::open_with_keys(&n.config_path, &keys).unwrap();
    assert_eq!(how, OpenedWith::Migrated(FallbackKind::PreviousKey));
    let jobs = JobsDb::open_live(&n.jobs_path, &keys).unwrap();
    assert!(super::super::probe_key(&n.jobs_path, KEY).unwrap());
    assert!(!super::super::probe_key(&n.jobs_path, OLD).unwrap());
    assert_eq!(count(&jobs.conn, "event_outbox"), 4);
    assert_eq!(config.load_users().unwrap().len(), 1);
}

fn rekey_file_for_test(path: &Path, from: &str, to: &str) {
    super::super::rekey_file(path, from, to).unwrap();
}

/// A jobs DB that no key of the keyring opens is an error, and the file
/// stays as it is (same bytes, no new file next to it). Before, it was
/// renamed to `.unreadable-*` and a new, empty DB took its place: the
/// running jobs were forgotten.
#[test]
fn a_jobs_db_that_no_key_opens_is_an_error_and_stays() {
    let n = node();
    const OLD: &str = "old-config-db-key-0123456789abcdef01234567";
    drop(JobsDb::open_or_create(&n.jobs_path, OLD).unwrap());
    let before = std::fs::read(&n.jobs_path).unwrap();
    let files = || {
        std::fs::read_dir(n.jobs_path.parent().unwrap())
            .unwrap()
            .count()
    };
    let count_before = files();
    let err = JobsDb::open_live(&n.jobs_path, &ConfigDbKeys::primary_only(KEY))
        .err()
        .expect("no key opens it");
    assert!(matches!(err, ConfigDbError::WrongPassphrase(_)), "{err}");
    assert_eq!(
        std::fs::read(&n.jobs_path).unwrap(),
        before,
        "the file changed"
    );
    assert_eq!(files(), count_before, "a file was added or removed");
    // The fallback of the keyring opens it, and moves it to the primary key.
    let keys = ConfigDbKeys::primary_only(KEY).with_fallback(FallbackKind::PreviousKey, OLD);
    drop(JobsDb::open_live(&n.jobs_path, &keys).unwrap());
    assert!(super::super::probe_key(&n.jobs_path, KEY).unwrap());
}

#[test]
fn the_boot_error_names_the_fix() {
    let msg = no_key_opens_the_jobs_db(Path::new("/d/deltaglider_jobs.db"), "bad key");
    assert!(msg.contains("/d/deltaglider_jobs.db"), "{msg}");
    assert!(msg.contains("DGP_CONFIG_DB_KEY_PREVIOUS"), "{msg}");
    assert!(msg.contains("untouched"), "{msg}");
}

#[test]
fn a_newer_jobs_db_is_refused() {
    let n = node();
    {
        let db = JobsDb::open_or_create(&n.jobs_path, KEY).unwrap();
        db.conn
            .pragma_update(None, "user_version", JOBS_SCHEMA_VERSION + 1)
            .unwrap();
    }
    let err = JobsDb::open_live(&n.jobs_path, &ConfigDbKeys::primary_only(KEY))
        .err()
        .expect("a newer jobs DB must not open");
    assert!(matches!(err, ConfigDbError::SchemaTooNew { .. }), "{err}");
}

/// Pure: the node-local tables that `text` reads or writes in SQL.
fn node_local_sql(text: &str) -> Vec<&'static str> {
    NODE_LOCAL_TABLES
        .iter()
        .chain(JOBS_ONLY_TABLES)
        .copied()
        .filter(|t| {
            ["FROM", "INTO", "UPDATE", "JOIN"].iter().any(|kw| {
                text.contains(&format!("{kw} {t}")) || text.contains(&format!("{kw} main.{t}"))
            })
        })
        .collect()
}

/// Fix the class: job rows go to the jobs DB only. A production file that
/// has SQL on a node-local table never names `ConfigDb` (whose `conn` is
/// `pub(crate)`), so no code path can write a job row into the synced file.
#[test]
fn node_local_sql_never_runs_on_the_config_db() {
    let mut offenders = Vec::new();
    let mut seen = 0;
    let files = crate::source_scan::rust_files("src");
    let tests = crate::source_scan::out_of_line_test_modules(&files);
    for path in files.iter().filter(|f| !tests.contains(f)) {
        let rel = crate::source_scan::rel(path);
        if rel == "src/config_db/jobs_db.rs" {
            continue; // the move itself: reads the config DB's copies once
        }
        let text = std::fs::read_to_string(path).unwrap();
        let prod = crate::source_scan::prod_text(&text);
        let tables = node_local_sql(&prod);
        if tables.is_empty() {
            continue;
        }
        seen += 1;
        let names_config_db = prod
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|w| w == "ConfigDb");
        if names_config_db {
            offenders.push(format!("{rel}: {tables:?}"));
        }
    }
    assert!(seen >= 4, "the scan found the job stores ({seen})");
    assert!(
        offenders.is_empty(),
        "SQL on node-local tables next to ConfigDb (use JobsDb): {offenders:#?}"
    );
}

#[test]
fn node_local_sql_matches_reads_and_writes() {
    assert_eq!(
        node_local_sql("SELECT x FROM event_outbox"),
        ["event_outbox"]
    );
    assert_eq!(
        node_local_sql("UPDATE main.lifecycle_state SET"),
        ["lifecycle_state"]
    );
    assert!(node_local_sql("SELECT * FROM users").is_empty());
}

fn pragma_text(conn: &Connection, name: &str) -> String {
    conn.query_row(&format!("PRAGMA {name}"), [], |r| {
        r.get::<_, rusqlite::types::Value>(0)
    })
    .map(|v| match v {
        Value::Text(t) => t,
        Value::Integer(i) => i.to_string(),
        other => format!("{other:?}"),
    })
    .unwrap()
}

/// E6/F20: the jobs DB is node-local and never synced, so it runs in WAL
/// (one fsync per commit, readers never block the writer) with
/// `synchronous = NORMAL`. A rollback journal with FULL cost about 10 ms per
/// commit. The config DB stays as it is: it is uploaded as ONE file.
#[test]
fn the_jobs_db_opens_in_wal_and_the_config_db_does_not() {
    let n = node();
    let jobs = JobsDb::open_or_create(&n.jobs_path, KEY).unwrap();
    assert_eq!(pragma_text(&jobs.conn, "journal_mode"), "wal");
    assert_eq!(pragma_text(&jobs.conn, "synchronous"), "1", "NORMAL");
    assert_eq!(
        pragma_text(&jobs.conn, "auto_vacuum"),
        "2",
        "WAL must not lose incremental auto-vacuum"
    );
    let config = ConfigDb::open_or_create(&n.config_path, KEY).unwrap();
    assert_ne!(
        pragma_text(&config.conn, "journal_mode"),
        "wal",
        "the config DB is synced as one file: a WAL would leave committed rows out of it"
    );
}

/// A jobs DB made by an older release (rollback journal) moves to WAL on its
/// next open, and keeps its rows.
#[test]
fn an_older_jobs_db_moves_to_wal_and_keeps_its_rows() {
    let n = node();
    {
        let db = JobsDb::open_or_create(&n.jobs_path, KEY).unwrap();
        db.conn
            .execute_batch(
                "PRAGMA journal_mode = DELETE; PRAGMA synchronous = FULL;
                 INSERT INTO listener_cursors (listener_name, last_event_id) VALUES ('a', 7);",
            )
            .unwrap();
    }
    let db = JobsDb::open_or_create(&n.jobs_path, KEY).unwrap();
    assert_eq!(pragma_text(&db.conn, "journal_mode"), "wal");
    assert_eq!(db.listener_cursor_load("a").unwrap(), 7);
}

/// E6/F20 + rekey: a committed row that still sits in the `-wal` file (a
/// second connection is open, so nothing checkpointed it) must be in the
/// re-encrypted file. A rekey that copies only the main file loses it, and a
/// stale `-wal` that the new file does not match must not stay beside it.
#[test]
fn a_rekey_of_a_wal_jobs_db_keeps_rows_that_only_the_wal_holds() {
    let n = node();
    const OLD: &str = "old-config-db-key-0123456789abcdef01234567";
    let live = JobsDb::open_or_create(&n.jobs_path, OLD).unwrap();
    live.conn
        .execute(
            "INSERT INTO listener_cursors (listener_name, last_event_id) VALUES ('wal-only', 42)",
            [],
        )
        .unwrap();
    let wal = {
        let mut name = n.jobs_path.file_name().unwrap().to_os_string();
        name.push("-wal");
        n.jobs_path.with_file_name(name)
    };
    assert!(
        std::fs::metadata(&wal).map_or(0, |m| m.len()) > 0,
        "the test needs frames in the WAL"
    );
    super::super::rekey_file(&n.jobs_path, OLD, KEY).unwrap();
    drop(live);
    assert!(super::super::probe_key(&n.jobs_path, KEY).unwrap());
    let db = JobsDb::open_or_create(&n.jobs_path, KEY).unwrap();
    assert_eq!(
        db.listener_cursor_load("wal-only").unwrap(),
        42,
        "a row that only the WAL held is gone after the rekey"
    );
}

/// The same through the boot path: a fallback key opens a WAL jobs DB.
#[test]
fn a_fallback_key_opens_a_wal_jobs_db_and_moves_it() {
    let n = node();
    const OLD: &str = "old-config-db-key-0123456789abcdef01234567";
    {
        let db = JobsDb::open_or_create(&n.jobs_path, OLD).unwrap();
        db.conn
            .execute(
                "INSERT INTO listener_cursors (listener_name, last_event_id) VALUES ('x', 9)",
                [],
            )
            .unwrap();
    }
    let keys = ConfigDbKeys::primary_only(KEY).with_fallback(FallbackKind::PreviousKey, OLD);
    let db = JobsDb::open_live(&n.jobs_path, &keys).unwrap();
    assert_eq!(pragma_text(&db.conn, "journal_mode"), "wal");
    assert_eq!(db.listener_cursor_load("x").unwrap(), 9);
}

fn failure_db() -> (tempfile::TempDir, JobsDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = JobsDb::open_or_create(&dir.path().join("f.db"), KEY).unwrap();
    db.replication_ensure_state("r", 1).unwrap();
    db.lifecycle_ensure_state("l", 1).unwrap();
    (dir, db)
}

fn replication_failure(db: &JobsDb, i: usize) {
    db.replication_record_failure(
        "r",
        crate::replication::state_store::FailureInsert {
            run_id: None,
            occurred_at: 1,
            source_key: &format!("k{i}"),
            dest_key: &format!("k{i}"),
            error_message: "boom",
        },
        20,
    )
    .unwrap();
}

/// E6/F20 class guard: recording one failure (the insert AND the ring
/// prune) is ONE commit, for every job kind. Before, the insert and the
/// prune were two commits, each with its own fsync.
#[test]
fn recording_a_failure_is_one_commit() {
    let (_dir, db) = failure_db();
    let commits = db.count_commits();
    replication_failure(&db, 0);
    assert_eq!(commits.get(), 1, "replication failure");
    db.lifecycle_record_failure(
        "l",
        crate::lifecycle::state_store::LifecycleFailureInsert {
            run_id: None,
            occurred_at: 1,
            bucket: "b",
            object_key: "k",
            error_message: "boom",
        },
        20,
    )
    .unwrap();
    assert_eq!(commits.get(), 2, "lifecycle failure");
    let job = db
        .maintenance_create_job("reencrypt", "b", "counting", None, "admin", 1)
        .unwrap()
        .unwrap();
    let before = commits.get();
    db.maintenance_record_failure(job, "k", "boom", 20).unwrap();
    assert_eq!(commits.get() - before, 1, "maintenance failure");
    let before = commits.get();
    db.replication_record_event_failure("r", 1, "ek", "ek", "boom", 20)
        .unwrap();
    assert_eq!(commits.get() - before, 1, "event-path failure");
    let before = commits.get();
    db.replication_record_object_failure("r", "ok", "boom", 1)
        .unwrap();
    assert_eq!(commits.get() - before, 1, "consecutive-failure ledger");
}

/// A page of failures that one `atomic` settles is ONE commit, however many
/// rows it has: the bound that a page of N failures costs.
#[test]
fn a_page_of_failures_settles_in_one_commit() {
    let (_dir, db) = failure_db();
    let commits = db.count_commits();
    db.atomic(|db| {
        for i in 0..100 {
            replication_failure(db, i);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(commits.get(), 1, "100 failures, one commit");
    assert_eq!(db.replication_failure_count("r").unwrap(), 20, "the ring");
}

/// An error inside `atomic` rolls the whole page back.
#[test]
fn a_failed_atomic_page_leaves_nothing() {
    let (_dir, db) = failure_db();
    let r: Result<(), _> = db.atomic(|db| {
        replication_failure(db, 1);
        Err(ConfigDbError::Other("stop".into()))
    });
    assert!(r.is_err());
    assert_eq!(db.replication_failure_count("r").unwrap(), 0);
}

/// Evidence for the PR (run by hand): commits per second of a failure
/// record, rollback journal + FULL (the old open) against WAL + NORMAL.
/// `cargo test --lib --release failure_commit_rate -- --ignored --nocapture`
#[test]
#[ignore = "manual: cargo test --lib failure_commit_rate -- --ignored --nocapture"]
fn failure_commit_rate() {
    const N: usize = 300;
    let run = |old: bool| {
        let dir = tempfile::tempdir().unwrap();
        let db = JobsDb::open_or_create(&dir.path().join("m.db"), KEY).unwrap();
        db.replication_ensure_state("r", 1).unwrap();
        if old {
            db.conn
                .execute_batch("PRAGMA journal_mode = DELETE; PRAGMA synchronous = FULL;")
                .unwrap();
        }
        let t = std::time::Instant::now();
        for i in 0..N {
            replication_failure(&db, i);
        }
        let e = t.elapsed();
        (e, N as f64 / e.as_secs_f64())
    };
    let (old, old_rate) = run(true);
    let (new, new_rate) = run(false);
    eprintln!(
        "MEASURE {N} failure records: journal+FULL {old:?} ({old_rate:.0}/s), WAL+NORMAL {new:?} ({new_rate:.0}/s)"
    );
}

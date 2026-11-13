// SPDX-License-Identifier: BUSL-1.1

//! The disk level (`disk_watch`) gates NEW spool reservations of writes
//! and nothing else: a running reservation, a read and a delete go on.

use super::resources::SpoolWait;
use super::*;
use crate::config::Config;
use crate::disk_watch::Level;
use crate::storage::{DynStorageBackend, FilesystemBackend};
use std::collections::HashMap;

const MIB: u64 = 1024 * 1024;

async fn engine() -> (tempfile::TempDir, DynEngine) {
    let dir = tempfile::tempdir().unwrap();
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(
        FilesystemBackend::new(dir.path().join("data"))
            .await
            .unwrap(),
    );
    let mut engine =
        DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
    engine.spool = Arc::new(
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 64 * MIB).unwrap(),
    );
    engine.create_bucket("b").await.unwrap();
    (dir, engine)
}

fn set_level(engine: &DynEngine, level: Level) {
    engine
        .spool
        .gate()
        .set(level, format!("at level {}", level.name()));
}

fn refused(r: Result<impl std::fmt::Debug, EngineError>) -> String {
    match r {
        Err(EngineError::Overloaded(m)) => m,
        other => panic!("want Overloaded, got {other:?}"),
    }
}

#[tokio::test]
async fn a_new_write_reservation_is_refused_at_high_and_critical() {
    let (_dir, engine) = engine().await;
    for level in [Level::Ok, Level::Low] {
        set_level(&engine, level);
        engine
            .reserve_ingest_spool(MIB, 0, SpoolWait::Queue)
            .await
            .unwrap();
        engine.codec_source_spool_now(1024).unwrap();
    }
    for level in [Level::High, Level::Critical] {
        set_level(&engine, level);
        for wait in [SpoolWait::Queue, SpoolWait::Now] {
            let m = refused(engine.reserve_ingest_spool(MIB, 0, wait).await);
            assert!(m.contains(level.name()), "{m}");
        }
        let m = refused(engine.codec_source_spool_now(1024));
        assert!(m.contains(level.name()), "{m}");
    }
}

#[tokio::test]
async fn a_running_write_keeps_its_budget_and_a_recovery_admits_again() {
    let (_dir, engine) = engine().await;
    set_level(&engine, Level::Ok);
    let held = engine
        .reserve_ingest_spool(MIB, 0, SpoolWait::Queue)
        .await
        .unwrap();
    set_level(&engine, Level::Critical);
    // An op that holds a spool already (a multipart upload's next part)
    // runs to its end.
    engine
        .reserve_ingest_spool(MIB, held.reserved_mib(), SpoolWait::Queue)
        .await
        .unwrap();
    set_level(&engine, Level::Low);
    engine
        .reserve_ingest_spool(MIB, 0, SpoolWait::Queue)
        .await
        .unwrap();
}

#[tokio::test]
async fn reads_and_deletes_go_on_at_critical() {
    let (_dir, engine) = engine().await;
    set_level(&engine, Level::Ok);
    engine
        .store("b", "k.txt", b"hello", None, HashMap::new())
        .await
        .unwrap();
    set_level(&engine, Level::Critical);
    let (body, _) = engine.retrieve("b", "k.txt").await.unwrap();
    assert_eq!(body, b"hello");
    // The read path takes its spool without the write gate.
    engine.spool_acquire(MIB).await.unwrap();
    engine.delete("b", "k.txt").await.unwrap();
}

/// CLASS GUARD: every function of `resources.rs` that reserves spool for a
/// WRITE asks the disk gate; a read helper never does.
#[test]
fn every_write_reservation_asks_the_disk_gate() {
    let src = crate::source_scan::read("src/deltaglider/engine/resources.rs");
    for f in [
        "reserve_ingest_spool",
        "reserve_storage_spool",
        "codec_source_spool_now",
    ] {
        let body = crate::source_scan::prod_fn_text("src/deltaglider/engine/resources.rs", f);
        assert!(
            body.contains("admit_new_spool_write("),
            "{f} reserves spool for a write and must ask admit_new_spool_write"
        );
    }
    for f in ["spool_acquire_group", "spool_acquire"] {
        let body = crate::source_scan::prod_fn_text("src/deltaglider/engine/resources.rs", f);
        assert!(
            !body.contains("admit_new_spool_write("),
            "{f} serves reads and must not ask the write gate"
        );
    }
    assert_eq!(
        src.matches("self.spool.admit_new_write()").count(),
        1,
        "one place asks the spool gate"
    );
}

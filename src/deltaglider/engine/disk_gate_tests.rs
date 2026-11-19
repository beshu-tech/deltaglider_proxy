// SPDX-License-Identifier: BUSL-1.1

//! The disk level (`disk_watch`) gates NEW spool reservations of writes
//! and nothing else: a running reservation, a read and a delete go on.

use super::resources::SpoolWait;
use super::*;
use crate::config::Config;
use crate::deltaglider::spool::HeldSpool;
use crate::deltaglider::RelayPart;
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

/// The ingest reservation the way the ingest makes it: the gate, then the wrapper.
async fn reserve(
    engine: &DynEngine,
    held: HeldSpool,
    wait: SpoolWait,
) -> Result<crate::deltaglider::spool::SpoolReservation, EngineError> {
    let admission = engine.admit_new_spool_write(held)?;
    engine.reserve_ingest_spool(MIB, admission, wait).await
}

fn refused<T>(r: Result<T, EngineError>) -> String {
    match r {
        Err(EngineError::Overloaded(m)) => m,
        Err(e) => panic!("want Overloaded, got {e}"),
        Ok(_) => panic!("want Overloaded, the reservation was admitted"),
    }
}

#[tokio::test]
async fn a_new_write_reservation_is_refused_at_high_and_critical() {
    let (_dir, engine) = engine().await;
    for level in [Level::Ok, Level::Low] {
        set_level(&engine, level);
        reserve(&engine, HeldSpool::NOTHING, SpoolWait::Queue)
            .await
            .unwrap();
        engine.codec_source_spool_now(1024).unwrap();
    }
    for level in [Level::High, Level::Critical] {
        set_level(&engine, level);
        for wait in [SpoolWait::Queue, SpoolWait::Now] {
            let m = refused(reserve(&engine, HeldSpool::NOTHING, wait).await);
            assert!(m.contains(level.name()), "{m}");
        }
        // A small codec source (a reference of at most the threshold) is no
        // new body: the rule is "larger than the threshold".
        engine.codec_source_spool_now(1024).unwrap();
    }
}

#[tokio::test]
async fn a_running_write_keeps_its_budget_and_a_recovery_admits_again() {
    let (_dir, engine) = engine().await;
    set_level(&engine, Level::Ok);
    let held = reserve(&engine, HeldSpool::NOTHING, SpoolWait::Queue)
        .await
        .unwrap();
    set_level(&engine, Level::Critical);
    // An op that holds a spool already (a multipart upload's next part)
    // runs to its end.
    reserve(&engine, HeldSpool::of_reservation(&held), SpoolWait::Queue)
        .await
        .unwrap();
    set_level(&engine, Level::Low);
    reserve(&engine, HeldSpool::NOTHING, SpoolWait::Queue)
        .await
        .unwrap();
}

/// The admission is the gate's verdict: nothing held asks the level; a held
/// real spool is a holder and runs to its end.
#[tokio::test]
async fn admission_refuses_critical_for_a_new_write_and_admits_a_holder() {
    let (_dir, engine) = engine().await;
    set_level(&engine, Level::Ok);
    let spool = engine.spool_acquire_ungated(MIB).await.unwrap();
    engine.admit_new_spool_write(HeldSpool::NOTHING).unwrap();
    set_level(&engine, Level::Critical);
    let m = refused(engine.admit_new_spool_write(HeldSpool::NOTHING));
    assert!(m.contains(Level::Critical.name()), "{m}");
    engine.admit_new_spool_write(HeldSpool::of(&spool)).unwrap();
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
    engine.spool_acquire_ungated(MIB).await.unwrap();
    engine
        .delete("b", "k.txt", crate::maintenance::gate::Counted::for_test())
        .await
        .unwrap();
}

/// How a spool acquisition of `src/` relates to the disk gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spool {
    /// A new write: the fn asks `admit_new_spool_write` (or is the gate).
    Gated,
    /// A reference or body that the buffered codec holds, at most the spool
    /// threshold: no gate (the rule is "LARGER than the threshold").
    SmallCodec,
    /// A wrapper that takes the gate's verdict (`SpoolWriteAdmission`) by value.
    Admitted,
    /// The primitive that the gated, small and read callers share.
    Primitive,
    /// A write of the CLI (`cp`): the CLI process runs no disk monitor, so
    /// its spool gate never leaves level ok.
    NoMonitor,
}

/// Every fn of `src/` (outside `spool.rs`) that acquires spool space, by
/// file and name. A new acquisition fails the guard until it is classified:
/// a write that forgot the gate is a disk that fills under a refusing proxy.
const SPOOL_SITES: &[(&str, &str, Spool)] = &[
    (
        "src/deltaglider/engine/resources.rs",
        "spool_acquire_ungated",
        Spool::Primitive,
    ),
    (
        "src/deltaglider/engine/resources.rs",
        "spool_acquire",
        Spool::Admitted,
    ),
    (
        "src/deltaglider/engine/resources.rs",
        "spool_acquire_now",
        Spool::Admitted,
    ),
    (
        "src/deltaglider/engine/store.rs",
        "encode_spooled",
        Spool::Gated,
    ),
    (
        "src/deltaglider/engine/store.rs",
        "restamp_reference",
        Spool::Gated,
    ),
    (
        "src/deltaglider/engine/store.rs",
        "store_relayed_parts_delta_with_multipart_etag",
        Spool::Gated,
    ),
    (
        "src/deltaglider/engine/resources.rs",
        "spool_acquire_group",
        Spool::Primitive,
    ),
    (
        "src/deltaglider/engine/resources.rs",
        "reserve_storage_spool",
        Spool::Gated,
    ),
    (
        "src/deltaglider/engine/resources.rs",
        "reserve_ingest_spool",
        Spool::Admitted,
    ),
    (
        "src/deltaglider/engine/resources.rs",
        "codec_source_spool_now",
        Spool::SmallCodec,
    ),
    // Asks the gate, and degrades to a whole store (`NoSpool`) instead of failing.
    (
        "src/deltaglider/engine/store.rs",
        "encode_memory_body_spooled",
        Spool::Gated,
    ),
    (
        "src/deltaglider/engine/rewrite.rs",
        "rewrite_passthrough",
        Spool::Gated,
    ),
    ("src/cli/transfer_io.rs", "upload_file", Spool::NoMonitor),
];

/// CLASS GUARD: every acquisition of spool space in `src/` is classified in
/// [`SPOOL_SITES`], and the table has no row that no code matches. (The
/// gate itself is a type: the reserving wrappers take a `HeldSpool` or a
/// `SpoolWriteAdmission`.)
#[test]
fn every_spool_acquisition_is_classified() {
    const ACQUIRING: &[&str] = &[
        "spool_acquire",
        "spool_acquire_group",
        "spool_acquire_ungated",
        "spool_acquire_now",
        "spool_acquire_beside",
        "spool_acquire_beside_mib",
        "spool_acquire_pair_beside",
        "reserve_beside",
        "try_reserve",
        "try_acquire",
        "acquire",
        "acquire_pair",
        "acquire_group",
    ];
    // A call on a spool (`self.spool.try_acquire(..)`, `spool().acquire(..)`)
    // or one of the engine's own wrappers. The names also mean semaphores
    // and the memory budget, so the receiver decides.
    let on_spool = regex_lite::Regex::new(
        r"spool\w*(?:\(\))?\s*\.\s*(?:try_acquire|acquire|acquire_pair|acquire_group|reserve_beside|try_reserve)\s*\(|\bspool_acquire\w*\s*\(",
    )
    .unwrap();
    let mut found: Vec<(String, String)> = Vec::new();
    for src in crate::source_scan::ast::src() {
        if matches!(
            src.rel.as_str(),
            "src/deltaglider/spool.rs" | "src/deltaglider/memory_budget.rs"
        ) {
            continue;
        }
        for call in &src.prod.calls {
            if !ACQUIRING.iter().any(|n| call.named(n)) {
                continue;
            }
            let name = src.fn_name(call.func).to_string();
            let text = crate::source_scan::prod_fn_text(&src.rel, &name);
            if !on_spool.is_match(&text) {
                continue;
            }
            let row = (src.rel.clone(), name);
            if !found.contains(&row) {
                found.push(row);
            }
        }
    }
    let mut problems = Vec::new();
    for (file, name) in &found {
        if !SPOOL_SITES.iter().any(|(f, n, _)| f == file && n == name) {
            // A read helper that is classed by its file below.
            if READ_FILES.contains(&file.as_str()) {
                continue;
            }
            problems.push(format!("{file}::{name} acquires spool space: classify it"));
        }
    }
    for (file, name, class) in SPOOL_SITES {
        if !found.iter().any(|(f, n)| f == file && n == name) {
            problems.push(format!(
                "{file}::{name} is in SPOOL_SITES but acquires nothing"
            ));
            continue;
        }
        let body = crate::source_scan::prod_fn_text(file, name);
        let asks = body.contains("admit_new_spool_write(");
        match class {
            Spool::Gated if !asks => {
                problems.push(format!("{file}::{name} is a write and must ask the gate"))
            }
            Spool::SmallCodec | Spool::Primitive | Spool::NoMonitor | Spool::Admitted if asks => {
                problems.push(format!(
                    "{file}::{name} is {class:?} and must not ask the gate"
                ))
            }
            _ => {}
        }
    }
    for file in READ_FILES {
        assert!(
            !crate::source_scan::prod_text(&crate::source_scan::read(file))
                .contains("admit_new_spool_write("),
            "{file} serves reads and must not ask the write gate"
        );
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert_eq!(
        crate::source_scan::read("src/deltaglider/engine/resources.rs")
            .matches("self.spool.admit_new_write()")
            .count(),
        1,
        "one place asks the spool gate"
    );
}

/// The gated acquire wrappers take the gate's verdict by value, and the one
/// ungated acquire (reads, small codec sources) takes none.
#[test]
fn the_gated_acquire_wrappers_take_the_admission() {
    let file = "src/deltaglider/engine/resources.rs";
    for name in [
        "spool_acquire",
        "spool_acquire_now",
        "spool_acquire_beside",
        "spool_acquire_beside_mib",
        "spool_acquire_pair_beside",
    ] {
        let body = crate::source_scan::prod_fn_text(file, name);
        assert!(
            body.contains("SpoolWriteAdmission"),
            "{name} is gated: it must take a SpoolWriteAdmission"
        );
    }
    let body = crate::source_scan::prod_fn_text(file, "spool_acquire_ungated");
    assert!(
        !body.contains("SpoolWriteAdmission"),
        "spool_acquire_ungated takes no admission"
    );
}

/// Files whose spool acquisitions are all reads (a GET and a range read):
/// reads and deletes work at level critical, so they never ask the gate.
const READ_FILES: &[&str] = &["src/deltaglider/engine/retrieve.rs"];

/// CLASS GUARD: every client verb that STORES bytes asks the store gate (it
/// adds the critical-disk refusal of a filesystem backend to the write
/// gate); a verb that only deletes asks the plain write gate. The admin bulk
/// copy and move ask the disk refusal for their destination.
#[test]
fn every_storing_verb_asks_the_store_gate_and_no_delete_does() {
    for (file, f) in [
        ("src/s3_adapter_s3s/object.rs", "put_object"),
        ("src/s3_adapter_s3s/copy.rs", "copy_object"),
        ("src/s3_adapter_s3s/copy.rs", "upload_part_copy"),
        ("src/s3_adapter_s3s/multipart.rs", "create_multipart_upload"),
        ("src/s3_adapter_s3s/multipart.rs", "upload_part"),
        (
            "src/s3_adapter_s3s/multipart.rs",
            "complete_multipart_upload",
        ),
    ] {
        let body = crate::source_scan::prod_fn_text(file, f);
        assert!(
            body.contains("check_client_store_allowed("),
            "{file} {f} stores bytes and must ask check_client_store_allowed"
        );
    }
    for (file, f) in [
        ("src/s3_adapter_s3s/object.rs", "delete_object"),
        ("src/s3_adapter_s3s/object.rs", "delete_objects"),
        ("src/s3_adapter_s3s/bucket.rs", "delete_bucket"),
    ] {
        let body = crate::source_scan::prod_fn_text(file, f);
        assert!(
            !body.contains("check_client_store_allowed("),
            "{file} {f} frees space and must not ask the store gate"
        );
    }
    for f in ["copy_objects", "move_objects"] {
        let body = crate::source_scan::prod_fn_text("src/api/admin/objects.rs", f);
        assert!(
            body.contains("reject_if_disk_critical(&state, &req.dest_bucket)"),
            "{f} stores into its destination and must ask reject_if_disk_critical"
        );
    }
    let form = crate::source_scan::read("src/api/handlers/form_post.rs");
    assert!(form.contains("check_client_store_allowed(state, bucket)"));
}

/// A rewrite of a passthrough object ABOVE the spool threshold takes a spool
/// file (the re-encrypt job): a new write, refused with a retryable 503 at
/// the spool gate. A small object rewrites in memory and is not asked.
#[tokio::test]
async fn a_large_rewrite_is_refused_at_a_full_spool_disk_and_a_small_one_is_not() {
    let (raw, _fake) = s3_engine().await;
    let dir = tempfile::tempdir().unwrap();
    let engine = raw.with_spool_dir(
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 128 * MIB).unwrap(),
    );
    let large = vec![1u8; (engine.spool_threshold() + MIB) as usize];
    engine
        .store("b", "large.jpg", &large, None, HashMap::new())
        .await
        .unwrap();
    engine
        .store("b", "small.jpg", &[2u8; 1024], None, HashMap::new())
        .await
        .unwrap();
    let rewrite = |key: &'static str| {
        let engine = &engine;
        async move {
            let read = engine.head("b", key).await.unwrap();
            engine
                .rewrite_object_if_unchanged(
                    "b",
                    key,
                    crate::storage::ObjectVariant::Passthrough,
                    &read,
                    crate::deltaglider::Unconditional::Allowed,
                )
                .await
        }
    };
    set_level(&engine, Level::High);
    let m = refused(rewrite("large.jpg").await);
    assert!(m.contains("level high"), "{m}");
    assert!(
        matches!(
            rewrite("small.jpg").await,
            Ok(crate::deltaglider::Rewritten::Done { .. })
        ),
        "a small rewrite needs no spool"
    );
    set_level(&engine, Level::Ok);
    assert!(matches!(
        rewrite("large.jpg").await,
        Ok(crate::deltaglider::Rewritten::Done { .. })
    ));
}

/// An empty multipart upload to a delta-eligible key stores whole when the
/// disk gate refuses the joined spool (as a failed acquire did before).
#[tokio::test]
async fn relayed_delta_parts_refused_by_the_disk_gate_store_whole() {
    let (dir, engine) = engine().await;
    for level in [Level::High, Level::Critical] {
        set_level(&engine, level);
        let key = format!("empty-{}.zip", level.name());
        let lock = engine.lock_object_write("b", &key).await;
        let stored = engine
            .store_relayed_parts_delta_with_multipart_etag(
                &lock,
                "b",
                &key,
                &[],
                0,
                None,
                HashMap::new(),
                "\"d41d8cd98f00b204e9800998ecf8427e-0\"".to_string(),
            )
            .await
            .unwrap_or_else(|e| panic!("{level:?}: {e}"));
        drop(lock);
        assert_eq!(stored.metadata.file_size, 0);

        let part = dir.path().join(format!("part-{}", level.name()));
        std::fs::write(&part, b"some bytes").unwrap();
        let parts = [RelayPart::of_file(&part)];
        let key = format!("full-{}.zip", level.name());
        let lock = engine.lock_object_write("b", &key).await;
        engine
            .store_relayed_parts_delta_with_multipart_etag(
                &lock,
                "b",
                &key,
                &parts,
                10,
                None,
                HashMap::new(),
                "\"d41d8cd98f00b204e9800998ecf8427e-1\"".to_string(),
            )
            .await
            .unwrap_or_else(|e| panic!("{level:?} non-empty: {e}"));
    }
}

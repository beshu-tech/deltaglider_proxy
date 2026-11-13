// SPDX-License-Identifier: BUSL-1.1

//! The two refusal probes of the maintenance gate, made from the app state.
//! They live in the library (not in `main.rs`) so a test reaches the
//! wiring: what each probe asks decides which writers a full disk stops.
//!
//! * the WRITE probe (witness, encryption atomicity): asked by every
//!   background writer, deletes included;
//! * the DISK probe (level critical of the disk of the bucket's filesystem
//!   backend): asked only by a writer that STORES bytes
//!   ([`super::gate::WriteKind::Store`]). Reads and deletes must work at
//!   level critical, so a delete job, a mirror delete and a lifecycle expiry
//!   never ask it.

use std::sync::{Arc, Weak};

use crate::api::handlers::AppState;
use crate::disk_watch::DiskWatch;

use super::gate::MaintenanceGate;

/// The write probe: why this node refuses writes (of any kind) to `bucket`.
pub fn write_refusal(state: &AppState, bucket: &str) -> Option<String> {
    state
        .engine
        .load()
        .write_refusal(bucket)
        // An encrypted bucket on a backend without atomic writes is
        // read-only (N2): background writers wait.
        .or_else(|| {
            state
                .atomic_write_block_now(bucket)
                .map(|b| b.admin_message(bucket))
        })
}

/// The disk probe: why a write that stores bytes into `bucket` waits now.
pub fn disk_refusal(state: &AppState, watch: &DiskWatch, bucket: &str) -> Option<String> {
    crate::disk_watch::bucket_write_refusal_now(watch, &state.engine.load(), bucket)
}

/// Why a maintenance job that STORES bytes into `bucket` (a re-encrypt
/// rewrite, the copy of a migration) waits now: the disk of the spool is at
/// level high or critical (each object would fail for want of spool space),
/// or the disk of the filesystem backend that holds `bucket` is at level
/// critical. A job that stores nothing (a backfill stamps metadata in place,
/// a prune deletes) never asks.
pub fn store_refusal(state: &AppState, bucket: &str) -> Option<String> {
    store_refusal_with(
        state,
        crate::disk_watch::installed().map(|w| w.as_ref()),
        bucket,
    )
}

/// [`store_refusal`] with the monitor given.
pub fn store_refusal_with(
    state: &AppState,
    watch: Option<&DiskWatch>,
    bucket: &str,
) -> Option<String> {
    let engine = state.engine.load();
    engine.spool_dir().gate().refusal().or_else(|| {
        watch.and_then(|w| crate::disk_watch::bucket_write_refusal_now(w, &engine, bucket))
    })
}

/// Install both probes on `gate` (boot). `watch` is `None` when the disk
/// monitor is off: no disk probe then.
pub fn install(gate: &MaintenanceGate, state: Weak<AppState>, watch: Option<Arc<DiskWatch>>) {
    let weak = state.clone();
    gate.set_write_refusal_probe(Arc::new(move |bucket: &str| {
        weak.upgrade().and_then(|s| write_refusal(&s, bucket))
    }));
    if let Some(watch) = watch {
        gate.set_disk_refusal_probe(Arc::new(move |bucket: &str| {
            state
                .upgrade()
                .and_then(|s| disk_refusal(&s, &watch, bucket))
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_watch::{
        DiskProbe, DiskWatch, Level, PathFacts, Sizing, Targets, SPOOL_BUDGET_ROLE,
    };
    use crate::maintenance::gate::{admit_all, admit_stores, WriteKind};
    use std::time::Instant;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// Every path is one 100 GiB disk with `free` GiB free.
    struct OneDisk(u64);

    impl DiskProbe for OneDisk {
        fn measure(&self, _path: &std::path::Path) -> Option<PathFacts> {
            Some(PathFacts {
                free: self.0 * GIB,
                total: 100 * GIB,
                device: 1,
            })
        }
    }

    /// An app state whose engine has a spool of its own: the process-wide
    /// spool (and its gate) must not feel a test's critical disk.
    async fn state() -> (tempfile::TempDir, Arc<AppState>) {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<crate::storage::DynStorageBackend<'static>> =
            crate::storage::DynStorageBackend::new_box(
                crate::storage::FilesystemBackend::new(dir.path().join("data"))
                    .await
                    .unwrap(),
            );
        let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &crate::config::Config::default(),
            None,
        )
        .with_spool_dir(
            crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 8 * 1024 * 1024)
                .unwrap(),
        );
        (dir, AppState::for_tests(engine))
    }

    fn tick(watch: &DiskWatch, free_gib: u64) {
        let tuning = crate::config::tuning::RuntimeTuning::default().disk_monitor;
        let targets = Targets {
            spool_dir: "/spool".into(),
            backends: vec![("default".into(), "/data".into())],
            data_dir: "/state".into(),
            default_backend: "default".into(),
            spool_by_use: false,
        };
        let sizing = Sizing {
            budget: 8 * GIB,
            used: 0,
            largest_reservation: GIB,
        };
        watch.tick(
            &OneDisk(free_gib),
            &targets,
            &tuning,
            sizing,
            Instant::now(),
            None,
        );
    }

    /// THE wiring of the boot (`main.rs` calls [`install`]): at level
    /// critical a writer that stores bytes waits and a delete goes on; with
    /// the monitor off there is no disk probe.
    #[tokio::test]
    async fn the_installed_probes_stop_a_store_at_a_critical_disk_and_never_a_delete() {
        let (_dir, state) = state().await;
        let watch = Arc::new(DiskWatch::new(
            state.engine.load().spool_dir().gate().clone(),
        ));
        let gate = state.maintenance_gate.clone();
        install(&gate, Arc::downgrade(&state), Some(watch.clone()));

        tick(&watch, 50);
        assert_eq!(gate.store_blocked("b"), None, "room: nothing waits");

        tick(&watch, 3);
        let why = gate.store_blocked("b").expect("critical: a store waits");
        assert!(why.contains("backend 'default'"), "{why}");
        // A delete (a delete job, a mirror delete, a lifecycle expiry) goes on.
        assert_eq!(gate.write_blocked("b"), None);
        assert!(admit_all(Some(&gate), &["b"]).is_ok());
        assert!(admit_stores(Some(&gate), &["b"]).is_err());
        assert_eq!(gate.blocked("b", WriteKind::FreesSpace), None);

        // The monitor off: no disk probe, no refusal.
        let (_d2, state2) = self::state().await;
        install(&state2.maintenance_gate, Arc::downgrade(&state2), None);
        assert_eq!(state2.maintenance_gate.store_blocked("b"), None);
    }

    /// A maintenance job that stores (re-encrypt, the copy of a migration)
    /// waits for the spool disk and for the disk of its bucket's backend.
    #[tokio::test]
    async fn a_storing_job_waits_for_the_spool_disk_and_the_backend_disk() {
        let (_dir, state) = state().await;
        let gate = state.engine.load().spool_dir().gate().clone();
        let watch = DiskWatch::new(gate.clone());
        assert_eq!(store_refusal_with(&state, Some(&watch), "b"), None);

        // The spool disk at level high: each object would fail for want of
        // spool space.
        gate.set(Level::High, "the spool disk is high".into());
        assert_eq!(
            store_refusal_with(&state, Some(&watch), "b").as_deref(),
            Some("the spool disk is high")
        );
        gate.set(Level::Ok, String::new());
        // The backend disk at level critical (one fake disk: the spool is
        // critical too, so open its gate to see the backend's own refusal).
        tick(&watch, 3);
        assert_eq!(gate.level(), Level::Critical);
        gate.set(Level::Ok, String::new());
        assert!(store_refusal_with(&state, Some(&watch), "b")
            .unwrap()
            .contains("backend 'default'"));
        // No monitor: the spool gate only.
        assert_eq!(store_refusal_with(&state, None, "b"), None);
        // The use of the budget is not a reason to defer a job (its own
        // reservations count): only the disk is.
        let _ = SPOOL_BUDGET_ROLE;
    }

    /// Source guards of the wiring: the boot installs the probes through
    /// [`install`] (not by hand), and the writers that free space or store
    /// nothing never ask the store gate.
    #[test]
    fn the_boot_installs_the_probes_and_the_delete_paths_never_ask_the_store_gate() {
        let main = crate::source_scan::read("src/main.rs");
        assert!(main.contains("maintenance::probes::install("));
        assert!(!main.contains("set_write_refusal_probe("));
        assert!(!main.contains("set_disk_refusal_probe("));
        for file in [
            "src/maintenance/delete.rs",
            "src/replication/mirror_delete.rs",
        ] {
            let text = crate::source_scan::prod_text(&crate::source_scan::read(file));
            for forbidden in ["store_blocked(", "admit_stores(", "WriteKind::Store"] {
                assert!(
                    !text.contains(forbidden),
                    "{file} frees space and must not ask `{forbidden}`"
                );
            }
        }
        // The storing paths do ask it.
        for (file, needle) in [
            ("src/replication/worker/driver.rs", "admit_stores("),
            ("src/replication/worker.rs", "store_blocked("),
            ("src/replication/scheduler.rs", "store_blocked("),
            ("src/api/admin/replication.rs", "store_blocked("),
        ] {
            assert!(
                crate::source_scan::prod_text(&crate::source_scan::read(file)).contains(needle),
                "{file} stores bytes and must ask `{needle}`"
            );
        }
    }

    /// The lifecycle windows: the copies of a transition store, the delete
    /// batches and the retain-newest acts free space. Exactly one window of
    /// `lifecycle/worker.rs` is a storing window.
    #[test]
    fn only_the_lifecycle_copy_window_is_a_storing_window() {
        let src = crate::source_scan::ast::src()
            .iter()
            .find(|s| s.rel == "src/lifecycle/worker.rs")
            .expect("lifecycle worker");
        let windows: Vec<_> = src
            .prod
            .calls
            .iter()
            .filter(|c| !c.method && c.named("admit_write_windows"))
            .collect();
        assert_eq!(windows.len(), 3, "the three windows of a run");
        let storing = windows
            .iter()
            .filter(|c| c.arg_bools.last() == Some(&Some(true)))
            .count();
        assert_eq!(storing, 1, "only the copy window stores");
        assert!(windows
            .iter()
            .all(|c| matches!(c.arg_bools.last(), Some(Some(_)))));
    }
}

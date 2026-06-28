// SPDX-License-Identifier: GPL-3.0-only

//! Quota'd temp spool space for streaming delta codec ops.
//!
//! The streaming GET path reconstructs a delta to a temp file, then streams that
//! file to the client; the streaming PUT path tees the upload to a passthrough
//! spool. Both can be multi-GB. Without a budget, N concurrent large ops would
//! exhaust `/tmp` (ENOSPC) — the adversarial review flagged this (blocker 7).
//!
//! `SpoolDir` gates spool allocation on a BYTE budget: acquiring space for N
//! bytes takes a weighted permit from a semaphore; the returned `Spool` holds a
//! `NamedTempFile` in the configured directory and releases the permit on drop.
//! When the budget is exhausted, acquirers wait (back-pressure) rather than
//! failing the underlying storage with ENOSPC.
//!
//! Configured via `DGP_SPOOL_DIR` (default = system temp dir) and
//! `DGP_SPOOL_MAX_BYTES` (default 16 GiB).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::NamedTempFile;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A weighted-semaphore-gated pool of temp spool bytes.
#[derive(Clone)]
pub struct SpoolDir {
    dir: PathBuf,
    budget: Arc<Semaphore>,
    max_bytes: u64,
}

/// A spool's budget permit: either solely owned (single `acquire`) or shared
/// across a pair (`acquire_pair`, so two files draw on ONE reservation). The
/// budget is released when the last holder drops. The inner permits are RAII
/// guards — never read, held purely so their Drop frees the semaphore.
#[allow(dead_code)]
enum SharedOrOwned {
    Owned(OwnedSemaphorePermit),
    Shared(std::sync::Arc<OwnedSemaphorePermit>),
}

/// A reserved spool file. Holds its share of the budget until dropped.
pub struct Spool {
    file: NamedTempFile,
    _permit: SharedOrOwned,
}

impl SpoolDir {
    /// Build from env: `DGP_SPOOL_DIR` + `DGP_SPOOL_MAX_BYTES` (default 16 GiB).
    ///
    /// Default dir is a DEDICATED `dgp-spool` subdir of the system temp — NOT the
    /// shared temp root (review M3.4: sweeping or filling the shared root is
    /// dangerous; a dedicated subdir we own is safe to sweep). The directory is
    /// created if missing and SWEPT of orphans at startup (review M1.8: a hard
    /// crash leaves spool files behind since `NamedTempFile`'s Drop never ran;
    /// any file present at boot is from a previous process and is safe to delete
    /// — every live spool is held by a `NamedTempFile` in THIS process).
    pub fn from_env() -> std::io::Result<Self> {
        let dir = std::env::var("DGP_SPOOL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("dgp-spool"));
        let max_bytes: u64 =
            crate::config::env_parse_with_default("DGP_SPOOL_MAX_BYTES", 16 * 1024 * 1024 * 1024);
        let pool = Self::new(dir, max_bytes)?;
        pool.sweep_orphans();
        Ok(pool)
    }

    pub fn new(dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        // Semaphore permits are usize; we account in MiB to stay well under the
        // permit cap (Semaphore::MAX_PERMITS) for terabyte-scale budgets.
        let max_mib = mib_ceil(max_bytes).max(1);
        Ok(Self {
            dir,
            budget: Arc::new(Semaphore::new(max_mib)),
            max_bytes,
        })
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Delete STALE spool files orphaned by a hard crash before `NamedTempFile`'s
    /// Drop could run. AGE-based (older than `STALE`), not delete-everything: the
    /// spool dir may be shared by another live DGP instance (or parallel tests),
    /// whose ACTIVE spools are always freshly-touched — only files untouched for
    /// `STALE` are safe to reclaim. Best-effort; logs and continues on errors.
    fn sweep_orphans(&self) {
        // 1h: comfortably longer than any single reconstruction, short enough to
        // reclaim crash debris promptly.
        self.sweep_older_than(std::time::Duration::from_secs(3600));
    }

    /// Sweep files whose mtime is at least `max_age` old. Split out for testing.
    fn sweep_older_than(&self, max_age: std::time::Duration) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut removed = 0u64;
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .ok()
                .filter(|m| m.is_file())
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .map(|age| age >= max_age)
                .unwrap_or(false);
            if stale && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        if removed > 0 {
            tracing::info!(
                "Swept {removed} stale spool file(s) from {} at startup",
                self.dir.display()
            );
        }
    }

    /// Reserve `bytes` of budget as a single weighted permit (clamped to the full
    /// budget). Awaits on back-pressure.
    async fn reserve(&self, bytes: u64) -> std::io::Result<OwnedSemaphorePermit> {
        let max_mib = mib_ceil(self.max_bytes).max(1) as u32;
        let want_mib = (mib_ceil(bytes).max(1) as u32).min(max_mib);
        self.budget
            .clone()
            .acquire_many_owned(want_mib)
            .await
            .map_err(|_| std::io::Error::other("spool budget semaphore closed"))
    }

    /// Reserve `bytes` of spool budget and create a temp file for it. Awaits if
    /// the budget is currently exhausted (back-pressure). A single request larger
    /// than the whole budget is clamped to the full budget (it runs alone).
    pub async fn acquire(&self, bytes: u64) -> std::io::Result<Spool> {
        let permit = self.reserve(bytes).await?;
        let file = NamedTempFile::new_in(&self.dir)?;
        Ok(Spool {
            file,
            _permit: SharedOrOwned::Owned(permit),
        })
    }

    /// Reserve budget for TWO spool files in ONE permit (sum clamped to the
    /// budget), returning both. This is the deadlock-safe primitive for an op
    /// that needs two spools at once (delta reconstruction needs ref + out): a
    /// single reservation can't half-acquire and self-deadlock, and two
    /// concurrent ops never hold one spool while waiting for the other (each
    /// op's whole reservation is atomic). The shared permit drops when BOTH
    /// returned `Spool`s drop.
    pub async fn acquire_pair(
        &self,
        a_bytes: u64,
        b_bytes: u64,
    ) -> std::io::Result<(Spool, Spool)> {
        let permit = std::sync::Arc::new(self.reserve(a_bytes.saturating_add(b_bytes)).await?);
        let a = NamedTempFile::new_in(&self.dir)?;
        let b = NamedTempFile::new_in(&self.dir)?;
        Ok((
            Spool {
                file: a,
                _permit: SharedOrOwned::Shared(permit.clone()),
            },
            Spool {
                file: b,
                _permit: SharedOrOwned::Shared(permit),
            },
        ))
    }
}

impl Spool {
    /// The spool file's path. Callers write/read it directly (the codec hands it
    /// to xdelta3 as a file arg; storage hardlinks/streams it). The `Spool` owns
    /// the `NamedTempFile`, so the file lives until this drops.
    pub fn path(&self) -> &Path {
        self.file.path()
    }
}

/// Bytes → MiB, rounded up. Budget accounting unit (keeps semaphore permits small).
fn mib_ceil(bytes: u64) -> usize {
    const MIB: u64 = 1024 * 1024;
    bytes.div_ceil(MIB) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mib_ceil_rounds_up() {
        assert_eq!(mib_ceil(0), 0);
        assert_eq!(mib_ceil(1), 1);
        assert_eq!(mib_ceil(1024 * 1024), 1);
        assert_eq!(mib_ceil(1024 * 1024 + 1), 2);
    }

    #[tokio::test]
    async fn acquire_creates_a_writable_file_in_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 64 * 1024 * 1024).unwrap();
        let spool = pool.acquire(4 * 1024 * 1024).await.unwrap();
        assert!(spool.path().starts_with(tmp.path()));
        std::fs::write(spool.path(), b"hello").unwrap();
        let back = std::fs::read(spool.path()).unwrap();
        assert_eq!(&back, b"hello");
    }

    #[tokio::test]
    async fn budget_blocks_until_released() {
        // Budget = 4 MiB. Two 4 MiB reservations can't coexist; the second
        // must wait until the first drops.
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 4 * 1024 * 1024).unwrap();
        let first = pool.acquire(4 * 1024 * 1024).await.unwrap();

        let pool2 = pool.clone();
        let waiter = tokio::spawn(async move { pool2.acquire(4 * 1024 * 1024).await.map(|_| ()) });

        // The waiter can't complete while `first` holds the whole budget.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "second acquire should block on budget"
        );

        drop(first); // release budget
                     // Now it proceeds.
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("waiter should finish once budget freed")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn oversized_request_clamps_to_full_budget() {
        // A request larger than the whole budget runs alone (clamped), not deadlocks.
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 4 * 1024 * 1024).unwrap();
        let spool = pool.acquire(64 * 1024 * 1024).await.unwrap(); // > budget
        assert!(spool.path().exists());
    }

    #[test]
    fn sweep_keeps_fresh_files() {
        // A just-written file is younger than any positive threshold → kept.
        // (This is the safety property: a live instance's active spools survive.)
        let tmp = tempfile::tempdir().unwrap();
        let fresh = tmp.path().join("fresh");
        std::fs::write(&fresh, b"y").unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 64 * 1024 * 1024).unwrap();
        pool.sweep_older_than(std::time::Duration::from_secs(3600));
        assert!(fresh.exists(), "a fresh (live) spool must NOT be swept");
    }

    #[test]
    fn sweep_removes_stale_files() {
        // max_age=0 → every file is "at least 0s old" → all swept (proves the
        // delete path; a real run uses 1h so only crash debris qualifies).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("orphan1"), b"x").unwrap();
        std::fs::write(tmp.path().join("orphan2"), b"x").unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 64 * 1024 * 1024).unwrap();
        pool.sweep_older_than(std::time::Duration::from_secs(0));
        let left = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(left, 0, "stale orphans should be swept");
    }

    #[tokio::test]
    async fn acquire_pair_does_not_self_deadlock_on_large_object() {
        // The x-ray blocker: two SEQUENTIAL acquire(file_size) of an object whose
        // 2×size exceeds the budget self-deadlocked (first took the budget, second
        // waited on budget the same task held). acquire_pair takes ONE combined
        // (clamped) reservation, so it must complete for ANY size — here each half
        // alone exceeds the 4 MiB budget.
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 4 * 1024 * 1024).unwrap();
        let fut = pool.acquire_pair(64 * 1024 * 1024, 64 * 1024 * 1024);
        let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(2), fut)
            .await
            .expect("acquire_pair must not deadlock on a large object")
            .unwrap();
        assert!(a.path().exists() && b.path().exists());
        assert_ne!(a.path(), b.path(), "pair gets two distinct files");
    }

    #[tokio::test]
    async fn acquire_pair_shares_one_reservation() {
        // Both files of a pair draw on ONE permit; a second pair must wait until
        // the first fully drops (budget = exactly one pair's worth).
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 8 * 1024 * 1024).unwrap();
        let (a, b) = pool
            .acquire_pair(4 * 1024 * 1024, 4 * 1024 * 1024)
            .await
            .unwrap();

        let pool2 = pool.clone();
        let waiter = tokio::spawn(async move { pool2.acquire(8 * 1024 * 1024).await.map(|_| ()) });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "second op blocks until pair frees budget"
        );

        drop(a); // ONE of the pair drops — budget still held by the other
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "dropping one of the pair must NOT release the shared budget"
        );
        drop(b); // now the shared permit drops → budget freed
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("waiter proceeds once the whole pair drops")
            .unwrap()
            .unwrap();
    }
}

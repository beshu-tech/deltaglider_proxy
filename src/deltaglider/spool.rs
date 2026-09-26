// SPDX-License-Identifier: BUSL-1.1

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
//! The encrypting storage wrapper's temp files (ciphertext, joined multipart
//! parts) live here too: see `SpoolBudget`.
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

/// `io::ErrorKind` of a reservation refused to an op that already holds a
/// spool, because the budget is not free now (see `reserve_within`).
pub const CONTENDED: std::io::ErrorKind = std::io::ErrorKind::WouldBlock;

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

    /// THE process-wide spool, built from env on first use (the orphan sweep
    /// runs once, then). Every engine, including one rebuilt on a config
    /// reload, shares it. A spool per engine gave a reload a second full
    /// budget while the old engine's requests still held the first, and
    /// re-ran the sweep each time.
    pub fn shared() -> std::io::Result<Self> {
        static SHARED: std::sync::OnceLock<SpoolDir> = std::sync::OnceLock::new();
        if let Some(pool) = SHARED.get() {
            return Ok(pool.clone());
        }
        let pool = Self::from_env()?;
        // A racing first caller may win; either way every caller gets the
        // one stored pool.
        Ok(SHARED.get_or_init(|| pool).clone())
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

    /// Do both handles draw on one budget?
    #[cfg(test)]
    pub(crate) fn same_budget(&self, other: &SpoolDir) -> bool {
        Arc::ptr_eq(&self.budget, &other.budget)
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The directory the spool files live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Budget not reserved right now, in MiB.
    #[cfg(test)]
    pub(crate) fn free_mib(&self) -> usize {
        self.budget.available_permits()
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

    /// Reserve `bytes` of budget as a single weighted permit. Clamped so the
    /// op's TOTAL (with the `held_mib` it already holds) stays within the
    /// budget: the op can always run alone and never waits for budget it
    /// holds itself. Awaits on back-pressure.
    ///
    /// An op that already holds budget (`held_mib > 0`) NEVER waits: it gets
    /// the space now or a [`CONTENDED`] error, and goes on without it. Two
    /// holders that wait can each wait for the budget the other holds
    /// (hold-and-wait), until the acquire timeout.
    ///
    /// `may_wait = false` makes a non-holder behave like a holder: used by
    /// storage writes, which run under the caller's deltaspace lock.
    async fn reserve_within(
        &self,
        bytes: u64,
        held_mib: usize,
        may_wait: bool,
    ) -> std::io::Result<OwnedSemaphorePermit> {
        let max_mib = mib_ceil(self.max_bytes).max(1);
        let want_mib = mib_ceil(bytes).max(1).min(max_mib.saturating_sub(held_mib));
        let closed = || std::io::Error::other("spool budget semaphore closed");
        if held_mib == 0 && may_wait {
            return self
                .budget
                .clone()
                .acquire_many_owned(want_mib as u32)
                .await
                .map_err(|_| closed());
        }
        match self.budget.clone().try_acquire_many_owned(want_mib as u32) {
            Ok(permit) => Ok(permit),
            Err(tokio::sync::TryAcquireError::Closed) => Err(closed()),
            Err(tokio::sync::TryAcquireError::NoPermits) => Err(std::io::Error::new(
                CONTENDED,
                "spool budget contended: an op that holds a spool, or a storage write, does not wait for more",
            )),
        }
    }

    /// Reserve `bytes` of spool budget and create a temp file for it. Awaits if
    /// the budget is currently exhausted (back-pressure). A single request larger
    /// than the whole budget is clamped to the full budget (it runs alone).
    pub async fn acquire(&self, bytes: u64) -> std::io::Result<Spool> {
        self.acquire_beside(None, bytes).await
    }

    /// `acquire` for an op that already holds the spool `held`: clamped like
    /// [`Self::acquire_pair_beside`], so the op never waits on itself.
    pub async fn acquire_beside(&self, held: Option<&Spool>, bytes: u64) -> std::io::Result<Spool> {
        let permit = self
            .reserve_within(bytes, held.map_or(0, Spool::reserved_mib), true)
            .await?;
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
        self.acquire_pair_beside(None, a_bytes, b_bytes).await
    }

    /// `acquire_pair` for an op that already holds the spool `held` (the
    /// streaming PUT holds its body spool, then needs ref + delta). The pair
    /// is clamped so the op's total stays within the budget. A plain
    /// `acquire_pair` there waited for budget the op held itself, until the
    /// acquire timeout (120 s), whenever body + pair exceeded the budget.
    pub async fn acquire_pair_beside(
        &self,
        held: Option<&Spool>,
        a_bytes: u64,
        b_bytes: u64,
    ) -> std::io::Result<(Spool, Spool)> {
        let held_mib = held.map_or(0, Spool::reserved_mib);
        let permit = std::sync::Arc::new(
            self.reserve_within(a_bytes.saturating_add(b_bytes), held_mib, true)
                .await?,
        );
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

    /// Reserve `bytes` for the temp files of a storage write, with no file
    /// yet. The caller takes it BEFORE its deltaspace lock and hands it down
    /// in a [`SpoolBudget`]; waiting for budget under that lock is
    /// hold-and-wait. Same rules as [`Self::acquire_beside`]: clamped, and
    /// an op that holds `held` does not wait ([`CONTENDED`]).
    pub async fn reserve_beside(
        &self,
        held: Option<&Spool>,
        bytes: u64,
    ) -> std::io::Result<SpoolReservation> {
        let permit = self
            .reserve_within(bytes, held.map_or(0, Spool::reserved_mib), true)
            .await?;
        Ok(SpoolReservation {
            dir: self.dir.clone(),
            permit: Arc::new(permit),
        })
    }
}

/// Budget reserved for a storage write's temp files. Each file made from
/// it shares the one permit; the budget is released when the reservation
/// and every file made from it drop.
pub struct SpoolReservation {
    dir: PathBuf,
    permit: Arc<OwnedSemaphorePermit>,
}

impl SpoolReservation {
    fn file(&self) -> std::io::Result<Spool> {
        Ok(Spool {
            file: NamedTempFile::new_in(&self.dir)?,
            _permit: SharedOrOwned::Shared(self.permit.clone()),
        })
    }
}

/// The spool a file-streaming storage write (`put_passthrough_file`,
/// `put_passthrough_parts`) uses for its own temp files: the encrypting
/// wrapper's ciphertext and joined parts. It names the spool the caller
/// holds and the reservation the caller took before its lock, so the
/// write NEVER waits for budget: it uses the reservation, or takes free
/// budget now, or fails with [`CONTENDED`].
#[derive(Clone, Copy)]
pub struct SpoolBudget<'a> {
    dir: &'a SpoolDir,
    held: Option<&'a Spool>,
    reserved: Option<&'a SpoolReservation>,
}

impl<'a> SpoolBudget<'a> {
    pub fn new(
        dir: &'a SpoolDir,
        held: Option<&'a Spool>,
        reserved: Option<&'a SpoolReservation>,
    ) -> Self {
        Self {
            dir,
            held,
            reserved,
        }
    }

    /// The same spool for a nested write that also holds `spool`. The
    /// reservation is not passed on: it is sized for this write only.
    pub fn holding(&self, spool: &'a Spool) -> Self {
        Self {
            dir: self.dir,
            held: Some(spool),
            reserved: None,
        }
    }

    /// A temp file for `bytes` in the spool dir. Never waits.
    pub async fn file(&self, bytes: u64) -> std::io::Result<Spool> {
        if let Some(r) = self.reserved {
            return r.file();
        }
        let permit = self
            .dir
            .reserve_within(bytes, self.held.map_or(0, Spool::reserved_mib), false)
            .await?;
        Ok(Spool {
            file: NamedTempFile::new_in(&self.dir.dir)?,
            _permit: SharedOrOwned::Owned(permit),
        })
    }
}

impl Spool {
    /// The spool file's path. Callers write/read it directly (the codec hands it
    /// to xdelta3 as a file arg; storage hardlinks/streams it). The `Spool` owns
    /// the `NamedTempFile`, so the file lives until this drops.
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// Budget this spool holds, in MiB (a pair's shared permit counts once
    /// per holder; callers pass one spool of a pair at most).
    fn reserved_mib(&self) -> usize {
        match &self._permit {
            SharedOrOwned::Owned(p) => p.num_permits(),
            SharedOrOwned::Shared(p) => p.num_permits(),
        }
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
    fn shared_spool_is_one_budget() {
        let a = SpoolDir::shared().unwrap();
        let b = SpoolDir::shared().unwrap();
        assert!(Arc::ptr_eq(&a.budget, &b.budget), "one budget per process");
    }

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
    async fn a_holder_never_waits_for_more_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 4 * 1024 * 1024).unwrap();
        let a = pool.acquire(2 * 1024 * 1024).await.unwrap();
        let _b = pool.acquire(2 * 1024 * 1024).await.unwrap();
        let err = pool
            .acquire_pair_beside(Some(&a), 1024 * 1024, 1024 * 1024)
            .await
            .err()
            .expect("the budget is full: a holder must not get more");
        assert_eq!(err.kind(), CONTENDED);
        drop(_b);
        assert!(pool.acquire_beside(Some(&a), 1024 * 1024).await.is_ok());
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

    /// Tier 4: an op that holds its body spool and then needs a pair must not
    /// wait for budget it holds itself.
    #[tokio::test]
    async fn pair_beside_a_held_spool_never_waits_on_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 8 * 1024 * 1024).unwrap();
        for body_mib in [4u64, 8, 64] {
            let body = pool.acquire(body_mib * 1024 * 1024).await.unwrap();
            let fut = pool.acquire_pair_beside(Some(&body), 6 * 1024 * 1024, 6 * 1024 * 1024);
            let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(2), fut)
                .await
                .unwrap_or_else(|_| panic!("body {body_mib} MiB: pair waited on its own budget"))
                .unwrap();
            drop((a, b, body));
        }
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

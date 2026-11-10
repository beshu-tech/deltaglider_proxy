// SPDX-License-Identifier: BUSL-1.1

//! Quota'd temp spool space for streaming delta codec ops.
//!
//! The streaming GET path reconstructs a delta to a temp file, then streams that
//! file to the client; the streaming PUT path tees the upload to a passthrough
//! spool. Both can be multi-GB. Without a budget, N concurrent large ops would
//! exhaust `/tmp` (ENOSPC) — the adversarial review flagged this (blocker 7).
//!
//! `SpoolDir` gates spool allocation on a BYTE budget: acquiring space for N
//! bytes takes a share of the budget (see `Budget`); the returned `Spool` holds
//! a `NamedTempFile` in the configured directory and releases the share on drop.
//! When the budget is exhausted, acquirers wait (back-pressure) rather than
//! failing the underlying storage with ENOSPC.
//!
//! Every scratch file of the proxy lives here: the codec files, the multipart
//! relay parts and the encrypting wrapper's temps (see `SpoolBudget`).
//!
//! Configured via `DGP_SPOOL_DIR` (default = system temp dir) and
//! `DGP_SPOOL_MAX_BYTES` (default 16 GiB).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::NamedTempFile;

tokio::task_local! {
    /// The backend whose share of the budget a request's spool counts
    /// against (see [`scoped`]).
    static SPOOL_SCOPE: Arc<str>;
}

/// Run `fut` with its spool reservations counted against `backend`'s share
/// of the budget as well (`DGP_BACKEND_SHARE_PERCENT`, see
/// [`SpoolDir::with_backend_share`]). The S3 router runs every request to a
/// bucket in the scope of its backend when several backends are
/// configured. A reservation outside a scope (background jobs) counts
/// against the whole budget only.
pub async fn scoped<F: std::future::Future>(backend: &str, fut: F) -> F::Output {
    SPOOL_SCOPE.scope(Arc::from(backend), fut).await
}

/// A byte-budget-gated pool of temp spool bytes.
#[derive(Clone)]
pub struct SpoolDir {
    dir: PathBuf,
    budget: Arc<Budget>,
    max_bytes: u64,
    /// The most of the budget (MiB) that the reservations made in one
    /// backend's scope may hold together.
    share_mib: usize,
    /// The share budgets, one per backend scope.
    shares: Arc<parking_lot::Mutex<std::collections::HashMap<Arc<str>, Arc<Budget>>>>,
    /// Holders of optional spool files (the range-read reconstruction
    /// cache) that give budget back when an acquire finds it short.
    evictors: Arc<parking_lot::Mutex<Vec<std::sync::Weak<dyn SpoolEvictor>>>>,
}

/// A holder of spool files that are optional (a cache): it drops one when
/// an acquire finds the budget short, before the acquire waits or fails.
pub(crate) trait SpoolEvictor: Send + Sync {
    /// Drop one idle file; `false` when there is none. The file's budget
    /// returns once no reader holds it.
    fn evict_one(&self) -> bool;
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
    Owned(BudgetPermit),
    Shared(std::sync::Arc<BudgetPermit>),
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
    #[allow(
        clippy::disallowed_methods,
        reason = "the spool's own default directory"
    )]
    pub fn from_env() -> std::io::Result<Self> {
        let dir = crate::config::process_env_os("DGP_SPOOL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("dgp-spool"));
        let max_bytes: u64 =
            crate::config::env_parse_with_default("DGP_SPOOL_MAX_BYTES", 16 * 1024 * 1024 * 1024);
        let share: u8 = crate::config::env_parse_with_default(
            "DGP_BACKEND_SHARE_PERCENT",
            crate::config::tuning::DEFAULT_BACKEND_SHARE_PERCENT,
        );
        let pool = Self::new(dir, max_bytes)?.with_backend_share(share);
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

    /// A spool rooted at `dir`, created private to this user (0700) when
    /// missing. An existing root must be private too
    /// (`private_dir_verdict`): a root that another user owns or can
    /// write to lets that user replace a spool file (a relay part, a body)
    /// between its check and its read. One that only its group can write
    /// to is set to 0700; any other is refused, and the proxy does not
    /// start.
    pub fn new(dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        create_private_dir(&dir)?;
        // Accounted in MiB: small numbers for terabyte-scale budgets.
        let max_mib = mib_ceil(max_bytes).max(1);
        Ok(Self {
            dir,
            budget: Budget::new(max_mib),
            max_bytes,
            share_mib: backend_share_mib(
                max_mib,
                crate::config::tuning::DEFAULT_BACKEND_SHARE_PERCENT,
            ),
            shares: Default::default(),
            evictors: Default::default(),
        })
    }

    /// Let the reservations made in one backend's scope ([`scoped`]) hold
    /// at most `percent` of the budget together. One slow backend's large
    /// GETs held the whole budget, and every large request on the other
    /// backends waited for it, then got 503 SlowDown. 100 turns it off.
    pub fn with_backend_share(mut self, percent: u8) -> Self {
        self.share_mib = backend_share_mib(self.budget.max, percent);
        self
    }

    /// The share budget of the current backend scope; `None` outside a
    /// scope or without a share.
    fn scope_share(&self) -> Option<Arc<Budget>> {
        if self.share_mib >= self.budget.max {
            return None;
        }
        let scope = SPOOL_SCOPE.try_with(Arc::clone).ok()?;
        Some(
            self.shares
                .lock()
                .entry(scope)
                .or_insert_with(|| Budget::new(self.share_mib))
                .clone(),
        )
    }

    /// Let `evictor` give budget back when an acquire finds it short. A
    /// dropped evictor is forgotten.
    pub(crate) fn register_evictor(&self, evictor: std::sync::Weak<dyn SpoolEvictor>) {
        let mut evictors = self.evictors.lock();
        evictors.retain(|e| e.strong_count() > 0);
        evictors.push(evictor);
    }

    /// Evict optional spool files until `want_mib` is free or none is left.
    /// Runs outside the budget lock (a dropped file releases into it).
    fn make_room(&self, want_mib: usize) {
        self.make_room_in(&self.budget, want_mib);
    }

    /// [`Self::make_room`] in `budget` (the whole budget, or a backend's
    /// share of it: a cached file holds the share of the backend that made
    /// it).
    fn make_room_in(&self, budget: &Budget, want_mib: usize) {
        if budget.free() >= want_mib {
            return;
        }
        let evictors: Vec<_> = self
            .evictors
            .lock()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        for evictor in evictors {
            while budget.free() < want_mib && evictor.evict_one() {}
        }
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
        self.budget.free()
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
    async fn reserve_within(&self, bytes: u64, held_mib: usize) -> std::io::Result<BudgetPermit> {
        if held_mib == 0 {
            let want = self.want_mib(bytes, 0);
            // The backend's share first: a request over its backend's share
            // waits for that backend's own spool, and never queues for the
            // whole budget ahead of the other backends' requests.
            let share = match self.scope_share() {
                Some(share) => {
                    self.make_room_in(&share, want.min(share.max));
                    Some(share.acquire(want).await)
                }
                None => None,
            };
            self.make_room(want);
            let mut permit = self.budget.acquire(want).await;
            permit.share = share.map(Box::new);
            return Ok(permit);
        }
        self.try_permit(bytes, held_mib)
    }

    /// Clamped want: the op's total (with `held_mib`) stays within the budget.
    fn want_mib(&self, bytes: u64, held_mib: usize) -> usize {
        let max_mib = mib_ceil(self.max_bytes).max(1);
        mib_ceil(bytes).max(1).min(max_mib.saturating_sub(held_mib))
    }

    /// The no-wait half of [`Self::reserve_within`]: the space now, or
    /// [`CONTENDED`]. Sync, so a sync caller under a lock can use it.
    fn try_permit(&self, bytes: u64, held_mib: usize) -> std::io::Result<BudgetPermit> {
        let want = self.want_mib(bytes, held_mib);
        let contended = || {
            std::io::Error::new(
                CONTENDED,
                "spool budget contended: an op that holds a spool, or a storage write, does not wait for more",
            )
        };
        let share = match self.scope_share() {
            Some(share) => {
                self.make_room_in(&share, want.min(share.max));
                Some(share.try_acquire(want).ok_or_else(contended)?)
            }
            None => None,
        };
        self.make_room(want);
        let mut permit = self.budget.try_acquire(want).ok_or_else(contended)?;
        permit.share = share.map(Box::new);
        Ok(permit)
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
        self.acquire_beside_mib(held.map_or(0, Spool::reserved_mib), bytes)
            .await
    }

    /// [`Self::acquire_beside`] for an op whose held budget is not one
    /// `Spool` but a number of MiB: the relay parts of a multipart upload,
    /// which hold one reservation each.
    pub async fn acquire_beside_mib(&self, held_mib: usize, bytes: u64) -> std::io::Result<Spool> {
        let permit = self.reserve_within(bytes, held_mib).await?;
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
    /// returned `Spool`s drop. Tests only: production holds a spool already
    /// and uses [`Self::acquire_pair_beside`] or [`Self::acquire_group`].
    #[cfg(test)]
    pub async fn acquire_pair(
        &self,
        a_bytes: u64,
        b_bytes: u64,
    ) -> std::io::Result<(Spool, Spool)> {
        self.acquire_pair_beside(None, a_bytes, b_bytes).await
    }

    /// A pair of spools in ONE permit for an op that already holds the spool `held` (the
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
        let group = self.acquire_group_beside(held, &[a_bytes, b_bytes]).await?;
        let [a, b] = <[Spool; 2]>::try_from(group)
            .map_err(|_| std::io::Error::other("a pair is two spool files"))?;
        Ok((a, b))
    }

    /// Any number of spool files in ONE permit for all of
    /// them (the large delta GET needs reference + output + delta), shared
    /// by the returned spools in the order of `sizes`. The budget is
    /// released when the last of them drops.
    pub async fn acquire_group(&self, sizes: &[u64]) -> std::io::Result<Vec<Spool>> {
        self.acquire_group_beside(None, sizes).await
    }

    /// [`Self::acquire_group`] for an op that holds `held` (see
    /// [`Self::acquire_pair_beside`]): clamped, and no wait for a holder.
    pub async fn acquire_group_beside(
        &self,
        held: Option<&Spool>,
        sizes: &[u64],
    ) -> std::io::Result<Vec<Spool>> {
        let held_mib = held.map_or(0, Spool::reserved_mib);
        let total = sizes.iter().fold(0u64, |acc, s| acc.saturating_add(*s));
        // A holder of the whole budget would get the group with 0 MiB
        // accounted (the clamp), so the op used held + the group on disk.
        // The group is optional work (the streaming PUT stores passthrough
        // without it): refuse it. A storage write's own temp files
        // (`SpoolBudget`) keep the clamp, because the object cannot be
        // stored without them.
        if held_mib > 0 && self.want_mib(total, held_mib) == 0 {
            return Err(std::io::Error::new(
                CONTENDED,
                "spool budget: the op holds the whole budget, no room for more spool files",
            ));
        }
        let permit = std::sync::Arc::new(self.reserve_within(total, held_mib).await?);
        sizes
            .iter()
            .map(|_| {
                Ok(Spool {
                    file: NamedTempFile::new_in(&self.dir)?,
                    _permit: SharedOrOwned::Shared(permit.clone()),
                })
            })
            .collect()
    }

    /// Reserve `bytes` for the temp files of a storage write, with no file
    /// yet. The caller takes it BEFORE its deltaspace lock and hands it down
    /// in a [`SpoolBudget`]; waiting for budget under that lock is
    /// hold-and-wait. Same rules as [`Self::acquire_beside`]: clamped, and
    /// an op that holds `held` does not wait ([`CONTENDED`]).
    /// `held_mib`: the budget the op holds already (`Spool::reserved_mib`,
    /// `mib_ceil`).
    pub async fn reserve_beside(
        &self,
        held_mib: usize,
        bytes: u64,
    ) -> std::io::Result<SpoolReservation> {
        let permit = self.reserve_within(bytes, held_mib).await?;
        Ok(self.reservation(permit))
    }

    /// [`Self::reserve_beside`] that never waits, for a sync caller (the
    /// multipart store, under its uploads lock). `held_mib` clamps as there.
    pub fn try_reserve(&self, bytes: u64, held_mib: usize) -> std::io::Result<SpoolReservation> {
        Ok(self.reservation(self.try_permit(bytes, held_mib)?))
    }

    /// A spool file for `bytes` now, or [`CONTENDED`]. Never waits: for a
    /// caller that holds a lock (the buffered codec runs under the
    /// deltaspace lock on PUT).
    pub fn try_acquire(&self, bytes: u64) -> std::io::Result<Spool> {
        self.reservation(self.try_permit(bytes, 0)?).file()
    }

    fn reservation(&self, permit: BudgetPermit) -> SpoolReservation {
        SpoolReservation {
            dir: self.dir.clone(),
            permit: Arc::new(permit),
        }
    }
}

/// Budget reserved for a storage write's temp files. Each file made from
/// it shares the one permit; the budget is released when the reservation
/// and every file made from it drop.
pub struct SpoolReservation {
    dir: PathBuf,
    permit: Arc<BudgetPermit>,
}

impl SpoolReservation {
    /// Budget this reservation holds, in MiB.
    pub fn reserved_mib(&self) -> usize {
        self.permit.num_permits()
    }

    /// A temp file in the spool dir that shares this reservation.
    pub(crate) fn file(&self) -> std::io::Result<Spool> {
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

/// Manual: a [`crate::storage::RefWrite`] carries one and derives `Debug`.
impl std::fmt::Debug for SpoolBudget<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpoolBudget")
            .field("dir", &self.dir.dir)
            .field("held_mib", &self.held.map(Spool::reserved_mib))
            .field(
                "reserved_mib",
                &self.reserved.map(SpoolReservation::reserved_mib),
            )
            .finish()
    }
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
            .try_permit(bytes, self.held.map_or(0, Spool::reserved_mib))?;
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

    /// Write `stream` into this spool file, hashing it (MD5, and SHA-256
    /// as `sha256` asks: a SHA-256 proven for these bytes by the caller
    /// is used as is) on the way. At most `cap` bytes (the
    /// reservation): a longer stream stops at the first chunk past `cap`,
    /// so a source that grew after its size was read never overruns the
    /// spool budget. The caller judges a short stream. The one spool writer
    /// of the proxy: writes go through a 1 MiB buffer, so small chunks do
    /// not each cost a trip to the blocking pool.
    pub async fn fill_from_stream<S, E>(
        &self,
        stream: &mut S,
        cap: u64,
        sha256: crate::deltaglider::ingest::Sha256Need,
    ) -> Result<SpoolFill, SpoolFillError<E>>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    {
        let mut hasher = crate::deltaglider::ingest::BodyHasher::for_need(sha256);
        let written = self.fill(stream, cap, Some(&mut hasher)).await?;
        Ok(SpoolFill {
            written,
            hashes: hasher.finish(),
        })
    }

    /// [`Self::fill_from_stream`] without hashes, for bytes that are checked
    /// another way (the delta of a GET: the decoded object is checked).
    /// The number of bytes written.
    pub async fn fill_unhashed<S, E>(
        &self,
        stream: &mut S,
        cap: u64,
    ) -> Result<u64, SpoolFillError<E>>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    {
        self.fill(stream, cap, None).await
    }

    async fn fill<S, E>(
        &self,
        stream: &mut S,
        cap: u64,
        mut hasher: Option<&mut crate::deltaglider::ingest::BodyHasher>,
    ) -> Result<u64, SpoolFillError<E>>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let file = tokio::fs::File::create(self.path())
            .await
            .map_err(SpoolFillError::Write)?;
        let mut file = tokio::io::BufWriter::with_capacity(FILL_BUFFER, file);
        let mut written: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(SpoolFillError::Source)?;
            written += chunk.len() as u64;
            if written > cap {
                return Err(SpoolFillError::Overrun { cap, written });
            }
            if let Some(h) = hasher.as_deref_mut() {
                h.update(&chunk);
            }
            file.write_all(&chunk)
                .await
                .map_err(SpoolFillError::Write)?;
        }
        file.flush().await.map_err(SpoolFillError::Write)?;
        Ok(written)
    }

    /// Keep the file past this `Spool`: rename it to `path` (a new name in
    /// the spool's file system, never an existing one) and hand back the
    /// reservation that holds its budget. A multipart part becomes a relay
    /// part this way, with no copy and no second reservation. On an error
    /// the temp file is deleted and the budget released.
    pub(crate) fn keep_as(self, path: &Path) -> std::io::Result<(PathBuf, SpoolReservation)> {
        let Spool { file, _permit } = self;
        let dir = file
            .path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        file.persist_noclobber(path).map_err(|e| e.error)?;
        let permit = match _permit {
            SharedOrOwned::Owned(p) => Arc::new(p),
            SharedOrOwned::Shared(p) => p,
        };
        Ok((path.to_path_buf(), SpoolReservation { dir, permit }))
    }

    /// Budget this spool holds, in MiB (a pair's shared permit counts once
    /// per holder; callers pass one spool of a pair at most).
    pub(crate) fn reserved_mib(&self) -> usize {
        match &self._permit {
            SharedOrOwned::Owned(p) => p.num_permits(),
            SharedOrOwned::Shared(p) => p.num_permits(),
        }
    }
}

/// The write buffer of [`Spool::fill_from_stream`].
const FILL_BUFFER: usize = 1024 * 1024;

/// What [`Spool::fill_from_stream`] wrote.
#[derive(Debug)]
pub struct SpoolFill {
    pub written: u64,
    /// The hashes of the bytes written.
    pub hashes: crate::deltaglider::ingest::BodyHashes,
}

/// Why [`Spool::fill_from_stream`] stopped.
#[derive(Debug)]
pub enum SpoolFillError<E> {
    /// The source stream failed.
    Source(E),
    /// Writing the spool file failed.
    Write(std::io::Error),
    /// The stream is longer than `cap`: the source changed after its size
    /// was read.
    Overrun { cap: u64, written: u64 },
}

/// Pure: one backend's share of a `max_mib` budget at `percent` (at least
/// 1 MiB; 100 % or more is the whole budget).
fn backend_share_mib(max_mib: usize, percent: u8) -> usize {
    (max_mib.saturating_mul(usize::from(percent.min(100))) / 100).max(1)
}

/// Bytes → MiB, rounded up. Budget accounting unit.
/// What [`private_dir_verdict`] decides about an existing spool root.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DirVerdict {
    /// Ours, and nobody else can write to it.
    Private,
    /// Ours, and only its group can write to it (a directory that an older
    /// release or a test made under umask 002): set it to 0700.
    Tighten,
    /// Not usable as the spool root.
    Refuse(String),
}

/// Pure: may a directory owned by `owner` with permission bits `mode` be
/// the spool root of the user `me`? Only when `me` owns it and no other
/// user can write to it. A root that only its group can write to is made
/// private; a world-writable or sticky one (a shared temp root) is
/// refused, because changing its mode would break its other users.
pub(crate) fn private_dir_verdict(owner: u32, me: u32, mode: u32) -> DirVerdict {
    if owner != me {
        return DirVerdict::Refuse(format!(
            "it is owned by uid {owner}, not by this process (uid {me}); give DGP_SPOOL_DIR \
             a directory of its own"
        ));
    }
    if mode & 0o1002 != 0 {
        return DirVerdict::Refuse(format!(
            "other users can write to it (mode {:o}); give DGP_SPOOL_DIR a directory of its \
             own, or run `chmod 700` on it",
            mode & 0o7777
        ));
    }
    if mode & 0o020 != 0 {
        return DirVerdict::Tighten;
    }
    DirVerdict::Private
}

/// Create `dir` (and missing parents) with mode 0700, then judge it with
/// [`private_dir_verdict`]: a pre-existing root keeps a private mode, has
/// its group write bit removed, or is refused.
#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = std::fs::metadata(dir)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    match private_dir_verdict(meta.uid(), me, meta.mode()) {
        DirVerdict::Private => Ok(()),
        DirVerdict::Tighten => {
            tracing::warn!(
                "spool directory {} was writable by its group (mode {:o}); setting it to 700",
                dir.display(),
                meta.mode() & 0o7777
            );
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        }
        DirVerdict::Refuse(why) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "the spool directory {} is not private: {why}",
                dir.display()
            ),
        )),
    }
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

pub(crate) fn mib_ceil(bytes: u64) -> usize {
    const MIB: u64 = 1024 * 1024;
    bytes.div_ceil(MIB) as usize
}

/// The spool byte budget, in MiB. Not a tokio `Semaphore`: that one hands
/// free permits to the head of its wait queue, so one large waiting GET made
/// the whole free budget unavailable to every no-wait caller (buffered delta
/// PUTs, relayed parts: all SlowDown).
///
/// Here a no-wait caller gets any budget that is free now, whatever the
/// queue. A waiter (FIFO) never takes budget that is free when it queues;
/// it collects only budget RELEASED while it waits, and completes once what
/// it collected plus what is free covers its need. Every holder releases
/// eventually, so a waiter is never starved forever.
struct Budget {
    max: usize,
    state: parking_lot::Mutex<BudgetState>,
}

struct BudgetState {
    /// Held by permits plus collected by waiters.
    used: usize,
    queue: std::collections::VecDeque<Arc<Waiter>>,
}

struct Waiter {
    need: usize,
    /// Released budget this waiter holds already (changed under the lock).
    collected: std::sync::atomic::AtomicUsize,
    granted: std::sync::atomic::AtomicBool,
    wake: tokio::sync::Notify,
}

/// A held share of the budget; released on drop.
pub(crate) struct BudgetPermit {
    budget: Arc<Budget>,
    n: usize,
    /// The same reservation in the backend's share budget, if any
    /// (released with this one).
    share: Option<Box<BudgetPermit>>,
}

impl BudgetPermit {
    fn num_permits(&self) -> usize {
        self.n
    }
}

impl Drop for BudgetPermit {
    fn drop(&mut self) {
        self.budget.release(self.n);
    }
}

impl Budget {
    fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            max,
            state: parking_lot::Mutex::new(BudgetState {
                used: 0,
                queue: Default::default(),
            }),
        })
    }

    fn free(&self) -> usize {
        self.max - self.state.lock().used
    }

    fn permit(self: &Arc<Self>, n: usize) -> BudgetPermit {
        BudgetPermit {
            budget: self.clone(),
            n,
            share: None,
        }
    }

    /// `n` now, or `None`. Queued waiters do not count.
    fn try_acquire(self: &Arc<Self>, n: usize) -> Option<BudgetPermit> {
        let n = n.min(self.max);
        let mut st = self.state.lock();
        if self.max - st.used < n {
            return None;
        }
        st.used += n;
        Some(self.permit(n))
    }

    async fn acquire(self: &Arc<Self>, n: usize) -> BudgetPermit {
        use std::sync::atomic::Ordering;
        let n = n.min(self.max);
        let waiter = {
            let mut st = self.state.lock();
            if n == 0 || (st.queue.is_empty() && self.max - st.used >= n) {
                st.used += n;
                return self.permit(n);
            }
            let w = Arc::new(Waiter {
                need: n,
                collected: 0.into(),
                granted: false.into(),
                wake: tokio::sync::Notify::new(),
            });
            st.queue.push_back(w.clone());
            w
        };
        // Gives the waiter's share back if the future is dropped (timeout).
        struct Cancel<'a>(&'a Budget, Option<Arc<Waiter>>);
        impl Drop for Cancel<'_> {
            fn drop(&mut self) {
                if let Some(w) = self.1.take() {
                    self.0.cancel(&w);
                }
            }
        }
        let mut cancel = Cancel(self, Some(waiter.clone()));
        while !waiter.granted.load(Ordering::SeqCst) {
            // `notify_one` stores a wake-up, so a grant between the check
            // and this await is not lost.
            waiter.wake.notified().await;
        }
        cancel.1 = None;
        self.permit(n)
    }

    fn release(&self, n: usize) {
        let mut st = self.state.lock();
        st.used -= n;
        self.hand_out(&mut st, n);
    }

    fn cancel(&self, w: &Arc<Waiter>) {
        use std::sync::atomic::Ordering;
        let mut st = self.state.lock();
        let give_back = if w.granted.load(Ordering::SeqCst) {
            w.need
        } else {
            st.queue.retain(|q| !Arc::ptr_eq(q, w));
            w.collected.load(Ordering::SeqCst)
        };
        st.used -= give_back;
        self.hand_out(&mut st, give_back);
    }

    /// Give `released` MiB to the queue head, then grant every head whose
    /// collected share plus the free budget covers its need.
    fn hand_out(&self, st: &mut BudgetState, mut released: usize) {
        use std::sync::atomic::Ordering;
        while let Some(head) = st.queue.front().cloned() {
            let collected = head.collected.load(Ordering::SeqCst);
            let take = released.min(head.need - collected);
            released -= take;
            st.used += take;
            let collected = collected + take;
            head.collected.store(collected, Ordering::SeqCst);
            let rest = head.need - collected;
            if self.max - st.used < rest {
                break;
            }
            st.used += rest;
            head.collected.store(head.need, Ordering::SeqCst);
            head.granted.store(true, Ordering::SeqCst);
            head.wake.notify_one();
            st.queue.pop_front();
        }
    }
}

/// THE blocking read loop over a spool or relay file: `path` in 256 KiB
/// reads on the calling thread, each chunk handed to `chunk` in order.
/// Returns the bytes read. It blocks: callers run it in `spawn_blocking`.
pub(crate) fn for_each_chunk_blocking(
    path: &Path,
    mut chunk: impl FnMut(&[u8]),
) -> std::io::Result<u64> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(total);
        }
        total = total.saturating_add(n as u64);
        chunk(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_each_chunk_blocking_hands_over_every_byte_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let data: Vec<u8> = (0..600 * 1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let mut seen = Vec::new();
        let mut chunks = 0;
        let n = for_each_chunk_blocking(&path, |c| {
            chunks += 1;
            seen.extend_from_slice(c);
        })
        .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(seen, data);
        assert!(chunks >= 3, "256 KiB reads: {chunks}");
        assert!(for_each_chunk_blocking(&dir.path().join("none"), |_| {}).is_err());
    }

    #[test]
    fn shared_spool_is_one_budget() {
        let a = SpoolDir::shared().unwrap();
        let b = SpoolDir::shared().unwrap();
        assert!(Arc::ptr_eq(&a.budget, &b.budget), "one budget per process");
    }

    #[test]
    fn private_dir_verdict_truth_table() {
        use DirVerdict::*;
        let refused = |v: DirVerdict| matches!(v, Refuse(_));
        assert_eq!(private_dir_verdict(1000, 1000, 0o40700), Private);
        assert_eq!(private_dir_verdict(1000, 1000, 0o40755), Private);
        assert_eq!(private_dir_verdict(0, 0, 0o40700), Private);
        assert_eq!(private_dir_verdict(1000, 1000, 0o40770), Tighten);
        assert_eq!(private_dir_verdict(1000, 1000, 0o40775), Tighten);
        let Refuse(why) = private_dir_verdict(0, 1000, 0o40700) else {
            panic!("another owner")
        };
        assert!(why.contains("uid 0"), "{why}");
        assert!(refused(private_dir_verdict(1000, 1000, 0o40777)));
        assert!(refused(private_dir_verdict(1000, 1000, 0o40702)));
        // A sticky dir is a shared temp root, even when ours (root's /tmp).
        assert!(refused(private_dir_verdict(0, 0, 0o41777)));
        assert!(refused(private_dir_verdict(1000, 1000, 0o41700)));
    }

    /// A new root is created 0700; an existing private root is kept as
    /// it is; one only its group can write to is made 0700; a root that
    /// other users can write to is refused with a message that names it.
    #[cfg(unix)]
    #[test]
    fn the_spool_root_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let set = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("a/spool");
        SpoolDir::new(fresh.clone(), 1 << 20).unwrap();
        assert_eq!(mode(&fresh), 0o700);

        let kept = dir.path().join("kept");
        std::fs::create_dir(&kept).unwrap();
        set(&kept, 0o750);
        SpoolDir::new(kept.clone(), 1 << 20).unwrap();
        assert_eq!(
            mode(&kept),
            0o750,
            "an existing private root keeps its mode"
        );

        let group = dir.path().join("group");
        std::fs::create_dir(&group).unwrap();
        set(&group, 0o775);
        SpoolDir::new(group.clone(), 1 << 20).unwrap();
        assert_eq!(mode(&group), 0o700, "a group-writable root is made private");

        for open in [0o777, 0o703, 0o1777] {
            let shared = dir.path().join(format!("shared-{open:o}"));
            std::fs::create_dir(&shared).unwrap();
            set(&shared, open);
            let err = SpoolDir::new(shared.clone(), 1 << 20)
                .err()
                .expect("refused");
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(err.to_string().contains("not private"), "{err}");
            assert_eq!(mode(&shared), open, "a refused root is not changed");
        }
    }

    #[test]
    fn mib_ceil_rounds_up() {
        assert_eq!(mib_ceil(0), 0);
        assert_eq!(mib_ceil(1), 1);
        assert_eq!(mib_ceil(1024 * 1024), 1);
        assert_eq!(mib_ceil(1024 * 1024 + 1), 2);
    }

    /// The one "stream into a spool" loop of the copy paths: exact bytes and
    /// hash, a stop at the first chunk past the cap, a source error kept.
    #[tokio::test]
    async fn fill_from_stream_caps_hashes_and_keeps_the_source_error() {
        use sha2::Digest;
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 64 * 1024 * 1024).unwrap();
        let chunks = |parts: &[&'static [u8]]| {
            futures::stream::iter(
                parts
                    .iter()
                    .map(|p| Ok::<_, std::io::Error>(bytes::Bytes::from_static(p)))
                    .collect::<Vec<_>>(),
            )
        };

        let spool = pool.acquire(10).await.unwrap();
        let fill = spool
            .fill_from_stream(
                &mut chunks(&[b"hello ", b"spool"]),
                11,
                crate::deltaglider::ingest::Sha256Need::Compute,
            )
            .await
            .unwrap();
        assert_eq!(fill.written, 11);
        assert_eq!(
            fill.hashes.sha256(),
            hex::encode(sha2::Sha256::digest(b"hello spool"))
        );
        assert_eq!(
            fill.hashes.md5(),
            hex::encode(<md5::Md5 as md5::Digest>::digest(b"hello spool"))
        );
        assert_eq!(std::fs::read(spool.path()).unwrap(), b"hello spool");

        // A SHA-256 the caller proved is taken as is; MD5 is still computed.
        let proven = crate::deltaglider::KnownSha256::of(b"proven elsewhere");
        let fill = spool
            .fill_from_stream(
                &mut chunks(&[b"hello ", b"spool"]),
                11,
                crate::deltaglider::ingest::Sha256Need::Known(proven),
            )
            .await
            .unwrap();
        assert_eq!(fill.hashes.sha256(), proven.to_hex());
        assert_eq!(
            fill.hashes.md5(),
            hex::encode(<md5::Md5 as md5::Digest>::digest(b"hello spool"))
        );

        // Short: the caller judges it.
        let fill = spool
            .fill_from_stream(
                &mut chunks(&[b"abc"]),
                11,
                crate::deltaglider::ingest::Sha256Need::Compute,
            )
            .await
            .unwrap();
        assert_eq!(fill.written, 3);
        assert_eq!(
            spool
                .fill_unhashed(&mut chunks(&[b"ab", b"c"]), 11)
                .await
                .unwrap(),
            3
        );

        let grown = spool
            .fill_from_stream(
                &mut chunks(&[b"12345", b"67890", b"x"]),
                10,
                crate::deltaglider::ingest::Sha256Need::Compute,
            )
            .await;
        assert!(
            matches!(
                grown,
                Err(SpoolFillError::Overrun {
                    cap: 10,
                    written: 11
                })
            ),
            "{grown:?}"
        );
        assert!(
            std::fs::metadata(spool.path()).unwrap().len() <= 10,
            "no byte past the cap"
        );

        let mut failing = futures::stream::iter(vec![
            Ok(bytes::Bytes::from_static(b"ok")),
            Err(crate::storage::StorageError::Other("reset".into())),
        ]);
        let err = spool
            .fill_from_stream(
                &mut failing,
                10,
                crate::deltaglider::ingest::Sha256Need::Compute,
            )
            .await;
        assert!(matches!(err, Err(SpoolFillError::Source(_))), "{err:?}");
    }

    /// The budget was global: the large GETs of one slow backend could
    /// hold all of it, and a large GET on any other backend waited the
    /// acquire timeout (120 s) and got 503 SlowDown.
    #[tokio::test]
    async fn one_backend_cannot_hold_the_whole_spool_budget() {
        const MIB: u64 = 1024 * 1024;
        let wait = std::time::Duration::from_millis(200);
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 64 * MIB).unwrap();
        let _first = scoped("hetzner-fsn1", pool.acquire(32 * MIB))
            .await
            .unwrap();
        let second =
            tokio::time::timeout(wait, scoped("hetzner-fsn1", pool.acquire(32 * MIB))).await;
        let other = tokio::time::timeout(wait, scoped("local-disk", pool.acquire(32 * MIB))).await;
        assert!(
            other.is_ok(),
            "a GET on another backend waited for the slow backend's spool"
        );
        assert!(second.is_err(), "one backend held the whole budget");
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
            let res = tokio::time::timeout(std::time::Duration::from_secs(2), fut)
                .await
                .unwrap_or_else(|_| panic!("body {body_mib} MiB: pair waited on its own budget"));
            // A body that fills the budget gets no pair (storage-6, below).
            match res {
                Ok(_pair) => assert_eq!(body_mib, 4, "{body_mib} MiB got a pair"),
                Err(e) => assert_eq!(e.kind(), CONTENDED, "{body_mib} MiB"),
            }
            drop(body);
        }
    }

    /// storage-6: a holder that already holds the whole budget got a pair
    /// with 0 MiB accounted, so a streaming PUT of a body at the budget
    /// used body + ref + delta on disk. It gets CONTENDED instead (the PUT
    /// stores passthrough).
    #[tokio::test]
    async fn a_full_holder_gets_no_unaccounted_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 4 * 1024 * 1024).unwrap();
        let body = pool.acquire(4 * 1024 * 1024).await.unwrap();
        let err = pool
            .acquire_pair_beside(Some(&body), 1024 * 1024, 1024 * 1024)
            .await
            .err()
            .expect("a full holder must not get an unaccounted pair");
        assert_eq!(err.kind(), CONTENDED);
    }

    /// A group (the large delta GET: reference + output + delta) is ONE
    /// reservation: all files or none, released when the last one drops,
    /// and a holder of the whole budget gets no group.
    #[tokio::test]
    async fn acquire_group_is_one_reservation_for_every_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 8 * 1024 * 1024).unwrap();
        let mib = 1024 * 1024;
        let group = pool
            .acquire_group(&[3 * mib, 3 * mib, 2 * mib])
            .await
            .unwrap();
        assert_eq!(group.len(), 3);
        assert_eq!(pool.free_mib(), 0, "the sum is reserved at once");
        let mut paths: Vec<_> = group.iter().map(|s| s.path().to_path_buf()).collect();
        paths.dedup();
        assert_eq!(paths.len(), 3, "three distinct files");
        let mut group = group;
        group.truncate(1);
        assert_eq!(
            pool.free_mib(),
            0,
            "a file of the group keeps the reservation"
        );
        let err = pool
            .acquire_group_beside(Some(&group[0]), &[mib, mib])
            .await
            .err()
            .expect("a holder of the whole budget gets no more files");
        assert_eq!(err.kind(), CONTENDED);
        drop(group);
        assert_eq!(pool.free_mib(), 8, "the last file releases the reservation");
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

#[cfg(test)]
mod review3_tests {
    use super::*;

    /// A queued waiter takes the free permits (tokio's semaphore assigns them
    /// to the queue head), so a no-wait request for 1 MiB of a budget with
    /// 4 MiB free is refused: every buffered delta PUT and relayed part is
    /// a SlowDown while one large GET waits.
    #[tokio::test]
    async fn review3_a_queued_waiter_starves_every_no_wait_request() {
        const MIB: u64 = 1024 * 1024;
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 8 * MIB).unwrap();
        let _held = pool.acquire(4 * MIB).await.unwrap();
        let p2 = pool.clone();
        let _waiter = tokio::spawn(async move { p2.acquire(6 * MIB).await.map(|_| ()) });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            pool.try_acquire(MIB).is_ok(),
            "4 MiB of 8 are not in use, but a no-wait 1 MiB request is refused"
        );
    }

    /// The other half of the rule: no-wait traffic that never leaves the
    /// budget free does not starve a waiter. It collects what is released.
    #[tokio::test]
    async fn a_waiter_collects_released_budget_under_no_wait_traffic() {
        const MIB: u64 = 1024 * 1024;
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 8 * MIB).unwrap();
        let mut held: Vec<Spool> = Vec::new();
        for _ in 0..8 {
            held.push(pool.try_acquire(MIB).unwrap());
        }
        let p2 = pool.clone();
        let waiter = tokio::spawn(async move { p2.acquire(6 * MIB).await.map(|_| ()) });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Each release goes to the waiter; a no-wait caller that tries to
        // take the slot right away finds none.
        for _ in 0..6 {
            held.pop();
            assert!(
                pool.try_acquire(MIB).is_err(),
                "released budget went to the waiter"
            );
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("the waiter completes once 6 MiB are released")
            .unwrap()
            .unwrap();
        assert_eq!(pool.free_mib(), 6, "the waiter's spool dropped");
    }

    /// A waiter that gives up (acquire timeout) returns what it collected.
    #[tokio::test]
    async fn a_cancelled_waiter_returns_its_collected_share() {
        const MIB: u64 = 1024 * 1024;
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 8 * MIB).unwrap();
        let a = pool.acquire(4 * MIB).await.unwrap();
        let _b = pool.acquire(4 * MIB).await.unwrap();
        let p2 = pool.clone();
        let waiter = tokio::spawn(async move { p2.acquire(6 * MIB).await.map(|_| ()) });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(a);
        assert_eq!(pool.free_mib(), 0, "4 MiB collected by the waiter");
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(
            pool.free_mib(),
            4,
            "the cancelled waiter gave its share back"
        );
        assert!(pool.try_acquire(4 * MIB).is_ok());
    }
}

/// Model test of the `Budget` state machine: random sequences of no-wait
/// takes, waits, releases, cancels and polls, with the invariants checked
/// after every step. `loom`/`shuttle` would need `Budget` built on their
/// primitives instead of `parking_lot` + `tokio::sync::Notify`; every state
/// change here happens under one mutex, so a sequential model over all
/// interleavings of those critical sections covers the same ground.
#[cfg(test)]
mod budget_model_tests {
    use super::*;
    use proptest::prelude::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    #[derive(Debug, Clone)]
    enum Op {
        Try(usize),
        Wait(usize),
        Release(usize),
        Cancel(usize),
    }

    fn op(max: usize) -> impl Strategy<Value = Op> {
        prop_oneof![
            (0..=max + 2).prop_map(Op::Try),
            (0..=max + 2).prop_map(Op::Wait),
            any::<usize>().prop_map(Op::Release),
            any::<usize>().prop_map(Op::Cancel),
        ]
    }

    type Pending = Pin<Box<dyn Future<Output = BudgetPermit>>>;

    struct Model {
        budget: Arc<Budget>,
        held: Vec<BudgetPermit>,
        /// Queued waiters, oldest first: (id, need, future).
        pending: Vec<(usize, usize, Pending)>,
        next_id: usize,
    }

    impl Model {
        fn used(&self) -> usize {
            self.budget.state.lock().used
        }

        /// Poll every pending waiter, oldest first. FIFO: none completes
        /// while an older one still waits (a zero-size wait needs nothing
        /// and never queues).
        fn poll_all(&mut self) {
            let mut cx = Context::from_waker(Waker::noop());
            let mut still = Vec::new();
            let mut older_waits = false;
            for (id, need, mut fut) in std::mem::take(&mut self.pending) {
                match fut.as_mut().poll(&mut cx) {
                    Poll::Ready(permit) => {
                        assert!(
                            !older_waits || need == 0,
                            "waiter {id} granted before an older one"
                        );
                        assert_eq!(permit.num_permits(), need.min(self.budget.max));
                        self.held.push(permit);
                    }
                    Poll::Pending => {
                        older_waits = true;
                        still.push((id, need, fut));
                    }
                }
            }
            self.pending = still;
        }

        /// `used` is exactly what permits hold plus what waiters collected.
        fn check(&self) {
            let st = self.budget.state.lock();
            let held: usize = self.held.iter().map(|p| p.num_permits()).sum();
            let collected: usize = st
                .queue
                .iter()
                .map(|w| w.collected.load(std::sync::atomic::Ordering::SeqCst))
                .sum();
            assert!(st.used <= self.budget.max, "used {} > max", st.used);
            assert_eq!(st.used, held + collected, "budget accounting drifted");
            assert_eq!(st.queue.len(), self.pending.len(), "queue mirrors waiters");
        }
    }

    fn run(max: usize, ops: Vec<Op>) {
        let mut m = Model {
            budget: Budget::new(max),
            held: Vec::new(),
            pending: Vec::new(),
            next_id: 0,
        };
        for op in ops {
            match op {
                Op::Try(n) => {
                    let free = max - m.used();
                    let got = m.budget.try_acquire(n);
                    // A no-wait caller gets any budget free now, whatever
                    // the queue holds.
                    assert_eq!(
                        got.is_some(),
                        free >= n.min(max),
                        "try {n} with {free} free"
                    );
                    m.held.extend(got);
                }
                Op::Wait(n) => {
                    let before = m.used();
                    let b = m.budget.clone();
                    let fut: Pending = Box::pin(async move { b.acquire(n).await });
                    m.pending.push((m.next_id, n, fut));
                    m.next_id += 1;
                    let queued = m.pending.len();
                    m.poll_all();
                    if m.pending.len() == queued {
                        // It queued: it takes nothing that was free.
                        assert_eq!(m.used(), before, "a new waiter took free budget");
                    }
                }
                Op::Release(i) if !m.held.is_empty() => {
                    let i = i % m.held.len();
                    drop(m.held.swap_remove(i));
                    m.poll_all();
                }
                Op::Cancel(i) if !m.pending.is_empty() => {
                    let i = i % m.pending.len();
                    drop(m.pending.remove(i));
                    m.poll_all();
                }
                Op::Release(_) | Op::Cancel(_) => {}
            }
            m.check();
        }
        // No starvation: once holders release, every waiter completes.
        for _ in 0..=m.pending.len() {
            m.held.clear();
            m.poll_all();
            m.check();
        }
        assert!(
            m.pending.is_empty(),
            "a waiter starved: {} left",
            m.pending.len()
        );
        // No leak: with every permit dropped the whole budget is free.
        m.held.clear();
        assert_eq!(m.used(), 0, "budget leaked");
        assert!(m.budget.state.lock().queue.is_empty());
    }

    proptest! {
        #[test]
        fn budget_invariants_hold_for_any_schedule(
            (max, ops) in (1usize..12).prop_flat_map(|max| {
                (Just(max), proptest::collection::vec(op(max), 0..60))
            })
        ) {
            run(max, ops);
        }
    }
}

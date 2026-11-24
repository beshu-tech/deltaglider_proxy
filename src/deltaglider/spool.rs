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
//! `DGP_SPOOL_MAX_BYTES` (default `auto`: derived once at boot from the free
//! space of the directory, see `config::limits`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::NamedTempFile;

/// What the spool measured at boot: the inputs of the auto-sized limits
/// (`config::limits::derive_limits`). Fixed for the life of the process.
#[derive(Debug, Clone)]
pub struct BootLimits {
    pub dir: PathBuf,
    pub facts: crate::config::limits::DiskFacts,
    /// How the budget was found (for a log line and an EntityTooLarge message).
    pub budget_note: String,
    /// The text of `DGP_SPOOL_MAX_BYTES` when it did not parse.
    pub spool_invalid: Option<String>,
}

static SHARED: std::sync::OnceLock<SpoolDir> = std::sync::OnceLock::new();

/// Set by the boot ([`SpoolDir::claim_shared`]). A spool that a unit test or
/// the CLI built on first use also measures its disk, but only the boot's
/// measure drives the limits of a [`crate::config::Config`]: a test must not
/// see its limits change with the order of the tests.
static CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How the budget of the process-wide spool was found (`auto: 50% of 46 GiB
/// free on /tmp/dgp-spool at boot`, `set explicitly`), for an EntityTooLarge message. Does
/// not create the spool.
pub fn shared_budget_note() -> String {
    SHARED
        .get()
        .map_or("set explicitly", |p| p.budget_note())
        .to_string()
}

/// The disk facts that the booted process measured; `None` before the boot
/// claimed the spool.
pub fn boot_limits() -> Option<BootLimits> {
    if !CLAIMED.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    SHARED.get().and_then(|p| p.boot().cloned())
}

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

/// The backend scope the caller runs in, if any. A task that outlives its
/// request (`tokio::spawn`) does not inherit it: capture it here and run
/// the task through [`scoped_in`].
pub fn current_scope() -> Option<Arc<str>> {
    SPOOL_SCOPE.try_with(Arc::clone).ok()
}

/// [`scoped`] for a scope captured with [`current_scope`] (`None`: no scope).
pub async fn scoped_in<F: std::future::Future>(scope: Option<Arc<str>>, fut: F) -> F::Output {
    match scope {
        Some(scope) => SPOOL_SCOPE.scope(scope, fut).await,
        None => fut.await,
    }
}

/// Who pays for the reads of a task ([`as_principal`], [`as_background`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payer {
    /// A client: its reads count against its share of the budget.
    Principal(Arc<str>),
    /// A background job (replication, lifecycle, maintenance, the CLI): not
    /// a client, so no share. Only those roots enter this scope.
    Background,
}

tokio::task_local! {
    /// The payer that the task runs for.
    static SPOOL_PAYER: Payer;
}

/// Run `fut` for `principal`: the response streams it opens on a spool file,
/// and the fills that make them, count against that principal's share of the
/// budget (`SpoolDir::charge_stream`, `SpoolDir::charge_fill`). The S3 router
/// and the bulk session middleware run every request in the scope of its
/// principal (the anonymous principal counts as one, and so does open
/// access).
pub async fn as_principal<F: std::future::Future>(principal: &str, fut: F) -> F::Output {
    SPOOL_PAYER
        .scope(Payer::Principal(Arc::from(principal)), fut)
        .await
}

/// Run `fut` as a background job: its reads are not charged to any share.
/// ONLY the roots of background work enter this scope. A read outside every
/// scope is refused (`SpoolDir::charge_stream`), so a new door that forgets
/// its scope fails loudly instead of reading for free.
pub async fn as_background<F: std::future::Future>(fut: F) -> F::Output {
    SPOOL_PAYER.scope(Payer::Background, fut).await
}

/// The payer that the caller runs for, if any. A task that outlives its
/// request (`tokio::spawn`, a response body) does not inherit it: capture it
/// here and run the task through [`as_payer`].
pub fn current_payer() -> Option<Payer> {
    SPOOL_PAYER.try_with(Payer::clone).ok()
}

/// [`as_principal`] / [`as_background`] for a payer captured with
/// [`current_payer`] (`None`: no scope, as before).
pub async fn as_payer<F: std::future::Future>(payer: Option<Payer>, fut: F) -> F::Output {
    match payer {
        Some(payer) => SPOOL_PAYER.scope(payer, fut).await,
        None => fut.await,
    }
}

/// What a spawned task needs to read like its parent: the payer and the
/// backend scope ([`scoped`]).
#[derive(Debug, Clone, Default)]
pub struct SpoolContext {
    payer: Option<Payer>,
    backend: Option<Arc<str>>,
}

/// The context of the caller, for a task that it spawns.
pub fn capture_context() -> SpoolContext {
    SpoolContext {
        payer: current_payer(),
        backend: current_scope(),
    }
}

/// Run `fut` in a captured context.
pub async fn in_context<F: std::future::Future>(ctx: SpoolContext, fut: F) -> F::Output {
    scoped_in(ctx.backend, as_payer(ctx.payer, fut)).await
}

/// The error of a read that no scope covers (release builds; debug builds
/// panic first).
fn unscoped_read_error() -> std::io::Error {
    std::io::Error::other(
        "a large read ran outside every spool scope (no principal, not a background job)",
    )
}

/// THE rule of who pays for a read: the payer of the task, `Ok(None)` for a
/// background job. No scope at all is a bug of the caller: a panic in debug
/// builds, a refusal in release builds.
fn read_payer() -> std::io::Result<Option<Arc<str>>> {
    match current_payer() {
        Some(Payer::Principal(p)) => Ok(Some(p)),
        Some(Payer::Background) => Ok(None),
        None => {
            if cfg!(debug_assertions) {
                panic!("a large read ran outside every spool scope: enter as_principal or as_background");
            }
            tracing::error!("a large read ran outside every spool scope: refused");
            Err(unscoped_read_error())
        }
    }
}

/// What each principal's live response streams hold of the spool, in MiB. A
/// charge exists only while a stream reads a spool file: a file that waits in
/// the range-spool cache is evictable and charges nobody.
struct ReadLedger {
    /// All reads together may hold this much (MiB), clause C aside.
    cap_mib: usize,
    shares: crate::budget_share::Shares,
    state: parking_lot::Mutex<LedgerState>,
}

#[derive(Default)]
struct LedgerState {
    /// The disk space of all live reads: each spool file counts ONCE, however
    /// many principals stream it (the cap is about disk space).
    total: usize,
    by_principal: std::collections::HashMap<Arc<str>, Held>,
    /// The files that are streamed now: their size and how many principals
    /// stream each. `total` counts a file while it is here.
    files: std::collections::HashMap<std::path::PathBuf, (usize, usize)>,
}

impl LedgerState {
    /// Is `file` streamed by some principal now? Its size, if so.
    fn on_disk(&self, file: &Path) -> Option<usize> {
        self.files.get(file).map(|(mib, _)| *mib)
    }

    /// `principal` opens a stream on `file` of `mib`. The principal's own
    /// count includes the file once, however many streams it opens on it; the
    /// total includes it once, however many principals stream it.
    fn add_stream(&mut self, principal: &Arc<str>, file: &Path, mib: usize) {
        let refs = self
            .by_principal
            .entry(principal.clone())
            .or_default()
            .files
            .entry(file.to_path_buf())
            .or_insert(0);
        *refs += 1;
        if *refs > 1 {
            return;
        }
        let entry = self.files.entry(file.to_path_buf()).or_insert((mib, 0));
        let mib = entry.0;
        entry.1 += 1;
        if entry.1 == 1 {
            self.total += mib;
        }
        self.by_principal
            .get_mut(principal)
            .expect("just inserted")
            .mib += mib;
    }

    /// A stream of `principal` on `file` ended.
    fn remove_stream(&mut self, principal: &Arc<str>, file: &Path) {
        let Some(held) = self.by_principal.get_mut(principal) else {
            return;
        };
        let Some(refs) = held.files.get_mut(file) else {
            return;
        };
        *refs -= 1;
        if *refs > 0 {
            return;
        }
        held.files.remove(file);
        let Some((mib, holders)) = self.files.get_mut(file) else {
            return;
        };
        let freed = *mib;
        *holders -= 1;
        if *holders == 0 {
            self.files.remove(file);
            self.total -= freed;
        }
        held.mib -= freed;
        if held.mib == 0 {
            self.by_principal.remove(principal);
        }
    }
}

/// One principal's holdings: the files its live streams read. Streams of one
/// principal on one file charge it once (the file is on disk once).
#[derive(Default)]
struct Held {
    /// What the live streams and the pending fills hold together.
    mib: usize,
    /// Of `mib`: the reservations of fills that run now ([`FillCharge`]).
    pending: usize,
    /// The streams of this principal on each file.
    files: std::collections::HashMap<std::path::PathBuf, usize>,
}

/// RAII: a principal's PENDING charge for a fill that runs now (the output
/// and delta reservation of a large delta decode, or the file of a
/// reference object). It counts like a stream from the moment the fill is
/// admitted, so a flood of fills is refused before any of them reserves or
/// decodes. `FillCharge::into_stream` replaces it by the stream charge of
/// the finished file: one charge, never two. Dropping it (a failed or
/// cancelled fill) releases it.
#[must_use = "the charge ends when this is dropped"]
pub struct FillCharge {
    ledger: Arc<ReadLedger>,
    principal: Arc<str>,
    mib: usize,
    /// The fill was admitted by clause C only (one huge read on an idle
    /// ledger): what it adds to itself ([`Self::extend`]) is admitted too.
    clause_c: bool,
}

/// RAII: a principal's charge for one live response stream of a spool file,
/// released when the stream drops.
#[must_use = "the charge ends when this is dropped"]
pub struct StreamCharge {
    ledger: Arc<ReadLedger>,
    principal: Arc<str>,
    file: std::path::PathBuf,
}

impl ReadLedger {
    /// Pure: is a read of `mib` admitted for a principal that holds `own`
    /// when all hold `total`? `new_in_total` is what the read adds to the
    /// total: `mib`, or 0 for a file that another principal streams already
    /// (no new disk space). The shares work inside the read cap, so reads in
    /// total stay within it; one read on an idle ledger (clause C) may pass it.
    fn admits(&self, own: usize, total: usize, mib: usize, new_in_total: usize) -> bool {
        (own == 0 && total == 0) || self.admits_by_rule(own, total, mib, new_in_total)
    }

    /// [`Self::admits`] without clause C: the clauses A and B, inside the cap.
    fn admits_by_rule(&self, own: usize, total: usize, mib: usize, new_in_total: usize) -> bool {
        let total_after = total.saturating_add(new_in_total);
        total_after <= self.cap_mib
            && (own.saturating_add(mib) as u64 <= self.shares.own_cap()
                || total_after as u64 <= self.shares.shared_cap())
    }

    /// Charge `principal` for a stream of `file` that holds `mib`.
    /// `None`: refused ([`crate::budget_share`] clauses A, B, C).
    fn try_charge(
        self: &Arc<Self>,
        principal: Arc<str>,
        file: &Path,
        mib: usize,
    ) -> Option<StreamCharge> {
        let mut state = self.state.lock();
        let (own, known) = state
            .by_principal
            .get(&principal)
            .map_or((0, false), |h| (h.mib, h.files.contains_key(file)));
        // A principal already reading this file pays nothing more for it.
        if !known {
            // A file on disk already has its size, and adds nothing to the total.
            let on_disk = state.on_disk(file);
            if !self.admits(
                own,
                state.total,
                on_disk.unwrap_or(mib),
                if on_disk.is_some() { 0 } else { mib },
            ) {
                return None;
            }
        }
        state.add_stream(&principal, file, mib);
        Some(StreamCharge {
            ledger: self.clone(),
            principal,
            file: file.to_path_buf(),
        })
    }
}

impl ReadLedger {
    /// Charge `principal` `mib` for a fill that is about to reserve it.
    /// `None`: refused, by the same rule as a stream.
    fn try_pending(self: &Arc<Self>, principal: Arc<str>, mib: usize) -> Option<FillCharge> {
        let mut state = self.state.lock();
        let own = state.by_principal.get(&principal).map_or(0, |h| h.mib);
        let clause_c = !self.admits_by_rule(own, state.total, mib, mib);
        if clause_c && !self.admits(own, state.total, mib, mib) {
            return None;
        }
        let held = state.by_principal.entry(principal.clone()).or_default();
        held.mib += mib;
        held.pending += mib;
        state.total += mib;
        Some(FillCharge {
            ledger: self.clone(),
            principal,
            mib,
            clause_c,
        })
    }

    /// Give back `mib` of a pending charge (the caller holds the lock).
    fn release_pending(state: &mut LedgerState, principal: &Arc<str>, mib: usize) {
        let Some(held) = state.by_principal.get_mut(principal) else {
            return;
        };
        held.pending -= mib;
        held.mib -= mib;
        if held.mib == 0 {
            state.by_principal.remove(principal);
        }
        state.total -= mib;
    }
}

impl FillCharge {
    /// A second charge of the same principal for another part of the same
    /// fill (the reference copy, whose size is known only after the fill
    /// was admitted). The same rule admits it, and a fill that clause C
    /// admitted may grow beyond the cap, as the one huge read may.
    /// `None`: refused.
    pub(crate) fn extend(&self, mib: usize) -> Option<FillCharge> {
        let mut state = self.ledger.state.lock();
        let own = state.by_principal.get(&self.principal).map_or(0, |h| h.mib);
        if !self.clause_c && !self.ledger.admits_by_rule(own, state.total, mib, mib) {
            return None;
        }
        let held = state
            .by_principal
            .entry(self.principal.clone())
            .or_default();
        held.mib += mib;
        held.pending += mib;
        state.total += mib;
        Some(FillCharge {
            ledger: self.ledger.clone(),
            principal: self.principal.clone(),
            mib,
            clause_c: self.clause_c,
        })
    }

    /// The fill is done: in one step, release this charge and charge the
    /// principal for the stream of `file` (`mib`, at most what this charge
    /// held, so no admission is asked again).
    pub(crate) fn into_stream(mut self, file: &Path, mib: usize) -> StreamCharge {
        let mut state = self.ledger.state.lock();
        let pending = std::mem::take(&mut self.mib);
        // The stream first, so the principal's entry is never dropped between.
        state.add_stream(&self.principal, file, mib);
        Self::drop_pending(&mut state, &self.principal, pending);
        StreamCharge {
            ledger: self.ledger.clone(),
            principal: self.principal.clone(),
            file: file.to_path_buf(),
        }
    }

    fn drop_pending(state: &mut LedgerState, principal: &Arc<str>, mib: usize) {
        ReadLedger::release_pending(state, principal, mib);
    }
}

impl Drop for FillCharge {
    fn drop(&mut self) {
        if self.mib > 0 {
            let mut state = self.ledger.state.lock();
            ReadLedger::release_pending(&mut state, &self.principal, self.mib);
        }
    }
}

impl Drop for StreamCharge {
    fn drop(&mut self) {
        let mut state = self.ledger.state.lock();
        state.remove_stream(&self.principal, &self.file);
    }
}

/// The refusal of a read that the share rule does not admit.
fn refused_read_error() -> std::io::Error {
    std::io::Error::new(
        CONTENDED,
        "this principal's downloads already hold its share of the spool budget \
         and the other principals use the rest (DGP_SPOOL_READ_SHARE_PERCENT, \
         DGP_SPOOL_READ_MAX_PERCENT); retry shortly",
    )
}

/// A byte-budget-gated pool of temp spool bytes.
#[derive(Clone)]
pub struct SpoolDir {
    dir: PathBuf,
    budget: Arc<Budget>,
    max_bytes: u64,
    /// The disk facts behind the budget; `None` for a spool built with a
    /// fixed size (`new`).
    boot: Option<Arc<BootLimits>>,
    /// The most of the budget (MiB) that the reservations made in one
    /// backend's scope may hold together.
    share_mib: usize,
    /// The share budgets, one per backend scope.
    shares: Arc<parking_lot::Mutex<std::collections::HashMap<Arc<str>, Arc<Budget>>>>,
    /// What each principal's live response streams hold ([`as_principal`]).
    reads: Arc<ReadLedger>,
    /// `DGP_SPOOL_READ_SHARE_PERCENT` and `DGP_SPOOL_READ_MAX_PERCENT`, which
    /// make `reads`.
    read_percents: (u8, u8),
    /// Holders of optional spool files (the range-read reconstruction
    /// cache) that give budget back when an acquire finds it short.
    evictors: Arc<parking_lot::Mutex<Vec<std::sync::Weak<dyn SpoolEvictor>>>>,
    /// The cached files were dropped for the disk level that refuses writes
    /// now (once for each rise of the level).
    dropped_for_level: Arc<std::sync::atomic::AtomicBool>,
    /// The disk level (`disk_watch`): at high or critical a new write is
    /// refused ([`SpoolDir::admit_new_write`]).
    gate: Arc<crate::disk_watch::SpoolGate>,
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
    /// Build from env: `DGP_SPOOL_DIR` + `DGP_SPOOL_MAX_BYTES` (`auto`, a
    /// percent of the free space, or bytes; default `auto`). The budget is
    /// derived here, once, AFTER the orphan sweep (the files that stay are
    /// counted as free), by `config::limits::derive_limits`. `data_roots`
    /// are the filesystem backend roots and the data directory: a spool on
    /// their device gets a smaller `auto` share.
    ///
    /// Default dir is a DEDICATED `dgp-spool` subdir of the system temp — NOT the
    /// shared temp root (review M3.4: sweeping or filling the shared root is
    /// dangerous; a dedicated subdir we own is safe to sweep). The directory is
    /// created if missing and SWEPT of stale orphans at startup (review M1.8: a
    /// hard crash leaves spool files behind since `NamedTempFile`'s Drop never
    /// ran). The sweep is AGE-based (`STALE_TEMP_MAX_AGE`, the same rule as the
    /// filesystem backend's temp sweep): the directory may be shared with
    /// another live process, whose active spool files are fresh, so only a
    /// file untouched for that long goes.
    #[allow(
        clippy::disallowed_methods,
        reason = "the spool's own default directory"
    )]
    pub fn from_env(data_roots: &[PathBuf]) -> std::io::Result<Self> {
        let dir = crate::config::process_env_os("DGP_SPOOL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("dgp-spool"));
        let (setting, invalid) = crate::config::limits::parse_spool_setting(
            crate::config::process_env("DGP_SPOOL_MAX_BYTES").as_deref(),
        );
        if let Some(raw) = &invalid {
            tracing::warn!("{}", crate::config::limits::invalid_spool_message(raw));
        }
        let share: u8 = crate::config::env_parse_with_default(
            "DGP_BACKEND_SHARE_PERCENT",
            crate::config::tuning::DEFAULT_BACKEND_SHARE_PERCENT,
        );
        let read_share: u8 = crate::config::env_parse_with_default(
            "DGP_SPOOL_READ_SHARE_PERCENT",
            crate::config::tuning::DEFAULT_SPOOL_READ_SHARE_PERCENT,
        );
        let read_max: u8 = crate::config::env_parse_with_default(
            "DGP_SPOOL_READ_MAX_PERCENT",
            crate::config::tuning::DEFAULT_SPOOL_READ_MAX_PERCENT,
        );
        Self::build(
            dir,
            setting,
            invalid,
            share,
            data_roots,
            &crate::config::process_env,
        )
        .map(|pool| {
            pool.with_read_share(read_share.clamp(1, 100))
                .with_read_max(read_max.clamp(1, 100))
        })
    }

    /// [`Self::from_env`] with its inputs given: create and sweep the
    /// directory, THEN measure its disk (the files that stay count as free),
    /// derive the budget, and build the pool.
    pub(crate) fn build(
        dir: PathBuf,
        setting: crate::config::limits::SpoolSetting,
        invalid: Option<String>,
        share: u8,
        data_roots: &[PathBuf],
        env: crate::config::EnvLookup,
    ) -> std::io::Result<Self> {
        let unusable = |e: std::io::Error| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "the spool directory '{}' (set by DGP_SPOOL_DIR, default: dgp-spool in \
                     the system temp dir) is not usable: {e}",
                    dir.display()
                ),
            )
        };
        // Create and sweep first: the budget counts what stays.
        Self::new(dir.clone(), 0).map_err(unusable)?.sweep_orphans();
        let facts = crate::config::limits::measure_disk_facts(&dir, data_roots, env);
        let (effective, _warnings) = crate::config::limits::derive_limits(
            facts,
            crate::config::limits::ExplicitLimits {
                spool: setting,
                ..Default::default()
            },
        );
        let mut pool = Self::new(dir.clone(), effective.spool_budget)
            .map_err(unusable)?
            .with_backend_share(share);
        pool.boot = Some(Arc::new(BootLimits {
            budget_note: effective.spool_note(&dir),
            dir,
            facts,
            spool_invalid: invalid,
        }));
        Ok(pool)
    }

    /// THE process-wide spool, built from env on first use (the orphan sweep
    /// runs once, then). Every engine, including one rebuilt on a config
    /// reload, shares it. A spool per engine gave a reload a second full
    /// budget while the old engine's requests still held the first, and
    /// re-ran the sweep each time.
    pub fn shared() -> std::io::Result<Self> {
        Self::shared_with(&[])
    }

    fn shared_with(data_roots: &[PathBuf]) -> std::io::Result<Self> {
        if let Some(pool) = SHARED.get() {
            return Ok(pool.clone());
        }
        let pool = Self::from_env(data_roots)?;
        // A racing first caller may win; either way every caller gets the
        // one stored pool.
        Ok(SHARED.get_or_init(|| pool).clone())
    }

    /// The boot's own call of [`Self::shared`]: it knows the data
    /// directories, and it publishes what the spool measured
    /// ([`boot_limits`]) so that the config derives its limits from it.
    /// Source guard: `the_environment_is_claimed_before_the_first_worker_spawns`.
    pub fn claim_shared(data_roots: &[PathBuf]) -> std::io::Result<Self> {
        let pool = Self::shared_with(data_roots)?;
        CLAIMED.store(true, std::sync::atomic::Ordering::Release);
        Ok(pool)
    }

    /// What this spool measured at boot (`None` for a spool of fixed size).
    pub fn boot(&self) -> Option<&BootLimits> {
        self.boot.as_deref()
    }

    /// How the budget was found, for an error message: `auto: 50% of 46
    /// GiB free on /tmp/dgp-spool at boot`, or `set explicitly`. A spool of
    /// fixed size gives `set explicitly`.
    pub fn budget_note(&self) -> &str {
        self.boot
            .as_deref()
            .map_or("set explicitly", |b| b.budget_note.as_str())
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
            boot: None,
            share_mib: backend_share_mib(
                max_mib,
                crate::config::tuning::DEFAULT_BACKEND_SHARE_PERCENT,
            ),
            shares: Default::default(),
            reads: new_read_ledger(
                max_mib,
                crate::config::tuning::DEFAULT_SPOOL_READ_SHARE_PERCENT,
                crate::config::tuning::DEFAULT_SPOOL_READ_MAX_PERCENT,
            ),
            read_percents: (
                crate::config::tuning::DEFAULT_SPOOL_READ_SHARE_PERCENT,
                crate::config::tuning::DEFAULT_SPOOL_READ_MAX_PERCENT,
            ),
            evictors: Default::default(),
            dropped_for_level: Default::default(),
            gate: Default::default(),
        })
    }

    /// The disk level that the monitor (`disk_watch`) sets and this spool
    /// reads. Clones of the spool share it.
    pub fn gate(&self) -> &Arc<crate::disk_watch::SpoolGate> {
        &self.gate
    }

    /// Admit a NEW write: an op that holds no spool yet. It is refused with
    /// [`CONTENDED`] (the engine answers a retryable `503 SlowDown`, with the
    /// message) while the disk of the spool is at level high or critical
    /// (the monitor's verdict), or, where the free space hides the real
    /// limit, while the reserved bytes leave less than the high threshold of
    /// the budget (judged now, from `used / budget`: no `statvfs`, one lock
    /// read). An op that holds a spool, a read and a delete never ask: they
    /// run to the end.
    pub fn admit_new_write(&self) -> std::io::Result<()> {
        match self.write_refusal() {
            Some(_) => Err(std::io::Error::new(
                CONTENDED,
                crate::disk_watch::DiskFullRefusal,
            )),
            None => Ok(()),
        }
    }

    /// THE answer to "would a new spool write be refused now": the disk level
    /// of the spool, else (where the free space hides the limit) the use of
    /// the budget. [`Self::admit_new_write`] and every job that decides on a
    /// refusal ask this one function, so none misses a refusal by use.
    ///
    /// Cached files are optional. A refusal by use drops them first, one at
    /// a time, and asks again. A refusal by level drops them all once, when
    /// the level rises (the monitor measures the freed space at its next
    /// tick); a cache keeps nothing new while the level refuses writes.
    pub fn write_refusal(&self) -> Option<String> {
        use std::sync::atomic::Ordering;
        if let Some(by_level) = self.gate.refusal() {
            if !self.dropped_for_level.swap(true, Ordering::SeqCst) {
                while self.evict_one_cached() {}
            }
            return Some(by_level);
        }
        self.dropped_for_level.store(false, Ordering::SeqCst);
        let th = self.gate.by_use()?;
        loop {
            let (budget, used) = (self.max_bytes(), self.used_bytes());
            let refusal = crate::disk_watch::use_refusal(&th, budget, used);
            if refusal.is_none() || !self.evict_one_cached() {
                self.gate
                    .note_use_refusal(refusal.is_some(), self.dir(), used, budget);
                return refusal;
            }
        }
    }

    /// Drop one cached file of any evictor; `false` when none is left.
    fn evict_one_cached(&self) -> bool {
        let evictors: Vec<_> = self
            .evictors
            .lock()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        evictors.iter().any(|e| e.evict_one())
    }

    /// Spool bytes reserved now (the budget in use), for the gauge. The
    /// reservations count whole MiB, and the budget rounds up to whole MiB, so
    /// the count is cut at the budget in bytes: it never shows more than
    /// [`Self::max_bytes`].
    pub fn used_bytes(&self) -> u64 {
        const MIB: u64 = 1024 * 1024;
        (self.budget.max.saturating_sub(self.budget.free()) as u64)
            .saturating_mul(MIB)
            .min(self.max_bytes)
    }

    /// Let the reservations made in one backend's scope ([`scoped`]) hold
    /// at most `percent` of the budget together. One slow backend's large
    /// GETs held the whole budget, and every large request on the other
    /// backends waited for it, then got 503 SlowDown. 100 turns it off.
    pub fn with_backend_share(mut self, percent: u8) -> Self {
        self.share_mib = backend_share_mib(self.budget.max, percent);
        self
    }

    /// Let the live response streams of one principal ([`as_principal`],
    /// `Self::charge_stream`) hold `percent` of the budget on their own, and
    /// more while all streams together hold at most the rest
    /// ([`crate::budget_share`]). One user's large GETs held the whole budget,
    /// and every other tenant's large write waited for it. 100 turns it off.
    pub fn with_read_share(mut self, percent: u8) -> Self {
        self.read_percents.0 = percent;
        self.reads = new_read_ledger(self.budget.max, percent, self.read_percents.1);
        self
    }

    /// Let all live response streams together hold at most `percent` of the
    /// budget (clause C aside), so writes always keep the rest. The shares
    /// of the principals work inside this cap. 100 turns the cap off.
    pub fn with_read_max(mut self, percent: u8) -> Self {
        self.read_percents.1 = percent;
        self.reads = new_read_ledger(self.budget.max, self.read_percents.0, percent);
        self
    }

    /// Charge the principal of the current scope ([`as_principal`]) for a
    /// live response stream of `file`, for as long as the returned charge
    /// lives. `Ok(None)`: a background job, or shares and read cap off.
    /// A caller in no scope at all panics (debug) or is refused (release).
    /// [`CONTENDED`] when the stream is refused: the principal holds its
    /// share and the other principals use more than the rest.
    pub(crate) fn charge_stream(&self, file: &Spool) -> std::io::Result<Option<StreamCharge>> {
        let Some(principal) = read_payer()? else {
            return Ok(None);
        };
        if self.read_percents == (100, 100) {
            return Ok(None);
        }
        self.reads
            .try_charge(principal, file.path(), file.reserved_mib())
            .map(Some)
            .ok_or_else(refused_read_error)
    }

    /// Charge the principal of the current scope for a fill that will reserve
    /// `mib` of spool (a large delta decode, a reference object): asked BEFORE
    /// the fill reserves, downloads or decodes anything. `Ok(None)`: no
    /// principal or no charge. [`CONTENDED`] when the principal is over its
    /// reach.
    pub(crate) fn charge_fill(&self, mib: usize) -> std::io::Result<Option<FillCharge>> {
        let Some(principal) = read_payer()? else {
            return Ok(None);
        };
        if self.read_percents == (100, 100) {
            return Ok(None);
        }
        self.reads
            .try_pending(principal, mib)
            .map(Some)
            .ok_or_else(refused_read_error)
    }

    /// [`FillCharge::extend`] of the fill that `parent` charges: `Ok(None)`
    /// when the fill is not charged (no principal, or no read charge).
    pub(crate) fn extend_fill(
        &self,
        parent: Option<&FillCharge>,
        mib: usize,
    ) -> std::io::Result<Option<FillCharge>> {
        match parent {
            None => Ok(None),
            Some(fill) => fill.extend(mib).map(Some).ok_or_else(refused_read_error),
        }
    }

    /// What `principal`'s live streams hold now, in MiB, and what all
    /// principals hold. For the tests.
    #[cfg(test)]
    pub(crate) fn read_held(&self, principal: &str) -> (usize, usize) {
        let state = self.reads.state.lock();
        (
            state.by_principal.get(principal).map_or(0, |h| h.mib),
            state.total,
        )
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

    /// Can a spool file be made now? Re-creates a missing directory, makes
    /// and drops one temp file. Blocking, cheap: `/_/ready` runs it.
    pub fn is_usable(&self) -> bool {
        temp_in(&self.dir).is_ok()
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
        // `STALE_TEMP_MAX_AGE` (1 h): comfortably longer than any single
        // reconstruction, short enough to reclaim crash debris promptly.
        self.sweep_older_than(crate::storage::STALE_TEMP_MAX_AGE);
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
            // The backend's share: a request over its backend's share
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
        let file = temp_in(&self.dir)?;
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
                    file: temp_in(&self.dir)?,
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
            file: temp_in(&self.dir)?,
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
            file: temp_in(&self.dir.dir)?,
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

/// Spool budget an op holds already, in MiB. The disk gate lets such an op
/// run to its end (`DeltaGliderEngine::admit_new_spool_write`); made from a
/// held spool, a held reservation, or the parts of an upload, never from a
/// bare number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeldSpool {
    mib: usize,
    /// The bytes that the response stream sends: all of them (`u64::MAX`) or
    /// the length of a range, which is less than the file the stream holds.
    send_bytes: u64,
}

impl HeldSpool {
    /// The op holds nothing: a new write.
    pub const NOTHING: HeldSpool = HeldSpool::mib_of(0);

    const fn mib_of(mib: usize) -> Self {
        Self {
            mib,
            send_bytes: u64::MAX,
        }
    }

    pub(crate) fn of(spool: &Spool) -> Self {
        Self::mib_of(spool.reserved_mib())
    }

    pub fn of_reservation(r: &SpoolReservation) -> Self {
        Self::mib_of(r.reserved_mib())
    }

    /// The budget of every item the op holds (the relay parts of an upload).
    pub fn of_all<'a>(held: impl IntoIterator<Item = &'a SpoolReservation>) -> Self {
        Self::mib_of(held.into_iter().map(SpoolReservation::reserved_mib).sum())
    }

    /// The budget the relay parts of an upload hold (one reservation each,
    /// whole MiB per part); an upload with no parts holds nothing.
    pub(crate) fn of_relay_parts(sizes: impl IntoIterator<Item = u64>) -> Self {
        Self::mib_of(sizes.into_iter().map(mib_ceil).sum())
    }

    /// This hold, for a stream that sends `bytes` of the file (a range).
    pub(crate) fn sending(self, bytes: u64) -> Self {
        Self {
            send_bytes: bytes,
            ..self
        }
    }

    /// The bytes the stream sends; `u64::MAX` when it sends the whole hold.
    pub fn send_bytes(self) -> u64 {
        self.send_bytes
    }

    #[cfg(test)]
    /// The budget of a stream that holds a spool file of `bytes`, rounded up
    /// to whole MiB; no bytes is `NOTHING`.
    pub(crate) fn of_stream_bytes(bytes: u64) -> Self {
        Self::mib_of(mib_ceil(bytes))
    }

    /// This budget, at least 1 MiB (a stream holds a file even when empty).
    pub(crate) fn at_least_one_mib(self) -> Self {
        Self {
            mib: self.mib.max(1),
            ..self
        }
    }

    /// This and `other` together.
    pub(crate) fn plus(self, other: HeldSpool) -> Self {
        Self {
            mib: self.mib + other.mib,
            send_bytes: self.send_bytes.max(other.send_bytes),
        }
    }

    pub fn mib(self) -> usize {
        self.mib
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

/// The ledger of a budget of `max_mib` with each principal's read share at
/// `percent`.
fn new_read_ledger(max_mib: usize, percent: u8, max_percent: u8) -> Arc<ReadLedger> {
    let cap_mib = (max_mib.saturating_mul(usize::from(max_percent.clamp(1, 100))) / 100).max(1);
    Arc::new(ReadLedger {
        cap_mib,
        shares: crate::budget_share::Shares::new(cap_mib as u64, percent, 1),
        state: Default::default(),
    })
}

/// Pure: one backend's share of a `max_mib` budget at `percent` (at least
/// 1 MiB; 100 % or more is the whole budget).
fn backend_share_mib(max_mib: usize, percent: u8) -> usize {
    (max_mib.saturating_mul(usize::from(percent.min(100))) / 100).max(1)
}

/// A temp file in the spool directory `dir`. Somebody can remove the
/// directory while the process runs (a tmp cleaner, an operator). The first
/// `ENOENT` creates the directory again and tries once more, like the
/// filesystem backend does for a folder that a DELETE pruned. Any other
/// error, and a second `ENOENT`, go to the caller. The directory is made by
/// [`create_private_dir`], the one function that makes a spool directory
/// (boot and re-creation): mode 0700 whatever the umask.
pub(crate) fn temp_in(dir: &Path) -> std::io::Result<NamedTempFile> {
    match NamedTempFile::new_in(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            create_private_dir(dir)?;
            NamedTempFile::new_in(dir)
        }
        other => other,
    }
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
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
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
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
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
    /// The gauge of the reserved bytes never shows more than the budget: the
    /// reservations count whole MiB and the budget rounds up, so a budget that
    /// is not a whole number of MiB would show up to 1 MiB too much.
    #[test]
    fn used_bytes_never_exceeds_a_budget_that_is_not_whole_mib() {
        const MIB: u64 = 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        for max in [3 * MIB + 1, 10_000_000_000, 7 * MIB, MIB / 2 + 5] {
            let spool = SpoolDir::new(dir.path().join(format!("s{max}")), max).unwrap();
            assert_eq!(spool.used_bytes(), 0);
            let held = spool.try_acquire(max).unwrap();
            assert_eq!(spool.used_bytes(), max, "budget {max} held in full");
            drop(held);
            assert_eq!(spool.used_bytes(), 0);
        }
    }

    /// One answer to "is a new write refused now": the disk level OR the use of
    /// the budget, and `admit_new_write` agrees with it.
    #[test]
    fn write_refusal_is_the_level_or_the_use_and_admission_agrees() {
        const MIB: u64 = 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path().join("s"), 64 * MIB).unwrap();
        assert_eq!(spool.write_refusal(), None);
        assert!(spool.admit_new_write().is_ok());
        // By level.
        spool.gate().set(crate::disk_watch::Level::High);
        assert_eq!(
            spool.write_refusal().as_deref(),
            Some(crate::disk_watch::UPLOADS_PAUSED_MESSAGE)
        );
        assert!(spool.admit_new_write().is_err());
        spool.gate().set(crate::disk_watch::Level::Ok);
        // By use: the budget is nearly all reserved.
        let mut th = crate::disk_watch::Thresholds::new(
            &crate::config::tuning::RuntimeTuning::default().disk_monitor,
            MIB,
        );
        th.low_floor = 0;
        th.high_floor = 32 * MIB;
        th.critical_floor = 0;
        spool.gate().set_by_use(Some(th));
        assert_eq!(spool.write_refusal(), None, "the budget is free");
        let _held = spool.try_acquire(40 * MIB).unwrap();
        assert!(spool.write_refusal().is_some(), "the budget is in use");
        assert!(spool.admit_new_write().is_err());
    }

    /// Cached files are optional: they never refuse a write. A refusal by
    /// the use of the budget drops them first.
    #[test]
    fn a_refusal_by_use_drops_cached_files_first() {
        const MIB: u64 = 1024 * 1024;
        struct Cache(parking_lot::Mutex<Vec<Spool>>);
        impl SpoolEvictor for Cache {
            fn evict_one(&self) -> bool {
                self.0.lock().pop().is_some()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path().join("s"), 64 * MIB).unwrap();
        let mut th = crate::disk_watch::Thresholds::new(
            &crate::config::tuning::RuntimeTuning::default().disk_monitor,
            MIB,
        );
        th.low_floor = 0;
        th.high_floor = 32 * MIB;
        th.critical_floor = 0;
        spool.gate().set_by_use(Some(th));
        let files = (0..4)
            .map(|_| spool.try_acquire(10 * MIB).unwrap())
            .collect();
        let cache = Arc::new(Cache(parking_lot::Mutex::new(files)));
        spool.register_evictor(Arc::downgrade(&cache) as std::sync::Weak<dyn SpoolEvictor>);
        assert_eq!(spool.write_refusal(), None, "the cached files made way");
        assert_eq!(cache.0.lock().len(), 3, "and only as many as needed");
        // Space that a request holds is not optional.
        let _held = spool.try_acquire(40 * MIB).unwrap();
        assert!(spool.write_refusal().is_some());
        assert!(cache.0.lock().is_empty());
    }

    /// A refusal by the disk level drops the cached files once, when the
    /// level rises, not at every question: a cache that fills again during
    /// the episode is not emptied by each refused write.
    #[test]
    fn a_refusal_by_level_drops_cached_files_once_per_rise() {
        const MIB: u64 = 1024 * 1024;
        struct Cache(parking_lot::Mutex<Vec<Spool>>);
        impl SpoolEvictor for Cache {
            fn evict_one(&self) -> bool {
                self.0.lock().pop().is_some()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let spool = SpoolDir::new(dir.path().join("s"), 64 * MIB).unwrap();
        let cache = Arc::new(Cache(parking_lot::Mutex::new(Vec::new())));
        spool.register_evictor(Arc::downgrade(&cache) as std::sync::Weak<dyn SpoolEvictor>);
        let fill = || cache.0.lock().push(spool.try_acquire(MIB).unwrap());
        let level = |l| spool.gate().set(l);
        fill();
        level(crate::disk_watch::Level::High);
        assert!(spool.write_refusal().is_some());
        assert!(cache.0.lock().is_empty(), "dropped when the level rose");
        fill();
        assert!(spool.write_refusal().is_some());
        assert_eq!(cache.0.lock().len(), 1, "not again in the same episode");
        level(crate::disk_watch::Level::Ok);
        assert_eq!(spool.write_refusal(), None);
        level(crate::disk_watch::Level::High);
        assert!(spool.write_refusal().is_some());
        assert!(cache.0.lock().is_empty(), "the next rise drops again");
    }

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

    /// The check itself: it sets the umask of its PROCESS, so it never runs
    /// with the other tests. It is `#[ignore]`d and runs only in the child
    /// process that the next test starts.
    #[cfg(unix)]
    #[test]
    #[ignore = "manual: cargo test --lib -- --ignored --exact deltaglider::spool::tests::child_a_spool_directory_made_again_is_private_under_a_wide_umask (the parent test runs it in a child process)"]
    fn child_a_spool_directory_made_again_is_private_under_a_wide_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("spool");
        let spool = SpoolDir::new(root.clone(), 1 << 20).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        // SAFETY: umask has no preconditions; this process runs this test only.
        unsafe { libc::umask(0) };
        let made = temp_in(&root);
        made.unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700, "the re-created spool directory");
        drop(spool);
    }

    /// A spool directory that the proxy makes again after a removal is 0700
    /// under a wide umask. The umask is process-wide, so the check runs in a
    /// child process (this test binary, one test), and no parallel test sees it.
    #[cfg(unix)]
    #[test]
    fn a_spool_directory_made_again_is_private_under_a_wide_umask() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "--ignored",
                "--test-threads=1",
                "deltaglider::spool::tests::child_a_spool_directory_made_again_is_private_under_a_wide_umask",
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("1 passed"),
            "the child test did not run: {text}"
        );
    }

    #[test]
    fn only_create_private_dir_makes_a_spool_directory() {
        let text = crate::source_scan::prod_text(include_str!("spool.rs"));
        assert_eq!(
            text.matches(concat!("create_dir", "_all(")).count(),
            1,
            "the non-unix twin of create_private_dir is the only create_dir_all"
        );
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
    async fn a_removed_dir_is_made_again_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("spool");
        let pool = SpoolDir::new(dir.clone(), 8 << 20).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let spool = pool.acquire(1).await.expect("the dir is made again");
        assert!(spool.path().starts_with(&dir));
        drop(spool);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(pool.try_acquire(1).is_ok());
        assert!(pool.is_usable());
    }

    #[test]
    fn a_dir_that_cannot_exist_is_not_usable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("spool");
        let pool = SpoolDir::new(dir.clone(), 8 << 20).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        // A file where the directory belongs: create_dir_all fails.
        std::fs::write(&dir, b"x").unwrap();
        assert!(!pool.is_usable());
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
        pool.sweep_older_than(crate::storage::STALE_TEMP_MAX_AGE);
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
#[cfg(test)]
mod read_share_tests {
    use super::*;
    use proptest::prelude::*;
    use std::path::PathBuf;

    const MIB: u64 = 1024 * 1024;

    fn ledger(cap_mib: usize, percent: u8) -> Arc<ReadLedger> {
        new_read_ledger(cap_mib, percent, 100)
    }

    fn file(i: usize) -> PathBuf {
        PathBuf::from(format!("/spool/file-{i}"))
    }

    fn name(p: usize) -> Arc<str> {
        ["dana", "ci-uploader", "backup-bot", "$anonymous"][p].into()
    }

    /// What `principal` holds, in MiB (each file it streams once).
    fn read_held_of(l: &ReadLedger, principal: &Arc<str>) -> usize {
        l.state
            .lock()
            .by_principal
            .get(principal)
            .map_or(0, |h| h.mib)
    }

    #[test]
    fn one_principal_alone_reaches_three_quarters_and_the_others_keep_their_share() {
        let l = ledger(100, 25);
        let mut mallory = Vec::new();
        for i in 0..5 {
            mallory.push(l.try_charge(name(0), &file(i), 15).expect("within 75"));
        }
        assert!(l.try_charge(name(0), &file(5), 1).is_none(), "76 > 75");
        // Every other principal still has its own 25.
        let bot = l.try_charge(name(2), &file(10), 25).expect("its own share");
        assert!(
            l.try_charge(name(2), &file(11), 1).is_none(),
            "past 25 and the rest"
        );
        // The abuser gives back a file: the budget is full while the bot holds
        // its share, and the abuser may take one again when the bot is gone.
        drop(mallory.pop());
        assert!(
            l.try_charge(name(0), &file(5), 15).is_none(),
            "all use is 85"
        );
        drop(bot);
        assert!(l.try_charge(name(0), &file(5), 15).is_some());
    }

    #[test]
    fn a_single_read_larger_than_the_rest_runs_on_an_idle_budget() {
        let l = ledger(100, 25);
        let big = l.try_charge(name(0), &file(0), 90).expect("clause C");
        // Nothing else of the same principal; another principal has its share.
        assert!(l.try_charge(name(0), &file(1), 1).is_none());
        let _other = l.try_charge(name(1), &file(2), 5).expect("its own share");
        drop(big);
        // A second big read needs the budget idle again.
        let _again = l.try_charge(name(0), &file(0), 90);
        assert!(
            l.try_charge(name(1), &file(3), 90).is_none(),
            "others hold space"
        );
    }

    /// Streams of one principal on one file charge it once: the file is on
    /// disk once (a parallel downloader reads one file with many ranges).
    #[test]
    fn streams_of_one_principal_on_one_file_charge_once() {
        let l = ledger(100, 25);
        let a = l.try_charge(name(0), &file(0), 70).unwrap();
        let b = l
            .try_charge(name(0), &file(0), 70)
            .expect("same file, no new charge");
        assert_eq!(l.state.lock().total, 70);
        // Another principal reading the same file counts it for itself, but
        // the file is on disk once: the total does not move.
        let c = l.try_charge(name(1), &file(0), 70).unwrap();
        assert_eq!(l.state.lock().total, 70);
        assert_eq!(l.state.lock().by_principal[&name(1)].mib, 70);
        drop(a);
        assert_eq!(l.state.lock().total, 70, "one stream of dana is still open");
        drop(b);
        assert_eq!(
            l.state.lock().total,
            70,
            "the other principal still reads it"
        );
        drop(c);
        let state = l.state.lock();
        assert_eq!(state.total, 0);
        assert!(state.by_principal.is_empty(), "no entry stays at 0");
    }

    /// Two principals read one large file at the same time: the cap is about
    /// disk space, and the file is on disk once.
    #[test]
    fn two_principals_can_read_one_large_file_at_once() {
        // Budget 100 MiB, read cap 75, own share 18, rest 56.
        let l = new_read_ledger(100, 25, 75);
        let a = l.try_charge(name(0), &file(0), 50).expect("alone");
        let b = l
            .try_charge(name(1), &file(0), 50)
            .expect("the file is on disk already: no new space");
        assert_eq!(l.state.lock().total, 50);
        // Each principal's own count still has the file (clause A).
        assert_eq!(read_held_of(&l, &name(0)), 50);
        assert_eq!(read_held_of(&l, &name(1)), 50);
        // A different file still needs room: 50 + 50 > 75.
        assert!(l.try_charge(name(2), &file(1), 50).is_none());
        drop(a);
        assert_eq!(l.state.lock().total, 50, "the second reader keeps it");
        drop(b);
        assert_eq!(l.state.lock().total, 0);
        assert!(l.state.lock().by_principal.is_empty());
    }

    /// Reads in total stop at the read cap: two principals together never
    /// pass it, and the rest of the budget stays for writes.
    #[test]
    fn reads_in_total_stop_at_the_read_cap() {
        // Budget 100 MiB, reads capped at 75, each principal's share 25 % of that.
        let l = new_read_ledger(100, 25, 75);
        assert_eq!(l.cap_mib, 75);
        let a = l
            .try_charge(name(0), &file(0), 56)
            .expect("alone: the larger of A and B");
        assert!(
            l.try_charge(name(0), &file(1), 1).is_none(),
            "past its reach"
        );
        let b = l.try_charge(name(1), &file(2), 18).expect("its own share");
        assert!(
            l.try_charge(name(1), &file(3), 2).is_none(),
            "total would pass 75"
        );
        assert_eq!(l.state.lock().total, 74);
        drop((a, b));
    }

    #[test]
    fn clause_c_passes_the_read_cap_only_on_an_idle_ledger() {
        let l = new_read_ledger(100, 25, 75);
        let big = l
            .try_charge(name(0), &file(0), 90)
            .expect("one big read, idle ledger");
        assert!(
            l.try_charge(name(1), &file(1), 1).is_none(),
            "nobody joins it"
        );
        drop(big);
        let small = l.try_charge(name(1), &file(1), 5).unwrap();
        assert!(l.try_charge(name(0), &file(0), 90).is_none(), "not idle");
        drop(small);
    }

    #[derive(Debug, Clone)]
    enum Op {
        Charge(usize, usize, usize),
        Drop(usize),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0usize..3, 0usize..6, 1usize..30).prop_map(|(p, f, n)| Op::Charge(p, f, n)),
            any::<usize>().prop_map(Op::Drop),
        ]
    }

    proptest! {
        /// Model: the ledger admits what `Shares::admits` says (a file already
        /// read by the principal is free), its books equal the live charges,
        /// and nothing is left when every charge is gone.
        #[test]
        fn the_ledger_follows_the_share_rule(
            cap in 10usize..200, percent in 1u8..=99,
            ops in proptest::collection::vec(op(), 0..80),
        ) {
            let l = ledger(cap, percent);
            let shares = l.shares;
            // (principal, file, mib, charge); the size of a file is the first one seen.
            let mut live: Vec<(usize, usize, usize, StreamCharge)> = Vec::new();
            let mut sizes: std::collections::HashMap<usize, usize> = Default::default();
            for op in ops {
                match op {
                    Op::Charge(p, f, n) => {
                        let known = live.iter().any(|(q, g, _, _)| *q == p && *g == f);
                        let on_disk = live.iter().any(|(_, g, _, _)| *g == f);
                        let own: usize = sizes_held(&live, p);
                        let total: usize = live_total(&live);
                        let mib = *sizes.entry(f).or_insert(n);
                        let expect = known || l.admits(own, total, mib, if on_disk { 0 } else { mib });
                        let _ = shares;
                        let got = l.try_charge(name(p), &file(f), mib);
                        prop_assert_eq!(got.is_some(), expect);
                        if let Some(c) = got { live.push((p, f, mib, c)); }
                    }
                    Op::Drop(i) if !live.is_empty() => {
                        let i = i % live.len();
                        let (p, f, _, c) = live.swap_remove(i);
                        drop(c);
                        let _ = p;
                        if !live.iter().any(|(_, g, _, _)| *g == f) {
                            sizes.remove(&f);
                        }
                    }
                    Op::Drop(_) => {}
                }
                prop_assert_eq!(l.state.lock().total, live_total(&live));
                for p in 0..3 {
                    let held = l.state.lock().by_principal.get(&name(p)).map_or(0, |h| h.mib);
                    prop_assert_eq!(held, sizes_held(&live, p));
                }
            }
            live.clear();
            let state = l.state.lock();
            prop_assert_eq!(state.total, 0);
            prop_assert!(state.by_principal.is_empty());
        }
    }

    proptest! {
        /// N principals streaming one file charge the total once, and
        /// releasing in any order returns the total to zero exactly once.
        #[test]
        fn many_principals_on_one_file_charge_the_total_once(
            mib in 1usize..60,
            readers in proptest::collection::vec((0usize..4, any::<u8>()), 1..12),
            release in proptest::collection::vec(any::<usize>(), 0..12),
        ) {
            let l = new_read_ledger(1000, 25, 100);
            let mut live: Vec<(usize, StreamCharge)> = Vec::new();
            for (p, _) in &readers {
                if let Some(c) = l.try_charge(name(*p), &file(0), mib) { live.push((*p, c)); }
            }
            prop_assert!(!live.is_empty());
            prop_assert_eq!(l.state.lock().total, mib);
            for p in 0..4 {
                let reads = live.iter().any(|(q, _)| *q == p);
                prop_assert_eq!(read_held_of(&l, &name(p)), if reads { mib } else { 0 });
            }
            for r in release {
                if live.is_empty() { break; }
                let (p, c) = live.swap_remove(r % live.len());
                drop(c);
                let any_left = !live.is_empty();
                prop_assert_eq!(l.state.lock().total, if any_left { mib } else { 0 });
                let own_left = live.iter().any(|(q, _)| *q == p);
                prop_assert_eq!(read_held_of(&l, &name(p)), if own_left { mib } else { 0 });
            }
            live.clear();
            let state = l.state.lock();
            prop_assert_eq!(state.total, 0);
            prop_assert!(state.by_principal.is_empty());
        }
    }

    proptest! {
        /// For any sequence of read admissions, the total read charge stays at
        /// or below the read cap, except through clause C (an idle ledger).
        #[test]
        fn total_read_charge_stays_within_the_read_cap_except_through_clause_c(
            budget in 20usize..200, share in 1u8..=100, max_percent in 10u8..=100,
            ops in proptest::collection::vec(op(), 0..100),
        ) {
            let l = new_read_ledger(budget, share, max_percent);
            let cap = l.cap_mib;
            let mut live: Vec<StreamCharge> = Vec::new();
            for op in ops {
                match op {
                    Op::Charge(p, f, n) => {
                        let before = l.state.lock().total;
                        if let Some(c) = l.try_charge(name(p), &file(f), n) {
                            let total = l.state.lock().total;
                            prop_assert!(total <= cap || before == 0 || total == before,
                                "total {total} over cap {cap} with {before} held before");
                            live.push(c);
                        }
                    }
                    Op::Drop(i) if !live.is_empty() => {
                        let i = i % live.len();
                        drop(live.swap_remove(i));
                    }
                    Op::Drop(_) => {}
                }
            }
        }
    }

    /// A fill counts as soon as it is admitted, and its stream charge replaces
    /// it: one charge for the file, never two.
    #[test]
    fn a_pending_fill_counts_and_the_stream_charge_replaces_it() {
        let l = new_read_ledger(100, 25, 75);
        let fill = l.try_pending(name(0), 10).expect("admitted");
        assert_eq!(l.state.lock().total, 10);
        let stream = fill.into_stream(&file(0), 6);
        {
            let state = l.state.lock();
            assert_eq!(state.total, 6, "the pending 10 became the stream's 6");
            assert_eq!(state.by_principal[&name(0)].pending, 0);
        }
        // A second request of the same principal on the same file pays nothing.
        let again = l.try_charge(name(0), &file(0), 6).unwrap();
        assert_eq!(l.state.lock().total, 6);
        drop((stream, again));
        assert!(l.state.lock().by_principal.is_empty());
        // A fill that fails or is cancelled releases its charge.
        let fill = l.try_pending(name(1), 20).unwrap();
        assert_eq!(l.state.lock().total, 20);
        drop(fill);
        assert_eq!(l.state.lock().total, 0);
        assert!(l.state.lock().by_principal.is_empty());
    }

    #[test]
    fn a_principal_over_its_reach_is_refused_by_its_fills() {
        let l = new_read_ledger(100, 25, 75);
        // Reach alone: 56 of a read cap of 75 (the rest, 75 % of it).
        let a = l.try_pending(name(0), 30).unwrap();
        let b = l.try_pending(name(0), 26).unwrap();
        assert!(
            l.try_pending(name(0), 1).is_none(),
            "a flood of fills stops"
        );
        assert!(l.try_charge(name(0), &file(9), 1).is_none(), "streams too");
        // Another principal has its own share.
        assert!(l.try_pending(name(1), 18).is_some());
        drop((a, b));
    }

    /// The reference copy of a fill is a second charge of the same fill: the
    /// same rule admits it, so the reads in flight stay within the cap; only
    /// a fill that clause C admitted (one huge read on an idle ledger) may grow
    /// past it.
    #[test]
    fn a_fills_reference_copy_is_admitted_by_the_same_rule() {
        let l = new_read_ledger(100, 25, 75);
        // Reach alone is 56 of 75. A fill of 30 admitted by the rule may add
        // 26, no more: 30 + 27 = 57 is over it.
        let fill = l.try_pending(name(0), 30).unwrap();
        assert!(!fill.clause_c);
        assert!(fill.extend(27).is_none(), "over the reach");
        let copy = fill.extend(26).expect("up to the reach");
        assert_eq!(l.state.lock().total, 56);
        drop(copy);
        assert_eq!(l.state.lock().total, 30, "the copy ends with its file");
        drop(fill);
        // One huge fill on an idle ledger runs (clause C), with its reference.
        let huge = l.try_pending(name(0), 90).expect("clause C");
        assert!(huge.clause_c);
        let copy = huge.extend(40).expect("a clause C fill may grow");
        assert!(l.try_pending(name(1), 1).is_none(), "nobody joins it");
        drop((huge, copy));
        let state = l.state.lock();
        assert_eq!(state.total, 0);
        assert!(state.by_principal.is_empty());
    }

    #[derive(Debug, Clone)]
    enum FillOp {
        Fill(usize, usize),
        Extend(usize, usize),
        Finish(usize, usize),
        Cancel(usize),
        Charge(usize, usize, usize),
        Drop(usize),
    }

    fn fill_op() -> impl Strategy<Value = FillOp> {
        prop_oneof![
            (0usize..3, 1usize..30).prop_map(|(p, n)| FillOp::Fill(p, n)),
            (any::<usize>(), 1usize..30).prop_map(|(i, n)| FillOp::Extend(i, n)),
            (any::<usize>(), 0usize..6).prop_map(|(i, f)| FillOp::Finish(i, f)),
            any::<usize>().prop_map(FillOp::Cancel),
            (0usize..3, 0usize..6, 1usize..30).prop_map(|(p, f, n)| FillOp::Charge(p, f, n)),
            any::<usize>().prop_map(FillOp::Drop),
        ]
    }

    proptest! {
        /// With fills in the mix, the books stay equal to the live charges (a
        /// pending fill counts its whole reservation, a finished one the stream),
        /// reads in total stay within the cap except through clause C, and
        /// nothing is left when every charge is gone.
        #[test]
        fn fills_and_streams_keep_the_books_and_the_cap(
            budget in 20usize..200, share in 1u8..=100, max_percent in 10u8..=100,
            ops in proptest::collection::vec(fill_op(), 0..100),
        ) {
            let l = new_read_ledger(budget, share, max_percent);
            let mut fills: Vec<(usize, usize, FillCharge)> = Vec::new();
            // The reference copies of fills: (principal, mib, charge).
            let mut copies: Vec<(usize, usize, FillCharge)> = Vec::new();
            let mut streams: Vec<(usize, usize, usize, StreamCharge)> = Vec::new();
            let mut sizes: std::collections::HashMap<usize, usize> = Default::default();
            for op in ops {
                let before = l.state.lock().total;
                match op {
                    FillOp::Fill(p, n) => {
                        if let Some(c) = l.try_pending(name(p), n) { fills.push((p, n, c)); }
                    }
                    FillOp::Extend(i, n) if !fills.is_empty() => {
                        let i = i % fills.len();
                        if let Some(c) = fills[i].2.extend(n) {
                            copies.push((fills[i].0, n, c));
                        }
                    }
                    FillOp::Finish(i, f) if !fills.is_empty() => {
                        let (p, n, c) = fills.swap_remove(i % fills.len());
                        // A finished fill makes a file of its own, at most its size.
                        match (0..6).map(|k| (f + k) % 6).find(|g| !sizes.contains_key(g)) {
                            Some(g) => {
                                let mib = (n / 2).max(1);
                                sizes.insert(g, mib);
                                streams.push((p, g, mib, c.into_stream(&file(g), mib)));
                            }
                            None => drop(c),
                        }
                    }
                    FillOp::Cancel(i) if !fills.is_empty() => {
                        drop(fills.swap_remove(i % fills.len()));
                    }
                    FillOp::Charge(p, f, n) => {
                        let mib = *sizes.entry(f).or_insert(n);
                        if let Some(c) = l.try_charge(name(p), &file(f), mib) {
                            streams.push((p, f, mib, c));
                        }
                    }
                    FillOp::Drop(i) if !streams.is_empty() => {
                        let (p, f, _, c) = streams.swap_remove(i % streams.len());
                        drop(c);
                        let _ = p;
                        if !streams.iter().any(|(_, g, _, _)| *g == f) {
                            sizes.remove(&f);
                        }
                    }
                    _ => {}
                }
                let mut seen = std::collections::HashSet::new();
                let stream_total: usize = streams
                    .iter()
                    .filter(|(_, f, _, _)| seen.insert(*f))
                    .map(|(_, _, m, _)| m)
                    .sum();
                let pending: usize = fills.iter().map(|(_, n, _)| n).sum();
                let copied: usize = copies.iter().map(|(_, n, _)| n).sum();
                let total = l.state.lock().total;
                prop_assert_eq!(total, stream_total + pending + copied);
                let by_clause_c = fills.iter().any(|f| f.2.clause_c)
                    || copies.iter().any(|c| c.2.clause_c);
                prop_assert!(total <= l.cap_mib || before == 0 || total <= before || by_clause_c,
                    "total {total} over cap {} (was {before})", l.cap_mib);
                // A copy ends with its file, at any time.
                if let Some(k) = copies.len().checked_sub(1).filter(|k| k % 3 == 0) {
                    drop(copies.swap_remove(k));
                }
            }
            drop(fills);
            copies.clear();
            streams.clear();
            let state = l.state.lock();
            prop_assert_eq!(state.total, 0);
            prop_assert!(state.by_principal.is_empty());
        }
    }

    /// What principal `p` holds: each file once.
    fn sizes_held(live: &[(usize, usize, usize, StreamCharge)], p: usize) -> usize {
        let mut seen = std::collections::HashSet::new();
        live.iter()
            .filter(|(q, f, _, _)| *q == p && seen.insert(*f))
            .map(|(_, _, m, _)| m)
            .sum()
    }

    /// The disk space of the live files: each file once, whoever reads it.
    fn live_total(live: &[(usize, usize, usize, StreamCharge)]) -> usize {
        let mut seen = std::collections::HashSet::new();
        live.iter()
            .filter(|(_, f, _, _)| seen.insert(*f))
            .map(|(_, _, m, _)| m)
            .sum()
    }

    /// A read in no scope at all is a bug of its door: a hard error in debug
    /// builds. A background job enters its own scope and is not charged.
    #[tokio::test]
    #[should_panic(expected = "outside every spool scope")]
    async fn a_read_outside_every_scope_panics_in_debug_builds() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 100 * MIB).unwrap();
        let a = pool.acquire(MIB).await.unwrap();
        let _ = pool.charge_stream(&a);
    }

    #[tokio::test]
    async fn a_background_job_is_in_a_scope_and_is_not_charged() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 100 * MIB).unwrap();
        let a = pool.acquire(40 * MIB).await.unwrap();
        let charged = as_background(async { pool.charge_stream(&a) }).await;
        assert!(charged.unwrap().is_none());
        assert!(as_background(async { pool.charge_fill(40) })
            .await
            .unwrap()
            .is_none());
        assert_eq!(pool.read_held("anyone"), (0, 0));
        // The release-build answer of an unscoped read is a plain error.
        assert_eq!(unscoped_read_error().kind(), std::io::ErrorKind::Other);
    }

    /// A spawned task reads like its parent when it enters a captured context.
    #[tokio::test]
    async fn a_captured_context_carries_the_payer_and_the_backend_into_a_task() {
        let ctx = as_principal("dana", scoped("hetzner-fsn1", async { capture_context() })).await;
        let seen = tokio::spawn(in_context(ctx, async {
            (current_payer(), current_scope())
        }))
        .await
        .unwrap();
        assert_eq!(seen.0, Some(Payer::Principal("dana".into())));
        assert_eq!(seen.1.as_deref(), Some("hetzner-fsn1"));
        // No scope captured, none entered.
        let none = tokio::spawn(in_context(SpoolContext::default(), async {
            current_payer()
        }))
        .await
        .unwrap();
        assert_eq!(none, None);
    }

    #[tokio::test]
    async fn charge_stream_needs_a_principal_and_a_share_below_100() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = SpoolDir::new(tmp.path().to_path_buf(), 100 * MIB)
            .unwrap()
            .with_read_share(25);
        let a = pool.acquire(40 * MIB).await.unwrap();
        let b = pool.acquire(40 * MIB).await.unwrap();
        // A background job: no charge.
        assert!(as_background(async { pool.charge_stream(&a) })
            .await
            .unwrap()
            .is_none());
        assert_eq!(pool.read_held("dana"), (0, 0));
        // With a principal: charged for the life of the charge.
        let held = as_principal("dana", async { pool.charge_stream(&a) })
            .await
            .unwrap()
            .expect("charged");
        assert_eq!(pool.read_held("dana"), (40, 40));
        // 40 + 40 is above the 75 the principal reaches alone.
        let err = as_principal("dana", async { pool.charge_stream(&b) })
            .await
            .err()
            .expect("refused");
        assert_eq!(err.kind(), CONTENDED);
        drop(held);
        assert_eq!(pool.read_held("dana"), (0, 0));
        // A refused charge leaves nothing behind.
        assert_eq!(pool.used_bytes(), 80 * MIB);
        // A share of 100 and a read cap of 100 turn the charge off.
        let open = SpoolDir::new(tmp.path().to_path_buf(), 100 * MIB)
            .unwrap()
            .with_read_share(100)
            .with_read_max(100);
        let c = open.acquire(40 * MIB).await.unwrap();
        assert!(as_principal("dana", async { open.charge_stream(&c) })
            .await
            .unwrap()
            .is_none());
    }
}

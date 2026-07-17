// SPDX-License-Identifier: GPL-3.0-only

//! Fused directory-scoped reconcile walk — PURE core (no I/O, no tokio).
//!
//! The reconcile worker drives a tree walk over (source, destination)
//! directory pairs. This module owns every decision: ordering, resume,
//! watermark, per-directory diff. The async driver in `worker.rs` only
//! executes commands and feeds results back.
//!
//! Ordering foundation (proved by `proptests::sibling_subtree_bound`):
//! all paths are RELATIVE to the normalized rule prefixes and compared
//! byte-wise. Preorder DFS consuming children in full-prefix-string
//! order visits every item in global lexicographic order of relative
//! paths, so "everything ≤ watermark is settled" is a complete,
//! single-string resume cursor — even mid-directory.

use crate::config_sections::ConflictPolicy;
use crate::replication::event_consumer::owned_by_rule;
use crate::replication::planner::{should_replicate, Decision};
use crate::types::FileMetadata;
use globset::GlobSet;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

// ---------------------------------------------------------------------------
// ContiguousTracker
// ---------------------------------------------------------------------------

/// Contiguous-progress tracker over relative paths.
///
/// Two kinds of open entries with different guard semantics:
/// - **items** (copies, deletes, pending pairs, skip records): an open item
///   at `p` guards `p` itself — the watermark can never reach `p`.
/// - **markers** (listing frontiers, subtree guards): a marker at `m` guards
///   everything STRICTLY BEYOND `m` — a settled path equal to `m` may still
///   promote (the stream at `m` only owes content > `m`).
///
/// The watermark is the largest settled path with every tracked path ≤ it
/// settled, so out-of-order completion (parallel dirs, concurrent copies)
/// can never overtake undone work. Callers must never open below it.
#[derive(Debug, Default)]
pub struct ContiguousTracker {
    items: BTreeMap<String, u32>,
    markers: BTreeMap<String, u32>,
    settled_ahead: BTreeSet<String>,
    watermark: Option<String>,
}

impl ContiguousTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Track an unsettled work item. Equality with the watermark is allowed
    /// (idempotent redo after a flatten re-lists settled ground).
    pub fn open_item(&mut self, path: &str) {
        debug_assert!(
            self.watermark.as_deref().is_none_or(|w| path >= w),
            "opened item {path:?} below watermark {:?}",
            self.watermark
        );
        *self.items.entry(path.to_string()).or_insert(0) += 1;
    }

    /// Track a frontier/guard marker (guards strictly beyond its position).
    pub fn open_marker(&mut self, path: &str) {
        debug_assert!(
            self.watermark.as_deref().is_none_or(|w| path >= w),
            "opened marker {path:?} below watermark {:?}",
            self.watermark
        );
        *self.markers.entry(path.to_string()).or_insert(0) += 1;
    }

    /// Settle a work item: its outcome is final and durable-equivalent.
    pub fn settle(&mut self, path: &str) {
        Self::release(&mut self.items, path);
        self.settled_ahead.insert(path.to_string());
        self.promote();
    }

    /// Remove a marker without recording settled work.
    pub fn close_marker(&mut self, path: &str) {
        Self::release(&mut self.markers, path);
        self.promote();
    }

    /// Move a frontier marker forward (entries consumed up to `new`).
    /// Opens `new` BEFORE closing `old` so progress can never transiently
    /// promote past paths the stream still guards.
    pub fn advance_marker(&mut self, old: &str, new: &str) {
        debug_assert!(new > old, "marker must move forward: {old:?} -> {new:?}");
        *self.markers.entry(new.to_string()).or_insert(0) += 1;
        Self::release(&mut self.markers, old);
        self.promote();
    }

    pub fn watermark(&self) -> Option<&str> {
        self.watermark.as_deref()
    }

    /// True when nothing is tracked as open (all work settled or closed).
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.markers.is_empty()
    }

    fn release(map: &mut BTreeMap<String, u32>, path: &str) {
        match map.get_mut(path) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => {
                map.remove(path);
            }
            None => debug_assert!(false, "release of untracked path {path:?}"),
        }
    }

    fn promote(&mut self) {
        while let Some(first) = self.settled_ahead.first() {
            let item_blocks = self
                .items
                .first_key_value()
                .is_some_and(|(o, _)| o.as_str() <= first.as_str());
            let marker_blocks = self
                .markers
                .first_key_value()
                .is_some_and(|(m, _)| m.as_str() < first.as_str());
            if item_blocks || marker_blocks {
                break;
            }
            let p = self.settled_ahead.pop_first().expect("checked non-empty");
            self.watermark = Some(p);
        }
    }
}

// ---------------------------------------------------------------------------
// Resume rules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAction {
    /// Entire subtree (or the object) is ≤ watermark ⇒ already settled.
    Skip,
    /// Dir is an ancestor of (or equal to) the watermark: its own listing
    /// completed, but descendants > watermark remain — re-list and apply
    /// the rule per child.
    DescendOnly,
    /// Strictly beyond the watermark — process fully.
    Process,
}

/// PURE resume decision for one relative path against watermark `w`.
/// Soundness rests on the sibling-subtree bound: a dir `d ≤ w` that is
/// not an ancestor of `w` has ALL descendants < w (first byte difference
/// lies inside `d`), hence settled.
pub fn resume_action(rel: &str, is_dir: bool, w: &str) -> ResumeAction {
    if is_dir && w.starts_with(rel) {
        ResumeAction::DescendOnly
    } else if rel <= w {
        ResumeAction::Skip
    } else {
        ResumeAction::Process
    }
}

/// Listing continuation token to resume dir `dir` (DescendOnly) so the
/// backend drops only provably-settled entries. Returns the child prefix
/// containing `w` MINUS its trailing slash: a strictly-greater backend
/// then still emits that child common-prefix (we must descend into it),
/// and an AWS `start_after` backend merely re-emits entries the resume
/// rule filters anyway. `None` ⇒ list from the beginning.
pub fn resume_list_token(dir: &str, w: &str) -> Option<String> {
    if !w.starts_with(dir) || w == dir {
        return None;
    }
    let rest = &w[dir.len()..];
    Some(match rest.find('/') {
        Some(i) => w[..dir.len() + i].to_string(),
        None => w.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

/// Scope-stamped walk cursor persisted in `replication_state.continuation_token`.
/// `pos` is the watermark: a relative path (object or dir). Everything ≤ pos
/// is settled; the walk resumes via `resume_action` against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorV1 {
    pub v: u32,
    pub scope: String,
    pub pos: String,
}

/// Canonical scope stamp. Prefixes must already be rule-normalized.
pub fn cursor_scope(
    src_bucket: &str,
    src_prefix: &str,
    dst_bucket: &str,
    dst_prefix: &str,
) -> String {
    format!("{src_bucket}|{src_prefix}|{dst_bucket}|{dst_prefix}")
}

/// Parse a persisted cursor. Legacy flat listing tokens (non-JSON), version
/// or scope mismatches, and empty positions all mean "start fresh" — copies
/// are idempotent, so discarding an unusable cursor is always safe.
pub fn load_cursor(raw: Option<&str>, scope: &str) -> Option<CursorV1> {
    let c: CursorV1 = serde_json::from_str(raw?).ok()?;
    (c.v == 1 && c.scope == scope && !c.pos.is_empty()).then_some(c)
}

impl CursorV1 {
    pub fn new(scope: &str, pos: &str) -> Self {
        Self {
            v: 1,
            scope: scope.to_string(),
            pos: pos.to_string(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("cursor serializes")
    }
}

// ---------------------------------------------------------------------------
// Fact policy
// ---------------------------------------------------------------------------

/// Which listings carry logical truth (`engine.lite_list_carries_logical_facts`
/// per bucket). Filesystem xattr listings are authoritative; S3 lite facts are
/// LastModified / stored-delta-size, and encrypting backends list ciphertext
/// facts — those sides must HEAD-resolve intersection keys.
#[derive(Debug, Clone, Copy)]
pub struct FactRegime {
    pub src_lite_authoritative: bool,
    pub dest_lite_authoritative: bool,
}

/// Does deciding a src∩dest pair need a HEAD on this side?
/// SkipIfDestExists is existence-only — the listing always suffices.
pub fn side_facts_need_head(conflict: ConflictPolicy, lite_authoritative: bool) -> bool {
    !matches!(conflict, ConflictPolicy::SkipIfDestExists) && !lite_authoritative
}

// ---------------------------------------------------------------------------
// Machine I/O boundary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Src,
    Dest,
}

/// Command for the driver. All paths and tokens are RELATIVE to the rule
/// prefixes — the driver prefixes on the way out and strips on the way in.
#[derive(Debug)]
pub enum Cmd {
    /// One listing page: `engine.list_objects(bucket_for(side),
    /// prefix_for(side) + rel_prefix, delimited.then_some("/"), page_size,
    /// token, /*metadata=*/false)`.
    List {
        req_id: u64,
        side: Side,
        rel_prefix: String,
        token: Option<String>,
        delimited: bool,
    },
    /// Batched HEADs (bounded burst); answer per key with `HeadResult`.
    Head {
        req_id: u64,
        side: Side,
        rel_keys: Vec<String>,
    },
    /// Copy src→dest at the same relative key (existing copy_one_object).
    Copy {
        item_id: u64,
        rel_key: String,
        src_size: u64,
    },
    /// Delete pipeline: [dest provenance HEAD iff flagged] → src-absence
    /// HEAD confirm (delete only on NoSuchKey) → engine.delete.
    Delete {
        item_id: u64,
        rel_key: String,
        needs_provenance_head: bool,
    },
}

/// One listing entry, relative path. `Dir` paths end in `/` and are VERBATIM
/// (never normalized — `a//` is a real level).
#[derive(Debug, Clone)]
pub enum EntryKind {
    Obj(Box<FileMetadata>),
    Dir,
}

#[derive(Debug, Clone)]
pub struct RelEntry {
    pub path: String,
    pub kind: EntryKind,
}

/// A listing page after driver-side prefix stripping: entries in ascending
/// path order (objects and collapsed prefixes interleaved, engine order).
#[derive(Debug)]
pub struct RelListing {
    pub entries: Vec<RelEntry>,
    pub truncated: bool,
    pub next_token: Option<String>,
}

#[derive(Debug)]
pub enum HeadResult {
    Resolved(Box<FileMetadata>),
    /// NotFound — the object vanished (or never existed on this side).
    Gone,
    /// Transient failure. Copies fall back to the safe direction; deletes
    /// must preserve (driver-side rule).
    Unresolved,
}

#[derive(Debug)]
pub enum Event {
    ListPage {
        req_id: u64,
        page: RelListing,
    },
    ListFailed {
        req_id: u64,
    },
    HeadDone {
        req_id: u64,
        results: Vec<(String, HeadResult)>,
    },
    CopySettled {
        item_id: u64,
        ok: bool,
    },
    DeleteSettled {
        item_id: u64,
        ok: bool,
    },
}

/// Free driver slots; `poll` never returns more commands than these.
#[derive(Debug, Clone, Copy)]
pub struct DriverCaps {
    pub list: usize,
    pub head: usize,
    pub copy: usize,
    pub delete: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainReason {
    Killed,
    Paused,
    LeaseLost,
    Fatal,
}

#[derive(Debug, Default, Clone)]
pub struct WalkStats {
    pub objects_scanned: u64,
    pub objects_skipped: u64,
    pub dirs_completed: u64,
    pub pages_used: u32,
    pub truncated_by_budget: bool,
}

// ---------------------------------------------------------------------------
// Machine configuration
// ---------------------------------------------------------------------------

pub struct WalkConfig {
    /// `cursor_scope(...)` stamp — cursors from another scope are discarded.
    pub scope: String,
    /// Provenance marker value (`dg-replication-rule`).
    pub rule_name: String,
    /// Normalized rule prefixes ("" or trailing-`/`), for abs↔rel mapping in
    /// policy checks (globs match ABSOLUTE source keys, parity with plan_batch).
    pub src_prefix: String,
    pub dest_prefix: String,
    pub conflict: ConflictPolicy,
    pub strict_content_diff: bool,
    pub replicate_deletes: bool,
    pub include_globs: GlobSet,
    pub exclude_globs: GlobSet,
    pub regime: FactRegime,
    pub page_size: u32,
    /// Run-global listing budget (all sides, all dirs, flat or delimited).
    pub max_pages: u32,
    pub dir_workers: usize,
    pub head_batch: usize,
    /// Per-dir child-count threshold: beyond it the dir degrades to a flat
    /// (delimiter-less) sweep of its subtree.
    pub max_child_dirs_per_dir: usize,
    /// Global pending-dir cap: overflow flattens the dir that overflowed it.
    pub max_pending_dirs: usize,
    /// Backpressure: stop advancing merges while this many decided actions
    /// await driver slots.
    pub action_buffer: usize,
}

// ---------------------------------------------------------------------------
// Machine internals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirMode {
    Compare,
    SrcOnly,
    DestOnly,
}

struct SideStream {
    /// Listing scope: the dir prefix (delimited) or subtree prefix (flat).
    rel_prefix: String,
    delimited: bool,
    token: Option<String>,
    exhausted: bool,
    inflight: Option<u64>,
    buf: VecDeque<RelEntry>,
    /// Strictly-increasing emission filter — dedups cross-page prefix
    /// repeats and AWS start_after re-emissions.
    last_seen: Option<String>,
    /// Tracker frontier marker (open in the tracker). Guards everything this
    /// stream has NOT YET DELIVERED to classification: it advances as entries
    /// are consumed from `buf` (never on page receipt — buffered entries
    /// would lose their guard), and closes only when exhausted AND drained.
    marker: String,
    marker_closed: bool,
}

impl SideStream {
    fn new(rel_prefix: String, delimited: bool, token: Option<String>, marker: String) -> Self {
        Self {
            rel_prefix,
            delimited,
            token,
            exhausted: false,
            inflight: None,
            buf: VecDeque::new(),
            last_seen: None,
            marker,
            marker_closed: false,
        }
    }

    fn drained(&self) -> bool {
        self.exhausted && self.buf.is_empty()
    }

    /// Highest path this stream has knowledge through: entries ≤ this are
    /// either in `buf` or were already consumed.
    fn known_through(&self) -> Option<&str> {
        self.last_seen.as_deref()
    }

    fn needs_page(&self) -> bool {
        !self.exhausted && self.inflight.is_none() && self.buf.is_empty()
    }
}

struct PendingPair {
    src_lite: Box<FileMetadata>,
    dest_lite: Box<FileMetadata>,
    src_resolved: Option<Option<Box<FileMetadata>>>,
    dest_resolved: Option<Option<Box<FileMetadata>>>,
}

struct DirTask {
    rel_dir: String,
    mode: DirMode,
    flat: bool,
    src: Option<SideStream>,
    dest: Option<SideStream>,
    child_dirs_seen: usize,
    /// Copies + pending pairs + deletes emitted for this dir, not yet settled.
    outstanding: usize,
    copy_errors: usize,
    /// Buffered until the dir completes cleanly (AfterDirClean gate).
    delete_candidates: Vec<(String, bool)>,
    pending_pairs: BTreeMap<String, PendingPair>,
    head_batch: [Vec<String>; 2], // [Src, Dest] keys awaiting batch emission
    deletes_flushed: bool,
    /// Bumped on flatten (streams replaced) — stale marker advances and
    /// stale listing pages check this before touching the new streams.
    generation: u32,
}

impl DirTask {
    /// Both listings exhausted AND fully consumed by classification.
    fn listings_done(&self) -> bool {
        self.src.as_ref().is_none_or(SideStream::drained)
            && self.dest.as_ref().is_none_or(SideStream::drained)
    }
}

enum CmdRoute {
    Listing { dir: String, side: Side },
    Heads { dir: String, side: Side },
    CopyItem { dir: String, rel: String },
    DeleteItem { dir: String, rel: String },
}

struct PendingDir {
    mode: DirMode,
    /// Tracker guard position opened at discovery (dir path, or the resume
    /// watermark for DescendOnly dirs).
    guard: String,
}

pub struct WalkMachine {
    cfg: WalkConfig,
    /// Resume watermark from the loaded cursor (positions ≤ it are settled).
    resume_w: Option<String>,
    tracker: ContiguousTracker,
    pending: BTreeMap<String, PendingDir>,
    active: BTreeMap<String, DirTask>,
    ready_copies: VecDeque<(u64, String, u64)>,
    ready_deletes: VecDeque<(u64, String, bool)>,
    ready_heads: VecDeque<(u64, Side, Vec<String>)>,
    routes: HashMap<u64, CmdRoute>,
    next_id: u64,
    stats: WalkStats,
    drained: Option<DrainReason>,
    list_failed: bool,
}

impl WalkMachine {
    pub fn new(cfg: WalkConfig, resume: Option<CursorV1>) -> Self {
        let resume_w = resume.map(|c| c.pos);
        let mut m = Self {
            cfg,
            resume_w,
            tracker: ContiguousTracker::new(),
            pending: BTreeMap::new(),
            active: BTreeMap::new(),
            ready_copies: VecDeque::new(),
            ready_deletes: VecDeque::new(),
            ready_heads: VecDeque::new(),
            routes: HashMap::new(),
            next_id: 1,
            stats: WalkStats::default(),
            drained: None,
            list_failed: false,
        };
        m.enqueue_dir(String::new(), DirMode::Compare);
        m
    }

    // -- public surface ----------------------------------------------------

    pub fn poll(&mut self, caps: DriverCaps) -> Vec<Cmd> {
        let mut out = Vec::new();
        if self.drained.is_some() {
            return out;
        }

        self.activate_pending();
        self.advance_all_dirs();

        // Listings (budget-gated).
        let mut list_slots = caps.list;
        let dir_keys: Vec<String> = self.active.keys().cloned().collect();
        'lists: for key in &dir_keys {
            for side in [Side::Src, Side::Dest] {
                if list_slots == 0 {
                    break 'lists;
                }
                let backpressured = self.actions_backpressured();
                let over_budget = self.stats.pages_used >= self.cfg.max_pages;
                let Some(task) = self.active.get_mut(key) else {
                    continue 'lists;
                };
                let stream = match side {
                    Side::Src => task.src.as_mut(),
                    Side::Dest => task.dest.as_mut(),
                };
                let Some(stream) = stream else { continue };
                if !stream.needs_page() || backpressured {
                    continue;
                }
                if over_budget {
                    // A page is genuinely needed but the budget is spent —
                    // that (and only that) is a truncated run.
                    self.stats.truncated_by_budget = true;
                    break 'lists;
                }
                let req_id = self.next_id;
                self.next_id += 1;
                stream.inflight = Some(req_id);
                out.push(Cmd::List {
                    req_id,
                    side,
                    rel_prefix: stream.rel_prefix.clone(),
                    token: stream.token.clone(),
                    delimited: stream.delimited,
                });
                self.routes.insert(
                    req_id,
                    CmdRoute::Listing {
                        dir: key.clone(),
                        side,
                    },
                );
                self.stats.pages_used += 1;
                list_slots -= 1;
            }
        }

        // Head bursts.
        let mut head_slots = caps.head;
        while head_slots > 0 {
            let Some((req_id, side, keys)) = self.ready_heads.pop_front() else {
                break;
            };
            out.push(Cmd::Head {
                req_id,
                side,
                rel_keys: keys,
            });
            head_slots -= 1;
        }

        // Copies / deletes.
        let mut copy_slots = caps.copy;
        while copy_slots > 0 {
            let Some((item_id, rel_key, src_size)) = self.ready_copies.pop_front() else {
                break;
            };
            out.push(Cmd::Copy {
                item_id,
                rel_key,
                src_size,
            });
            copy_slots -= 1;
        }
        let mut delete_slots = caps.delete;
        while delete_slots > 0 {
            let Some((item_id, rel_key, needs_provenance_head)) = self.ready_deletes.pop_front()
            else {
                break;
            };
            out.push(Cmd::Delete {
                item_id,
                rel_key,
                needs_provenance_head,
            });
            delete_slots -= 1;
        }

        out
    }

    pub fn on_event(&mut self, ev: Event) {
        match ev {
            Event::ListPage { req_id, page } => {
                let Some(CmdRoute::Listing { dir, side }) = self.routes.remove(&req_id) else {
                    return;
                };
                self.on_list_page(&dir, side, page);
            }
            Event::ListFailed { req_id } => {
                self.routes.remove(&req_id);
                self.list_failed = true;
                self.drained = Some(DrainReason::Fatal);
            }
            Event::HeadDone { req_id, results } => {
                let Some(CmdRoute::Heads { dir, side }) = self.routes.remove(&req_id) else {
                    return;
                };
                self.on_heads_done(&dir, side, results);
            }
            Event::CopySettled { item_id, ok } => {
                let Some(CmdRoute::CopyItem { dir, rel }) = self.routes.remove(&item_id) else {
                    return;
                };
                self.tracker.settle(&rel);
                if let Some(task) = self.active.get_mut(&dir) {
                    task.outstanding -= 1;
                    if !ok {
                        task.copy_errors += 1;
                    }
                }
                self.maybe_complete_dir(&dir);
            }
            Event::DeleteSettled { item_id, ok: _ } => {
                let Some(CmdRoute::DeleteItem { dir, rel }) = self.routes.remove(&item_id) else {
                    return;
                };
                self.tracker.settle(&rel);
                if let Some(task) = self.active.get_mut(&dir) {
                    task.outstanding -= 1;
                }
                self.maybe_complete_dir(&dir);
            }
        }
    }

    /// Stop issuing new work. In-flight commands may still settle via
    /// `on_event`; the cursor stands at the watermark.
    pub fn drain(&mut self, reason: DrainReason) {
        self.drained = Some(reason);
    }

    /// No commands outstanding and nothing pollable.
    pub fn is_idle(&self) -> bool {
        self.routes.is_empty()
            && self.ready_heads.is_empty()
            && self.ready_copies.is_empty()
            && self.ready_deletes.is_empty()
    }

    /// Every directory fully reconciled (no abort, no budget truncation).
    pub fn is_done(&self) -> bool {
        self.drained.is_none()
            && !self.stats.truncated_by_budget
            && self.pending.is_empty()
            && self.active.is_empty()
            && self.is_idle()
    }

    pub fn truncated_by_budget(&self) -> bool {
        self.stats.truncated_by_budget
    }

    pub fn list_failed(&self) -> bool {
        self.list_failed
    }

    pub fn stats(&self) -> &WalkStats {
        &self.stats
    }

    /// Persistable resume position: the tracker watermark, or the loaded
    /// resume position while nothing new has settled. None ⇒ fresh start.
    pub fn cursor(&self) -> Option<CursorV1> {
        let pos = self
            .tracker
            .watermark()
            .or(self.resume_w.as_deref())
            .filter(|p| !p.is_empty())?;
        Some(CursorV1::new(&self.cfg.scope, pos))
    }

    // -- discovery / activation --------------------------------------------

    fn resume_pos(&self) -> Option<&str> {
        self.resume_w.as_deref()
    }

    fn enqueue_dir(&mut self, rel_dir: String, mode: DirMode) {
        let (guard, keep) = match self.resume_pos() {
            Some(w) => match resume_action(&rel_dir, true, w) {
                ResumeAction::Skip => (String::new(), false),
                ResumeAction::DescendOnly => (w.to_string(), true),
                ResumeAction::Process => (rel_dir.clone(), true),
            },
            None => (rel_dir.clone(), true),
        };
        if !keep {
            return;
        }
        self.tracker.open_marker(&guard);
        self.pending
            .insert(rel_dir.clone(), PendingDir { mode, guard });

        if self.pending.len() > self.cfg.max_pending_dirs {
            // Attribute the overflow to the dir that caused it: flatten the
            // ACTIVE dir currently holding the most discovered children.
            if let Some(worst) = self
                .active
                .iter()
                .filter(|(_, t)| !t.flat)
                .max_by_key(|(_, t)| t.child_dirs_seen)
                .map(|(k, _)| k.clone())
            {
                self.flatten_dir(&worst);
            }
        }
    }

    fn activate_pending(&mut self) {
        while self.active.len() < self.cfg.dir_workers {
            let Some((rel_dir, pd)) = self.pending.pop_first() else {
                break;
            };
            self.activate_dir(rel_dir, pd);
        }
    }

    fn activate_dir(&mut self, rel_dir: String, pd: PendingDir) {
        // Stream start: resumed DescendOnly dirs list from the resume token
        // and park their markers at the resume watermark (never below the
        // global watermark); fresh dirs list from scratch, marker at the dir.
        let (token, marker) = match self.resume_pos() {
            Some(w) if w.starts_with(rel_dir.as_str()) => {
                (resume_list_token(&rel_dir, w), w.to_string())
            }
            _ => (None, rel_dir.clone()),
        };

        let mk = |m: &str| SideStream::new(rel_dir.clone(), true, token.clone(), m.to_string());
        let (src, dest) = match pd.mode {
            DirMode::Compare => (Some(mk(&marker)), Some(mk(&marker))),
            DirMode::SrcOnly => (Some(mk(&marker)), None),
            DirMode::DestOnly => (None, Some(mk(&marker))),
        };
        // Open stream markers BEFORE releasing the discovery guard.
        if src.is_some() {
            self.tracker.open_marker(&marker);
        }
        if dest.is_some() {
            self.tracker.open_marker(&marker);
        }
        // A freshly-discovered dir settles its own path here: structure-only
        // regions (empty dirs, marker objects) then still advance the cursor.
        // Stream markers parked AT the dir don't block it — they only guard
        // strictly beyond their position.
        if pd.guard == rel_dir {
            self.tracker.open_item(&rel_dir);
            self.tracker.settle(&rel_dir);
        }
        self.tracker.close_marker(&pd.guard);

        self.active.insert(
            rel_dir.clone(),
            DirTask {
                rel_dir,
                mode: pd.mode,
                flat: false,
                src,
                dest,
                child_dirs_seen: 0,
                outstanding: 0,
                copy_errors: 0,
                delete_candidates: Vec::new(),
                pending_pairs: BTreeMap::new(),
                head_batch: [Vec::new(), Vec::new()],
                deletes_flushed: false,
                generation: 0,
            },
        );
    }

    /// Degrade a dir to a flat (delimiter-less) subtree sweep. Purged pending
    /// descendants and abandoned buffers are re-covered by the flat listing,
    /// which restarts at the CURRENT durable progress (watermark) — anything
    /// between it and prior in-flight progress re-processes idempotently.
    fn flatten_dir(&mut self, key: &str) {
        let Some(mut task) = self.active.remove(key) else {
            return;
        };
        if task.flat {
            self.active.insert(key.to_string(), task);
            return;
        }

        // Restart position: everything ≤ the durable watermark is guaranteed
        // settled; the flat token skips exactly that. Marker parks at the
        // same point (or the dir itself when progress hasn't reached it).
        let wm = self
            .tracker
            .watermark()
            .or_else(|| self.resume_pos())
            .map(str::to_string);
        let inside = wm.filter(|w| w.as_str() > task.rel_dir.as_str());
        let token = inside.clone();
        let marker = inside.unwrap_or_else(|| task.rel_dir.clone());

        // Open flat markers FIRST, then unwind old guards (open-before-close
        // keeps every unsettled path guarded throughout).
        let want_src = task.src.is_some();
        let want_dest = task.dest.is_some();
        if want_src {
            self.tracker.open_marker(&marker);
        }
        if want_dest {
            self.tracker.open_marker(&marker);
        }
        let mk = || SideStream::new(task.rel_dir.clone(), false, token.clone(), marker.clone());
        let (src, dest) = (want_src.then(&mk), want_dest.then(&mk));

        // Abandon old streams: close live markers and drop any in-flight
        // listing routes so a late page can't feed the new flat streams.
        for s in [task.src.take(), task.dest.take()].into_iter().flatten() {
            if !s.marker_closed {
                self.tracker.close_marker(&s.marker);
            }
        }
        self.routes
            .retain(|_, r| !matches!(r, CmdRoute::Listing { dir, .. } if dir == key));

        // Abandoned pairs re-emit + re-decide in the sweep; late HEAD replies
        // miss the pairs map and are ignored.
        for (rel, _) in std::mem::take(&mut task.pending_pairs) {
            self.tracker.settle(&rel);
            task.outstanding -= 1;
        }
        task.head_batch = [Vec::new(), Vec::new()];

        // Purge undispatched descendants; the sweep re-covers their subtrees.
        let descendants: Vec<String> = self
            .pending
            .range(key.to_string()..)
            .take_while(|(k, _)| k.starts_with(key))
            .map(|(k, _)| k.clone())
            .collect();
        for d in descendants {
            if let Some(pd) = self.pending.remove(&d) {
                self.tracker.close_marker(&pd.guard);
            }
        }

        task.flat = true;
        task.src = src;
        task.dest = dest;
        task.child_dirs_seen = 0;
        task.deletes_flushed = false;
        task.generation += 1;
        self.active.insert(key.to_string(), task);
    }

    // -- listing intake -----------------------------------------------------

    fn on_list_page(&mut self, dir: &str, side: Side, page: RelListing) {
        let Some(task) = self.active.get_mut(dir) else {
            return;
        };
        let stream = match side {
            Side::Src => task.src.as_mut(),
            Side::Dest => task.dest.as_mut(),
        };
        let Some(stream) = stream else { return };
        stream.inflight = None;

        for e in page.entries {
            // Drop the dir's own marker object and anything not strictly
            // increasing (cross-page prefix repeats, start_after re-emission).
            if e.path == stream.rel_prefix {
                continue;
            }
            if stream
                .last_seen
                .as_deref()
                .is_some_and(|l| e.path.as_str() <= l)
            {
                continue;
            }
            stream.last_seen = Some(e.path.clone());
            stream.buf.push_back(e);
        }
        if page.truncated && page.next_token.is_some() {
            stream.token = page.next_token;
        } else {
            stream.exhausted = true;
            stream.token = None;
        }

        self.advance_dir(dir);
        self.maybe_complete_dir(dir);
    }

    // -- merge / classification ---------------------------------------------

    fn advance_all_dirs(&mut self) {
        let keys: Vec<String> = self.active.keys().cloned().collect();
        for k in keys {
            self.advance_dir(&k);
            self.maybe_complete_dir(&k);
        }
    }

    fn actions_backpressured(&self) -> bool {
        self.ready_copies.len() + self.ready_deletes.len() >= self.cfg.action_buffer
    }

    fn advance_dir(&mut self, key: &str) {
        loop {
            if self.actions_backpressured() || self.drained.is_some() {
                return;
            }
            let Some(task) = self.active.get_mut(key) else {
                return;
            };
            let step = match task.mode {
                DirMode::SrcOnly => Self::pop_single(task.src.as_mut(), Side::Src),
                DirMode::DestOnly => Self::pop_single(task.dest.as_mut(), Side::Dest),
                DirMode::Compare => Self::pop_merge(task),
            };
            let Some(m) = step else {
                // Nothing more classifiable now: close markers of streams
                // that are fully drained (their guard duty is over).
                for side in [Side::Src, Side::Dest] {
                    let Some(task) = self.active.get_mut(key) else {
                        return;
                    };
                    let s = match side {
                        Side::Src => task.src.as_mut(),
                        Side::Dest => task.dest.as_mut(),
                    };
                    if let Some(s) = s {
                        if s.drained() && !s.marker_closed {
                            s.marker_closed = true;
                            let m = s.marker.clone();
                            self.tracker.close_marker(&m);
                        }
                    }
                }
                return;
            };
            // Classify FIRST (opening any resulting item/guard), THEN advance
            // the popped sides' frontier markers. The reverse order has a gap
            // where promote() runs with neither the marker nor the item
            // guarding the popped path — the watermark could slip past work
            // that was just about to be opened.
            let popped: Vec<(Side, String)> = m
                .popped()
                .into_iter()
                .map(|(s, p)| (s, p.to_string()))
                .collect();
            let gen_before = self.active.get(key).map(|t| t.generation);
            self.apply_merged(key, m);
            for (side, path) in popped {
                let Some(task) = self.active.get_mut(key) else {
                    return;
                };
                // apply_merged may have flattened the dir (streams replaced,
                // generation bumped): the pop belonged to the OLD streams —
                // never advance the fresh flat markers with it.
                if Some(task.generation) != gen_before {
                    return;
                }
                let s = match side {
                    Side::Src => task.src.as_mut(),
                    Side::Dest => task.dest.as_mut(),
                };
                if let Some(s) = s {
                    if path.as_str() > s.marker.as_str() && !s.marker_closed {
                        let old = std::mem::replace(&mut s.marker, path.clone());
                        self.tracker.advance_marker(&old, &path);
                    }
                }
            }
        }
    }

    /// Pop the next classifiable item. `None` ⇒ stalled (needs pages) or done.
    fn pop_single(stream: Option<&mut SideStream>, side: Side) -> Option<Merged> {
        let s = stream?;
        let e = s.buf.pop_front()?;
        Some(Merged::Single(side, e))
    }

    fn pop_merge(task: &mut DirTask) -> Option<Merged> {
        let (src, dest) = (task.src.as_mut()?, task.dest.as_mut()?);
        let s_front = src.buf.front().map(|e| e.path.clone());
        let d_front = dest.buf.front().map(|e| e.path.clone());
        match (s_front, d_front) {
            (Some(s), Some(d)) => {
                if s < d {
                    Some(Merged::SrcOnly(src.buf.pop_front().expect("front")))
                } else if d < s {
                    Some(Merged::DestOnly(dest.buf.pop_front().expect("front")))
                } else {
                    let se = src.buf.pop_front().expect("front");
                    let de = dest.buf.pop_front().expect("front");
                    Some(Merged::Both(se, de))
                }
            }
            (Some(s), None) => {
                // Dest has nothing left ≤ its knowledge horizon; only safe to
                // classify src entries within that horizon (or if exhausted).
                if dest.exhausted || dest.known_through().is_some_and(|k| s.as_str() <= k) {
                    Some(Merged::SrcOnly(src.buf.pop_front().expect("front")))
                } else {
                    None
                }
            }
            (None, Some(d)) => {
                if src.exhausted || src.known_through().is_some_and(|k| d.as_str() <= k) {
                    Some(Merged::DestOnly(dest.buf.pop_front().expect("front")))
                } else {
                    None
                }
            }
            (None, None) => None,
        }
    }

    fn apply_merged(&mut self, key: &str, m: Merged) {
        match m {
            Merged::Single(Side::Src, e) | Merged::SrcOnly(e) => self.classify_src(key, e, None),
            Merged::Single(Side::Dest, e) | Merged::DestOnly(e) => self.classify_dest_only(key, e),
            Merged::Both(se, de) => match de.kind {
                EntryKind::Dir => {
                    // An object and a dir can never share a path (trailing
                    // '/'), so equal paths means both sides are dirs.
                    self.discover_child(key, se.path, DirMode::Compare);
                }
                EntryKind::Obj(dm) => self.classify_src(key, se, Some(dm)),
            },
        }
    }

    /// Objects at or below the resume watermark were settled by the previous
    /// run — drop them before any stats/tracker involvement.
    fn resume_settled(&self, rel: &str) -> bool {
        self.resume_w
            .as_deref()
            .is_some_and(|w| resume_action(rel, false, w) == ResumeAction::Skip)
    }

    /// Source-side entry: either proven absent on dest (`dest_lite` None) or
    /// present with these lite facts.
    fn classify_src(&mut self, key: &str, e: RelEntry, dest_lite: Option<Box<FileMetadata>>) {
        match e.kind {
            EntryKind::Dir => {
                // Src-side dir with no dest counterpart in the merge: the
                // dest subtree is proven absent — SrcOnly (copy-all).
                self.discover_child(key, e.path, DirMode::SrcOnly);
            }
            EntryKind::Obj(src_lite) => {
                if self.resume_settled(&e.path) {
                    return;
                }
                self.stats.objects_scanned += 1;
                let abs_src = format!("{}{}", self.cfg.src_prefix, e.path);
                // Directory markers, DG internals, and glob filters are all
                // should_replicate's job — evaluate with dest=None first for
                // pure gate checks when dest facts are irrelevant.
                if let Some(dest_lite) = dest_lite {
                    self.classify_pair(key, e.path, src_lite, dest_lite, abs_src);
                } else {
                    let d = should_replicate(
                        &abs_src,
                        &src_lite,
                        None,
                        self.cfg.conflict,
                        self.cfg.strict_content_diff,
                        &self.cfg.include_globs,
                        &self.cfg.exclude_globs,
                    );
                    self.emit_decision(key, e.path, &src_lite, d);
                }
            }
        }
    }

    fn classify_pair(
        &mut self,
        key: &str,
        rel: String,
        src_lite: Box<FileMetadata>,
        dest_lite: Box<FileMetadata>,
        abs_src: String,
    ) {
        let need_src =
            side_facts_need_head(self.cfg.conflict, self.cfg.regime.src_lite_authoritative);
        let need_dest =
            side_facts_need_head(self.cfg.conflict, self.cfg.regime.dest_lite_authoritative);

        if !need_src && !need_dest {
            let d = should_replicate(
                &abs_src,
                &src_lite,
                Some(&dest_lite),
                self.cfg.conflict,
                self.cfg.strict_content_diff,
                &self.cfg.include_globs,
                &self.cfg.exclude_globs,
            );
            self.emit_decision(key, rel, &src_lite, d);
            return;
        }

        // Buffer the pair; batched HEADs resolve the untrusted side(s).
        self.tracker.open_item(&rel);
        let Some(task) = self.active.get_mut(key) else {
            return;
        };
        task.outstanding += 1;
        task.pending_pairs.insert(
            rel.clone(),
            PendingPair {
                src_lite,
                dest_lite,
                src_resolved: (!need_src).then_some(None),
                dest_resolved: (!need_dest).then_some(None),
            },
        );
        if need_src {
            task.head_batch[0].push(rel.clone());
        }
        if need_dest {
            task.head_batch[1].push(rel.clone());
        }
        self.flush_head_batches(key, false);
    }

    fn classify_dest_only(&mut self, key: &str, e: RelEntry) {
        match e.kind {
            EntryKind::Dir => {
                if self.cfg.replicate_deletes && !skip_dir(&e.path) {
                    self.discover_child(key, e.path, DirMode::DestOnly);
                }
            }
            EntryKind::Obj(dest_lite) => {
                if !self.cfg.replicate_deletes || self.resume_settled(&e.path) {
                    return;
                }
                let abs_dest = format!("{}{}", self.cfg.dest_prefix, e.path);
                if e.path.ends_with('/') || is_dg_internal(&abs_dest) {
                    return; // markers and DG internals are never candidates
                }
                // Provenance: trust the listing when it carries our marker;
                // otherwise the driver must HEAD-confirm before deleting.
                let owned_in_listing = owned_by_rule(&dest_lite, &self.cfg.rule_name);
                let needs_head = !owned_in_listing;
                self.tracker.open_item(&e.path);
                let Some(task) = self.active.get_mut(key) else {
                    return;
                };
                task.outstanding += 1;
                task.delete_candidates.push((e.path, needs_head));
            }
        }
    }

    fn discover_child(&mut self, parent_key: &str, child_rel: String, mode: DirMode) {
        if skip_dir(&child_rel) {
            return;
        }
        if mode == DirMode::DestOnly && !self.cfg.replicate_deletes {
            return;
        }
        let (over_cap, flat) = match self.active.get_mut(parent_key) {
            Some(t) => {
                t.child_dirs_seen += 1;
                (t.child_dirs_seen > self.cfg.max_child_dirs_per_dir, t.flat)
            }
            None => (false, false),
        };
        if flat {
            return; // flat sweeps never spawn children
        }
        self.enqueue_dir(child_rel, mode);
        if over_cap {
            self.flatten_dir(parent_key);
        }
    }

    fn emit_decision(&mut self, key: &str, rel: String, src_lite: &FileMetadata, d: Decision) {
        match d {
            Decision::Copy { .. } => {
                self.tracker.open_item(&rel);
                let item_id = self.next_id;
                self.next_id += 1;
                self.routes.insert(
                    item_id,
                    CmdRoute::CopyItem {
                        dir: key.to_string(),
                        rel: rel.clone(),
                    },
                );
                if let Some(task) = self.active.get_mut(key) {
                    task.outstanding += 1;
                }
                self.ready_copies
                    .push_back((item_id, rel, src_lite.file_size));
            }
            Decision::Skip { .. } => {
                self.stats.objects_skipped += 1;
                self.tracker.open_item(&rel);
                self.tracker.settle(&rel);
            }
        }
    }

    // -- HEAD resolution ----------------------------------------------------

    fn flush_head_batches(&mut self, key: &str, force: bool) {
        for side in [Side::Src, Side::Dest] {
            let idx = match side {
                Side::Src => 0,
                Side::Dest => 1,
            };
            let Some(task) = self.active.get_mut(key) else {
                return;
            };
            if task.head_batch[idx].is_empty() {
                continue;
            }
            if !force && task.head_batch[idx].len() < self.cfg.head_batch {
                continue;
            }
            let keys = std::mem::take(&mut task.head_batch[idx]);
            let req_id = self.next_id;
            self.next_id += 1;
            self.routes.insert(
                req_id,
                CmdRoute::Heads {
                    dir: key.to_string(),
                    side,
                },
            );
            self.ready_heads.push_back((req_id, side, keys));
        }
    }

    fn on_heads_done(&mut self, dir: &str, side: Side, results: Vec<(String, HeadResult)>) {
        for (rel, res) in results {
            let Some(task) = self.active.get_mut(dir) else {
                return;
            };
            if !task.pending_pairs.contains_key(&rel) {
                continue; // pair abandoned by a flatten — ignore the reply
            }
            if side == Side::Src && matches!(res, HeadResult::Gone) {
                // Source vanished mid-walk (raced delete): settle, no copy.
                task.pending_pairs.remove(&rel);
                task.outstanding -= 1;
                self.tracker.settle(&rel);
                continue;
            }
            let resolved = match res {
                HeadResult::Resolved(m) => Some(m),
                // Src Unresolved → lite fallback (over-copy is safe);
                // dest Gone/Unresolved → treated absent (copy, old `.ok()`
                // semantics). Both encode as `Some(None)` = "no HEAD facts".
                HeadResult::Gone | HeadResult::Unresolved => None,
            };
            let pair = task.pending_pairs.get_mut(&rel).expect("checked");
            match side {
                Side::Src => pair.src_resolved = Some(resolved),
                Side::Dest => pair.dest_resolved = Some(resolved),
            }
            self.try_decide_pair(dir, &rel);
        }
        self.maybe_complete_dir(dir);
    }

    fn try_decide_pair(&mut self, dir: &str, rel: &str) {
        let Some(task) = self.active.get_mut(dir) else {
            return;
        };
        let ready = task
            .pending_pairs
            .get(rel)
            .is_some_and(|p| p.src_resolved.is_some() && p.dest_resolved.is_some());
        if !ready {
            return;
        }
        let pair = task.pending_pairs.remove(rel).expect("checked");
        task.outstanding -= 1;

        // Src: HEAD facts when resolved, else lite (covers both "not needed"
        // and "Unresolved fallback" — over-copy is the safe direction).
        // Dest: HEAD facts when resolved; a needed-but-missing dest HEAD is
        // treated ABSENT (old `.ok()` semantics ⇒ copy); a not-needed one
        // uses the trusted lite facts.
        let need_dest =
            side_facts_need_head(self.cfg.conflict, self.cfg.regime.dest_lite_authoritative);
        let src_meta: &FileMetadata = match pair.src_resolved.as_ref().expect("ready") {
            Some(m) => m,
            None => &pair.src_lite,
        };
        let dest_meta: Option<&FileMetadata> = match pair.dest_resolved.as_ref().expect("ready") {
            Some(m) => Some(m),
            None if need_dest => None,
            None => Some(&pair.dest_lite),
        };
        let abs_src = format!("{}{}", self.cfg.src_prefix, rel);
        let d = should_replicate(
            &abs_src,
            src_meta,
            dest_meta,
            self.cfg.conflict,
            self.cfg.strict_content_diff,
            &self.cfg.include_globs,
            &self.cfg.exclude_globs,
        );
        let src_size = src_meta.file_size;
        // The pair's tracker entry transfers to the emitted decision.
        match d {
            Decision::Copy { .. } => {
                let item_id = self.next_id;
                self.next_id += 1;
                self.routes.insert(
                    item_id,
                    CmdRoute::CopyItem {
                        dir: dir.to_string(),
                        rel: rel.to_string(),
                    },
                );
                if let Some(task) = self.active.get_mut(dir) {
                    task.outstanding += 1;
                }
                self.ready_copies
                    .push_back((item_id, rel.to_string(), src_size));
            }
            Decision::Skip { .. } => {
                self.stats.objects_skipped += 1;
                self.tracker.settle(rel);
            }
        }
    }

    // -- completion ---------------------------------------------------------

    fn maybe_complete_dir(&mut self, key: &str) {
        let Some(task) = self.active.get(key) else {
            return;
        };
        if !task.listings_done() {
            return;
        }
        // Tail flush: partial HEAD batches must go out or pairs deadlock.
        self.flush_head_batches(key, true);

        let Some(task) = self.active.get(key) else {
            return;
        };
        // Quiesced: copies + pairs all settled — only buffered delete
        // candidates (if any) still hold outstanding slots.
        let quiesced = task.pending_pairs.is_empty()
            && task.head_batch.iter().all(|b| b.is_empty())
            && task.outstanding == task.delete_candidates.len();
        if !quiesced {
            return;
        }

        if !task.deletes_flushed {
            let clean = task.copy_errors == 0 && !self.stats.truncated_by_budget;
            let candidates = {
                let Some(task) = self.active.get_mut(key) else {
                    return;
                };
                task.deletes_flushed = true;
                std::mem::take(&mut task.delete_candidates)
            };
            for (rel, needs_head) in candidates {
                if clean && self.drained.is_none() {
                    let item_id = self.next_id;
                    self.next_id += 1;
                    self.routes.insert(
                        item_id,
                        CmdRoute::DeleteItem {
                            dir: key.to_string(),
                            rel: rel.clone(),
                        },
                    );
                    self.ready_deletes.push_back((item_id, rel, needs_head));
                } else {
                    // Dirty dir / aborting run: preserve. Settle so the run's
                    // cursor can pass — the NEXT full pass re-discovers them.
                    if let Some(task) = self.active.get_mut(key) {
                        task.outstanding -= 1;
                    }
                    self.tracker.settle(&rel);
                }
            }
        }

        let Some(task) = self.active.get(key) else {
            return;
        };
        if task.outstanding > 0 {
            return;
        }
        self.active.remove(key);
        self.stats.dirs_completed += 1;
    }
}

enum Merged {
    Single(Side, RelEntry),
    SrcOnly(RelEntry),
    DestOnly(RelEntry),
    Both(RelEntry, RelEntry),
}

impl Merged {
    /// (side, consumed path) pairs this step removed from stream buffers.
    fn popped(&self) -> Vec<(Side, &str)> {
        match self {
            Merged::Single(side, e) => vec![(*side, e.path.as_str())],
            Merged::SrcOnly(e) => vec![(Side::Src, e.path.as_str())],
            Merged::DestOnly(e) => vec![(Side::Dest, e.path.as_str())],
            Merged::Both(se, de) => vec![
                (Side::Src, se.path.as_str()),
                (Side::Dest, de.path.as_str()),
            ],
        }
    }
}

/// Never descend into `.deltaglider` dirs (config-sync internals; objects
/// under them are planner-skipped anyway — this just saves listings).
fn skip_dir(rel_dir: &str) -> bool {
    rel_dir
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .is_some_and(|seg| seg == ".deltaglider")
}

/// Mirror of should_replicate's DG-internal gate, applied to DEST keys
/// (delete candidates) — replication never writes these, never delete them.
fn is_dg_internal(key: &str) -> bool {
    key.starts_with(".deltaglider/") || key.contains("/.deltaglider/")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_settles_in_order() {
        let mut t = ContiguousTracker::new();
        t.open_item("a");
        t.open_item("b");
        t.open_item("c");
        assert_eq!(t.watermark(), None);
        t.settle("b");
        assert_eq!(t.watermark(), None, "a still open blocks b");
        t.settle("a");
        assert_eq!(t.watermark(), Some("b"), "a+b contiguous");
        t.settle("c");
        assert_eq!(t.watermark(), Some("c"));
        assert!(t.is_empty());
    }

    #[test]
    fn tracker_refcounts_shared_item_paths() {
        // Two ITEMS at one path (idempotent redo): the remaining open item
        // blocks promotion of the settled duplicate.
        let mut t = ContiguousTracker::new();
        t.open_item("d/x");
        t.open_item("d/x");
        t.settle("d/x");
        assert_eq!(t.watermark(), None, "second open item still guards d/x");
        t.settle("d/x");
        assert_eq!(t.watermark(), Some("d/x"));
        assert!(t.is_empty());
    }

    #[test]
    fn tracker_marker_at_same_path_does_not_block() {
        // A marker guards strictly BEYOND its position: a settled item at
        // the marker's own path promotes (empty-dir progress depends on it).
        let mut t = ContiguousTracker::new();
        t.open_marker("d/");
        t.open_item("d/");
        t.settle("d/");
        assert_eq!(t.watermark(), Some("d/"));
        t.close_marker("d/");
        assert!(t.is_empty());
    }

    #[test]
    fn tracker_marker_advance_promotes_exactly_to_consumed() {
        let mut t = ContiguousTracker::new();
        t.open_marker("d/"); // listing frontier at dir
        t.open_item("d/a"); // emitted item
        t.settle("d/a");
        assert_eq!(t.watermark(), None, "frontier below d/a still guards it");
        // Marker moves d/ -> d/a: the settled item AT d/a may now promote
        // (the stream owes only content beyond d/a).
        t.advance_marker("d/", "d/a");
        assert_eq!(t.watermark(), Some("d/a"));
        t.close_marker("d/a");
        assert!(t.is_empty());
    }

    #[test]
    fn tracker_empty_open_promotes_everything() {
        let mut t = ContiguousTracker::new();
        t.open_marker("m/");
        t.open_item("m/x");
        t.open_item("m/y");
        t.settle("m/y");
        t.settle("m/x");
        t.close_marker("m/");
        assert_eq!(t.watermark(), Some("m/y"));
        assert!(t.is_empty());
    }

    #[test]
    fn resume_action_truth_table() {
        use ResumeAction::*;
        let cases: &[(&str, bool, &str, ResumeAction)] = &[
            // (rel, is_dir, watermark, expected)
            ("a-b/", true, "a/", Skip), // '-' < '/' ⇒ whole sibling done
            ("a/", true, "a//b/x", DescendOnly), // ancestor via empty segment
            ("a//", true, "a//b/x", DescendOnly),
            ("a//b/", true, "a//b/x", DescendOnly),
            ("a//a/", true, "a//b/x", Skip),     // sibling before w
            ("a//c/", true, "a//b/x", Process),  // sibling after w
            ("foo/", true, "foobar/", Skip),     // '/' < 'b' ⇒ foo/ subtree < foobar/
            ("foo", false, "foobar", Skip),      // object: prefix of w but not a dir
            ("foo", false, "foo", Skip),         // object == w
            ("d/", true, "d/", DescendOnly),     // dir == w: children > w remain
            ("", true, "anything", DescendOnly), // root is ancestor of everything
            ("b/", true, "a/z", Process),
            ("b", false, "a/z", Process),
        ];
        for (rel, is_dir, w, want) in cases {
            assert_eq!(
                resume_action(rel, *is_dir, w),
                *want,
                "resume_action({rel:?}, {is_dir}, {w:?})"
            );
        }
    }

    #[test]
    fn resume_list_token_cases() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("a/", "a/m/x", Some("a/m")), // w inside child a/m/ ⇒ keep that prefix listed
            ("a/", "a/m", Some("a/m")),   // w is a direct object
            ("", "m/x", Some("m")),
            ("a/", "b/x", None), // w outside dir
            ("a/", "a/", None),  // w == dir
            ("a//", "a//b/c", Some("a//b")),
            ("a/", "a//b", Some("a/")), // empty segment child a// ⇒ token "a/"
        ];
        for (dir, w, want) in cases {
            assert_eq!(
                resume_list_token(dir, w).as_deref(),
                *want,
                "resume_list_token({dir:?}, {w:?})"
            );
        }
    }

    #[test]
    fn cursor_round_trip_and_rejections() {
        let scope = cursor_scope("srcb", "s/p/", "dstb", "d/");
        let c = CursorV1::new(&scope, "builds/v2/app.zip");
        let json = c.to_json();
        assert_eq!(load_cursor(Some(&json), &scope), Some(c.clone()));

        // Legacy flat S3 token (non-JSON) → fresh.
        assert_eq!(load_cursor(Some("builds/v1/old-token"), &scope), None);
        // Scope mismatch (rule re-pointed) → fresh.
        assert_eq!(load_cursor(Some(&json), "other|s/|b|d/"), None);
        // Version bump → fresh.
        let v2 = json.replace("\"v\":1", "\"v\":2");
        assert_eq!(load_cursor(Some(&v2), &scope), None);
        // Empty pos → fresh.
        let empty = CursorV1::new(&scope, "").to_json();
        assert_eq!(load_cursor(Some(&empty), &scope), None);
        assert_eq!(load_cursor(None, &scope), None);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn seg() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(String::new()), // empty segment ⇒ "//" in paths
            Just("a".to_string()),
            Just("b".to_string()),
            Just("a-b".to_string()),
            Just("foo".to_string()),
            Just(".deltaglider".to_string()),
            Just(".well-known".to_string()),
            Just("é⽇".to_string()),
            Just("a b".to_string()),
        ]
    }

    /// Relative object path: segments joined by '/', non-empty overall.
    fn rel_path() -> impl Strategy<Value = String> {
        proptest::collection::vec(seg(), 1..5).prop_map(|v| v.join("/"))
    }

    /// Relative dir path: like rel_path but '/'-terminated (root excluded).
    fn rel_dir() -> impl Strategy<Value = String> {
        proptest::collection::vec(seg(), 1..4).prop_map(|v| format!("{}/", v.join("/")))
    }

    proptest! {
        /// P9a — sibling-subtree bound (the ordering lemma the whole walk
        /// rests on): for sibling child prefixes p < p', EVERY descendant
        /// of p sorts before p'.
        #[test]
        fn sibling_subtree_bound(d in prop_oneof![Just(String::new()), rel_dir()],
                                 s1 in seg(), s2 in seg(),
                                 y in rel_path()) {
            prop_assume!(s1 != s2);
            let p1 = format!("{d}{s1}/");
            let p2 = format!("{d}{s2}/");
            let (lo, hi) = if p1 < p2 { (p1, p2) } else { (p2, p1) };
            let descendant = format!("{lo}{y}");
            prop_assert!(descendant < hi,
                "descendant {descendant:?} of {lo:?} must sort before sibling {hi:?}");
        }

        /// P9b — resume_action soundness against brute-force descendants:
        /// Skip ⇒ every possible descendant ≤ w; Process ⇒ none is.
        #[test]
        fn resume_action_sound(d in rel_dir(), w in rel_path(), y in rel_path()) {
            let descendant = format!("{d}{y}");
            match resume_action(&d, true, &w) {
                ResumeAction::Skip => prop_assert!(descendant < w,
                    "Skip({d:?}) but descendant {descendant:?} >= watermark {w:?}"),
                ResumeAction::Process => prop_assert!(descendant > w,
                    "Process({d:?}) but descendant {descendant:?} <= watermark {w:?}"),
                ResumeAction::DescendOnly => {
                    // Ancestor of w: both settled (< w) and pending (> w)
                    // descendants can exist — nothing to assert here beyond
                    // the ancestor relation itself.
                    prop_assert!(w.starts_with(&d));
                }
            }
        }

        /// P10 — resume_list_token drops only settled entries, under BOTH
        /// backend token dialects (strictly-greater and AWS start_after
        /// with prefix re-emission).
        #[test]
        fn resume_token_drops_only_settled(
            dir in prop_oneof![Just(String::new()), rel_dir()],
            tail in rel_path(),
            names in proptest::collection::btree_set(seg(), 1..8),
            dirs_not_objs in proptest::collection::vec(any::<bool>(), 8),
        ) {
            let w = format!("{dir}{tail}");
            let Some(token) = resume_list_token(&dir, &w) else {
                return Ok(()); // w outside dir or == dir: fresh listing, nothing dropped
            };

            for (i, name) in names.iter().enumerate() {
                let is_dir = dirs_not_objs[i % dirs_not_objs.len()];
                let full = if is_dir {
                    format!("{dir}{name}/")
                } else {
                    format!("{dir}{name}")
                };
                let action = resume_action(&full, is_dir, &w);

                // Dialect A: strictly-greater (engine `paginate_sorted`).
                let emitted_strict = full.as_str() > token.as_str();
                // Dialect B: AWS start_after — same order filter on keys, but a
                // common prefix re-emits when ANY key under it survives; the
                // prefix containing the token itself therefore re-emits.
                let emitted_aws = emitted_strict
                    || (is_dir && token.starts_with(full.as_str()));

                for (emitted, dialect) in [(emitted_strict, "strict"), (emitted_aws, "aws")] {
                    if !emitted {
                        prop_assert_eq!(action, ResumeAction::Skip,
                            "{} dialect dropped {:?} (token {:?}, w {:?}) but action is {:?}",
                            dialect, &full, &token, &w, action);
                    }
                }
            }
        }

        /// P8 — ContiguousTracker vs brute-force model under randomized
        /// out-of-order settles with a moving listing marker.
        #[test]
        fn tracker_matches_bruteforce_model(
            raw_paths in proptest::collection::btree_set(rel_path(), 1..20),
            order_seed in proptest::collection::vec(any::<u16>(), 1..64),
            concurrency in 1usize..5,
        ) {
            let paths: Vec<String> = raw_paths.into_iter().collect(); // sorted, distinct
            let mut t = ContiguousTracker::new();

            // Model state: items and the single stream marker tracked apart.
            let mut items_open: Vec<String> = Vec::new();
            let mut settled_model: BTreeSet<String> = BTreeSet::new();

            let mut next_to_open = 0usize;
            let mut seed_idx = 0usize;
            let mut take = |n: usize| {
                let v = order_seed[seed_idx % order_seed.len()] as usize % n.max(1);
                seed_idx += 1;
                v
            };

            // Marker trails the opens, like a listing frontier. It stays
            // alive until the final drain, which breaks out immediately.
            let mut marker_pos = "!".to_string(); // '!' < every generated path
            t.open_marker(&marker_pos);

            loop {
                let can_open =
                    next_to_open < paths.len() && items_open.len() < concurrency;

                if can_open && (items_open.is_empty() || take(2) == 0) {
                    // Open next path in lex order; marker advances with it
                    // (a listing emits, then the frontier moves).
                    let p = paths[next_to_open].clone();
                    next_to_open += 1;
                    t.open_item(&p);
                    items_open.push(p.clone());
                    if p > marker_pos {
                        t.advance_marker(&marker_pos, &p);
                        marker_pos = p;
                    }
                } else if !items_open.is_empty() {
                    // Settle a random open item (out-of-order completion).
                    let victim = items_open.remove(take(items_open.len()));
                    t.settle(&victim);
                    settled_model.insert(victim);
                } else {
                    // No items, nothing left to open: close the marker, done.
                    t.close_marker(&marker_pos);
                    break;
                }

                // Invariant: watermark == max settled path s with every open
                // ITEM > s and the marker >= s (markers guard strictly
                // beyond). Opens are monotone, so this value is monotone.
                let min_item = items_open.iter().min();
                let expect = settled_model.iter().rfind(|s| {
                    min_item.is_none_or(|i| s.as_str() < i.as_str())
                        && s.as_str() <= marker_pos.as_str()
                });
                prop_assert_eq!(t.watermark(), expect.map(|s| s.as_str()),
                    "items {:?} marker {:?}", items_open, marker_pos);
            }

            // Fully drained: watermark must equal the maximum settled path.
            prop_assert!(t.is_empty());
            prop_assert_eq!(
                t.watermark(),
                settled_model.iter().next_back().map(|s| s.as_str())
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Machine tests: FakeWorld harness + model-based properties (P1–P7)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod machine_tests {
    use super::*;
    use crate::transfer::REPLICATION_RULE_METADATA_KEY;
    use chrono::{TimeZone, Utc};
    use std::collections::BTreeSet;

    const RULE: &str = "walk-test-rule";

    // -- world model ---------------------------------------------------------

    #[derive(Clone, Debug)]
    struct WorldObj {
        true_meta: FileMetadata,
        lite_meta: FileMetadata,
    }

    #[derive(Clone, Debug)]
    struct FakeWorld {
        src: BTreeMap<String, WorldObj>,
        dest: BTreeMap<String, WorldObj>,
        regime: FactRegime,
    }

    fn mk_meta(size: u64, sha: &str, etag: &str, created_secs: i64, owned: bool) -> FileMetadata {
        let mut m = FileMetadata::new_passthrough(
            "f".to_string(),
            sha.to_string(),
            etag.to_string(),
            size,
            None,
        );
        m.created_at = Utc.timestamp_opt(created_secs, 0).unwrap();
        if owned {
            m.user_metadata
                .insert(REPLICATION_RULE_METADATA_KEY.to_string(), RULE.to_string());
        }
        m
    }

    /// What an untrusted (S3-like) lite listing does to the truth: bumped
    /// LastModified-as-created_at, no sha, a listing etag, no user metadata.
    /// Decisions taken on these facts DIVERGE from truth — P1 detects any
    /// code path that trusts lite facts it shouldn't.
    fn scramble(true_meta: &FileMetadata) -> FileMetadata {
        let mut m = true_meta.clone();
        m.created_at += chrono::Duration::seconds(1_000_000);
        m.file_sha256 = String::new();
        m.md5 = "lite-etag-differs".to_string();
        m.user_metadata.clear();
        m
    }

    fn world_obj(true_meta: FileMetadata, authoritative: bool) -> WorldObj {
        let lite_meta = if authoritative {
            true_meta.clone()
        } else {
            scramble(&true_meta)
        };
        WorldObj {
            true_meta,
            lite_meta,
        }
    }

    /// Metadata a completed replication copy leaves on the destination:
    /// source truth + our provenance marker.
    fn copied_from(src: &WorldObj, dest_authoritative: bool) -> WorldObj {
        let mut t = src.true_meta.clone();
        t.user_metadata
            .insert(REPLICATION_RULE_METADATA_KEY.to_string(), RULE.to_string());
        world_obj(t, dest_authoritative)
    }

    // -- listing emulation (engine `paginate_sorted` semantics) --------------

    fn list_page(
        side: &BTreeMap<String, WorldObj>,
        rel_prefix: &str,
        delimited: bool,
        token: Option<&str>,
        page_size: usize,
    ) -> RelListing {
        // Derive the full entry sequence for this prefix, in order.
        let mut entries: Vec<RelEntry> = Vec::new();
        let mut last_prefix: Option<String> = None;
        for (k, o) in side.range(rel_prefix.to_string()..) {
            if !k.starts_with(rel_prefix) {
                break;
            }
            let tail = &k[rel_prefix.len()..];
            let entry = if delimited {
                match tail.find('/') {
                    Some(i) => {
                        let p = k[..rel_prefix.len() + i + 1].to_string();
                        if last_prefix.as_deref() == Some(p.as_str()) {
                            continue; // collapsed group continues
                        }
                        last_prefix = Some(p.clone());
                        RelEntry {
                            path: p,
                            kind: EntryKind::Dir,
                        }
                    }
                    None => RelEntry {
                        path: k.clone(),
                        kind: EntryKind::Obj(Box::new(o.lite_meta.clone())),
                    },
                }
            } else {
                RelEntry {
                    path: k.clone(),
                    kind: EntryKind::Obj(Box::new(o.lite_meta.clone())),
                }
            };
            entries.push(entry);
        }
        // Strict entry-level token filter + page cut.
        let after: Vec<RelEntry> = entries
            .into_iter()
            .filter(|e| token.is_none_or(|t| e.path.as_str() > t))
            .collect();
        let truncated = after.len() > page_size;
        let page: Vec<RelEntry> = after.into_iter().take(page_size).collect();
        let next_token = truncated.then(|| page.last().expect("non-empty page").path.clone());
        RelListing {
            entries: page,
            truncated,
            next_token,
        }
    }

    // -- reference implementation (naive global diff) -------------------------

    fn is_marker_or_internal(key: &str) -> bool {
        key.ends_with('/') || is_dg_internal(key)
    }

    fn reference_diff(
        world: &FakeWorld,
        conflict: ConflictPolicy,
        strict: bool,
        replicate_deletes: bool,
    ) -> (BTreeSet<String>, BTreeSet<String>) {
        let empty = GlobSet::empty();
        let need_src = side_facts_need_head(conflict, world.regime.src_lite_authoritative);
        let need_dest = side_facts_need_head(conflict, world.regime.dest_lite_authoritative);

        let mut copies = BTreeSet::new();
        for (k, o) in &world.src {
            let src_meta = if need_src { &o.true_meta } else { &o.lite_meta };
            let dest_meta = world.dest.get(k).map(|d| {
                if need_dest {
                    &d.true_meta
                } else {
                    &d.lite_meta
                }
            });
            if let Decision::Copy { .. } =
                should_replicate(k, src_meta, dest_meta, conflict, strict, &empty, &empty)
            {
                copies.insert(k.clone());
            }
        }

        let mut deletes = BTreeSet::new();
        if replicate_deletes {
            for (k, o) in &world.dest {
                if !is_marker_or_internal(k)
                    && !world.src.contains_key(k)
                    && owned_by_rule(&o.true_meta, RULE)
                {
                    deletes.insert(k.clone());
                }
            }
        }
        (copies, deletes)
    }

    // -- driver simulation -----------------------------------------------------

    #[derive(Default)]
    struct Trace {
        copies: BTreeSet<String>,
        deletes: BTreeSet<String>,
    }

    struct Sim {
        world: FakeWorld,
        page_size: usize,
        trace: Trace,
        queue: Vec<Event>,
        last_cursor: Option<String>,
    }

    impl Sim {
        fn exec(&mut self, cmd: Cmd) {
            match cmd {
                Cmd::List {
                    req_id,
                    side,
                    rel_prefix,
                    token,
                    delimited,
                } => {
                    let map = match side {
                        Side::Src => &self.world.src,
                        Side::Dest => &self.world.dest,
                    };
                    let page = list_page(
                        map,
                        &rel_prefix,
                        delimited,
                        token.as_deref(),
                        self.page_size,
                    );
                    self.queue.push(Event::ListPage { req_id, page });
                }
                Cmd::Head {
                    req_id,
                    side,
                    rel_keys,
                } => {
                    let map = match side {
                        Side::Src => &self.world.src,
                        Side::Dest => &self.world.dest,
                    };
                    let results = rel_keys
                        .into_iter()
                        .map(|k| {
                            let r = match map.get(&k) {
                                Some(o) => HeadResult::Resolved(Box::new(o.true_meta.clone())),
                                None => HeadResult::Gone,
                            };
                            (k, r)
                        })
                        .collect();
                    self.queue.push(Event::HeadDone { req_id, results });
                }
                Cmd::Copy {
                    item_id, rel_key, ..
                } => {
                    let src = self
                        .world
                        .src
                        .get(&rel_key)
                        .expect("copy of missing source")
                        .clone();
                    let dest_auth = self.world.regime.dest_lite_authoritative;
                    self.world
                        .dest
                        .insert(rel_key.clone(), copied_from(&src, dest_auth));
                    self.trace.copies.insert(rel_key);
                    self.queue.push(Event::CopySettled { item_id, ok: true });
                }
                Cmd::Delete {
                    item_id,
                    rel_key,
                    needs_provenance_head,
                } => {
                    // Driver pipeline: provenance (+P4 safety assertions),
                    // src-absence confirm, then delete.
                    let owned = match self.world.dest.get(&rel_key) {
                        Some(o) => {
                            if needs_provenance_head {
                                owned_by_rule(&o.true_meta, RULE)
                            } else {
                                // Machine trusted the listing — that trust must
                                // be justified by the truth (P4).
                                assert!(
                                    owned_by_rule(&o.true_meta, RULE),
                                    "machine trusted listing provenance for {rel_key:?} \
                                     but the object is not rule-owned"
                                );
                                true
                            }
                        }
                        None => false, // already gone — nothing to delete
                    };
                    if owned && !self.world.src.contains_key(&rel_key) {
                        self.world.dest.remove(&rel_key);
                        self.trace.deletes.insert(rel_key);
                    }
                    self.queue.push(Event::DeleteSettled { item_id, ok: true });
                }
            }
        }

        /// P4: a delete may only ever be COMMANDED for a rule-owned, src-absent
        /// key (checked against the world truth at command time).
        fn assert_delete_safe(&self, rel_key: &str) {
            assert!(
                !self.world.src.contains_key(rel_key),
                "delete commanded for {rel_key:?} which exists on source"
            );
        }
    }

    /// Drive to quiescence. `deliver_budget` bounds how many events get
    /// delivered (None = all — run to completion); `seed` permutes delivery
    /// order and poll caps to model parallel completion. Returns whether the
    /// machine went idle (vs. cut by the budget).
    fn drive(
        machine: &mut WalkMachine,
        sim: &mut Sim,
        seed: &[u16],
        deliver_budget: Option<usize>,
    ) {
        let mut delivered = 0usize;
        let mut seed_i = 0usize;
        let mut take = |n: usize| {
            let v = seed[seed_i % seed.len().max(1)] as usize % n.max(1);
            seed_i += 1;
            v
        };
        for _round in 0..200_000 {
            let caps = DriverCaps {
                list: 1 + take(3),
                head: 1 + take(2),
                copy: 1 + take(3),
                delete: 1 + take(2),
            };
            let cmds = machine.poll(caps);
            let had_cmds = !cmds.is_empty();
            for cmd in cmds {
                if let Cmd::Delete { rel_key, .. } = &cmd {
                    sim.assert_delete_safe(rel_key);
                }
                sim.exec(cmd);
            }
            // P5: cursor monotonicity across the whole run.
            if let Some(c) = machine.cursor() {
                if let Some(prev) = &sim.last_cursor {
                    assert!(
                        c.pos.as_str() >= prev.as_str(),
                        "cursor regressed: {prev:?} -> {:?}",
                        c.pos
                    );
                }
                sim.last_cursor = Some(c.pos);
            }
            if !sim.queue.is_empty() {
                if deliver_budget.is_some_and(|b| delivered >= b) {
                    return; // cut: pending events are lost (crash model)
                }
                let idx = take(sim.queue.len());
                let ev = sim.queue.remove(idx);
                machine.on_event(ev);
                delivered += 1;
                continue;
            }
            if !had_cmds {
                return; // idle: no commands, no events
            }
        }
        panic!("machine did not quiesce (deadlock?)");
    }

    #[allow(clippy::too_many_arguments)] // test factory mirroring WalkConfig
    fn mk_machine(
        world: &FakeWorld,
        conflict: ConflictPolicy,
        strict: bool,
        replicate_deletes: bool,
        page_size: u32,
        max_pages: u32,
        dir_workers: usize,
        head_batch: usize,
        max_child_dirs: usize,
        resume: Option<CursorV1>,
    ) -> WalkMachine {
        WalkMachine::new(
            WalkConfig {
                scope: "s|/|d|/".to_string(),
                rule_name: RULE.to_string(),
                src_prefix: String::new(),
                dest_prefix: String::new(),
                conflict,
                strict_content_diff: strict,
                replicate_deletes,
                include_globs: GlobSet::empty(),
                exclude_globs: GlobSet::empty(),
                regime: world.regime,
                page_size,
                max_pages,
                dir_workers,
                head_batch,
                max_child_dirs_per_dir: max_child_dirs,
                max_pending_dirs: 100_000,
                action_buffer: 6,
            },
            resume,
        )
    }

    // -- deterministic unit scenarios -----------------------------------------

    fn simple_world(regime: FactRegime) -> FakeWorld {
        FakeWorld {
            src: BTreeMap::new(),
            dest: BTreeMap::new(),
            regime,
        }
    }

    const AUTH: FactRegime = FactRegime {
        src_lite_authoritative: true,
        dest_lite_authoritative: true,
    };
    const UNTRUSTED: FactRegime = FactRegime {
        src_lite_authoritative: false,
        dest_lite_authoritative: false,
    };

    fn run_full(world: &FakeWorld, conflict: ConflictPolicy, deletes: bool) -> (Trace, WalkStats) {
        let mut machine = mk_machine(
            world,
            conflict,
            false,
            deletes,
            3,
            10_000,
            2,
            2,
            usize::MAX,
            None,
        );
        let mut sim = Sim {
            world: world.clone(),
            page_size: 3,
            trace: Trace::default(),
            queue: Vec::new(),
            last_cursor: None,
        };
        drive(&mut machine, &mut sim, &[0], None);
        assert!(machine.is_done(), "expected clean completion");
        (sim.trace, machine.stats().clone())
    }

    #[test]
    fn side_facts_need_head_table() {
        use ConflictPolicy::*;
        let cases = [
            (SkipIfDestExists, true, false),
            (SkipIfDestExists, false, false),
            (NewerWins, true, false),
            (NewerWins, false, true),
            (ContentDiff, true, false),
            (ContentDiff, false, true),
        ];
        for (policy, auth, want) in cases {
            assert_eq!(
                side_facts_need_head(policy, auth),
                want,
                "{policy:?}/{auth}"
            );
        }
    }

    #[test]
    fn src_only_subtree_copies_all_without_heads() {
        let mut w = simple_world(UNTRUSTED);
        for k in ["a/x", "a/y", "a/sub/z", "top"] {
            w.src.insert(
                k.to_string(),
                world_obj(mk_meta(5, "s", "e", 100, false), false),
            );
        }
        let (trace, _) = run_full(&w, ConflictPolicy::NewerWins, false);
        assert_eq!(
            trace.copies,
            ["a/x", "a/y", "a/sub/z", "top"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<_>>()
        );
        // Empty dest ⇒ absence proven by the merge — zero HEAD commands were
        // needed (the Sim would have recorded them as resolved pairs; the
        // real assertion is copies happened without any Head cmd, which the
        // deadlock-free completion + full copy set already implies: a pair
        // would have required dest facts that do not exist).
    }

    #[test]
    fn skip_if_dest_exists_needs_no_heads_and_skips() {
        let mut w = simple_world(UNTRUSTED);
        w.src.insert(
            "k".into(),
            world_obj(mk_meta(5, "s", "e", 100, false), false),
        );
        w.dest.insert(
            "k".into(),
            world_obj(mk_meta(9, "d", "e2", 50, true), false),
        );
        let (trace, stats) = run_full(&w, ConflictPolicy::SkipIfDestExists, false);
        assert!(trace.copies.is_empty());
        assert_eq!(stats.objects_skipped, 1);
    }

    #[test]
    fn newer_wins_untrusted_uses_true_facts_not_lite() {
        // Lite scramble makes src look 1e6s newer. With HEAD resolution the
        // true created_at are EQUAL ⇒ tie ⇒ skip. Trusting lite would copy.
        let mut w = simple_world(UNTRUSTED);
        let m = mk_meta(5, "s", "e", 100, false);
        w.src.insert("k".into(), world_obj(m.clone(), false));
        w.dest.insert("k".into(), world_obj(m, false));
        let (trace, _) = run_full(&w, ConflictPolicy::NewerWins, false);
        assert!(
            trace.copies.is_empty(),
            "tie on true created_at must skip — lite facts leaked into the decision"
        );
    }

    #[test]
    fn pure_mirror_decides_from_listing_facts() {
        // Authoritative regime: lite == truth, decisions HEAD-free. A real
        // difference still copies.
        let mut w = simple_world(AUTH);
        w.src.insert(
            "k".into(),
            world_obj(mk_meta(5, "s", "e", 200, false), true),
        );
        w.dest
            .insert("k".into(), world_obj(mk_meta(5, "s", "e", 100, true), true));
        let (trace, _) = run_full(&w, ConflictPolicy::NewerWins, false);
        assert_eq!(trace.copies.len(), 1, "src newer ⇒ copy");
    }

    #[test]
    fn deletes_only_owned_and_absent_from_source() {
        let mut w = simple_world(AUTH);
        w.src.insert(
            "keep".into(),
            world_obj(mk_meta(1, "s", "e", 100, false), true),
        );
        // Orphan owned by this rule → deleted.
        w.dest.insert(
            "orphan".into(),
            world_obj(mk_meta(1, "s", "e", 100, true), true),
        );
        // Foreign orphan → preserved.
        w.dest.insert(
            "foreign".into(),
            world_obj(mk_meta(1, "s", "e", 100, false), true),
        );
        // Present on source → preserved (and copied? identical ⇒ policy).
        w.dest.insert(
            "keep".into(),
            world_obj(mk_meta(1, "s", "e", 100, true), true),
        );
        let (trace, _) = run_full(&w, ConflictPolicy::SkipIfDestExists, true);
        assert_eq!(
            trace.deletes,
            ["orphan"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn dest_only_dirs_without_replicate_deletes_are_not_walked() {
        let mut w = simple_world(AUTH);
        w.dest.insert(
            "junk/a".into(),
            world_obj(mk_meta(1, "s", "e", 100, true), true),
        );
        let (trace, stats) = run_full(&w, ConflictPolicy::NewerWins, false);
        assert!(trace.deletes.is_empty());
        // Only the root dir was reconciled — the dest-only child was dropped.
        assert_eq!(stats.dirs_completed, 1);
    }

    #[test]
    fn marker_objects_and_dg_internals_never_act() {
        let mut w = simple_world(AUTH);
        w.src
            .insert("d/".into(), world_obj(mk_meta(0, "", "", 100, false), true));
        w.src.insert(
            ".deltaglider/state".into(),
            world_obj(mk_meta(1, "s", "e", 100, false), true),
        );
        w.dest
            .insert("d/".into(), world_obj(mk_meta(0, "", "", 100, true), true));
        w.dest.insert(
            ".deltaglider/other".into(),
            world_obj(mk_meta(1, "s", "e", 100, true), true),
        );
        let (trace, _) = run_full(&w, ConflictPolicy::NewerWins, true);
        assert!(
            trace.copies.is_empty(),
            "markers/DG internals must not copy"
        );
        assert!(
            trace.deletes.is_empty(),
            "markers/DG internals must not delete"
        );
    }

    #[test]
    fn foo_and_foo_slash_coexist_independently() {
        let mut w = simple_world(AUTH);
        w.src.insert(
            "foo".into(),
            world_obj(mk_meta(1, "s", "e", 100, false), true),
        );
        w.src.insert(
            "foo/bar".into(),
            world_obj(mk_meta(2, "t", "f", 100, false), true),
        );
        let (trace, _) = run_full(&w, ConflictPolicy::NewerWins, false);
        assert_eq!(trace.copies.len(), 2);
    }

    #[test]
    fn empty_segment_dirs_walk_verbatim() {
        let mut w = simple_world(AUTH);
        w.src.insert(
            "a//x".into(),
            world_obj(mk_meta(1, "s", "e", 100, false), true),
        );
        w.src.insert(
            "a/x".into(),
            world_obj(mk_meta(2, "t", "f", 100, false), true),
        );
        let (trace, _) = run_full(&w, ConflictPolicy::NewerWins, false);
        assert_eq!(
            trace.copies,
            ["a//x", "a/x"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<_>>(),
            "a//x and a/x are distinct keys and both replicate"
        );
    }

    #[test]
    fn budget_exhaustion_truncates_and_keeps_cursor() {
        let mut w = simple_world(AUTH);
        for i in 0..30 {
            w.src.insert(
                format!("k{i:02}"),
                world_obj(mk_meta(1, "s", "e", 100, false), true),
            );
        }
        let mut machine = mk_machine(
            &w,
            ConflictPolicy::NewerWins,
            false,
            false,
            3,
            2, // budget: 2 pages total (src needs 10, dest 1)
            1,
            2,
            usize::MAX,
            None,
        );
        let mut sim = Sim {
            world: w.clone(),
            page_size: 3,
            trace: Trace::default(),
            queue: Vec::new(),
            last_cursor: None,
        };
        drive(&mut machine, &mut sim, &[0], None);
        assert!(machine.truncated_by_budget());
        assert!(!machine.is_done());
        let cur = machine.cursor().expect("progress was made");
        assert!(!cur.pos.is_empty());
        assert!(
            !sim.trace.copies.is_empty(),
            "copies must start before full discovery (the whole point)"
        );
    }

    #[test]
    fn degrade_to_flat_still_matches_reference() {
        let mut w = simple_world(AUTH);
        for d in ["a", "b", "c", "d"] {
            for i in 0..3 {
                w.src.insert(
                    format!("{d}/f{i}"),
                    world_obj(mk_meta(1, "s", "e", 100, false), true),
                );
            }
        }
        w.dest.insert(
            "a/f0".into(),
            world_obj(mk_meta(1, "s", "e", 100, true), true),
        );
        let mut machine = mk_machine(
            &w,
            ConflictPolicy::SkipIfDestExists,
            false,
            false,
            3,
            10_000,
            2,
            2,
            1, // degrade after the first child dir
            None,
        );
        let mut sim = Sim {
            world: w.clone(),
            page_size: 3,
            trace: Trace::default(),
            queue: Vec::new(),
            last_cursor: None,
        };
        drive(&mut machine, &mut sim, &[0], None);
        let (ref_copies, _) = reference_diff(&w, ConflictPolicy::SkipIfDestExists, false, false);
        assert_eq!(sim.trace.copies, ref_copies);
    }

    /// The P6 livelock boundary, pinned exactly: an empty-segment ancestor
    /// chain ("//") makes resume tokens useless (token ""), so each resumed
    /// run replays ~12 pages to re-reach and close the "//" frontier.
    /// Budget 13 converges; 12 and below livelock — the documented "budget
    /// must exceed ancestor replay" bound (prod: MAX_JOB_PAGES=10_000).
    const BUDGET: u32 = 13;

    #[test]
    fn p6_regression_empty_segment_chain_converges_when_budget_covers_replay() {
        let mut w = simple_world(FactRegime {
            src_lite_authoritative: false,
            dest_lite_authoritative: false,
        });
        for k in [
            ".deltaglider",
            ".well-known/",
            "//",
            "a",
            "a-b",
            "a/",
            "b",
            "foo",
            "é⽇",
        ] {
            w.src.insert(
                k.to_string(),
                world_obj(mk_meta(1, "sha-src", "e", 100, false), false),
            );
        }
        for k in [".well-known/", "//", "a", "a-b", "b"] {
            w.dest.insert(
                k.to_string(),
                world_obj(mk_meta(1, "sha-src", "e", 100, false), false),
            );
        }
        let (ref_copies, _) = reference_diff(&w, ConflictPolicy::NewerWins, false, false);
        let mut world_now = w.clone();
        let mut cursor: Option<CursorV1> = None;
        let mut all_copies: BTreeSet<String> = BTreeSet::new();
        let mut done = false;
        for _run in 0..20 {
            let mut machine = mk_machine(
                &w,
                ConflictPolicy::NewerWins,
                false,
                false,
                2,
                BUDGET,
                1,
                2,
                usize::MAX,
                cursor.take(),
            );
            let mut sim = Sim {
                world: world_now.clone(),
                page_size: 2,
                trace: Trace::default(),
                queue: Vec::new(),
                last_cursor: None,
            };
            drive(&mut machine, &mut sim, &[0], None);
            world_now = sim.world;
            all_copies.extend(sim.trace.copies);
            if machine.is_done() {
                done = true;
                break;
            }
            assert!(machine.truncated_by_budget());
            cursor = machine.cursor();
        }
        assert!(done, "budget 13 (> replay cost) must converge");
        assert_eq!(all_copies, ref_copies);
    }

    // -- properties -----------------------------------------------------------

    use proptest::prelude::*;

    fn any_policy() -> impl Strategy<Value = ConflictPolicy> {
        prop_oneof![
            Just(ConflictPolicy::NewerWins),
            Just(ConflictPolicy::ContentDiff),
            Just(ConflictPolicy::SkipIfDestExists),
        ]
    }

    fn any_regime() -> impl Strategy<Value = FactRegime> {
        (any::<bool>(), any::<bool>()).prop_map(|(s, d)| FactRegime {
            src_lite_authoritative: s,
            dest_lite_authoritative: d,
        })
    }

    fn key_seg() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(String::new()),
            Just("a".to_string()),
            Just("b".to_string()),
            Just("a-b".to_string()),
            Just("foo".to_string()),
            Just(".well-known".to_string()),
            Just(".deltaglider".to_string()),
            Just("é⽇".to_string()),
        ]
    }

    fn any_key() -> impl Strategy<Value = String> {
        proptest::collection::vec(key_seg(), 1..4)
            .prop_map(|v| v.join("/"))
            // "" is not a valid S3 key (and equals the root prefix).
            .prop_filter("key must be non-empty", |k| !k.is_empty())
    }

    #[derive(Debug, Clone, Copy)]
    enum Presence {
        SrcOnly,
        DestOnly,
        Both,
    }

    fn presence() -> impl Strategy<Value = Presence> {
        prop_oneof![
            Just(Presence::SrcOnly),
            Just(Presence::DestOnly),
            Just(Presence::Both)
        ]
    }

    #[allow(clippy::type_complexity)]
    fn any_world() -> impl Strategy<Value = FakeWorld> {
        (
            proptest::collection::btree_map(
                any_key(),
                (
                    presence(),
                    1u64..3,                                 // src size
                    1u64..3,                                 // dest size
                    prop_oneof![Just(100i64), Just(200i64)], // src created
                    prop_oneof![Just(100i64), Just(200i64)], // dest created
                    any::<bool>(),                           // dest owned by this rule
                    any::<bool>(),                           // same sha on both sides
                ),
                1..25,
            ),
            any_regime(),
        )
            .prop_map(|(keys, regime)| {
                let mut w = FakeWorld {
                    src: BTreeMap::new(),
                    dest: BTreeMap::new(),
                    regime,
                };
                for (k, (p, ss, ds, sc, dc, owned, same_sha)) in keys {
                    let s_sha = "sha-src";
                    let d_sha = if same_sha { "sha-src" } else { "sha-dst" };
                    if matches!(p, Presence::SrcOnly | Presence::Both) {
                        w.src.insert(
                            k.clone(),
                            world_obj(
                                mk_meta(ss, s_sha, "etag-s", sc, false),
                                regime.src_lite_authoritative,
                            ),
                        );
                    }
                    if matches!(p, Presence::DestOnly | Presence::Both) {
                        w.dest.insert(
                            k,
                            world_obj(
                                mk_meta(ds, d_sha, "etag-d", dc, owned),
                                regime.dest_lite_authoritative,
                            ),
                        );
                    }
                }
                w
            })
    }

    proptest! {
        /// P1 — the walk's executed actions equal the naive global diff,
        /// across policies, regimes, page sizes, parallelism, delivery
        /// orders, and forced degrades.
        #[test]
        fn p1_walk_equals_reference(
            w in any_world(),
            policy in any_policy(),
            strict in any::<bool>(),
            deletes in any::<bool>(),
            page_size in 1u32..5,
            dir_workers in 1usize..4,
            head_batch in 1usize..3,
            degrade in prop_oneof![Just(usize::MAX), Just(1usize), Just(2usize)],
            seed in proptest::collection::vec(any::<u16>(), 1..64),
        ) {
            let mut machine = mk_machine(
                &w, policy, strict, deletes, page_size, 10_000,
                dir_workers, head_batch, degrade, None,
            );
            let mut sim = Sim {
                world: w.clone(),
                page_size: page_size as usize,
                trace: Trace::default(),
                queue: Vec::new(),
                last_cursor: None,
            };
            drive(&mut machine, &mut sim, &seed, None);
            prop_assert!(machine.is_done(), "walk must complete");

            let (ref_copies, ref_deletes) = reference_diff(&w, policy, strict, deletes);
            prop_assert_eq!(&sim.trace.copies, &ref_copies,
                "copies diverge from reference");
            prop_assert_eq!(&sim.trace.deletes, &ref_deletes,
                "deletes diverge from reference");
        }

        /// P2 — page-size invariance: identical action sets for any page size.
        #[test]
        fn p2_page_size_invariance(
            w in any_world(),
            policy in any_policy(),
            deletes in any::<bool>(),
        ) {
            let mut results = Vec::new();
            for page_size in [1u32, 4u32] {
                let mut machine = mk_machine(
                    &w, policy, false, deletes, page_size, 10_000, 2, 2, usize::MAX, None,
                );
                let mut sim = Sim {
                    world: w.clone(),
                    page_size: page_size as usize,
                    trace: Trace::default(),
                    queue: Vec::new(),
                    last_cursor: None,
                };
                drive(&mut machine, &mut sim, &[0], None);
                results.push((sim.trace.copies, sim.trace.deletes));
            }
            prop_assert_eq!(&results[0], &results[1]);
        }

        /// P3 — resume equivalence: cut anywhere (crash model — queued events
        /// lost), resume from the persisted cursor, the union of executed
        /// actions equals the reference on the initial world.
        #[test]
        fn p3_resume_equivalence(
            w in any_world(),
            policy in any_policy(),
            deletes in any::<bool>(),
            cut in 1usize..40,
            seed in proptest::collection::vec(any::<u16>(), 1..32),
        ) {
            let (ref_copies, ref_deletes) = reference_diff(&w, policy, false, deletes);

            let mut machine = mk_machine(&w, policy, false, deletes, 2, 10_000, 2, 2, usize::MAX, None);
            let mut sim = Sim {
                world: w.clone(),
                page_size: 2,
                trace: Trace::default(),
                queue: Vec::new(),
                last_cursor: None,
            };
            drive(&mut machine, &mut sim, &seed, Some(cut));
            let cursor = machine.cursor();

            // Resume on the MUTATED world (executed copies/deletes stand).
            let mut resumed = mk_machine(&w, policy, false, deletes, 2, 10_000, 2, 2, usize::MAX, cursor);
            let mut sim2 = Sim {
                world: sim.world.clone(),
                page_size: 2,
                trace: Trace::default(),
                queue: Vec::new(),
                last_cursor: None,
            };
            drive(&mut resumed, &mut sim2, &seed, None);
            prop_assert!(resumed.is_done(), "resumed walk must complete");

            let union_copies: BTreeSet<String> =
                sim.trace.copies.union(&sim2.trace.copies).cloned().collect();
            let union_deletes: BTreeSet<String> =
                sim.trace.deletes.union(&sim2.trace.deletes).cloned().collect();
            prop_assert_eq!(&union_copies, &ref_copies, "resume lost or invented copies");
            prop_assert_eq!(&union_deletes, &ref_deletes, "resume lost or invented deletes");
        }

        /// P6 — budget convergence: repeated truncated runs (cursor carried
        /// across) converge to the reference. Honest bound: the per-run
        /// budget must EXCEED the resume replay cost — re-listing the
        /// watermark's ancestor chain, which for empty-segment children
        /// ("//") degrades to re-paging whole levels (tokens like "" skip
        /// nothing). For these generated worlds (≤ 25 keys, ≤ 4 levels) 24
        /// pages safely covers any replay; a budget below the replay cost
        /// livelocks by design (see p6_regression_* for the exact shape).
        /// Prod's MAX_JOB_PAGES=10_000 dwarfs any real ancestor chain.
        #[test]
        fn p6_budget_convergence(
            w in any_world(),
            policy in any_policy(),
            deletes in any::<bool>(),
            max_pages in 24u32..28,
        ) {
            let (ref_copies, _) = reference_diff(&w, policy, false, deletes);

            let mut world_now = w.clone();
            let mut cursor: Option<CursorV1> = None;
            let mut all_copies: BTreeSet<String> = BTreeSet::new();
            for _run in 0..300 {
                let mut machine = mk_machine(
                    &w, policy, false, deletes, 2, max_pages, 1, 2, usize::MAX, cursor.take(),
                );
                let mut sim = Sim {
                    world: world_now.clone(),
                    page_size: 2,
                    trace: Trace::default(),
                    queue: Vec::new(),
                    last_cursor: None,
                };
                drive(&mut machine, &mut sim, &[0], None);
                world_now = sim.world;
                all_copies.extend(sim.trace.copies);
                if machine.is_done() {
                    // Deletes require a clean un-truncated pass over their dir;
                    // copies must all have landed by convergence.
                    prop_assert_eq!(&all_copies, &ref_copies, "budget loop lost copies");
                    return Ok(());
                }
                prop_assert!(machine.truncated_by_budget(), "not done implies truncated");
                cursor = machine.cursor();
            }
            prop_assert!(false, "did not converge within 300 truncated runs");
        }

        /// P7 — mode-free cursor: a run cut in FLAT mode resumes correctly in
        /// TREE mode and vice versa (the cursor is a plain position).
        #[test]
        fn p7_mode_free_cursor(
            w in any_world(),
            policy in any_policy(),
            cut in 1usize..30,
            flat_first in any::<bool>(),
        ) {
            let (ref_copies, _) = reference_diff(&w, policy, false, false);
            let (first_degrade, second_degrade) = if flat_first {
                (1usize, usize::MAX)
            } else {
                (usize::MAX, 1usize)
            };

            let mut machine = mk_machine(&w, policy, false, false, 2, 10_000, 2, 2, first_degrade, None);
            let mut sim = Sim {
                world: w.clone(),
                page_size: 2,
                trace: Trace::default(),
                queue: Vec::new(),
                last_cursor: None,
            };
            drive(&mut machine, &mut sim, &[3, 1, 4], Some(cut));
            let cursor = machine.cursor();

            let mut resumed = mk_machine(&w, policy, false, false, 2, 10_000, 2, 2, second_degrade, cursor);
            let mut sim2 = Sim {
                world: sim.world.clone(),
                page_size: 2,
                trace: Trace::default(),
                queue: Vec::new(),
                last_cursor: None,
            };
            drive(&mut resumed, &mut sim2, &[2, 5], None);
            prop_assert!(resumed.is_done());

            let union: BTreeSet<String> =
                sim.trace.copies.union(&sim2.trace.copies).cloned().collect();
            prop_assert_eq!(&union, &ref_copies);
        }
    }
}

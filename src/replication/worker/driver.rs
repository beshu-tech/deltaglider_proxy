// SPDX-License-Identifier: BUSL-1.1

//! The run's walk driver. The pure [`walk::WalkMachine`] owns every
//! decision; [`drive_walk`] only executes its commands against the engine
//! ([`WalkDriver`]), folds the results back, and persists the progress at a
//! bounded rate ([`Checkpointer`]). The run's terminal status is the pure
//! `settle_run` in the parent module.

use super::*;

/// What one run needs, fixed for the whole walk.
pub(super) struct RunScope<'a> {
    pub db: &'a Arc<Mutex<ConfigDb>>,
    pub engine: &'a Arc<DynEngine>,
    pub rule: &'a ReplicationRule,
    pub run_id: i64,
    pub source_prefix: &'a str,
    pub dest_prefix: &'a str,
    pub max_failures_retained: u32,
    pub object_timeout: Option<std::time::Duration>,
    pub object_skip_after_failures: u32,
    pub upload_concurrency: usize,
    pub transfers: usize,
    pub dir_workers: usize,
    pub page_size: u32,
    pub events_sink: &'a EventSink,
    pub ctrl: &'a RunControl,
    /// The walk started from a persisted cursor.
    pub was_resumed: bool,
}

/// Why and how the walk stopped; the settle reads these.
#[derive(Debug, Default)]
pub(super) struct RunFlags {
    pub had_any_error: bool,
    pub hit_fatal_error: bool,
    /// A dest-unusable abort (dead bucket / over quota). Such a dest won't
    /// recover in 60s, so the run backs off to the rule's normal cadence
    /// rather than re-firing every minute against a dead endpoint.
    pub dest_unusable: bool,
    /// A whole-window throttle abort (503 SlowDown): same backoff as
    /// `dest_unusable` — a backend shedding load needs breathing room.
    pub backend_throttled: bool,
    /// The operator paused the rule mid-run (DB `paused` flag, re-read at
    /// each control boundary). Not an error: the cursor is kept for resume,
    /// the run settles "stopped".
    pub stopped_paused: bool,
    /// Operator KILL: the run flipped to 'cancelling' mid-flight.
    pub killed: bool,
    /// The poison-cursor guard cleared a stale resume cursor: the settle
    /// must not write it back.
    pub poison_cleared_any: bool,
}

/// Pure: how many commands of each kind the machine may issue now.
pub(super) fn driver_caps(
    dir_workers: usize,
    transfers: usize,
    inflight: &InflightCounts,
) -> walk::DriverCaps {
    walk::DriverCaps {
        list: (dir_workers * 2).saturating_sub(inflight.lists),
        head: 2usize.saturating_sub(inflight.heads),
        copy: transfers.saturating_sub(inflight.copies),
        delete: 2usize.saturating_sub(inflight.deletes),
    }
}

/// Operations in flight, by kind.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct InflightCounts {
    pub lists: usize,
    pub heads: usize,
    pub copies: usize,
    pub deletes: usize,
}

/// The rolling abort-classification window, reset at each control
/// boundary: the dest-fatal and throttle-abort gates keep their
/// zero-successes semantics over it.
#[derive(Debug, Default)]
struct AbortWindow {
    attempted: i64,
    copied: i64,
    throttled: i64,
}

type Inflight = futures::stream::FuturesUnordered<
    std::pin::Pin<Box<dyn std::future::Future<Output = DriverDone> + Send>>,
>;

/// Executes the machine's commands and folds their results.
struct WalkDriver<'a> {
    s: &'a RunScope<'a>,
    inflight: Inflight,
    counts: InflightCounts,
    window: AbortWindow,
    first_list_done: bool,
    /// Poison-skips recorded by the copy path; planner skips come from the
    /// machine's stats. Both feed objects_skipped.
    poison_skipped: i64,
    /// Failure-ledger clears the next checkpoint writes.
    clear_failure_keys: Vec<String>,
}

impl<'a> WalkDriver<'a> {
    fn new(s: &'a RunScope<'a>) -> Self {
        Self {
            s,
            inflight: Inflight::new(),
            counts: InflightCounts::default(),
            window: AbortWindow::default(),
            first_list_done: false,
            poison_skipped: 0,
            clear_failure_keys: Vec::new(),
        }
    }

    /// Start one machine command.
    async fn dispatch(&mut self, machine: &mut walk::WalkMachine, cmd: walk::Cmd) {
        let s = self.s;
        let rule = s.rule;
        match cmd {
            walk::Cmd::List {
                req_id,
                side,
                rel_prefix,
                token,
                delimited,
            } => {
                self.counts.lists += 1;
                let engine = s.engine.clone();
                let (bucket, side_prefix) = side_target(rule, s.source_prefix, s.dest_prefix, side);
                let page_size = s.page_size;
                self.inflight.push(Box::pin(async move {
                    let abs_prefix = format!("{side_prefix}{rel_prefix}");
                    let abs_token = token.map(|t| format!("{side_prefix}{t}"));
                    if let Some(m) = engine.metrics() {
                        m.replication_list_calls_total.inc();
                    }
                    let result = engine
                        .list_objects(
                            &bucket,
                            &abs_prefix,
                            delimited.then_some("/"),
                            page_size,
                            abs_token.as_deref(),
                            false,
                        )
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|p| to_rel_listing(p, &side_prefix));
                    DriverDone::List { req_id, result }
                }));
            }
            walk::Cmd::Head {
                req_id,
                side,
                rel_keys,
            } => {
                self.counts.heads += 1;
                let engine = s.engine.clone();
                let (bucket, side_prefix) = side_target(rule, s.source_prefix, s.dest_prefix, side);
                self.inflight.push(Box::pin(async move {
                    let mut results = Vec::with_capacity(rel_keys.len());
                    for rel in rel_keys {
                        let abs = format!("{side_prefix}{rel}");
                        if let Some(m) = engine.metrics() {
                            m.replication_head_calls_total.inc();
                        }
                        let r = match engine.head(&bucket, &abs).await {
                            Ok(meta) => walk::HeadResult::Resolved(Box::new(meta)),
                            Err(e) => {
                                let s3e: crate::api::S3Error = e.into();
                                if matches!(s3e, crate::api::S3Error::NoSuchKey(_)) {
                                    walk::HeadResult::Gone
                                } else {
                                    walk::HeadResult::Unresolved
                                }
                            }
                        };
                        results.push((rel, r));
                    }
                    DriverDone::Heads { req_id, results }
                }));
            }
            walk::Cmd::Copy {
                item_id,
                rel_key,
                src_size,
            } => {
                // Poison-object guard, INLINE (main task — the copy future
                // itself is DB-free by contract): skip an object that failed
                // `object_skip_after_failures` consecutive runs.
                if s.object_skip_after_failures > 0 {
                    let skipped = {
                        let db = s.db.lock().await;
                        db.replication_object_skipped(
                            &rule.name,
                            &format!("{}{rel_key}", s.source_prefix),
                            s.object_skip_after_failures,
                            current_unix_seconds(),
                        )
                        .unwrap_or(false)
                    };
                    if skipped {
                        debug!(
                            "replication rule '{}' skipping poison object {:?}",
                            rule.name, rel_key
                        );
                        self.poison_skipped += 1;
                        machine.on_event(walk::Event::CopySettled { item_id, ok: true });
                        return;
                    }
                }
                self.counts.copies += 1;
                let engine = s.engine.clone();
                let rule_name = rule.name.clone();
                let src_bucket = rule.source.bucket.clone();
                let dst_bucket = rule.destination.bucket.clone();
                let events = s.events_sink.clone();
                let abs_src = format!("{}{rel_key}", s.source_prefix);
                let abs_dest = format!("{}{rel_key}", s.dest_prefix);
                let (object_timeout, upload_concurrency) = (s.object_timeout, s.upload_concurrency);
                // Register this dest write with the maintenance gate BEFORE
                // it starts (H22): a migrate/re-encrypt arming the gate
                // mid-run either sees this +1 and drains it, or the next
                // control check defers the run. The RAII guard rides in the
                // copy future — a kill dropping the future releases it.
                let write_guard = s
                    .ctrl
                    .maintenance_gate
                    .as_ref()
                    .map(|g| g.begin_write(&s.ctrl.dest_bucket));
                self.inflight.push(Box::pin(async move {
                    let _write_guard = write_guard;
                    // Guard increments objects_inflight (+peak) on entry and
                    // decrements on drop → proves the `transfers` concurrency.
                    let _obj_guard = engine.metrics().cloned().map(ObjectGuard::new);
                    // Live "currently copying" registration for the Jobs UI
                    // (RAII: a kill dropping this future unregisters it).
                    let _inflight_reg = InFlightGuard::new(&rule_name, &abs_src, src_size);
                    let res = copy_one_object(
                        &engine,
                        &rule_name,
                        &src_bucket,
                        &dst_bucket,
                        &abs_src,
                        &abs_dest,
                        object_timeout,
                        upload_concurrency,
                        &events,
                    )
                    .await;
                    DriverDone::Copy {
                        item_id,
                        rel_key,
                        res: Box::new(res),
                    }
                }));
            }
            walk::Cmd::Delete { item_id, rel_key } => {
                self.counts.deletes += 1;
                let engine = s.engine.clone();
                let src_bucket = rule.source.bucket.clone();
                let dst_bucket = rule.destination.bucket.clone();
                let abs_src = format!("{}{rel_key}", s.source_prefix);
                let abs_dest = format!("{}{rel_key}", s.dest_prefix);
                self.inflight.push(Box::pin(async move {
                    execute_delete(
                        &engine,
                        &src_bucket,
                        &dst_bucket,
                        &abs_src,
                        &abs_dest,
                        item_id,
                    )
                    .await
                }));
            }
        }
    }

    /// Fold one completed operation into the machine, the totals and the
    /// abort flags.
    async fn settle(
        &mut self,
        machine: &mut walk::WalkMachine,
        done: DriverDone,
        totals: &mut RunTotals,
        flags: &mut RunFlags,
    ) {
        let s = self.s;
        let rule = s.rule;
        match done {
            DriverDone::List { req_id, result } => {
                self.counts.lists -= 1;
                match result {
                    Ok(page) => {
                        self.first_list_done = true;
                        machine.on_event(walk::Event::ListPage { req_id, page });
                    }
                    Err(msg) => {
                        warn!("replication rule '{}' list failed: {}", rule.name, msg);
                        // Poison-token guard: a RESUMED walk whose FIRST
                        // listing fails most likely holds a stale cursor —
                        // clear it so the next tick starts fresh.
                        if s.was_resumed && !self.first_list_done {
                            flags.poison_cleared_any = true;
                            let db = s.db.lock().await;
                            let _ = db.replication_set_continuation_token(&rule.name, None);
                        }
                        ring_failure(s, "", "", &format!("list failed: {msg}")).await;
                        totals.errors += 1;
                        flags.hit_fatal_error = true;
                        machine.on_event(walk::Event::ListFailed { req_id });
                    }
                }
            }
            DriverDone::Heads { req_id, results } => {
                self.counts.heads -= 1;
                machine.on_event(walk::Event::HeadDone { req_id, results });
            }
            DriverDone::Copy {
                item_id,
                rel_key,
                res: r,
            } => {
                self.counts.copies -= 1;
                // Persist the failure ring + poison ledger INLINE (the copy
                // future is DB-free by contract).
                if let Some(err_msg) = &r.error_message {
                    let abs_src = format!("{}{rel_key}", s.source_prefix);
                    let abs_dest = format!("{}{rel_key}", s.dest_prefix);
                    if !r.throttled {
                        let db = s.db.lock().await;
                        let _ = db.replication_record_object_failure(
                            &rule.name,
                            &abs_src,
                            err_msg,
                            current_unix_seconds(),
                        );
                    }
                    ring_failure(s, &abs_src, &abs_dest, err_msg).await;
                }
                if let Some(k) = r.clear_failure_key.clone() {
                    self.clear_failure_keys.push(k);
                }
                totals.objects_copied += r.objects_copied;
                self.poison_skipped += r.objects_skipped;
                totals.bytes_copied += r.bytes_copied;
                totals.errors += r.errors;
                totals.delta_passthrough += r.delta_passthrough;
                totals.bytes_egress_saved += r.bytes_egress_saved;
                totals.reconstructed += r.reconstructed;
                self.window.attempted += 1;
                self.window.copied += r.objects_copied;
                if r.throttled {
                    self.window.throttled += 1;
                }
                if r.had_error {
                    flags.had_any_error = true;
                }
                machine.on_event(walk::Event::CopySettled {
                    item_id,
                    ok: !r.had_error,
                });
                // Destination unusable (bucket missing / over quota): abort
                // instead of retrying every remaining object. Gated on zero
                // successes this window — a stray token in one error must
                // not abort a healthy run.
                if r.dest_fatal && self.window.copied == 0 {
                    warn!(
                        "replication rule '{}' aborting run: destination unusable (bucket missing or over quota)",
                        rule.name
                    );
                    flags.hit_fatal_error = true;
                    flags.dest_unusable = true;
                    machine.drain(walk::DrainReason::Fatal);
                }
                // Backend shedding load (503 SlowDown / 429): abort with
                // backoff instead of grinding the key list.
                let w = &self.window;
                if page_is_throttle_aborted(w.copied, w.throttled, w.attempted) {
                    let throttled = w.throttled;
                    warn!(
                        "replication rule '{}' aborting run: backend throttled ({} SlowDown rejections this window)",
                        rule.name, throttled
                    );
                    let _ = log_failure(
                        s.db,
                        &rule.name,
                        s.run_id,
                        "",
                        "",
                        &format!(
                            "run aborted: backend throttled ({throttled} SlowDown rejections); \
                             resuming from cursor after backoff"
                        ),
                        s.max_failures_retained,
                    )
                    .await;
                    flags.hit_fatal_error = true;
                    flags.backend_throttled = true;
                    machine.drain(walk::DrainReason::Fatal);
                }
            }
            DriverDone::Delete {
                item_id,
                deleted,
                error,
            } => {
                self.counts.deletes -= 1;
                if deleted {
                    totals.objects_deleted += 1;
                }
                let errored = error.is_some();
                if let Some(msg) = error {
                    totals.errors += 1;
                    flags.had_any_error = true;
                    // Delete futures are DB-free; persist the failure here.
                    ring_failure(s, "", "", &msg).await;
                }
                machine.on_event(walk::Event::DeleteSettled {
                    item_id,
                    ok: !errored,
                });
            }
        }
    }
}

/// Record a failure in the run's ring; a ring write failure is logged.
async fn ring_failure(s: &RunScope<'_>, source_key: &str, dest_key: &str, msg: &str) {
    if let Err(le) = log_failure(
        s.db,
        &s.rule.name,
        s.run_id,
        source_key,
        dest_key,
        msg,
        s.max_failures_retained,
    )
    .await
    {
        warn!(
            "replication rule '{}': failure-ring write failed: {le}",
            s.rule.name
        );
    }
}

/// The fused, rate-limited checkpoint: failure-ledger clears + cursor + run
/// progress + event flush under ONE db.lock (the old per-page contract).
pub(super) struct Checkpointer {
    events_since_flush: usize,
    last_persisted_pos: Option<String>,
    last_dirs_reported: u64,
}

impl Checkpointer {
    fn new(machine: &walk::WalkMachine) -> Self {
        Self {
            events_since_flush: 0,
            last_persisted_pos: machine.cursor().map(|c| c.pos),
            last_dirs_reported: 0,
        }
    }

    /// Pure: flush now? The cursor advances on almost every settle, and a
    /// checkpoint per event would hammer the DB mutex and starve every
    /// other DB user: flush on 16 events, or on durable progress once at
    /// least 4 events have accumulated.
    pub(super) fn due(events_since_flush: usize, cursor_moved: bool) -> bool {
        events_since_flush >= 16 || (cursor_moved && events_since_flush >= 4)
    }

    /// Sync the machine's stats into the totals (and the dirs metric).
    fn sync_stats(
        &mut self,
        s: &RunScope<'_>,
        machine: &walk::WalkMachine,
        totals: &mut RunTotals,
        poison_skipped: i64,
    ) {
        let stats = machine.stats().clone();
        totals.objects_scanned = stats.objects_scanned as i64;
        totals.objects_skipped = stats.objects_skipped as i64 + poison_skipped;
        if let Some(m) = s.engine.metrics() {
            let delta = stats.dirs_completed.saturating_sub(self.last_dirs_reported);
            if delta > 0 {
                m.replication_dirs_completed_total.inc_by(delta);
                self.last_dirs_reported = stats.dirs_completed;
            }
        }
    }

    /// Count one event; flush when [`Checkpointer::due`].
    async fn after_event(
        &mut self,
        driver: &mut WalkDriver<'_>,
        machine: &mut walk::WalkMachine,
        totals: &mut RunTotals,
        flags: &mut RunFlags,
    ) {
        let s = driver.s;
        self.events_since_flush += 1;
        let cur_pos = machine.cursor().map(|c| c.pos);
        if !Self::due(self.events_since_flush, cur_pos != self.last_persisted_pos) {
            return;
        }
        self.sync_stats(s, machine, totals, driver.poison_skipped);
        let (dirs_done, dirs_pending) = machine.progress();
        // Re-apply the source prefix so the UI shows the absolute path.
        let scanning = machine
            .scanning()
            .map(|rel| format!("{}{rel}", s.source_prefix));
        WALK_PROGRESS.lock().insert(
            s.rule.name.clone(),
            WalkProgress {
                dirs_completed: dirs_done,
                dirs_pending,
                scanning,
            },
        );
        let cursor_json = machine.cursor().map(|c| c.to_json());
        let db = s.db.lock().await;
        for k in driver.clear_failure_keys.drain(..) {
            let _ = db.replication_clear_object_failure(&s.rule.name, &k);
        }
        let persist = db
            .replication_set_continuation_token(&s.rule.name, cursor_json.as_deref())
            .and_then(|_| db.replication_update_run_progress(s.run_id, *totals));
        let mut drained: Vec<NewEvent> = std::mem::take(&mut *s.events_sink.lock());
        flush_page_events_locked(&db, &s.rule.name, &mut drained);
        drop(db);
        if let Err(e) = persist {
            warn!(
                "replication rule '{}': cursor/progress persist failed: {e}",
                s.rule.name
            );
            totals.errors += 1;
            flags.hit_fatal_error = true;
            machine.drain(walk::DrainReason::Fatal);
        }
        self.last_persisted_pos = cur_pos;
        self.events_since_flush = 0;
    }

    /// After the walk: the final stats sync (the loop may break between
    /// checkpoints) and the failure-ledger clears of the run's tail.
    async fn finish(
        &mut self,
        driver: &mut WalkDriver<'_>,
        machine: &walk::WalkMachine,
        totals: &mut RunTotals,
    ) {
        let s = driver.s;
        self.sync_stats(s, machine, totals, driver.poison_skipped);
        if !driver.clear_failure_keys.is_empty() {
            let db = s.db.lock().await;
            for k in driver.clear_failure_keys.drain(..) {
                let _ = db.replication_clear_object_failure(&s.rule.name, &k);
            }
        }
    }
}

/// Run the walk to its end (done, truncated, drained or failed).
pub(super) async fn drive_walk(
    s: &RunScope<'_>,
    machine: &mut walk::WalkMachine,
    totals: &mut RunTotals,
    flags: &mut RunFlags,
) {
    let mut driver = WalkDriver::new(s);
    let mut checkpoint = Checkpointer::new(machine);
    // Control-check cadence counter: independent of the checkpoint counter
    // (which resets on every flush) — kill/pause/lease checks must keep
    // firing through long copy/delete drains.
    let mut events_since_check = 0usize;
    let mut need_check = true;
    // Kill poll: a LOCK-FREE interval tick; the DB check runs INLINE in the
    // main task after the tick fires. Never keep a future that locks the
    // config-DB parked across select iterations — tokio's fair semaphore
    // GRANTS the lock to a queued waiter on release, and a waiter that is
    // only polled inside select! deadlocks the task the moment the task also
    // awaits the same mutex directly.
    let mut kill_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    kill_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    'walk: loop {
        // Control boundary (kill / pause / lease / maintenance), page-boundary
        // cadence: every completed listing re-arms it.
        if need_check {
            need_check = false;
            driver.window = AbortWindow::default();
            match s.ctrl.check(true).await {
                Err(e) => {
                    warn!(
                        "replication rule '{}': control check failed: {e}",
                        s.rule.name
                    );
                    totals.errors += 1;
                    flags.hit_fatal_error = true;
                    machine.drain(walk::DrainReason::Fatal);
                    break 'walk;
                }
                Ok(ControlVerdict::Continue) => {}
                Ok(ControlVerdict::Killed) => {
                    info!("replication rule '{}' killed mid-walk", s.rule.name);
                    flags.killed = true;
                    machine.drain(walk::DrainReason::Killed);
                    break 'walk;
                }
                Ok(ControlVerdict::Paused) => {
                    info!(
                        "replication rule '{}' paused mid-walk (cursor preserved for resume)",
                        s.rule.name
                    );
                    flags.stopped_paused = true;
                    machine.drain(walk::DrainReason::Paused);
                    break 'walk;
                }
                Ok(ControlVerdict::LeaseLost) => {
                    totals.errors += 1;
                    flags.hit_fatal_error = true;
                    machine.drain(walk::DrainReason::LeaseLost);
                    break 'walk;
                }
            }
        }

        let cmds = machine.poll(driver_caps(s.dir_workers, s.transfers, &driver.counts));
        let dispatched = !cmds.is_empty();
        for cmd in cmds {
            driver.dispatch(machine, cmd).await;
        }

        if driver.inflight.is_empty() {
            if !dispatched {
                break 'walk; // done, truncated, drained, or failed
            }
            continue;
        }

        // Await one completion, racing the operator kill so a wedged object
        // aborts NOW (dropping `inflight` cancels every transfer).
        let done = tokio::select! {
            biased;
            _ = kill_tick.tick() => {
                // Inline DB check — the ONLY pending db.lock of this task.
                let killed_now = {
                    let g = s.db.lock().await;
                    g.replication_run_cancel_requested(s.run_id).unwrap_or(false)
                };
                if killed_now {
                    flags.killed = true;
                    machine.drain(walk::DrainReason::Killed);
                    break 'walk;
                }
                continue 'walk;
            }
            done = driver.inflight.next() => match done {
                Some(d) => d,
                None => break 'walk,
            },
        };
        if matches!(done, DriverDone::List { .. }) {
            need_check = true;
        }
        driver.settle(machine, done, totals, flags).await;

        // Control cadence: re-arm every 32 events (≈ the old page boundary)
        // so kill/pause/lease-loss land promptly mid-drain.
        events_since_check += 1;
        if events_since_check >= 32 {
            need_check = true;
            events_since_check = 0;
        }
        checkpoint
            .after_event(&mut driver, machine, totals, flags)
            .await;
    }

    checkpoint.finish(&mut driver, machine, totals).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_leave_room_for_what_is_in_flight() {
        let none = InflightCounts::default();
        let c = driver_caps(4, 8, &none);
        assert_eq!((c.list, c.head, c.copy, c.delete), (8, 2, 8, 2));
        let busy = InflightCounts {
            lists: 9,
            heads: 1,
            copies: 3,
            deletes: 2,
        };
        let c = driver_caps(4, 8, &busy);
        assert_eq!((c.list, c.head, c.copy, c.delete), (0, 1, 5, 0));
    }

    #[test]
    fn checkpoint_cadence() {
        assert!(!Checkpointer::due(15, false));
        assert!(Checkpointer::due(16, false));
        assert!(!Checkpointer::due(3, true));
        assert!(Checkpointer::due(4, true));
    }
}

// SPDX-License-Identifier: BUSL-1.1

//! THE paged phase loop of the maintenance kinds: reencrypt and backfill
//! (`counting`, `objects`) and migrate (`copy`, `verify`, the mirror prune).
//!
//! Each kind used to re-carry the same loop around [`Pager`]: cancel check,
//! list, poison-token restart, per-object shutdown check, save, heartbeat,
//! page-budget refusal. Each copy forgot one of them. [`paged_phase`] owns
//! the loop; a phase supplies only a [`PageStep`] (which bucket it lists,
//! what it does per object, how it saves progress).

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::Mutex;
use tracing::warn;

use crate::config_db::ConfigDb;
use crate::job_loop::{list_retry_delay, ListFailure, Pager, LIST_ATTEMPTS};

use super::store::MaintenanceJob;
use super::worker::{check_cancel, heartbeat, stop_if_shutting_down, Holder, PhaseStop};

/// Objects of one page that a phase works on at the same time. One object
/// costs several round trips (HEADs, a GET, a PUT), so one at a time made
/// a 100,000-object job last days. The cursor stays at the page boundary:
/// a page is saved only after every object of it ended.
pub(crate) const PAGE_CONCURRENCY: usize = 4;

/// The job a phase runs for.
#[derive(Clone, Copy)]
pub(crate) struct JobCtx<'a> {
    pub db: &'a Arc<Mutex<ConfigDb>>,
    pub holder: Holder<'a>,
    pub job: &'a MaintenanceJob,
}

/// One listing page, reduced to what the loop needs.
pub(crate) struct KeyPage {
    pub keys: Vec<String>,
    pub is_truncated: bool,
    pub next_token: Option<String>,
}

impl From<crate::deltaglider::ListObjectsPage> for KeyPage {
    fn from(p: crate::deltaglider::ListObjectsPage) -> Self {
        Self {
            keys: p.objects.into_iter().map(|(k, _)| k).collect(),
            is_truncated: p.is_truncated,
            next_token: p.next_continuation_token,
        }
    }
}

/// A page listing that failed: what the failure means for the loop, and
/// the text that becomes the job's error when the listing does not recover.
#[derive(Debug)]
pub(crate) struct ListError {
    pub failure: ListFailure,
    pub message: String,
}

impl ListError {
    /// `"{context}: {e}"`, classed by the engine error.
    pub(crate) fn engine(context: &str, e: &crate::deltaglider::EngineError) -> Self {
        Self {
            failure: ListFailure::of(e),
            message: format!("{context}: {e}"),
        }
    }
}

/// The phase-specific part of a paged phase.
pub(crate) trait PageStep {
    /// What the work on one object ended in.
    type Done;
    /// Before each page's listing: re-read what a config apply can change.
    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        Ok(())
    }
    /// List one page.
    async fn list(&mut self, token: Option<&str>) -> Result<KeyPage, ListError>;
    /// The resume token was refused: the phase restarts at page 0.
    fn on_restart(&mut self) {}
    /// Save the progress with `token` as the resume cursor.
    async fn save(&mut self, token: Option<&str>) -> Result<(), PhaseStop>;
    /// Every [`PhaseSpec::cancel_every`] objects, before the cancel check.
    /// `page_token` is the cursor of the page in progress.
    async fn mid_page(&mut self, _page_token: Option<&str>) -> Result<(), PhaseStop> {
        Ok(())
    }
    /// The work on one user object (keys that end in `/` are skipped). Up
    /// to [`PhaseSpec::concurrency`] objects run at once, so it only reads
    /// the step: [`PageStep::settle`] records the outcome.
    async fn object(&self, key: &str) -> Self::Done;
    /// `true` when `done` stops the phase: no further object of the page
    /// starts.
    fn stops(_done: &Self::Done) -> bool {
        false
    }
    /// Record one object's outcome (counters, failure rows), in key order.
    /// An `Err` stops the phase.
    async fn settle(&mut self, key: &str, done: Self::Done) -> Result<(), PhaseStop>;
    /// After the last object of a page, before its save.
    async fn end_page(&mut self) -> Result<(), PhaseStop> {
        Ok(())
    }
}

/// How one paged phase runs.
pub(crate) struct PhaseSpec<'a> {
    /// Names the phase in the logs.
    pub phase: &'a str,
    /// The saved cursor: `Some` only when the job was persisted IN this
    /// phase (a fresh transition from the phase before starts at page 0).
    pub resume: Option<String>,
    /// Also check for a cancel every this many objects of a page.
    pub cancel_every: Option<usize>,
    /// The job's error when the page budget runs out with pages pending:
    /// falling through would report the phase done with its tail unseen.
    pub budget_exhausted: &'a str,
    /// Objects of a page in flight at once ([`PAGE_CONCURRENCY`]).
    pub concurrency: usize,
}

/// Run one paged phase to its end. Per page: cancel check, backend-health
/// check, [`PageStep::begin_page`], list, then the objects (a chunk of
/// [`PhaseSpec::cancel_every`] at a time, [`PhaseSpec::concurrency`] in
/// flight), then save and heartbeat.
///
/// A listing that fails transiently (a timeout, a 5xx, a throttle) is sent
/// again with the SAME token, [`LIST_ATTEMPTS`] times with a growing pause.
/// Only a refused token on the first page of a resumed phase restarts the
/// phase from page 0 (and saves the clean cursor, so a crash mid-retry does
/// not re-poison it): engine tokens are keys, and a timeout says nothing
/// about them. Restarting on any error threw away hours of progress.
pub(crate) async fn paged_phase<S: PageStep>(
    ctx: JobCtx<'_>,
    spec: PhaseSpec<'_>,
    step: &mut S,
) -> Result<(), PhaseStop> {
    let mut pager = Pager::resuming(spec.resume);
    'pages: while pager.begin_page().is_some() {
        check_cancel(ctx.db, ctx.job.id).await?;
        ctx.holder.check_backend()?;
        step.begin_page().await?;
        let mut attempt = 0u32;
        let page = loop {
            let e = match step.list(pager.token()).await {
                Ok(p) => break p,
                Err(e) => e,
            };
            attempt += 1;
            if let Some(pause) = list_retry_delay(e.failure, attempt) {
                warn!(
                    "maintenance: job #{} {} listing failed ({}); listing the same page \
                     again ({attempt}/{LIST_ATTEMPTS})",
                    ctx.job.id, spec.phase, e.message
                );
                tokio::time::sleep(pause).await;
                continue;
            }
            if pager.restarts_on(e.failure) {
                warn!(
                    "maintenance: job #{} {} resume token rejected ({}); restarting phase fresh",
                    ctx.job.id, spec.phase, e.message
                );
                pager.restart_fresh();
                step.on_restart();
                step.save(None).await?;
                continue 'pages;
            }
            return Err(match e.failure {
                ListFailure::Transient => PhaseStop::FailedTransient(e.message),
                _ => PhaseStop::Failed(e.message),
            });
        };
        let keys: Vec<&str> = page
            .keys
            .iter()
            .map(String::as_str)
            .filter(|k| !k.ends_with('/'))
            .collect();
        let chunk = spec.cancel_every.unwrap_or(usize::MAX).max(1);
        for (i, part) in keys.chunks(chunk).enumerate() {
            if i > 0 {
                step.mid_page(pager.token()).await?;
                check_cancel(ctx.db, ctx.job.id).await?;
            }
            run_objects(step, part, spec.concurrency).await?;
        }
        step.end_page().await?;
        let more = pager.advance(page.is_truncated, page.next_token);
        step.save(pager.token()).await?;
        heartbeat(ctx.db, ctx.job.id, ctx.holder).await?;
        if !more {
            break;
        }
    }
    if pager.truncated_by_page_budget() {
        return Err(spec.budget_exhausted.into());
    }
    Ok(())
}

/// The objects `keys`, `concurrency` at a time, then their outcomes
/// settled in key order. An object starts only while the process does not
/// shut down and no earlier outcome stopped the phase (a graceful stop
/// waits for the objects in flight only).
async fn run_objects<S: PageStep>(
    step: &mut S,
    keys: &[&str],
    concurrency: usize,
) -> Result<(), PhaseStop> {
    let mut outcomes: Vec<S::Done> = Vec::with_capacity(keys.len());
    {
        let step: &S = step;
        let mut next = keys.iter();
        let mut in_flight = futures::stream::FuturesOrdered::new();
        let mut stopped = false;
        loop {
            while !stopped
                && in_flight.len() < concurrency.max(1)
                && !crate::shutdown::is_shutting_down()
            {
                let Some(key) = next.next() else { break };
                in_flight.push_back(step.object(key));
            }
            let Some(done) = in_flight.next().await else {
                break;
            };
            stopped |= S::stops(&done);
            outcomes.push(done);
        }
    }
    let started = outcomes.len();
    for (key, done) in keys.iter().zip(outcomes) {
        step.settle(key, done).await?;
    }
    if started < keys.len() {
        stop_if_shutting_down()?;
        return Err(PhaseStop::Failed(format!(
            "the phase stopped before '{}'",
            keys[started]
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintenance::store::current_unix_seconds;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A phase over canned pages that records what the loop asked of it.
    #[derive(Default)]
    struct Fake {
        /// token → page (`None` = the first page).
        pages: std::collections::HashMap<Option<String>, KeyPage>,
        /// token → (failure, how many more listings of it fail).
        fail: std::sync::Mutex<std::collections::HashMap<String, (ListFailure, u32)>>,
        log: std::sync::Mutex<Vec<String>>,
        /// Each object takes this long.
        object_ms: u64,
        /// The object whose outcome stops the phase.
        stop_at: Option<&'static str>,
        in_flight: AtomicUsize,
        peak: AtomicUsize,
    }

    impl Fake {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
        fn push(&self, line: String) {
            self.log.lock().unwrap().push(line);
        }
        fn fail_listing(&self, token: &str, failure: ListFailure, times: u32) {
            self.fail
                .lock()
                .unwrap()
                .insert(token.to_string(), (failure, times));
        }
    }

    fn page(keys: &[&str], next: Option<&str>) -> KeyPage {
        KeyPage {
            keys: keys.iter().map(|k| k.to_string()).collect(),
            is_truncated: next.is_some(),
            next_token: next.map(str::to_string),
        }
    }

    impl PageStep for Fake {
        /// `false`: this object's outcome stops the phase.
        type Done = bool;

        async fn list(&mut self, token: Option<&str>) -> Result<KeyPage, ListError> {
            self.push(format!("list {token:?}"));
            if let Some((failure, left)) = self
                .fail
                .lock()
                .unwrap()
                .get_mut(token.unwrap_or(""))
                .filter(|(_, left)| *left > 0)
            {
                *left -= 1;
                return Err(ListError {
                    failure: *failure,
                    message: format!("{failure:?}"),
                });
            }
            let p = &self.pages[&token.map(str::to_string)];
            Ok(KeyPage {
                keys: p.keys.clone(),
                is_truncated: p.is_truncated,
                next_token: p.next_token.clone(),
            })
        }
        fn on_restart(&mut self) {
            self.push("restart".into());
        }
        async fn save(&mut self, token: Option<&str>) -> Result<(), PhaseStop> {
            self.push(format!("save {token:?}"));
            Ok(())
        }
        async fn mid_page(&mut self, token: Option<&str>) -> Result<(), PhaseStop> {
            self.push(format!("mid {token:?}"));
            Ok(())
        }
        async fn object(&self, key: &str) -> bool {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            if self.object_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.object_ms)).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.push(format!("ran {key}"));
            self.stop_at != Some(key)
        }
        fn stops(done: &bool) -> bool {
            !*done
        }
        async fn settle(&mut self, key: &str, done: bool) -> Result<(), PhaseStop> {
            self.push(format!("obj {key}"));
            if done {
                Ok(())
            } else {
                Err(PhaseStop::Failed(format!("{key} failed")))
            }
        }
    }

    /// The loop's log without the `ran` lines (their order across
    /// concurrent objects is not fixed).
    fn settled(f: &Fake) -> Vec<String> {
        f.log()
            .into_iter()
            .filter(|l| !l.starts_with("ran "))
            .collect()
    }

    fn claimed(db: &ConfigDb) -> MaintenanceJob {
        let id = db
            .maintenance_create_job("reencrypt", "b", "objects", None, "admin", 1)
            .unwrap()
            .unwrap();
        db.maintenance_claim_next_job("inst", current_unix_seconds(), 60)
            .unwrap()
            .unwrap();
        db.maintenance_job_by_id(id).unwrap().unwrap()
    }

    fn spec(resume: Option<&str>, cancel_every: Option<usize>) -> PhaseSpec<'static> {
        PhaseSpec {
            phase: "objects",
            resume: resume.map(str::to_string),
            cancel_every,
            budget_exhausted: "budget",
            concurrency: PAGE_CONCURRENCY,
        }
    }

    fn two_pages() -> Fake {
        let mut f = Fake::default();
        f.pages.insert(None, page(&["a", "dir/", "b"], Some("b")));
        f.pages.insert(Some("b".into()), page(&["c"], None));
        f
    }

    struct Db {
        db: Arc<Mutex<ConfigDb>>,
        job: MaintenanceJob,
    }

    fn db() -> Db {
        let db = ConfigDb::in_memory("t").unwrap();
        let job = claimed(&db);
        Db {
            db: Arc::new(Mutex::new(db)),
            job,
        }
    }

    impl Db {
        fn ctx(&self) -> JobCtx<'_> {
            JobCtx {
                db: &self.db,
                holder: Holder::test("inst"),
                job: &self.job,
            }
        }
    }

    #[tokio::test]
    async fn walks_every_page_skips_dirs_and_saves_each_cursor() {
        let d = db();
        let mut f = two_pages();
        paged_phase(d.ctx(), spec(None, None), &mut f)
            .await
            .unwrap();
        assert_eq!(
            settled(&f),
            [
                "list None",
                "obj a",
                "obj b",
                "save Some(\"b\")",
                "list Some(\"b\")",
                "obj c",
                "save None",
            ]
        );
    }

    /// A refused token (an InvalidArgument / not-found class error) on the
    /// first page of a resumed phase restarts it at page 0.
    #[tokio::test]
    async fn a_refused_resume_token_restarts_at_page_zero_and_saves_first() {
        let d = db();
        let mut f = two_pages();
        f.fail_listing("stale", ListFailure::Token, u32::MAX);
        paged_phase(d.ctx(), spec(Some("stale"), None), &mut f)
            .await
            .unwrap();
        assert_eq!(
            &settled(&f)[..4],
            ["list Some(\"stale\")", "restart", "save None", "list None"]
        );
        // A later page with a refused listing is a job error, not a restart.
        let mut f = two_pages();
        f.fail_listing("b", ListFailure::Token, u32::MAX);
        let err = paged_phase(d.ctx(), spec(None, None), &mut f)
            .await
            .unwrap_err();
        assert_eq!(err, PhaseStop::Failed("Token".into()));
    }

    /// jobs-14: a timeout on the first page of a resumed phase is not the
    /// token's fault (engine tokens are keys). The page is listed again with
    /// the same token; the phase used to restart from page 0 and redo every
    /// page before the cursor.
    #[tokio::test]
    async fn a_transient_listing_error_is_retried_with_the_same_token() {
        let d = db();
        let mut f = two_pages();
        f.fail_listing("b", ListFailure::Transient, 1);
        paged_phase(d.ctx(), spec(Some("b"), None), &mut f)
            .await
            .unwrap();
        assert_eq!(
            settled(&f),
            ["list Some(\"b\")", "list Some(\"b\")", "obj c", "save None"],
            "the same page is listed again, nothing restarts"
        );
        // A later page is retried the same way, and so is a fatal error not.
        let mut f = two_pages();
        f.fail_listing("b", ListFailure::Transient, 2);
        paged_phase(d.ctx(), spec(None, None), &mut f)
            .await
            .unwrap();
        assert_eq!(
            settled(&f)
                .iter()
                .filter(|l| *l == "list Some(\"b\")")
                .count(),
            3
        );
        let mut f = two_pages();
        f.fail_listing("b", ListFailure::Fatal, 1);
        let err = paged_phase(d.ctx(), spec(Some("b"), None), &mut f)
            .await
            .unwrap_err();
        assert_eq!(err, PhaseStop::Failed("Fatal".into()));
        assert!(!f.log().contains(&"restart".to_string()), "{:?}", f.log());
    }

    /// A listing that keeps failing transiently fails the phase as
    /// TRANSIENT (a migrate then keeps its staged copies), after
    /// `LIST_ATTEMPTS` listings of the same page.
    #[tokio::test]
    async fn a_listing_that_keeps_failing_transiently_fails_as_transient() {
        let d = db();
        let mut f = two_pages();
        f.fail_listing("", ListFailure::Transient, u32::MAX);
        let err = paged_phase(d.ctx(), spec(None, None), &mut f)
            .await
            .unwrap_err();
        assert_eq!(err, PhaseStop::FailedTransient("Transient".into()));
        assert_eq!(f.log().len(), LIST_ATTEMPTS as usize, "{:?}", f.log());
    }

    /// jobs-17: the objects of a page run `concurrency` at a time; their
    /// outcomes are still settled in key order, and the page is saved after
    /// all of them.
    #[tokio::test]
    async fn a_page_runs_its_objects_four_at_a_time() {
        let d = db();
        let mut f = Fake {
            object_ms: 30,
            ..Fake::default()
        };
        let keys = ["k0", "k1", "k2", "k3", "k4", "k5", "k6", "k7"];
        f.pages.insert(None, page(&keys, None));
        paged_phase(d.ctx(), spec(None, None), &mut f)
            .await
            .unwrap();
        assert_eq!(f.peak.load(Ordering::SeqCst), PAGE_CONCURRENCY);
        let mut want: Vec<String> = vec!["list None".into()];
        want.extend(keys.iter().map(|k| format!("obj {k}")));
        want.push("save None".into());
        assert_eq!(settled(&f), want);
    }

    /// An outcome that stops the phase starts no further object, and the
    /// objects before it are settled.
    #[tokio::test]
    async fn an_object_that_stops_the_phase_starts_no_more_objects() {
        let d = db();
        let mut f = Fake {
            stop_at: Some("k1"),
            ..Fake::default()
        };
        let keys: Vec<String> = (0..12).map(|i| format!("k{i}")).collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        f.pages.insert(None, page(&keys, None));
        let mut s = spec(None, None);
        s.concurrency = 1;
        let err = paged_phase(d.ctx(), s, &mut f).await.unwrap_err();
        assert_eq!(err, PhaseStop::Failed("k1 failed".into()));
        assert_eq!(settled(&f), ["list None", "obj k0", "obj k1"]);
        assert_eq!(
            f.log().iter().filter(|l| l.starts_with("ran ")).count(),
            2,
            "{:?}",
            f.log()
        );
    }

    #[tokio::test]
    async fn mid_page_runs_before_the_cancel_check_and_a_cancel_stops_the_phase() {
        let d = db();
        let mut f = Fake::default();
        f.pages.insert(None, page(&["a", "b", "c"], None));
        paged_phase(d.ctx(), spec(None, Some(2)), &mut f)
            .await
            .unwrap();
        assert_eq!(
            settled(&f),
            [
                "list None",
                "obj a",
                "obj b",
                "mid None",
                "obj c",
                "save None"
            ]
        );

        d.db.lock()
            .await
            .maintenance_request_cancel(d.job.id)
            .unwrap();
        let mut f = two_pages();
        assert_eq!(
            paged_phase(d.ctx(), spec(None, None), &mut f).await,
            Err(PhaseStop::Cancelled)
        );
        assert!(f.log().is_empty(), "nothing listed after a cancel");
    }

    #[tokio::test]
    async fn a_refused_renewal_stops_the_phase_after_the_save() {
        let d = db();
        let ctx = JobCtx {
            holder: Holder::test("someone-else"),
            ..d.ctx()
        };
        let mut f = two_pages();
        assert_eq!(
            paged_phase(ctx, spec(None, None), &mut f).await,
            Err(PhaseStop::LeaseLost)
        );
        assert_eq!(f.log().last().unwrap(), "save Some(\"b\")");
    }

    #[tokio::test]
    async fn the_page_budget_is_a_job_error() {
        let d = db();
        // A token loop: every page points at itself.
        let mut f = Fake::default();
        f.pages.insert(None, page(&["a"], Some("t")));
        f.pages.insert(Some("t".into()), page(&["a"], Some("t")));
        assert_eq!(
            paged_phase(d.ctx(), spec(None, None), &mut f).await,
            Err(PhaseStop::Failed("budget".into()))
        );
    }
}

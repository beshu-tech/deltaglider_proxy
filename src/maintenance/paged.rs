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

use tokio::sync::Mutex;
use tracing::warn;

use crate::config_db::ConfigDb;
use crate::job_loop::Pager;

use super::store::MaintenanceJob;
use super::worker::{check_cancel, heartbeat, stop_if_shutting_down, Holder, PhaseStop};

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

/// The phase-specific part of a paged phase.
pub(crate) trait PageStep {
    /// Before each page's listing: re-read what a config apply can change.
    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        Ok(())
    }
    /// List one page. The error text names the phase (it becomes the job's
    /// error when the listing does not recover).
    async fn list(&mut self, token: Option<&str>) -> Result<KeyPage, String>;
    /// The resume token was refused: the phase restarts at page 0.
    fn on_restart(&mut self) {}
    /// Save the progress with `token` as the resume cursor.
    async fn save(&mut self, token: Option<&str>) -> Result<(), PhaseStop>;
    /// Every [`PhaseSpec::cancel_every`] objects, before the cancel check.
    /// `page_token` is the cursor of the page in progress.
    async fn mid_page(&mut self, _page_token: Option<&str>) -> Result<(), PhaseStop> {
        Ok(())
    }
    /// One user object (keys that end in `/` are skipped).
    async fn object(&mut self, key: &str) -> Result<(), PhaseStop>;
    /// After the last object of a page, before its save.
    fn end_page(&mut self) {}
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
}

/// Run one paged phase to its end. Per page: cancel check,
/// [`PageStep::begin_page`], list (a refused resume token restarts the
/// phase from page 0 and saves the clean cursor, so a crash mid-retry does
/// not re-poison it), then per object a shutdown check and
/// [`PageStep::object`], then save and heartbeat.
pub(crate) async fn paged_phase<S: PageStep>(
    ctx: JobCtx<'_>,
    spec: PhaseSpec<'_>,
    step: &mut S,
) -> Result<(), PhaseStop> {
    let mut pager = Pager::resuming(spec.resume);
    while pager.begin_page().is_some() {
        check_cancel(ctx.db, ctx.job.id).await?;
        step.begin_page().await?;
        let page = match step.list(pager.token()).await {
            Ok(p) => p,
            Err(e) if pager.poisoned_resume_token() => {
                warn!(
                    "maintenance: job #{} {} resume token rejected ({e}); restarting phase fresh",
                    ctx.job.id, spec.phase
                );
                pager.restart_fresh();
                step.on_restart();
                step.save(None).await?;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        for (i, key) in page.keys.iter().filter(|k| !k.ends_with('/')).enumerate() {
            if let Some(every) = spec.cancel_every {
                if i > 0 && i % every == 0 {
                    step.mid_page(pager.token()).await?;
                    check_cancel(ctx.db, ctx.job.id).await?;
                }
            }
            stop_if_shutting_down()?;
            step.object(key).await?;
        }
        step.end_page();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintenance::store::current_unix_seconds;

    /// A phase over canned pages that records what the loop asked of it.
    #[derive(Default)]
    struct Fake {
        /// token → page (`None` = the first page).
        pages: std::collections::HashMap<Option<String>, KeyPage>,
        /// Tokens whose listing fails.
        refuse: Vec<String>,
        log: Vec<String>,
    }

    fn page(keys: &[&str], next: Option<&str>) -> KeyPage {
        KeyPage {
            keys: keys.iter().map(|k| k.to_string()).collect(),
            is_truncated: next.is_some(),
            next_token: next.map(str::to_string),
        }
    }

    impl PageStep for Fake {
        async fn list(&mut self, token: Option<&str>) -> Result<KeyPage, String> {
            self.log.push(format!("list {token:?}"));
            if token.is_some_and(|t| self.refuse.iter().any(|r| r == t)) {
                return Err("bad token".into());
            }
            let p = &self.pages[&token.map(str::to_string)];
            Ok(KeyPage {
                keys: p.keys.clone(),
                is_truncated: p.is_truncated,
                next_token: p.next_token.clone(),
            })
        }
        fn on_restart(&mut self) {
            self.log.push("restart".into());
        }
        async fn save(&mut self, token: Option<&str>) -> Result<(), PhaseStop> {
            self.log.push(format!("save {token:?}"));
            Ok(())
        }
        async fn mid_page(&mut self, token: Option<&str>) -> Result<(), PhaseStop> {
            self.log.push(format!("mid {token:?}"));
            Ok(())
        }
        async fn object(&mut self, key: &str) -> Result<(), PhaseStop> {
            self.log.push(format!("obj {key}"));
            Ok(())
        }
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
        }
    }

    fn two_pages() -> Fake {
        let mut f = Fake::default();
        f.pages.insert(None, page(&["a", "dir/", "b"], Some("b")));
        f.pages.insert(Some("b".into()), page(&["c"], None));
        f
    }

    #[tokio::test]
    async fn walks_every_page_skips_dirs_and_saves_each_cursor() {
        let db = ConfigDb::in_memory("t").unwrap();
        let job = claimed(&db);
        let db = Arc::new(Mutex::new(db));
        let ctx = JobCtx {
            db: &db,
            holder: Holder::test("inst"),
            job: &job,
        };
        let mut f = two_pages();
        paged_phase(ctx, spec(None, None), &mut f).await.unwrap();
        assert_eq!(
            f.log,
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

    #[tokio::test]
    async fn a_refused_resume_token_restarts_at_page_zero_and_saves_first() {
        let db = ConfigDb::in_memory("t").unwrap();
        let job = claimed(&db);
        let db = Arc::new(Mutex::new(db));
        let ctx = JobCtx {
            db: &db,
            holder: Holder::test("inst"),
            job: &job,
        };
        let mut f = two_pages();
        f.refuse.push("stale".into());
        paged_phase(ctx, spec(Some("stale"), None), &mut f)
            .await
            .unwrap();
        assert_eq!(
            &f.log[..4],
            ["list Some(\"stale\")", "restart", "save None", "list None"]
        );
        // A later page with a failing listing is a job error, not a restart.
        let mut f = two_pages();
        f.refuse.push("b".into());
        let err = paged_phase(ctx, spec(None, None), &mut f)
            .await
            .unwrap_err();
        assert_eq!(err, PhaseStop::Failed("bad token".into()));
    }

    #[tokio::test]
    async fn mid_page_runs_before_the_cancel_check_and_a_cancel_stops_the_phase() {
        let db = ConfigDb::in_memory("t").unwrap();
        let job = claimed(&db);
        let db = Arc::new(Mutex::new(db));
        let ctx = JobCtx {
            db: &db,
            holder: Holder::test("inst"),
            job: &job,
        };
        let mut f = Fake::default();
        f.pages.insert(None, page(&["a", "b", "c"], None));
        paged_phase(ctx, spec(None, Some(2)), &mut f).await.unwrap();
        assert_eq!(
            f.log,
            [
                "list None",
                "obj a",
                "obj b",
                "mid None",
                "obj c",
                "save None"
            ]
        );

        db.lock().await.maintenance_request_cancel(job.id).unwrap();
        let mut f = two_pages();
        assert_eq!(
            paged_phase(ctx, spec(None, None), &mut f).await,
            Err(PhaseStop::Cancelled)
        );
        assert!(f.log.is_empty(), "nothing listed after a cancel");
    }

    #[tokio::test]
    async fn a_refused_renewal_stops_the_phase_after_the_save() {
        let db = ConfigDb::in_memory("t").unwrap();
        let job = claimed(&db);
        let db = Arc::new(Mutex::new(db));
        let ctx = JobCtx {
            db: &db,
            holder: Holder::test("someone-else"),
            job: &job,
        };
        let mut f = two_pages();
        assert_eq!(
            paged_phase(ctx, spec(None, None), &mut f).await,
            Err(PhaseStop::LeaseLost)
        );
        assert_eq!(f.log.last().unwrap(), "save Some(\"b\")");
    }

    #[tokio::test]
    async fn the_page_budget_is_a_job_error() {
        let db = ConfigDb::in_memory("t").unwrap();
        let job = claimed(&db);
        let db = Arc::new(Mutex::new(db));
        let ctx = JobCtx {
            db: &db,
            holder: Holder::test("inst"),
            job: &job,
        };
        // A token loop: every page points at itself.
        let mut f = Fake::default();
        f.pages.insert(None, page(&["a"], Some("t")));
        f.pages.insert(Some("t".into()), page(&["a"], Some("t")));
        assert_eq!(
            paged_phase(ctx, spec(None, None), &mut f).await,
            Err(PhaseStop::Failed("budget".into()))
        );
    }
}

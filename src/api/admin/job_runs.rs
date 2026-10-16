// SPDX-License-Identifier: BUSL-1.1

//! `GET /_/api/admin/jobs/runs`: every run on one list, newest first — the
//! runs of replication and lifecycle rules, and the one-off jobs (a one-off
//! is a run without a rule). The Jobs page reads it; the rule pages read
//! `GET /jobs` for the rule definitions.
//!
//! One cursor pages across the three run tables. The page order is start
//! time DESC, then source rank DESC, then id DESC, so two runs that started
//! in the same second keep one order on every page.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use super::extract::AdminQuery;
use super::jobs::{
    lifecycle_progress, lifecycle_scope, maintenance_job_view, normalize_status,
    replication_row_progress, replication_scope, JobView,
};
use super::path_guard::AdminBucket;
use super::{AdminError, AdminState};
use crate::config_db::job_store::{RunPageFilter, RunStatusFilter};

/// Page size: default and ceiling.
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

/// The three run sources, by their rank in the page order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Maintenance = 1,
    Lifecycle = 2,
    Replication = 3,
}

/// Where a page ends: the sort key of its last run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Cursor {
    time: i64,
    rank: i64,
    id: i64,
}

impl Cursor {
    fn parse(s: &str) -> Option<Self> {
        let mut it = s.splitn(3, '.');
        let c = Cursor {
            time: it.next()?.parse().ok()?,
            rank: it.next()?.parse().ok()?,
            id: it.next()?.parse().ok()?,
        };
        Some(c)
    }

    fn format(self) -> String {
        format!("{}.{}.{}", self.time, self.rank, self.id)
    }

    /// The keyset bound of one source: its rows that sort after the cursor
    /// are `(time, id) < bound`. A source ranked below the cursor's keeps
    /// every row of the cursor's second; one ranked above keeps none.
    fn bound_for(self, source: Source) -> (i64, i64) {
        let rank = source as i64;
        let id = match rank.cmp(&self.rank) {
            std::cmp::Ordering::Less => i64::MAX,
            std::cmp::Ordering::Equal => self.id,
            std::cmp::Ordering::Greater => i64::MIN,
        };
        (self.time, id)
    }
}

/// How a run started: `schedule`, `run-now`, `file-browser` (a bulk delete),
/// `admin` (a one-off started in the admin UI or API).
pub(super) fn run_origin(kind: &str, triggered_by: &str) -> String {
    match (kind, triggered_by) {
        ("replication" | "lifecycle", "scheduler") => "schedule".into(),
        ("replication" | "lifecycle", t) => t.to_string(),
        (crate::maintenance::delete::KIND, _) => "file-browser".into(),
        _ => "admin".into(),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RunsQuery {
    /// `all` (default), `active`, `failed`.
    #[serde(default)]
    pub status: Option<String>,
    /// `replication`, `lifecycle`, or a one-off kind (`delete`, `reencrypt`,
    /// `migrate`, `backfill-metadata`).
    #[serde(default)]
    pub kind: Option<String>,
    /// `<kind>:<rule name>`: the runs of one rule.
    #[serde(default)]
    pub rule: Option<String>,
    /// Runs that read or write this bucket.
    #[serde(default)]
    pub bucket: Option<AdminBucket>,
    /// Also list finished rule runs that changed nothing.
    #[serde(default)]
    pub noop: bool,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One run. The job fields are the row shape of `GET /jobs`, with `id` the
/// run's own id (`<job id>#<run id>` for a rule run, the job id for a
/// one-off).
#[derive(Debug, Serialize)]
pub struct RunView {
    #[serde(flatten)]
    pub job: JobView,
    /// The job that ran: `replication:<rule>`, `lifecycle:<rule>`,
    /// `maintenance:<n>`.
    pub job_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<i64>,
    pub origin: String,
    /// Who started a one-off (`admin`, or the IAM user name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RunsPage {
    pub runs: Vec<RunView>,
    /// Pass back as `cursor` for the next page; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// The kinds of one-off job.
const ONEOFF_KINDS: &[&str] = &[
    crate::maintenance::delete::KIND,
    "reencrypt",
    "migrate",
    "backfill-metadata",
];

/// GET /_/api/admin/jobs/runs
pub async fn list_runs(
    State(state): State<Arc<AdminState>>,
    AdminQuery(q): AdminQuery<RunsQuery>,
) -> Result<Json<RunsPage>, AdminError> {
    let status = match q.status.as_deref() {
        None | Some("all") => RunStatusFilter::All,
        Some("active") => RunStatusFilter::Active,
        Some("failed") => RunStatusFilter::Failed,
        Some(other) => {
            return Err(AdminError::invalid(format!(
                "unknown status '{other}' (all, active, failed)"
            )))
        }
    };
    let cursor = match q.cursor.as_deref() {
        None => None,
        Some(c) => Some(Cursor::parse(c).ok_or_else(|| AdminError::invalid("invalid cursor"))?),
    };
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let bucket = match q.bucket {
        Some(b) => Some(b.admit(&state)?.into_string()),
        None => None,
    };

    let cfg = state.config.read().await;
    let repl_rules = cfg.replication.rules.clone();
    let lc_rules = cfg.lifecycle.rules.clone();
    drop(cfg);

    // Which sources take part, and the rules (or buckets) each keeps.
    let mut repl_keys: Option<Vec<String>> = None;
    let mut lc_keys: Option<Vec<String>> = None;
    let mut maint_keys: Option<Vec<String>> = None;
    let mut maint_kinds: Option<Vec<String>> = None;
    let (mut repl_on, mut lc_on, mut maint_on) = (true, true, true);
    match q.kind.as_deref() {
        None => {}
        Some("replication") => (lc_on, maint_on) = (false, false),
        Some("lifecycle") => (repl_on, maint_on) = (false, false),
        Some(k) if ONEOFF_KINDS.contains(&k) => {
            (repl_on, lc_on) = (false, false);
            maint_kinds = Some(vec![k.to_string()]);
        }
        Some(other) => return Err(AdminError::invalid(format!("unknown kind '{other}'"))),
    }
    if let Some(rule) = q.rule.as_deref() {
        match rule.split_once(':') {
            Some(("replication", name)) => {
                (lc_on, maint_on) = (false, false);
                repl_keys = Some(vec![name.to_string()]);
            }
            Some(("lifecycle", name)) => {
                (repl_on, maint_on) = (false, false);
                lc_keys = Some(vec![name.to_string()]);
            }
            _ => {
                return Err(AdminError::invalid(
                    "rule must be replication:<name> or lifecycle:<name>",
                ))
            }
        }
    }
    if let Some(bucket) = bucket.as_deref() {
        let touches = |scope: &super::jobs::JobScope| {
            scope.bucket == bucket || scope.target.as_deref() == Some(bucket)
        };
        let repl: Vec<String> = repl_rules
            .iter()
            .filter(|r| touches(&replication_scope(r)))
            .map(|r| r.name.clone())
            .collect();
        let lc: Vec<String> = lc_rules
            .iter()
            .filter(|r| touches(&lifecycle_scope(r)))
            .map(|r| r.name.clone())
            .collect();
        repl_keys = Some(intersect(repl_keys, repl));
        lc_keys = Some(intersect(lc_keys, lc));
        maint_keys = Some(vec![bucket.to_string()]);
    }
    let filter = |source: Source, keys: Option<Vec<String>>, kinds: Option<Vec<String>>| {
        RunPageFilter {
            keys,
            kinds,
            status,
            hide_noop: !q.noop,
            before: cursor.map(|c| c.bound_for(source)),
            // One more than the page: says whether another page follows.
            limit: limit + 1,
        }
    };
    let skip = |keys: &Option<Vec<String>>| keys.as_ref().is_some_and(|k| k.is_empty());

    let db = state
        .jobs_db
        .as_ref()
        .ok_or_else(AdminError::no_jobs_db)?
        .lock()
        .await;
    let mut runs: Vec<(Cursor, RunView)> = Vec::new();

    if repl_on && !skip(&repl_keys) {
        for r in db.replication_runs_page(&filter(Source::Replication, repl_keys, None))? {
            let Some(rule) = repl_rules.iter().find(|x| x.name == r.rule_name) else {
                continue;
            };
            let job_id = format!("replication:{}", r.rule_name);
            let key = Cursor {
                time: r.started_at,
                rank: Source::Replication as i64,
                id: r.id,
            };
            let view = RunView {
                origin: run_origin("replication", &r.triggered_by),
                started_by: None,
                run_id: Some(r.id),
                job: rule_run_job(
                    format!("{job_id}#{}", r.id),
                    "replication",
                    &r.rule_name,
                    replication_scope(rule),
                    "continuous",
                    &r.status,
                    replication_row_progress(Some(&r)),
                    r.started_at,
                    r.finished_at,
                    r.errors,
                ),
                job_id,
            };
            runs.push((key, view));
        }
    }
    if lc_on && !skip(&lc_keys) {
        for r in db.lifecycle_runs_page(&filter(Source::Lifecycle, lc_keys, None))? {
            let Some(rule) = lc_rules.iter().find(|x| x.name == r.rule_name) else {
                continue;
            };
            // The run in progress in this process: its live counters (the
            // history row gets its totals only at the end).
            let live = crate::lifecycle::live::running(&r.rule_name)
                .filter(|l| l.run_id == Some(r.id) && r.finished_at.is_none())
                .map(|l| l.counters());
            let progress = match live {
                Some(c) => lifecycle_progress(
                    c.objects_affected,
                    c.objects_skipped,
                    c.errors,
                    c.bytes_affected,
                    c.objects_deleted,
                ),
                None => lifecycle_progress(
                    r.objects_affected,
                    r.objects_skipped,
                    r.errors,
                    r.bytes_affected,
                    r.split.map_or(0, |s| s.objects_deleted),
                ),
            };
            let job_id = format!("lifecycle:{}", r.rule_name);
            let key = Cursor {
                time: r.started_at,
                rank: Source::Lifecycle as i64,
                id: r.id,
            };
            let mut job = rule_run_job(
                format!("{job_id}#{}", r.id),
                "lifecycle",
                &r.rule_name,
                lifecycle_scope(rule),
                "scheduled",
                &r.status,
                progress,
                r.started_at,
                r.finished_at,
                live.map_or(r.errors, |c| c.errors),
            );
            job.detail = serde_json::json!({ "action": rule.action.kind() });
            runs.push((
                key,
                RunView {
                    origin: run_origin("lifecycle", &r.triggered_by),
                    started_by: None,
                    run_id: Some(r.id),
                    job,
                    job_id,
                },
            ));
        }
    }
    if maint_on && !skip(&maint_keys) {
        for j in db.maintenance_runs_page(&filter(Source::Maintenance, maint_keys, maint_kinds))? {
            let key = Cursor {
                time: j.started_at.unwrap_or(j.created_at),
                rank: Source::Maintenance as i64,
                id: j.id,
            };
            let job = maintenance_job_view(&j);
            runs.push((
                key,
                RunView {
                    job_id: job.id.clone(),
                    run_id: None,
                    origin: run_origin(&j.kind, j.triggered_by.as_deref().unwrap_or("")),
                    started_by: j.triggered_by.clone(),
                    job,
                },
            ));
        }
    }
    drop(db);

    let (runs, next) = take_page(runs, limit);
    Ok(Json(RunsPage {
        runs,
        next_cursor: next.map(Cursor::format),
    }))
}

/// PURE: the newest `limit` runs of the merged sources, and the cursor of
/// the next page when more runs follow.
fn take_page(mut runs: Vec<(Cursor, RunView)>, limit: usize) -> (Vec<RunView>, Option<Cursor>) {
    runs.sort_by_key(|r| std::cmp::Reverse(r.0));
    let more = runs.len() > limit;
    runs.truncate(limit);
    let next = if more { runs.last().map(|r| r.0) } else { None };
    (runs.into_iter().map(|r| r.1).collect(), next)
}

/// Keep only the names in both lists (no first list: the second).
fn intersect(a: Option<Vec<String>>, b: Vec<String>) -> Vec<String> {
    match a {
        None => b,
        Some(a) => a.into_iter().filter(|n| b.contains(n)).collect(),
    }
}

/// The job fields of one rule run.
#[allow(clippy::too_many_arguments)]
fn rule_run_job(
    id: String,
    kind: &str,
    rule: &str,
    scope: super::jobs::JobScope,
    trigger: &'static str,
    status: &str,
    progress: super::jobs::JobProgress,
    started_at: i64,
    finished_at: Option<i64>,
    errors: i64,
) -> JobView {
    JobView {
        id,
        kind: kind.into(),
        name: rule.into(),
        scope,
        trigger,
        enabled: None,
        paused: None,
        status: normalize_status(status),
        status_raw: status.into(),
        phase: None,
        percent: None,
        progress,
        lifetime: None,
        last_run_at: None,
        next_due_at: None,
        created_at: None,
        started_at: Some(started_at),
        finished_at,
        last_error: None,
        failed_total: Some(errors),
        detail: serde_json::json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_and_refuses_junk() {
        let c = Cursor {
            time: 1_700_000_000,
            rank: 2,
            id: 41,
        };
        assert_eq!(Cursor::parse(&c.format()), Some(c));
        for bad in ["", "1.2", "a.b.c", "1.2.x"] {
            assert_eq!(Cursor::parse(bad), None, "{bad}");
        }
    }

    /// The bound of each source keeps exactly the rows that sort after the
    /// cursor in (time, rank, id) DESC order.
    #[test]
    fn the_bound_of_each_source_matches_the_page_order() {
        let c = Cursor {
            time: 100,
            rank: Source::Lifecycle as i64,
            id: 5,
        };
        let after = |source: Source, time: i64, id: i64| {
            let (t, i) = c.bound_for(source);
            time < t || (time == t && id < i)
        };
        for source in [Source::Maintenance, Source::Lifecycle, Source::Replication] {
            for time in [99, 100, 101] {
                for id in [1, 5, 9] {
                    let row = Cursor {
                        time,
                        rank: source as i64,
                        id,
                    };
                    assert_eq!(after(source, time, id), row < c, "{source:?} {time} {id}");
                }
            }
        }
    }

    #[test]
    fn origins() {
        assert_eq!(run_origin("replication", "scheduler"), "schedule");
        assert_eq!(run_origin("lifecycle", "run-now"), "run-now");
        assert_eq!(run_origin("delete", "dana"), "file-browser");
        assert_eq!(run_origin("reencrypt", "admin"), "admin");
    }

    #[test]
    fn intersect_keeps_the_common_names() {
        assert_eq!(intersect(None, vec!["a".into()]), vec!["a".to_string()]);
        assert_eq!(
            intersect(Some(vec!["a".into(), "b".into()]), vec!["b".into()]),
            vec!["b".to_string()]
        );
    }
}

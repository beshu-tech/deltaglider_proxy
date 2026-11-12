// SPDX-License-Identifier: BUSL-1.1

//! Maintenance job kind **`delete`**: delete a browser selection (some
//! folders and some objects of one bucket) on the server.
//!
//! The browser only starts the job: closing the tab does not stop it, and a
//! restart resumes it from its cursor. Delete jobs run in their own worker
//! lane, several at a time ([`DELETE_PARALLEL`]), and they do not hold
//! their bucket: they arm no write gate, and other jobs (and other deletes)
//! can run on the same bucket. A job that arms the gate (a re-encryption)
//! makes a running delete wait.
//!
//! Each page lists up to 1,000 keys (`PAGE_KEYS`) of the current folder through
//! the S3 LIST's walk (`crate::iam::listing`), as the principal that
//! started the job sees them, and deletes them through the bulk delete of
//! the admin API (per-key permission check, events, reference reclaim).
//! The principal is checked again on every page: a user who is disabled
//! or loses the permission stops the job.

use std::net::IpAddr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::store::MaintenanceJob;
use super::worker::{
    check_cancel, heartbeat, persist, record_failure, stop_if_shutting_down, Counters, Holder,
    PhaseStop,
};
use crate::api::admin::objects::{run_bulk_delete, BulkActor};
use crate::api::admin::AdminObjectPath;
use crate::api::handlers::AppState;
use crate::config_db::JobsDb;

/// Job-kind string as stored in `maintenance_jobs.kind`.
pub const KIND: &str = "delete";
/// The one phase of a delete job.
pub const PHASE: &str = "deleting";
/// Delete jobs that run at the same time (the delete lane of the worker).
pub const DELETE_PARALLEL: usize = 4;
/// Keys listed and deleted per page.
const PAGE_KEYS: usize = 1000;
/// The most objects one job may name directly; folders have no limit.
pub const MAX_KEYS: usize = 10_000;
/// The most folders one job may name.
pub const MAX_PREFIXES: usize = 1_000;

/// Who started the job; the job acts with this principal's permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Principal {
    /// An admin session: no per-key check.
    Admin,
    /// Open access: no per-key check while IAM stays off.
    Open,
    /// An IAM user, and the address its session came from (`aws:SourceIp`).
    /// `user_id` is the user's database id: it stays the same when the keys
    /// rotate, so the owner still cancels the job and the job still acts as
    /// the user (A2, #171). A row from an older release has none; it is
    /// judged by `access_key_id`.
    User {
        access_key_id: String,
        client_ip: Option<IpAddr>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_id: Option<i64>,
    },
}

/// The job row's params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteParams {
    /// Folders, each with its trailing `/`; everything under each goes.
    pub prefixes: Vec<String>,
    /// Objects selected directly.
    pub keys: Vec<String>,
    pub principal: Principal,
    /// The browser's count when the job started: the total until the job
    /// has counted more.
    #[serde(default)]
    pub expected: u64,
}

/// Parse the job row's params JSON.
pub fn parse_params(json: &str) -> Result<DeleteParams, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid delete params: {e}"))
}

/// Pure: may `session` cancel a job that `job_params` (the job row's params
/// JSON) says was started by its stored principal? An admin or open session
/// cancels any job. A user cancels the jobs of the same IAM user: the same
/// database id, which a key rotation keeps; a row without an id (an older
/// release) is judged by the access key id. The decision never uses a
/// display name: a user may be NAMED `admin`, and an OIDC name claim is free
/// text. An unreadable row denies a user.
pub fn may_cancel(session: &Principal, job_params: Option<&str>) -> bool {
    match session {
        Principal::Admin | Principal::Open => true,
        Principal::User {
            access_key_id,
            user_id,
            ..
        } => matches!(
            job_params.and_then(|p| parse_params(p).ok()).map(|p| p.principal),
            Some(Principal::User { access_key_id: owner_key, user_id: owner_id, .. })
                if match (user_id, owner_id) {
                    (Some(session_id), Some(owner_id)) => *session_id == owner_id,
                    _ => owner_key == *access_key_id,
                }
        ),
    }
}

/// Where the job is: stage 0 is the directly selected objects (`skip` of
/// them done), stage `n` is folder `n - 1` (`after` its last handled key).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub stage: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skip: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl Cursor {
    /// The saved cursor; a missing or unreadable one starts at the top.
    pub fn parse(token: Option<&str>) -> Self {
        token
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default()
    }

    fn token(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// What a delete job needs besides the engine: the IAM state (to act as
/// its principal) and the savings cache (to drop the totals it changes).
#[derive(Clone)]
pub struct DeleteDeps {
    pub iam: crate::iam::SharedIamState,
    pub savings: Arc<crate::api::admin::SavingsCache>,
}

/// One page of keys and the cursor after it; `None` when every stage is done.
async fn next_page(
    state: &Arc<AppState>,
    budget: usize,
    bucket: &str,
    params: &DeleteParams,
    actor: &BulkActor,
    cursor: &Cursor,
) -> Result<Option<(Vec<String>, Cursor)>, PhaseStop> {
    if cursor.stage == 0 {
        let rest = params.keys.get(cursor.skip..).unwrap_or_default();
        let page: Vec<String> = rest.iter().take(PAGE_KEYS).cloned().collect();
        let next = if page.len() < rest.len() {
            Cursor {
                skip: cursor.skip + page.len(),
                ..Cursor::default()
            }
        } else {
            Cursor {
                stage: 1,
                ..Cursor::default()
            }
        };
        return Ok(Some((page, next)));
    }
    let Some(prefix) = params.prefixes.get(cursor.stage - 1) else {
        return Ok(None);
    };
    let engine = state.engine.load();
    let scope = actor.list_scope();
    let (keys, truncated) = crate::iam::listing::list_keys_for_caller(
        &**engine,
        bucket,
        prefix,
        scope.as_ref(),
        cursor.after.as_deref(),
        PAGE_KEYS,
        budget,
    )
    .await
    .map_err(|e| match e {
        crate::iam::listing::ListingError::ScanBudgetSpent => PhaseStop::Failed(format!(
            "the folder {prefix} holds more objects that {} cannot see than one listing \
             may read (advanced.filtered_list_max_engine_pages)",
            actor.label()
        )),
        crate::iam::listing::ListingError::Engine(e) => {
            PhaseStop::FailedTransient(format!("listing {bucket}/{prefix} failed: {e}"))
        }
    })?;
    let next = match keys.last() {
        Some(last) if truncated => Cursor {
            stage: cursor.stage,
            after: Some(last.clone()),
            skip: 0,
        },
        _ => Cursor {
            stage: cursor.stage + 1,
            ..Cursor::default()
        },
    };
    Ok(Some((keys, next)))
}

/// Pure: the total to show. The browser's count, until the job has handled
/// more than that (objects written since the count).
pub fn shown_total(expected: u64, done: i64, failed: i64) -> i64 {
    (expected as i64).max(done + failed)
}

/// Run a delete job to its end, page by page. A cancel, a shutdown, a lost
/// lease or a gated bucket stops it between pages; the cursor saved after
/// each page is where it resumes.
pub(crate) async fn execute_delete_phases(
    config: &crate::config::SharedConfig,
    db: &Arc<Mutex<JobsDb>>,
    state: &Arc<AppState>,
    holder: Holder<'_>,
    job: &MaintenanceJob,
) -> Result<(), PhaseStop> {
    let deps = holder
        .deletes
        .ok_or("delete jobs need the worker's delete lane")?;
    let params = parse_params(job.params.as_deref().unwrap_or("null"))?;
    let bucket = job.bucket.as_str();
    let mut cursor = Cursor::parse(job.continuation_token.as_deref());
    let mut c = Counters::of(job);
    // The total shows from the start, not after the first page.
    persist_page(db, job, &params, &mut c, &cursor).await;
    loop {
        check_cancel(db, job.id).await?;
        stop_if_shutting_down()?;
        holder.check_backend()?;
        // A re-encryption or a migration rewrites the bucket: wait for it.
        if let Some(why) = state.maintenance_gate.write_blocked(bucket) {
            return Err(PhaseStop::Deferred(why));
        }
        let iam = deps.iam.load();
        let view = state
            .coordination
            .credential_view(&iam)
            .map_err(|_| PhaseStop::Deferred("the credential store is not ready".into()))?;
        let actor = BulkActor::for_principal(view, &params.principal)?;
        let budget = config.read().await.filtered_list_max_engine_pages;
        let Some((keys, next)) = next_page(state, budget, bucket, &params, &actor, &cursor).await?
        else {
            return Ok(());
        };
        let paths: Vec<AdminObjectPath> = keys
            .into_iter()
            .filter_map(|k| AdminObjectPath::try_from(k).ok())
            .collect();
        if !paths.is_empty() {
            let answer = run_bulk_delete(state, bucket, &paths, &actor, &|_| {}).await;
            deps.savings
                .invalidate_keys(bucket, paths.iter().map(|k| k.as_str()))
                .await;
            // A job that armed the gate mid-page skipped the rest of it: do
            // the page again once the gate opens (deleted keys are gone,
            // so the listing starts at the skipped ones).
            if let Some(why) = state.maintenance_gate.write_blocked(bucket) {
                c.done += answer.deleted as i64;
                persist_page(db, job, &params, &mut c, &cursor).await;
                return Err(PhaseStop::Deferred(why));
            }
            c.done += answer.deleted as i64;
            c.failed += answer.failed as i64;
            for f in &answer.failures {
                record_failure(db, job.id, &f.key, &f.error).await?;
            }
        }
        cursor = next;
        persist_page(db, job, &params, &mut c, &cursor).await;
        heartbeat(db, job.id, holder).await?;
    }
}

async fn persist_page(
    db: &Arc<Mutex<JobsDb>>,
    job: &MaintenanceJob,
    params: &DeleteParams,
    c: &mut Counters,
    cursor: &Cursor,
) {
    c.total = Some(shown_total(params.expected, c.done, c.failed));
    persist(db, job, PHASE, c, Some(&cursor.token())).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(ak: &str) -> Principal {
        Principal::User {
            access_key_id: ak.into(),
            client_ip: None,
            user_id: None,
        }
    }

    fn user_with_id(ak: &str, id: i64) -> Principal {
        Principal::User {
            access_key_id: ak.into(),
            client_ip: None,
            user_id: Some(id),
        }
    }

    fn row(principal: Principal) -> String {
        serde_json::to_string(&DeleteParams {
            prefixes: vec!["a/".into()],
            keys: vec![],
            principal,
            expected: 0,
        })
        .unwrap()
    }

    /// F35: the cancel decision table. The display name is not an input.
    #[test]
    fn may_cancel_compares_principals() {
        let admin_job = row(Principal::Admin);
        let open_job = row(Principal::Open);
        let dana_job = row(user("AKDANA"));
        let bad = Some("{not json");
        #[rustfmt::skip]
        let table: [(&Principal, Option<&str>, bool, &str); 11] = [
            (&Principal::Admin, Some(&admin_job), true, "admin cancels an admin job"),
            (&Principal::Admin, Some(&dana_job), true, "admin cancels a user job"),
            (&Principal::Admin, bad, true, "admin cancels an unreadable row"),
            (&Principal::Open, Some(&dana_job), true, "open cancels any"),
            (&user("AKDANA"), Some(&dana_job), true, "a user cancels its own"),
            (&user("AKOTHER"), Some(&dana_job), false, "a user cannot cancel another user's"),
            (&user("AKADMINUSER"), Some(&admin_job), false, "a user (even one named admin) cannot cancel an admin job"),
            (&user("AKADMINUSER"), Some(&open_job), false, "a user cannot cancel an open job"),
            (&user("AKDANA"), None, false, "a row without params denies a user"),
            (&user("AKDANA"), bad, false, "an unreadable row denies a user"),
            (&user("akdana"), Some(&dana_job), false, "access key ids match exactly"),
        ];
        for (session, params, want, why) in table {
            assert_eq!(may_cancel(session, params), want, "{why}");
        }
    }

    /// A2 (#171): the owner cancels after a key rotation (same id, new key);
    /// a different user that holds the old key id does not.
    #[test]
    fn the_owner_cancels_after_a_key_rotation() {
        let job_before_rotation = row(user_with_id("AKOLD", 7));
        #[rustfmt::skip]
        let table: [(Principal, bool, &str); 5] = [
            (user_with_id("AKNEW", 7), true, "same id, rotated key"),
            (user_with_id("AKOLD", 7), true, "same id, same key"),
            (user_with_id("AKOLD", 8), false, "another user that holds the old key id"),
            (user_with_id("AKNEW", 8), false, "another user"),
            (user("AKOLD"), true, "a session without an id falls back to the key"),
        ];
        for (session, want, why) in table {
            assert_eq!(
                may_cancel(&session, Some(&job_before_rotation)),
                want,
                "{why}"
            );
        }
        // A row of an older release has no id: judged by the key.
        let old_row = row(user("AKOLD"));
        assert!(may_cancel(&user_with_id("AKOLD", 7), Some(&old_row)));
        assert!(!may_cancel(&user_with_id("AKNEW", 7), Some(&old_row)));
        // The old row format (no `user_id`) still parses.
        let legacy = r#"{"prefixes":["a/"],"keys":[],"principal":{"type":"user","access_key_id":"AK","client_ip":null}}"#;
        assert!(matches!(
            parse_params(legacy).unwrap().principal,
            Principal::User { user_id: None, .. }
        ));
    }

    #[test]
    fn params_round_trip_with_each_principal() {
        for principal in [
            Principal::Admin,
            Principal::Open,
            Principal::User {
                access_key_id: "AK".into(),
                client_ip: Some("10.0.0.1".parse().unwrap()),
                user_id: Some(4),
            },
        ] {
            let p = DeleteParams {
                prefixes: vec!["a/".into()],
                keys: vec!["b.txt".into()],
                principal,
                expected: 3,
            };
            assert_eq!(
                parse_params(&serde_json::to_string(&p).unwrap()).unwrap(),
                p
            );
        }
    }

    #[test]
    fn a_missing_or_broken_cursor_starts_at_the_top() {
        assert_eq!(Cursor::parse(None), Cursor::default());
        assert_eq!(Cursor::parse(Some("not json")), Cursor::default());
        let c = Cursor {
            stage: 2,
            after: Some("a/x".into()),
            skip: 0,
        };
        assert_eq!(Cursor::parse(Some(&c.token())), c);
    }

    #[test]
    fn the_total_is_the_count_until_the_job_has_handled_more() {
        assert_eq!(shown_total(100, 10, 0), 100);
        assert_eq!(shown_total(100, 99, 3), 102);
        assert_eq!(shown_total(0, 0, 0), 0);
    }
}

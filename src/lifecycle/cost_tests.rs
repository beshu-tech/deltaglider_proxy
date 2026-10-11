// SPDX-License-Identifier: BUSL-1.1

//! What a lifecycle run and a preview cost in backend requests (jobs.md
//! finding 7, ui.md finding E), on a fake S3 that counts them.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config_db::ConfigDb;
use crate::config_sections::LifecycleRule;
use crate::deltaglider::{s3_engine, store_deltas, DynEngine};
use crate::storage::FakeS3;

fn rule(name: &str, expire_after: &str) -> LifecycleRule {
    LifecycleRule {
        name: name.to_string(),
        enabled: true,
        bucket: "b".to_string(),
        prefix: String::new(),
        action: Default::default(),
        expire_after: Some(expire_after.to_string()),
        include_globs: vec![],
        exclude_globs: vec![],
        batch_size: 1000,
    }
}

async fn engine() -> (Arc<DynEngine>, Arc<FakeS3>) {
    let (engine, fake) = s3_engine().await;
    (Arc::new(engine), fake)
}

fn count(fake: &FakeS3, method: &str) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.starts_with(method))
        .count()
}

/// The LIST requests of the bucket listing itself (not the listing facts
/// under `.dg/`, not a folder's reclaim check).
fn scope_lists(fake: &FakeS3) -> usize {
    fake.requests()
        .iter()
        .filter(|r| {
            r.contains("list-type=2") && !r.contains("prefix=.dg") && !r.contains("delimiter=")
        })
        .count()
}

/// The reclaim checks of `folder`: a one-level LIST of it.
fn reclaim_checks(fake: &FakeS3, folder: &str) -> usize {
    fake.requests()
        .iter()
        .filter(|r| {
            r.contains("delimiter=%2F") && crate::storage::fake_s3::list_prefix(r) == folder
        })
        .count()
}

async fn run(
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
) -> crate::lifecycle::LifecycleRunOutcome {
    crate::lifecycle::run_rule(None, engine, rule, 10, "test", 60, None, None)
        .await
        .unwrap()
}

/// jobs-7: every hourly rescan sent a HEAD for every delta, because the
/// engine's metadata cache keeps an answer for 10 minutes only. A rescan
/// of objects whose stored version did not change sends none.
#[tokio::test]
async fn a_rescan_sends_no_head_for_unchanged_objects() {
    let (engine, fake) = engine().await;
    store_deltas(&engine, "run", 200).await;
    let keep = rule("keep-all", "30d");
    run(&engine, &keep).await;

    // An hour later: the engine's metadata cache has expired.
    engine.metadata_cache().invalidate_prefix("b", "");
    fake.clear();
    let out = run(&engine, &keep).await;
    assert_eq!(out.objects_scanned, 200, "{out:?}");
    assert_eq!(out.objects_affected, 0, "{out:?}");
    assert_eq!(
        count(&fake, "HEAD"),
        0,
        "the rescan sent a HEAD per unchanged object"
    );
    assert_eq!(scope_lists(&fake), 1, "{:#?}", fake.requests());
}

/// An overwrite changes the stored object: the rescan reads it again.
#[tokio::test]
async fn a_rescan_reads_an_overwritten_object_again() {
    let (engine, fake) = engine().await;
    let keys = store_deltas(&engine, "run", 3).await;
    let keep = rule("keep-all", "30d");
    run(&engine, &keep).await;
    engine
        .store("b", &keys[1], b"a new generation", None, Default::default())
        .await
        .unwrap();
    engine.metadata_cache().invalidate_prefix("b", "");
    fake.clear();
    run(&engine, &keep).await;
    let heads: Vec<String> = fake
        .requests()
        .into_iter()
        .filter(|r| r.starts_with("HEAD"))
        .collect();
    assert!(
        !heads.is_empty() && heads.iter().all(|r| r.contains("build-001")),
        "{heads:?}"
    );
}

/// jobs-7: an expiry wave deleted key by key, each delete checking its
/// folder for a reference to reclaim (a LIST per key). The page's deletes
/// now go in one batch: one check per folder, folders in parallel.
#[tokio::test]
async fn expired_objects_are_deleted_in_one_batch_per_page() {
    let (engine, fake) = engine().await;
    store_deltas(&engine, "run", 25).await;
    store_deltas(&engine, "rel", 25).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    fake.clear();
    fake.set_delete_delay_ms(20);
    let out = run(&engine, &rule("expire", "1ms")).await;
    assert_eq!(out.objects_affected, 50, "{out:?}");
    assert_eq!(out.errors, 0, "{out:?}");
    assert_eq!(reclaim_checks(&fake, "run/"), 1, "{:#?}", fake.requests());
    assert_eq!(reclaim_checks(&fake, "rel/"), 1);
    assert!(
        fake.peak_deletes_in_flight() >= 2,
        "the folders were deleted one after another"
    );
    let left = engine
        .list_objects("b", "", None, 1000, None, false)
        .await
        .unwrap();
    assert!(left.objects.is_empty(), "{:?}", left.objects);
}

/// ui-E: each open of the Preview tab and each Run-now confirmation
/// walked the rule's scope again. A preview of the same definition
/// answers from the last walk for a short time.
#[tokio::test]
async fn a_second_preview_answers_from_the_first() {
    let (engine, fake) = engine().await;
    store_deltas(&engine, "run", 20).await;
    let r = rule("preview-twice", "1ms");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let first = crate::lifecycle::preview_rule(&engine, &r, 100)
        .await
        .unwrap();
    assert_eq!(first.objects_affected, 20, "{first:?}");
    fake.clear();
    let second = crate::lifecycle::preview_rule(&engine, &r, 100)
        .await
        .unwrap();
    assert_eq!(first, second);
    assert!(fake.requests().is_empty(), "{:#?}", fake.requests());

    // Another definition walks again.
    let other = rule("preview-twice", "2ms");
    crate::lifecycle::preview_rule(&engine, &other, 100)
        .await
        .unwrap();
    assert_eq!(scope_lists(&fake), 1);
}

/// Previews asked while one walks wait for that walk (single-flight).
#[tokio::test]
async fn previews_asked_at_once_walk_once() {
    let (engine, fake) = engine().await;
    store_deltas(&engine, "run", 10).await;
    let r = rule("preview-at-once", "30d");
    fake.clear();
    fake.set_delay_ms("LIST", 50);
    let (a, b, c) = tokio::join!(
        crate::lifecycle::preview_rule(&engine, &r, 100),
        crate::lifecycle::preview_rule(&engine, &r, 100),
        crate::lifecycle::preview_rule(&engine, &r, 100),
    );
    assert_eq!(a.unwrap(), b.unwrap());
    assert!(c.is_ok());
    assert_eq!(scope_lists(&fake), 1, "{:#?}", fake.requests());
}

/// Run-now after a preview: the run plans from what the preview read, so
/// it sends no HEAD for an object that did not change.
#[tokio::test]
async fn a_run_after_a_preview_sends_no_head() {
    let (engine, fake) = engine().await;
    store_deltas(&engine, "run", 30).await;
    let r = rule("preview-then-run", "30d");
    engine.metadata_cache().invalidate_prefix("b", "");
    crate::lifecycle::preview_rule(&engine, &r, 100)
        .await
        .unwrap();
    engine.metadata_cache().invalidate_prefix("b", "");
    fake.clear();
    let out = run(&engine, &r).await;
    assert_eq!(out.objects_scanned, 30);
    assert_eq!(count(&fake, "HEAD"), 0, "{:#?}", fake.requests());
}

/// jobs-14: a resumed run whose first page times out (503) cleared its
/// cursor and failed; the next run started from the top. The page is
/// listed again with the same token.
#[tokio::test]
async fn a_transient_list_error_keeps_the_cursor() {
    let (engine, fake) = engine().await;
    store_deltas(&engine, "run", 30).await;
    let mut r = rule("resume", "30d");
    r.batch_size = 10;
    let db = Arc::new(Mutex::new(ConfigDb::in_memory("k").unwrap()));
    {
        let db = db.lock().await;
        db.lifecycle_ensure_state("resume", 0).unwrap();
        db.lifecycle_set_continuation_token("resume", Some("run/build-009.zip"), "b|")
            .unwrap();
    }
    fake.fail("LIST", "start-after=run", 503, "SlowDown", 1);
    let out = crate::lifecycle::run_rule(Some(db.clone()), &engine, &r, 10, "test", 60, None, None)
        .await
        .unwrap();
    assert_eq!(out.errors, 0, "{out:?}");
    assert_eq!(out.objects_scanned, 20, "resumed at the cursor: {out:?}");
}

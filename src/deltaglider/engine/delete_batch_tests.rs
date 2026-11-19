// SPDX-License-Identifier: BUSL-1.1

//! `delete_batch`: the one way to delete more than one key. What it costs
//! in backend requests, and what it reports per key.

use super::delete_cost_tests::folder_lists;
use super::*;
use crate::storage::FakeS3;

fn count(fake: &FakeS3, method: &str) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.starts_with(method))
        .count()
}

fn items(keys: &[String]) -> Vec<DeleteItem> {
    keys.iter().map(DeleteItem::new).collect()
}

fn deleted(outcomes: &[DeleteOutcome]) -> usize {
    outcomes
        .iter()
        .filter(|o| matches!(o, DeleteOutcome::Deleted(_)))
        .count()
}

/// A folder of N keys costs one reclaim check, after its last key. A loop
/// of per-key `delete` (replication, lifecycle, migrate, the CLI) checked
/// after every key: N LISTs for a folder of N.
#[tokio::test]
async fn a_batch_checks_each_folder_once() {
    let (engine, fake) = s3_engine().await;
    let keys = store_deltas(&engine, "run", 100).await;
    fake.clear();

    let outcomes = engine
        .delete_batch(
            "b",
            items(&keys),
            crate::deltaglider::DeleteHooks::new(crate::maintenance::gate::Counted::for_test()),
        )
        .await;

    assert_eq!(deleted(&outcomes), keys.len(), "{outcomes:?}");
    let lists = folder_lists(&fake).len();
    eprintln!("100 keys in one folder: {lists} LISTs");
    assert_eq!(lists, 1, "one reclaim check for the folder");
    assert!(!engine.storage().has_reference("b", "run").await.unwrap());
}

/// One outcome per item, in input order, whatever the folder order.
#[tokio::test]
async fn outcomes_come_back_in_input_order() {
    let (engine, _fake) = s3_engine().await;
    let a = store_deltas(&engine, "a", 2).await;
    let z = store_deltas(&engine, "z", 1).await;
    let batch = vec![
        DeleteItem::new(z[0].clone()),
        DeleteItem::new("a/never-stored.zip"),
        DeleteItem::only_if(a[0].clone(), |_: &FileMetadata| false),
        DeleteItem::new("a/reference.bin"),
        DeleteItem::new(a[1].clone()),
    ];

    let outcomes = engine
        .delete_batch(
            "b",
            batch,
            crate::deltaglider::DeleteHooks::new(crate::maintenance::gate::Counted::for_test()),
        )
        .await;

    let kinds: Vec<&str> = outcomes
        .iter()
        .map(|o| match o {
            DeleteOutcome::Deleted(_) => "deleted",
            DeleteOutcome::NotFound => "not found",
            DeleteOutcome::Changed => "changed",
            DeleteOutcome::Skipped => "skipped",
            DeleteOutcome::Failed(_) => "failed",
        })
        .collect();
    assert_eq!(
        kinds,
        ["deleted", "not found", "changed", "failed", "deleted"]
    );
    assert!(
        engine.head("b", &a[0]).await.is_ok(),
        "the refused key stays"
    );
    assert!(
        engine.storage().has_reference("b", "a").await.unwrap(),
        "a/ still holds an object"
    );
    assert!(!engine.storage().has_reference("b", "z").await.unwrap());
}

/// The folders of one batch are deleted at the same time.
#[tokio::test]
async fn a_batch_works_on_several_folders_at_once() {
    let (engine, fake) = s3_engine().await;
    let mut keys = Vec::new();
    for run in 0..BULK_DELETE_CONCURRENCY {
        keys.extend(store_deltas(&engine, &format!("run-{run}"), 2).await);
    }
    fake.set_delete_delay_ms(50);

    let outcomes = engine
        .delete_batch(
            "b",
            items(&keys),
            crate::deltaglider::DeleteHooks::new(crate::maintenance::gate::Counted::for_test()),
        )
        .await;

    assert_eq!(deleted(&outcomes), keys.len(), "{outcomes:?}");
    let peak = fake.peak_deletes_in_flight();
    eprintln!("peak DELETEs in flight: {peak}");
    assert!(peak > 1, "the folders were deleted one after another");
}

/// A folder left with its reference.bin alone (a batch cut before its
/// reclaim) is repaired by a retry: a key found gone marks its folder.
#[tokio::test]
async fn a_retry_reclaims_the_folder_of_keys_already_gone() {
    let (engine, _fake) = s3_engine().await;
    let keys = store_deltas(&engine, "run", 3).await;
    for key in &keys {
        engine.delete_in_sweep("b", key).await.unwrap();
    }
    assert!(engine.storage().has_reference("b", "run").await.unwrap());

    let outcomes = engine
        .delete_batch(
            "b",
            items(&keys),
            crate::deltaglider::DeleteHooks::new(crate::maintenance::gate::Counted::for_test()),
        )
        .await;

    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, DeleteOutcome::NotFound)),
        "{outcomes:?}"
    );
    assert!(!engine.storage().has_reference("b", "run").await.unwrap());
}

/// `on_outcome` runs as each key ends, before the next key of its folder:
/// a batch cut part-way has reported every key it deleted.
#[tokio::test]
async fn each_key_is_reported_as_it_ends() {
    let (engine, fake) = s3_engine().await;
    let keys = store_deltas(&engine, "run", 3).await;
    fake.clear();
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let on_outcome =
        |key: &str, outcome: &DeleteOutcome| -> futures::future::BoxFuture<'static, ()> {
            let deletes = count(&fake, "DELETE ");
            seen.lock().push((
                key.to_string(),
                matches!(outcome, DeleteOutcome::Deleted(_)),
                deletes,
            ));
            Box::pin(async {})
        };

    engine
        .delete_batch(
            "b",
            items(&keys),
            DeleteHooks {
                on_outcome: Some(&on_outcome),
                ..DeleteHooks::new(crate::maintenance::gate::Counted::for_test())
            },
        )
        .await;

    let seen = seen.lock().clone();
    let reported: Vec<&String> = seen.iter().map(|(k, _, _)| k).collect();
    assert_eq!(reported, keys.iter().collect::<Vec<_>>());
    assert!(seen.iter().all(|(_, deleted, _)| *deleted));
    let deletes: Vec<usize> = seen.iter().map(|(_, _, d)| *d).collect();
    assert!(
        deletes.windows(2).all(|w| w[0] < w[1]),
        "each report came before the next key's DELETE: {deletes:?}"
    );
}

/// `post_commit` runs inside the key's own task: a caller cut while the
/// key's DELETE is in flight still gets the step, once, for the key that went.
#[tokio::test]
async fn the_post_commit_step_of_a_key_survives_a_cut_caller() {
    let (engine, fake) = s3_engine().await;
    let keys = store_deltas(&engine, "run", 3).await;
    fake.set_delete_delay_ms(300);
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let post_commit: Arc<PostCommitDelete> = Arc::new(move |key: &str, _: &DeleteOutcome| {
        sink.lock().push(key.to_string());
        Box::pin(async {})
    });
    let gate = Arc::new(crate::maintenance::gate::MaintenanceGate::new());
    let counted =
        crate::maintenance::gate::Counted::By(Arc::new(gate.begin_write("b").commit_count()));

    let cut = fake
        .cut_at(
            engine.delete_batch(
                "b",
                items(&keys),
                DeleteHooks {
                    post_commit: Some(post_commit),
                    ..DeleteHooks::new(counted)
                },
            ),
            |r| r.starts_with("DELETE /b/run/"),
        )
        .await;
    assert!(cut, "the batch was cut in its first DELETE");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while seen.lock().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "the step was lost");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(seen.lock().clone(), vec![keys[0].clone()]);
}

/// `proceed` answering `false` stops the batch between keys: the rest is
/// skipped and the folder keeps its reference.bin.
#[tokio::test]
async fn a_batch_told_to_stop_skips_the_rest_and_the_reclaim() {
    let (engine, _fake) = s3_engine().await;
    let keys = store_deltas(&engine, "run", 3).await;
    let asked = std::sync::atomic::AtomicUsize::new(0);
    let proceed = || asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;

    let outcomes = engine
        .delete_batch(
            "b",
            items(&keys),
            DeleteHooks {
                proceed: Some(&proceed),
                ..DeleteHooks::new(crate::maintenance::gate::Counted::for_test())
            },
        )
        .await;

    assert!(matches!(outcomes[0], DeleteOutcome::Deleted(_)));
    assert!(
        outcomes[1..]
            .iter()
            .all(|o| matches!(o, DeleteOutcome::Skipped)),
        "{outcomes:?}"
    );
    for key in &keys[1..] {
        assert!(engine.head("b", key).await.is_ok(), "{key} stays");
    }
}

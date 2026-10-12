// SPDX-License-Identifier: BUSL-1.1

//! What a PUT costs in backend requests, on a fake S3 with a usage counter
//! (the counter is what makes a PUT read the key it replaces).

use super::*;
use crate::bucket_usage::BucketUsage;
use crate::storage::{DynStorageBackend, FakeS3};

/// An engine on a fake S3 with bucket `b` and a usage counter.
pub(super) async fn counted_s3_engine() -> (Arc<DynEngine>, Arc<FakeS3>) {
    let (s3, fake) = crate::storage::fake_s3_backend().await;
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
        .with_bucket_usage(Some(Arc::new(BucketUsage::in_memory().unwrap())));
    engine.create_bucket("b").await.unwrap();
    (Arc::new(engine), fake)
}

/// The object requests (not the background listing-facts upkeep under
/// `.dg/facts`), by method.
pub(super) fn count(fake: &FakeS3, method: &str) -> usize {
    object_requests(fake)
        .iter()
        .filter(|r| r.starts_with(&format!("{method} ")))
        .count()
}

/// Every request except the background listing-facts upkeep.
pub(super) fn object_requests(fake: &FakeS3) -> Vec<String> {
    fake.requests()
        .into_iter()
        .filter(|r| !r.contains(".dg/facts") && !r.contains("prefix=.dg"))
        .collect()
}

/// A folder marker (`dir/`) never has a `.delta` form: its prior read
/// HEADs the marker only. 3 requests before (2 HEADs + PUT), 2 now; a
/// marker HEAD 2 -> 1.
#[tokio::test]
async fn a_folder_marker_put_reads_one_form() {
    let (engine, fake) = counted_s3_engine().await;
    fake.clear();
    engine
        .store("b", "dir/", b"", None, Default::default())
        .await
        .unwrap();
    let reqs = object_requests(&fake);
    assert_eq!(count(&fake, "HEAD"), 1, "{reqs:#?}");
    assert_eq!(reqs.len(), 2, "{reqs:#?}");
    // The overwrite of a marker reads it as present (counted once).
    fake.clear();
    let again = engine
        .store("b", "dir/", b"", None, Default::default())
        .await
        .unwrap();
    assert!(again.replaced.is_some(), "the overwrite saw the marker");
    assert_eq!(object_requests(&fake).len(), 2);
    // HEAD of a marker shares the lookup: one request.
    fake.clear();
    engine.head("b", "dir/").await.unwrap();
    assert_eq!(object_requests(&fake).len(), 1);
}

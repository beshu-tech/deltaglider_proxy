// SPDX-License-Identifier: BUSL-1.1

//! Listing facts that a LIST did not find.

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::deltaglider::{DeltaGliderEngine, DynEngine};
    use crate::storage::{DynStorageBackend, FakeS3};
    use std::sync::Arc;

    fn engine_on(s3: S3Backend) -> DynEngine {
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
        DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &crate::config::Config::default(),
            None,
        )
    }

    /// `n` deltas in `dir/` whose durable listing facts never landed (their
    /// facts PUTs fail), and an engine that does not know them: a second
    /// backend on the same fake with another listing-cache scope (another
    /// node, or this one after a restart).
    async fn deltas_without_facts(dir: &str, n: usize) -> (DynEngine, Arc<FakeS3>, String) {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.fail("PUT", "/.dg/facts/", 500, "InternalError", u32::MAX);
        let writer = engine_on(test_support::for_test_endpoint(&endpoint));
        writer.create_bucket("b").await.unwrap();
        crate::deltaglider::store_deltas(&writer, dir, n).await;
        let mut reader = test_support::for_test_endpoint(&endpoint);
        reader.list_cache_scope = format!("{endpoint}/other-node");
        (engine_on(reader), fake, endpoint)
    }

    fn facts_lists(fake: &FakeS3) -> usize {
        crate::usage_scanner::test_support::facts_lists(fake)
    }

    async fn list(engine: &DynEngine, prefix: &str) -> crate::deltaglider::ListObjectsPage {
        engine
            .list_objects("b", prefix, None, 1000, None, false)
            .await
            .unwrap()
    }

    /// A delta without durable facts made every LIST of its page read the
    /// facts namespace again (up to 4 LISTs), for ever: the miss is now
    /// remembered for a minute per exact stored object.
    #[tokio::test]
    async fn a_facts_miss_is_read_once_a_minute() {
        let (engine, fake, _) = deltas_without_facts("rel", 5).await;
        let mut per_list = Vec::new();
        for _ in 0..3 {
            fake.clear();
            let page = list(&engine, "rel/").await;
            assert_eq!(page.objects.len(), 5);
            per_list.push(facts_lists(&fake));
        }
        assert!(per_list[0] >= 1, "{per_list:?}");
        assert_eq!(per_list[1..], [0, 0], "facts LISTs per client LIST");
    }
}

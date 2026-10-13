// SPDX-License-Identifier: BUSL-1.1

//! The facts healer: a LIST that finds no durable listing facts for an
//! entry (an object stored before the facts existed, or whose facts write
//! failed) queues one background HEAD of that stored object. The HEAD
//! learns the logical size and ETag, which go into the listing-size cache
//! and, through the bounded facts drain, into a durable facts object. The
//! object then lists its logical size on every node, for good.
//!
//! Off the client's path, and bounded: [`HEAL_CONCURRENCY`] HEADs in flight
//! per backend, [`MAX_PENDING`] queued, one HEAD per stored object (key,
//! ETag, size) per [`TRIED_TTL`], none for an object that another HEAD
//! learned meanwhile, nothing during a graceful shutdown, and
//! nothing for [`PAUSE`] after a HEAD fails (a backend that fails requests
//! is not sent more background work; its passive health signal comes from
//! client requests).

use super::facts::{backfill_facts, remember_facts};
use super::*;
use crate::storage::facts_cleanup::FactsWriter;
use moka::sync::Cache;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Background HEADs in flight per backend.
pub(super) const HEAL_CONCURRENCY: usize = 4;
/// Queued HEADs per backend; a miss past it waits for a later LIST.
const MAX_PENDING: usize = 1024;
/// How long one stored object is not HEADed again (a foreign object
/// without DG metadata never heals).
const TRIED_TTL: Duration = Duration::from_secs(3600);
/// How long the healer sends nothing after a HEAD failed.
const PAUSE: Duration = Duration::from_secs(60);

/// Why a miss is not queued, or `None` when it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Skip {
    ShuttingDown,
    Paused,
    Full,
    AlreadyTried,
}

/// Pure: whether a facts miss gets a background HEAD now.
pub(super) fn admit(
    shutting_down: bool,
    paused: bool,
    pending: usize,
    already_tried: bool,
) -> Option<Skip> {
    if shutting_down {
        Some(Skip::ShuttingDown)
    } else if paused {
        Some(Skip::Paused)
    } else if already_tried {
        Some(Skip::AlreadyTried)
    } else if pending >= MAX_PENDING {
        Some(Skip::Full)
    } else {
        None
    }
}

struct State {
    client: Client,
    permits: tokio::sync::Semaphore,
    pending: Mutex<HashSet<String>>,
    tried: Cache<String, ()>,
    paused_until: Mutex<Option<Instant>>,
}

impl State {
    fn paused(&self) -> bool {
        self.paused_until
            .lock()
            .ok()
            .and_then(|p| *p)
            .is_some_and(|until| Instant::now() < until)
    }

    fn pause(&self) {
        if let Ok(mut p) = self.paused_until.lock() {
            *p = Some(Instant::now() + PAUSE);
        }
    }
}

/// One stored object whose durable facts a LIST did not find.
pub(super) struct Miss {
    pub scope: String,
    pub bucket: String,
    pub stored_key: String,
    pub etag: String,
    pub size: u64,
}

impl Miss {
    fn id(&self) -> StoredObjectId<'_> {
        StoredObjectId {
            scope: &self.scope,
            bucket: &self.bucket,
            key: &self.stored_key,
            etag: &self.etag,
            size: self.size,
        }
    }

    fn dedup_key(&self) -> String {
        format!(
            "{}\0{}\0{}\0{}\0{}",
            self.scope,
            self.bucket,
            self.stored_key,
            self.etag.trim_matches('"'),
            self.size
        )
    }
}

/// See the module docs. Cheap to clone; the tasks hold a clone.
#[derive(Clone)]
pub(super) struct FactsHealer {
    state: Arc<State>,
}

impl FactsHealer {
    pub(super) fn new(client: Client) -> Self {
        Self {
            state: Arc::new(State {
                client,
                permits: tokio::sync::Semaphore::new(HEAL_CONCURRENCY),
                pending: Mutex::new(HashSet::new()),
                tried: Cache::builder()
                    .max_capacity(100_000)
                    .time_to_live(TRIED_TTL)
                    .build(),
                paused_until: Mutex::new(None),
            }),
        }
    }

    /// Queue a background HEAD for `miss`, unless [`admit`] refuses it.
    /// Never waits; without a tokio runtime it does nothing.
    pub(super) fn queue(&self, miss: Miss, facts: &FactsWriter) -> Option<Skip> {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return Some(Skip::ShuttingDown);
        };
        let key = miss.dedup_key();
        {
            let Ok(mut pending) = self.state.pending.lock() else {
                return Some(Skip::Full);
            };
            let tried = pending.contains(&key) || self.state.tried.contains_key(&key);
            if let Some(skip) = admit(
                crate::shutdown::is_shutting_down(),
                self.state.paused(),
                pending.len(),
                tried,
            ) {
                return Some(skip);
            }
            pending.insert(key.clone());
        }
        let (state, facts) = (self.state.clone(), facts.clone());
        runtime.spawn(async move {
            if let Ok(_permit) = state.permits.acquire().await {
                // A HEAD since the LIST (the listing's own, for
                // `metadata=true` or the lifecycle scan) took the mark and
                // wrote the facts: no second HEAD.
                if !crate::shutdown::is_shutting_down()
                    && !state.paused()
                    && list_size_cache::still_missing(&miss.id())
                {
                    // Tried only once its HEAD is sent: a miss that a pause
                    // skipped is queued again by the next LIST that sees it.
                    state.tried.insert(key.clone(), ());
                    heal_one(&state, &miss, &facts).await;
                }
            }
            if let Ok(mut pending) = state.pending.lock() {
                pending.remove(&key);
            }
        });
        None
    }
}

/// HEAD the stored object; when it still is the listed one and carries DG
/// metadata, remember and write its facts.
async fn heal_one(state: &State, miss: &Miss, facts: &FactsWriter) {
    BACKEND_HEAD_REQUESTS.inc();
    let response = match state
        .client
        .head_object()
        .bucket(&miss.bucket)
        .key(&miss.stored_key)
        .send()
        .await
    {
        Ok(r) => r,
        // Deleted since the LIST: nothing to heal.
        Err(SdkError::ServiceError(se))
            if matches!(
                se.err(),
                aws_sdk_s3::operation::head_object::HeadObjectError::NotFound(_)
            ) =>
        {
            return;
        }
        Err(e) => {
            let err = S3Backend::classify_s3_error(&miss.bucket, &e, S3Op::HeadObject);
            debug!(
                "facts healer: HEAD {}/{} failed, pausing: {err}",
                miss.bucket, miss.stored_key
            );
            state.pause();
            return;
        }
    };
    // Overwritten since the LIST: the new object has its own facts.
    let same = response.e_tag().map(|e| e.trim_matches('"')) == Some(miss.etag.trim_matches('"'))
        && response.content_length() == Some(miss.size as i64);
    if !same {
        return;
    }
    let Some(user) = response.metadata().filter(|m| !m.is_empty()) else {
        return; // a foreign object: nothing to learn
    };
    let created_at = response
        .last_modified()
        .and_then(|t| DateTime::parse_from_rfc3339(&t.to_string()).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let Ok(meta) = S3Backend::headers_to_metadata(user, created_at) else {
        return;
    };
    let id = miss.id();
    remember_facts(&id, &meta);
    backfill_facts(&id, &meta, |facts_key| {
        facts.write(&miss.bucket, &miss.stored_key, facts_key)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
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
        // The writer's facts PUTs all failed before the faults go away.
        assert!(wait_count(&fake, n, |r| r.starts_with("PUT /b/.dg/facts/")).await);
        fake.clear_faults();
        (reader(&endpoint, "other-node"), fake, endpoint)
    }

    /// An engine on the fake with its own listing-cache scope (a node that
    /// knows nothing yet).
    fn reader(endpoint: &str, node: &str) -> DynEngine {
        let mut reader = test_support::for_test_endpoint(endpoint);
        reader.list_cache_scope = format!("{endpoint}/{node}");
        engine_on(reader)
    }

    /// Wait (up to 10 s) until `n` requests match `pred`.
    async fn wait_count(fake: &FakeS3, n: usize, pred: impl Fn(&str) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if fake.requests().iter().filter(|r| pred(r)).count() >= n {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    fn delta_heads(r: &str) -> bool {
        r.starts_with("HEAD /b/") && r.ends_with(".delta")
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

    /// A LIST that misses the facts of a delta queues one background HEAD
    /// of it; the HEAD writes the durable facts, so another node lists the
    /// logical size from one facts LIST and no HEAD.
    #[tokio::test]
    async fn a_missed_entry_heals_for_good() {
        let (engine, fake, endpoint) = deltas_without_facts("rel", 5).await;
        fake.clear();
        let page = list(&engine, "rel/").await;
        assert!(page.objects.iter().all(|(_, m)| m.file_size < 65_536));
        assert!(
            wait_count(&fake, 5, delta_heads).await,
            "{:?}",
            fake.requests()
        );
        assert!(wait_count(&fake, 5, |r| r.starts_with("PUT /b/.dg/facts/")).await);
        // This node: from its process cache, no request but the LIST.
        fake.clear();
        let page = list(&engine, "rel/").await;
        assert!(page.objects.iter().all(|(_, m)| m.file_size == 65_536));
        assert_eq!(facts_lists(&fake), 0);
        // Another node: from the durable facts.
        let other = reader(&endpoint, "third-node");
        fake.clear();
        let page = list(&other, "rel/").await;
        assert!(page.objects.iter().all(|(_, m)| m.file_size == 65_536));
        assert!(page.facts_missing_keys.is_empty());
        assert_eq!(fake.requests().iter().filter(|r| delta_heads(r)).count(), 0);
    }

    /// The background HEADs run at most HEAL_CONCURRENCY at once, one per
    /// stored object.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn healing_is_bounded_and_heads_each_object_once() {
        let (engine, fake, _) = deltas_without_facts("rel", 24).await;
        fake.set_delay_ms("HEAD", 30);
        fake.clear();
        list(&engine, "rel/").await;
        assert!(wait_count(&fake, 24, delta_heads).await);
        assert!(fake.peak_in_flight("HEAD") <= HEAL_CONCURRENCY);
        list(&engine, "rel/").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let heads = fake.requests().iter().filter(|r| delta_heads(r)).count();
        assert_eq!(heads, 24, "one HEAD per stored object");
    }

    /// A listing that HEADs its own facts misses (`metadata=true`, the
    /// lifecycle scan) also queued a healer HEAD of each: two HEADs per
    /// delta (seen on MinIO: 80 HEADs for 40 deltas). The healer now skips
    /// an object whose facts a HEAD learned since the LIST.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_listing_that_heads_its_misses_sends_one_head_per_object() {
        const N: usize = 40;
        let (engine, fake, _) = deltas_without_facts("rel", N).await;
        fake.set_delay_ms("HEAD", 20);
        fake.clear();
        let page = engine
            .list_objects("b", "rel/", None, 1000, None, true)
            .await
            .unwrap();
        assert!(page.objects.iter().all(|(_, m)| m.file_size == 65_536));
        assert!(wait_count(&fake, N, |r| r.starts_with("PUT /b/.dg/facts/")).await);
        // Let the healer's queued tasks run out.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let heads = fake.requests().iter().filter(|r| delta_heads(r)).count();
        assert!(
            heads <= N + HEAL_CONCURRENCY,
            "{heads} HEADs for {N} deltas"
        );
        let puts = fake
            .requests()
            .iter()
            .filter(|r| r.starts_with("PUT /b/.dg/facts/"))
            .count();
        assert_eq!(puts, N, "one facts write per delta");
    }

    /// The prod case (v2.0.9, slow backend): deltas without durable facts,
    /// a node whose delete cleanup reads for a long time (a lifecycle run
    /// deleted many keys), and a scan that HEADs its misses. The facts the
    /// HEADs learned waited behind the cleanup, so no facts PUT was sent
    /// and every node and restart missed the deltas again. Now they land
    /// at once, and another node lists every delta from the facts: no miss,
    /// no HEAD.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn facts_converge_while_a_long_delete_cleanup_runs() {
        const N: usize = 20;
        let (_, fake, endpoint) = deltas_without_facts("rel", N).await;
        let mut s3 = test_support::for_test_endpoint(&endpoint);
        s3.list_cache_scope = format!("{endpoint}/busy-node");
        fake.set_delay_ms("LIST", 150);
        // 40 deleted keys, one per directory: 40 facts LISTs, 6 s.
        for i in 0..40 {
            s3.facts_cleanup
                .enqueue("b", &format!("e2e/run{i:03}/index.html"), None);
        }
        let busy = engine_on(s3);
        assert!(
            fake.wait_for(|r| r.contains("prefix=.dg%2Ffacts%2Fe2e"))
                .await
        );
        fake.clear();
        let started = std::time::Instant::now();
        let page = list_with_heads(&busy, "rel/").await;
        assert!(page.objects.iter().all(|(_, m)| m.file_size == 65_536));
        let landed = wait_count(&fake, N, |r| r.starts_with("PUT /b/.dg/facts/rel/")).await;
        assert!(
            landed && started.elapsed() < std::time::Duration::from_secs(4),
            "{N} facts PUTs after {:?}: {landed}",
            started.elapsed()
        );
        fake.set_delay_ms("LIST", 0);
        let other = reader(&endpoint, "fresh-node");
        fake.clear();
        let page = list(&other, "rel/").await;
        assert!(page.objects.iter().all(|(_, m)| m.file_size == 65_536));
        assert!(page.facts_missing_keys.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(fake.requests().iter().filter(|r| delta_heads(r)).count(), 0);
    }

    /// The lifecycle scan's listing: the page, then a HEAD of each delta.
    async fn list_with_heads(
        engine: &DynEngine,
        prefix: &str,
    ) -> crate::deltaglider::ListObjectsPage {
        let mut page = engine
            .list_objects_lite("b", prefix, None, 1000, None)
            .await
            .unwrap();
        engine
            .finish_listed_page_with_heads("b", &mut page)
            .await
            .unwrap();
        page
    }

    /// An engine with a proxy-encryption key on the fake, with its own
    /// listing-cache scope.
    fn encrypted_reader(endpoint: &str, node: &str) -> DynEngine {
        use crate::storage::{EncryptingBackend, EncryptionConfig, EncryptionKey};
        let mut s3 = test_support::for_test_endpoint(endpoint);
        s3.list_cache_scope = format!("{endpoint}/{node}");
        let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(EncryptionConfig {
            key: Some(EncryptionKey::from_hex(&"ab".repeat(32)).unwrap()),
            key_id: Some("kid".into()),
            ..Default::default()
        })));
        let backend: Box<DynStorageBackend<'static>> =
            DynStorageBackend::new_box(EncryptingBackend::new(s3, cfg));
        DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &crate::config::Config::default(),
            None,
        )
    }

    /// Q2: with an encryption key, a plain object stored before the key
    /// lists its stored facts, and they are its logical facts. The LIST
    /// cannot know that, so it reads the facts namespace for it; the healer
    /// HEAD learned the facts, found them equal and wrote nothing. Every
    /// LIST, on every node and after each restart, missed the object again
    /// (a facts LIST per page a minute, a HEAD per object an hour). The
    /// HEAD now records the facts, equal or not, once a LIST asked.
    #[tokio::test]
    async fn plain_objects_under_an_encryption_key_heal_for_good() {
        const N: usize = 5;
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let writer = engine_on(test_support::for_test_endpoint(&endpoint));
        writer.create_bucket("b").await.unwrap();
        for i in 0..N {
            let body = format!("plain object {i}");
            writer
                .store(
                    "b",
                    &format!("old/p{i}.txt"),
                    body.as_bytes(),
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
        }
        let first = encrypted_reader(&endpoint, "enc-node-1");
        fake.clear();
        let page = list(&first, "old/").await;
        assert_eq!(page.objects.len(), N);
        assert!(wait_count(&fake, N, |r| r.starts_with("HEAD /b/old/p")).await);
        assert!(
            wait_count(&fake, N, |r| r.starts_with("PUT /b/.dg/facts/old/p")).await,
            "the healer wrote no facts: {:?}",
            fake.requests()
        );
        // Another node (or this one after a restart) reads the facts.
        let second = encrypted_reader(&endpoint, "enc-node-2");
        fake.clear();
        let page = list(&second, "old/").await;
        assert!(page.facts_missing_keys.is_empty(), "{page:?}");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|r| r.starts_with("HEAD "))
                .count(),
            0,
            "{:?}",
            fake.requests()
        );
    }

    /// A miss that a LIST marked.
    fn miss(scope: &str, key: &str) -> Miss {
        let miss = Miss {
            scope: scope.into(),
            bucket: "b".into(),
            stored_key: key.into(),
            etag: "e".into(),
            size: 1,
        };
        list_size_cache::mark_missing_facts(&miss.id());
        miss
    }

    /// A failed HEAD pauses the healer; the same miss is queued once.
    #[tokio::test]
    async fn a_failed_head_pauses_the_healer() {
        let (s3, fake) = crate::storage::fake_s3_backend().await;
        let writer = s3.facts_cleanup.writer();
        assert_eq!(s3.healer.queue(miss("p", "a.delta"), &writer), None);
        assert_eq!(
            s3.healer.queue(miss("p", "a.delta"), &writer),
            Some(Skip::AlreadyTried)
        );
        assert!(wait_count(&fake, 1, |r| r.starts_with("HEAD /b/a.delta")).await);
        fake.fail("HEAD", "b.delta", 503, "SlowDown", u32::MAX);
        assert_eq!(s3.healer.queue(miss("p", "b.delta"), &writer), None);
        assert!(wait_count(&fake, 1, |r| r.starts_with("HEAD /b/b.delta")).await);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !s3.healer.state.paused() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            s3.healer.queue(miss("p", "c.delta"), &writer),
            Some(Skip::Paused)
        );
    }

    /// A miss whose task found the healer paused sent no HEAD, so it is not
    /// "tried": the next LIST that sees it queues it again.
    #[tokio::test]
    async fn a_miss_skipped_by_a_pause_is_queued_again() {
        let (s3, fake) = crate::storage::fake_s3_backend().await;
        let writer = s3.facts_cleanup.writer();
        let state = s3.healer.state.clone();
        let held = state
            .permits
            .acquire_many(HEAL_CONCURRENCY as u32)
            .await
            .unwrap();
        assert_eq!(s3.healer.queue(miss("p", "a.delta"), &writer), None);
        state.pause();
        drop(held);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !state.pending.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "task never ran");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        *state.paused_until.lock().unwrap() = None;
        assert_eq!(s3.healer.queue(miss("p", "a.delta"), &writer), None);
        assert!(wait_count(&fake, 1, |r| r.starts_with("HEAD /b/a.delta")).await);
    }

    /// A HEAD that learned the facts after the LIST queued the miss (a
    /// client's HEAD or GET) took the mark: the healer sends no HEAD.
    #[tokio::test]
    async fn a_miss_that_another_head_healed_is_not_headed_again() {
        let (s3, fake) = crate::storage::fake_s3_backend().await;
        let writer = s3.facts_cleanup.writer();
        let state = s3.healer.state.clone();
        let held = state
            .permits
            .acquire_many(HEAL_CONCURRENCY as u32)
            .await
            .unwrap();
        let healed = miss("p3", "h.delta");
        assert!(list_size_cache::take_missing_facts(&healed.id()));
        assert_eq!(s3.healer.queue(healed, &writer), None);
        assert_eq!(s3.healer.queue(miss("p3", "u.delta"), &writer), None);
        drop(held);
        assert!(wait_count(&fake, 1, |r| r.starts_with("HEAD /b/u.delta")).await);
        while !state.pending.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!fake.requests().iter().any(|r| r.contains("h.delta")));
    }

    #[test]
    fn admit_truth_table() {
        assert_eq!(admit(false, false, 0, false), None);
        assert_eq!(admit(true, false, 0, false), Some(Skip::ShuttingDown));
        assert_eq!(admit(false, true, 0, false), Some(Skip::Paused));
        assert_eq!(admit(false, false, 0, true), Some(Skip::AlreadyTried));
        assert_eq!(admit(false, false, MAX_PENDING, false), Some(Skip::Full));
        assert_eq!(admit(false, false, MAX_PENDING - 1, false), None);
    }
}

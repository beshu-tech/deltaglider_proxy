// SPDX-License-Identifier: BUSL-1.1

//! The facts healer: a LIST that finds no durable listing facts for a
//! delta (an object stored before the facts existed, or whose facts write
//! failed) HEADs that stored object ([`MissDecision::HeadNow`]). The HEAD
//! learns the logical size and ETag. They go into the listing-size cache
//! and, through the bounded facts drain, into a durable facts object. The
//! object then lists its logical size on every node, for good.
//!
//! A client never sees the stored size of a delta, so the LIST waits for
//! those HEADs ([`FactsHealer::head_inline`]): every miss of the page, at
//! most [`INLINE_CONCURRENCY`] HEADs in flight per backend, and at most
//! [`INLINE_BUDGET`] of waiting per page. A miss that the budget cuts off,
//! or that a failing backend did not answer, lists its stored size once and
//! is healed in the background ([`FactsHealer::queue`]), so the next LIST
//! is right.
//!
//! The background path is bounded: [`HEAL_CONCURRENCY`] HEADs in flight
//! per backend, [`MAX_PENDING`] queued, one HEAD per stored object (key,
//! ETag, size) per [`TRIED_TTL`], none for an object that another HEAD
//! learned meanwhile, nothing during a graceful shutdown, and
//! nothing for [`PAUSE`] after a HEAD fails (a backend that fails requests
//! is not sent more background work; its passive health signal comes from
//! client requests). The encrypting wrapper's token healer shares
//! [`decide_delta_miss`], [`admit`] and [`Skip`].

use super::facts::{backfill_facts, remember_facts};
use super::*;
use crate::storage::facts_cleanup::FactsWriter;
use futures::StreamExt;
use moka::sync::Cache;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Background HEADs in flight per backend.
pub(super) const HEAL_CONCURRENCY: usize = 4;
/// HEADs in flight per backend for the misses of a LIST (the bound of every
/// HEAD sweep: see `S3Backend::bounded_head_calls`).
pub(super) const INLINE_CONCURRENCY: usize = S3Backend::MAX_CONCURRENT_HEADS;
/// How long one listing page waits for its HEADs. A page has at most 1000
/// entries, so on a healthy backend it never runs out.
pub(super) const INLINE_BUDGET: Duration = Duration::from_secs(15);
/// Queued HEADs per backend; a miss past it waits for a later LIST.
const MAX_PENDING: usize = 1024;
/// How long one stored object is not HEADed again (a foreign object
/// without DG metadata never heals).
const TRIED_TTL: Duration = Duration::from_secs(3600);
/// How long the healer sends nothing after a HEAD failed.
const PAUSE: Duration = Duration::from_secs(60);

/// Why a miss is not queued, or `None` when it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::storage) enum Skip {
    ShuttingDown,
    Paused,
    Full,
    AlreadyTried,
}

/// Pure: whether a facts miss gets a background HEAD now.
pub(in crate::storage) fn admit(
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

/// What a LIST does with one delta entry that has no listing facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::storage) enum MissDecision {
    /// List this plaintext size (exact; the ETag stays the stored one
    /// until a background HEAD keeps the token). Encrypted buckets only.
    Invert(u64),
    /// HEAD the stored object now.
    HeadNow,
    /// List the stored size. With `heal`, a background HEAD keeps the
    /// facts for the next LIST.
    Keep { heal: bool },
}

/// THE decision for a delta without listing facts, for a plain and for a
/// sealed bucket. Pure. `may_head`: the page has HEAD budget left and this
/// stored object was not tried a short time ago. A delta that gets no HEAD
/// now is healed in the background.
pub(in crate::storage) fn decide_delta_miss(may_head: bool) -> MissDecision {
    if may_head {
        MissDecision::HeadNow
    } else {
        MissDecision::Keep { heal: true }
    }
}

/// What a LIST learned about one miss that it waited for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Answer {
    /// The logical size and ETag.
    Facts(LogicalFacts),
    /// Nothing to learn (deleted, overwritten, foreign, sealed).
    Nothing,
    /// No answer: a failed HEAD, or the page budget ended first.
    Unfinished,
}

/// How one inline miss ended.
enum Done {
    Facts(LogicalFacts),
    Head(Arc<Head>),
    /// No HEAD can be sent now (a shutdown, a pause).
    Gone,
}

/// The HEAD in flight for one stored object, shared by every LIST that
/// misses it.
type Flight = tokio::sync::watch::Receiver<Option<Arc<Head>>>;

/// The LIST that sends the HEAD of a stored object. It tells the others
/// ([`Leading::finish`]); a LIST that is dropped first (its page budget
/// ended) leaves them without an answer.
struct Leading {
    state: Arc<State>,
    key: String,
    tx: tokio::sync::watch::Sender<Option<Arc<Head>>>,
}

impl Leading {
    /// Publish the answer. A HEAD that answered marks the object tried
    /// before the flight ends, so no LIST sends a second one.
    fn finish(self, head: Arc<Head>) {
        if !matches!(*head, Head::Failed) {
            self.state.tried.insert(self.key.clone(), ());
        }
        self.tx.send_replace(Some(head));
    }
}

impl Drop for Leading {
    fn drop(&mut self) {
        self.state
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

enum Claim {
    Lead(Leading),
    Follow(Flight),
}

struct State {
    client: Client,
    permits: tokio::sync::Semaphore,
    /// HEADs in flight for the misses of a LIST (see [`INLINE_CONCURRENCY`]).
    inline_permits: tokio::sync::Semaphore,
    inline_budget: Duration,
    /// The inline HEAD in flight of each stored object.
    inflight: Mutex<std::collections::HashMap<String, Flight>>,
    pending: Mutex<HashSet<String>>,
    tried: Cache<String, ()>,
    paused_until: Mutex<Option<Instant>>,
}

impl State {
    /// Join the HEAD in flight of `key`, or become the one that sends it.
    fn claim(self: &Arc<Self>, key: &str) -> Claim {
        let mut flights = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rx) = flights.get(key) {
            return Claim::Follow(rx.clone());
        }
        let (tx, rx) = tokio::sync::watch::channel(None);
        flights.insert(key.to_string(), rx);
        Claim::Lead(Leading {
            state: self.clone(),
            key: key.to_string(),
            tx,
        })
    }

    /// The answer for one inline miss: from the listing-size cache when a
    /// HEAD learned it meanwhile, from the HEAD in flight, or from a HEAD of
    /// its own.
    async fn head_once(self: &Arc<Self>, miss: &Miss) -> Done {
        // A follower whose leader was dropped (its client left, or its page
        // budget ended) claims the HEAD again and leads it. The caller's
        // page budget bounds the loop; a leader that cannot send (shutdown,
        // pause) ends it with `Gone`.
        loop {
            if let Some(facts) = list_size_cache::lookup(&miss.id()) {
                return Done::Facts(facts);
            }
            match self.claim(&miss.dedup_key()) {
                Claim::Follow(mut flight) => {
                    if let Ok(Some(head)) = flight.wait_for(|v| v.is_some()).await.as_deref() {
                        return Done::Head(head.clone());
                    }
                }
                Claim::Lead(lead) => {
                    let Ok(_permit) = self.inline_permits.acquire().await else {
                        return Done::Gone;
                    };
                    if crate::shutdown::is_shutting_down() || self.paused() {
                        return Done::Gone;
                    }
                    let head = Arc::new(head_miss(self, miss).await);
                    lead.finish(head.clone());
                    return Done::Head(head);
                }
            }
        }
    }

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
    pub(super) fn id(&self) -> StoredObjectId<'_> {
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
                inline_permits: tokio::sync::Semaphore::new(INLINE_CONCURRENCY),
                inline_budget: INLINE_BUDGET,
                inflight: Mutex::new(Default::default()),
                pending: Mutex::new(HashSet::new()),
                tried: Cache::builder()
                    .max_capacity(100_000)
                    .time_to_live(TRIED_TTL)
                    .build(),
                paused_until: Mutex::new(None),
            }),
        }
    }

    /// The same healer with another page budget (tests).
    #[cfg(test)]
    pub(super) fn with_inline_budget(client: Client, budget: Duration) -> Self {
        let healer = Self::new(client);
        let mut state = Arc::try_unwrap(healer.state).ok().expect("fresh healer");
        state.inline_budget = budget;
        Self {
            state: Arc::new(state),
        }
    }

    /// May a LIST HEAD `miss` now? Not during a shutdown, not while the
    /// healer is paused after a failed HEAD, and not when a HEAD of this
    /// exact stored object answered less than [`TRIED_TTL`] ago (a foreign
    /// object without DG metadata never heals).
    pub(super) fn may_head_inline(&self, miss: &Miss) -> bool {
        !crate::shutdown::is_shutting_down()
            && !self.state.paused()
            && !self.state.tried.contains_key(&miss.dedup_key())
    }

    /// HEAD the stored objects of `misses` for the LIST that waits, at most
    /// [`INLINE_CONCURRENCY`] at once and for at most the page budget. One
    /// HEAD serves every LIST that misses the same stored object at the
    /// same time: the first LIST sends it, the others wait for its answer.
    /// Per miss: [`Answer::Facts`] when a HEAD learned the facts (they are
    /// remembered and written, as the background heal does),
    /// [`Answer::Nothing`] when there is nothing to learn, else
    /// [`Answer::Unfinished`]: a failed HEAD (which pauses the healer) or no
    /// time left. A miss that no HEAD answered is not marked tried, so a
    /// later LIST or the background queue HEADs it.
    pub(super) async fn head_inline(&self, misses: &[Miss], facts: &FactsWriter) -> Vec<Answer> {
        let mut answers: Vec<Answer> = misses.iter().map(|_| Answer::Unfinished).collect();
        let state = &self.state;
        let mut heads: futures::stream::FuturesUnordered<_> = misses
            .iter()
            .enumerate()
            .map(|(i, miss)| {
                let state = state.clone();
                async move { (i, state.head_once(miss).await) }
            })
            .collect();
        let deadline = tokio::time::Instant::now() + state.inline_budget;
        while let Ok(Some((i, done))) = tokio::time::timeout_at(deadline, heads.next()).await {
            answers[i] = match done {
                Done::Facts(f) => Answer::Facts(f),
                Done::Gone => Answer::Unfinished,
                Done::Head(head) => match &*head {
                    Head::Failed => {
                        state.pause();
                        Answer::Unfinished
                    }
                    Head::Nothing => Answer::Nothing,
                    Head::Learned(meta) => {
                        learn(&misses[i], meta, facts);
                        LogicalFacts::of(meta).map_or(Answer::Nothing, Answer::Facts)
                    }
                },
            };
        }
        answers
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

/// What a HEAD of a missed stored object answered.
enum Head {
    /// The listed object, with DG metadata.
    Learned(Box<FileMetadata>),
    /// Nothing to learn: deleted or overwritten since the LIST, or a
    /// foreign object without DG metadata.
    Nothing,
    /// The backend failed the HEAD.
    Failed,
}

/// HEAD the stored object of `miss`.
async fn head_miss(state: &State, miss: &Miss) -> Head {
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
            return Head::Nothing;
        }
        Err(e) => {
            let err = S3Backend::classify_s3_error(&miss.bucket, &e, S3Op::HeadObject);
            debug!(
                "facts healer: HEAD {}/{} failed, pausing: {err}",
                miss.bucket, miss.stored_key
            );
            return Head::Failed;
        }
    };
    // Overwritten since the LIST: the new object has its own facts.
    let same = response.e_tag().map(|e| e.trim_matches('"')) == Some(miss.etag.trim_matches('"'))
        && response.content_length() == Some(miss.size as i64);
    if !same {
        return Head::Nothing;
    }
    let Some(user) = response.metadata().filter(|m| !m.is_empty()) else {
        return Head::Nothing; // a foreign object: nothing to learn
    };
    let created_at = response
        .last_modified()
        .and_then(|t| DateTime::parse_from_rfc3339(&t.to_string()).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    match S3Backend::headers_to_metadata(user, created_at) {
        Ok(meta) => Head::Learned(Box::new(meta)),
        Err(_) => Head::Nothing,
    }
}

/// Remember the facts that a HEAD of `miss` learned and hand them to the
/// bounded facts drain.
fn learn(miss: &Miss, meta: &FileMetadata, facts: &FactsWriter) {
    let id = miss.id();
    remember_facts(&id, meta);
    backfill_facts(&id, meta, |facts_key| {
        facts.write(&miss.bucket, &miss.stored_key, facts_key)
    });
}

/// HEAD the stored object; when it still is the listed one and carries DG
/// metadata, remember and write its facts.
async fn heal_one(state: &State, miss: &Miss, facts: &FactsWriter) {
    match head_miss(state, miss).await {
        Head::Learned(meta) => learn(miss, &meta, facts),
        Head::Nothing => {}
        Head::Failed => state.pause(),
    }
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
        deltas_without_facts_slow(dir, n, 0).await
    }

    /// [`deltas_without_facts`] on a fake whose PUTs wait `put_delay_ms`
    /// before they answer: the slow host, made certain.
    async fn deltas_without_facts_slow(
        dir: &str,
        n: usize,
        put_delay_ms: u64,
    ) -> (DynEngine, Arc<FakeS3>, String) {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.fail("PUT", "/.dg/facts/", 500, "InternalError", u32::MAX);
        fake.set_put_delay_ms(put_delay_ms);
        let writer = engine_on(test_support::for_test_endpoint(&endpoint));
        writer.create_bucket("b").await.unwrap();
        crate::deltaglider::store_deltas(&writer, dir, n).await;
        // Every facts PUT must be ANSWERED with its 500 before the fault
        // goes away. The request log shows a PUT when it ARRIVES; one that
        // arrived but is not answered yet would succeed after the fault is
        // gone, and its object would have durable facts: no miss, no
        // healer HEAD (nightly #153, 1 run in ~40 on one starved core).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while fake.faulted("PUT", "/.dg/facts/") < n {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} of {n} facts PUTs failed",
                fake.faulted("PUT", "/.dg/facts/")
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        fake.clear_faults();
        fake.set_put_delay_ms(0);
        (reader(&endpoint, "other-node"), fake, endpoint)
    }

    /// An engine on the fake with its own listing-cache scope (a node that
    /// knows nothing yet).
    fn reader(endpoint: &str, node: &str) -> DynEngine {
        reader_and_scope(endpoint, node).0
    }

    /// [`reader`] and its listing-cache scope, for a test that looks up
    /// the process caches itself.
    fn reader_and_scope(endpoint: &str, node: &str) -> (DynEngine, String) {
        let mut reader = test_support::for_test_endpoint(endpoint);
        let scope = test_support::test_scope(endpoint, node);
        reader.list_cache_scope = scope.clone();
        (engine_on(reader), scope)
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

    /// Wait until `n` objects are STORED under `prefix` (`bucket/key`
    /// form). [`wait_count`] sees a PUT when it arrives, before the fake
    /// stores the object: a node that lists right after it can still miss
    /// the facts.
    async fn wait_stored(fake: &FakeS3, n: usize, prefix: &str) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if fake.stored_count(prefix) >= n {
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

    /// A client LIST page (the only kind that HEADs its misses inline).
    async fn list(engine: &DynEngine, prefix: &str) -> crate::deltaglider::ListObjectsPage {
        engine
            .list_objects_for_client("b", prefix, None, 1000, None, false)
            .await
            .unwrap()
    }

    /// A walk page (a folder listing, a scan, a job): no inline HEAD.
    async fn walk(engine: &DynEngine, prefix: &str) -> crate::deltaglider::ListObjectsPage {
        engine
            .list_objects("b", prefix, None, 1000, None, false)
            .await
            .unwrap()
    }

    /// An engine on the fake behind the encrypting wrapper with `cfg`, with
    /// its own listing-cache scope.
    fn wrapped(endpoint: &str, node: &str, cfg: crate::storage::EncryptionConfig) -> DynEngine {
        let mut s3 = test_support::for_test_endpoint(endpoint);
        s3.list_cache_scope = test_support::test_scope(endpoint, node);
        let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(cfg)));
        let backend: Box<DynStorageBackend<'static>> =
            DynStorageBackend::new_box(crate::storage::EncryptingBackend::new(s3, cfg));
        DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &crate::config::Config::default(),
            None,
        )
    }

    /// Bucket `b` encrypts under key `kid` (`stopped`: it stopped, and keeps
    /// `kid` as its legacy key until a re-encrypt job has converted it).
    fn bucket_b_config(stopped: bool) -> crate::storage::EncryptionConfig {
        use crate::storage::{BucketKeys, EncryptionConfig, EncryptionKey};
        let key = EncryptionKey::from_hex(&"ef".repeat(32)).unwrap();
        let bucket = if stopped {
            BucketKeys {
                legacy_key_id: Some("kid".into()),
                ..Default::default()
            }
        } else {
            BucketKeys {
                key: Some(key.clone()),
                key_id: Some("kid".into()),
                ..Default::default()
            }
        };
        EncryptionConfig {
            buckets: std::collections::HashMap::from([("b".to_string(), bucket)]),
            keyring: std::collections::HashMap::from([("kid".to_string(), key)]),
            ..Default::default()
        }
    }

    /// Plant a `!!1` facts object (size 7) for each stored delta under `rel/`
    /// of bucket `b`, next to the object, as a backend writer would.
    async fn plant_facts(endpoint: &str, fake: &FakeS3) -> usize {
        let plain = engine_on(test_support::for_test_endpoint(endpoint));
        let stub = plain
            .list_objects_lite("b", "rel/", None, 1000, None)
            .await
            .unwrap();
        for (key, meta) in &stub.objects {
            let facts_key = crate::storage::listing_facts::asked_facts_key(
                &format!("{key}.delta"),
                &meta.md5,
                meta.stored_size(),
                &LogicalFacts {
                    size: 7,
                    etag: "0".repeat(32),
                },
            )
            .expect("a facts key");
            fake.insert_object(&format!("b/{facts_key}"), &[], &[]);
        }
        stub.objects.len()
    }

    /// The setup helper leaves NO durable facts, whatever the host load:
    /// with every PUT answered 100 ms late, a facts PUT that had only
    /// ARRIVED when the fault was removed was stored (nightly #153:
    /// `healing_is_bounded_and_heads_each_object_once` saw no HEAD).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_setup_leaves_no_durable_facts_on_a_slow_backend() {
        let (_, fake, _) = deltas_without_facts_slow("rel", 12, 100).await;
        // Past the 100 ms of every PUT still in flight.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(fake.stored_count("b/.dg/facts/"), 0);
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

    /// A client LIST never shows a stored delta size: the first LIST of a
    /// delta without durable facts HEADs it on the LIST's path, lists the
    /// logical size and ETag, and keeps the facts for the next LIST.
    #[tokio::test]
    async fn the_first_list_of_a_delta_without_facts_shows_its_logical_size() {
        let (engine, fake, endpoint) = deltas_without_facts("rel", 5).await;
        fake.clear();
        let page = list(&engine, "rel/").await;
        assert_eq!(page.objects.len(), 5);
        for (key, meta) in &page.objects {
            assert_eq!(meta.file_size, 65_536, "{key}");
            let head = engine.head("b", key).await.unwrap();
            assert_eq!(meta.etag(), head.etag(), "{key} etag");
        }
        assert!(page.facts_missing_keys.is_empty(), "{page:?}");
        assert!(wait_stored(&fake, 5, "b/.dg/facts/").await, "facts kept");
        // This node: from its process cache, no request but the LIST.
        fake.clear();
        let page = list(&engine, "rel/").await;
        assert!(page.objects.iter().all(|(_, m)| m.file_size == 65_536));
        assert_eq!(facts_lists(&fake), 0);
        assert_eq!(fake.requests().iter().filter(|r| delta_heads(r)).count(), 0);
        // Another node lists them from the facts, with no HEAD.
        let other = reader(&endpoint, "third-node");
        fake.clear();
        let page = list(&other, "rel/").await;
        assert!(page.objects.iter().all(|(_, m)| m.file_size == 65_536));
        assert_eq!(fake.requests().iter().filter(|r| delta_heads(r)).count(), 0);
    }

    /// More misses than one page of HEAD workers: every one is resolved
    /// before the page answers, each HEADed once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_page_with_many_misses_shows_no_stored_size() {
        const N: usize = 40;
        let (engine, fake, _) = deltas_without_facts("rel", N).await;
        fake.clear();
        let page = list(&engine, "rel/").await;
        assert_eq!(page.objects.len(), N);
        assert!(
            page.objects.iter().all(|(_, m)| m.file_size == 65_536),
            "{:?}",
            page.objects
                .iter()
                .map(|(_, m)| m.file_size)
                .collect::<Vec<_>>()
        );
        assert!(page.facts_missing_keys.is_empty());
        assert!(fake.peak_in_flight("HEAD") <= S3Backend::MAX_CONCURRENT_HEADS);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let heads = fake.requests().iter().filter(|r| delta_heads(r)).count();
        assert_eq!(heads, N, "one HEAD per stored object");
    }

    /// A page whose HEADs do not finish in the page budget answers on time
    /// with the stored size of the unfinished entries, and heals them in
    /// the background: the next LIST is right.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_page_past_its_budget_answers_and_heals_in_the_background() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.fail("PUT", "/.dg/facts/", 500, "InternalError", u32::MAX);
        let writer = engine_on(test_support::for_test_endpoint(&endpoint));
        writer.create_bucket("b").await.unwrap();
        crate::deltaglider::store_deltas(&writer, "rel", 6).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while fake.faulted("PUT", "/.dg/facts/") < 6 {
            assert!(tokio::time::Instant::now() < deadline, "facts PUTs failed");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        fake.clear_faults();
        let mut slow = test_support::for_test_endpoint(&endpoint);
        slow.list_cache_scope = test_support::test_scope(&endpoint, "slow-node");
        slow.healer = FactsHealer::with_inline_budget(
            slow.client.clone(),
            std::time::Duration::from_millis(100),
        );
        let engine = engine_on(slow);
        fake.set_delay_ms("HEAD", 400);
        let started = std::time::Instant::now();
        let page = list(&engine, "rel/").await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "the page waited for every HEAD"
        );
        assert_eq!(page.objects.len(), 6);
        assert!(page.objects.iter().all(|(_, m)| m.file_size < 65_536));
        fake.set_delay_ms("HEAD", 0);
        let healed = async {
            loop {
                let page = list(&engine, "rel/").await;
                if page.objects.iter().all(|(_, m)| m.file_size == 65_536) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), healed)
            .await
            .expect("the background heal finished the page");
    }

    /// The production shape: bucket `b` writes plaintext, bucket `enc` has
    /// its own key, one backend. The wrapper holds a key, yet a delta in `b`
    /// is plaintext by the read rule, so a LIST shows its original size: from
    /// its `!!1` facts when it has them, from one HEAD when it has none.
    #[tokio::test]
    async fn a_plain_delta_on_a_backend_with_a_keyed_bucket_lists_its_original_size() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let writer = engine_on(test_support::for_test_endpoint(&endpoint));
        writer.create_bucket("b").await.unwrap();
        crate::deltaglider::store_deltas(&writer, "rel", 3).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while fake.stored_count("b/.dg/facts/") < 3 {
            assert!(tokio::time::Instant::now() < deadline, "facts written");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // With its facts (node 1), and after the facts are gone (node 2).
        let with_facts = bucket_keyed_reader(&endpoint, "keyed-node-1");
        let page = list(&with_facts, "rel/").await;
        assert_eq!(page.objects.len(), 3);
        let sizes = |p: &crate::deltaglider::ListObjectsPage| -> Vec<u64> {
            p.objects.iter().map(|(_, m)| m.file_size).collect()
        };
        assert_eq!(sizes(&page), [65_536; 3], "from the facts");
        assert_eq!(fake.remove_prefix("b/.dg/facts/"), 3);
        // Facts of another stored object (another ETag) are no answer.
        let plain = engine_on(test_support::for_test_endpoint(&endpoint));
        let stubs = plain
            .list_objects_lite("b", "rel/", None, 1000, None)
            .await
            .unwrap();
        for (key, meta) in &stubs.objects {
            let stale = crate::storage::listing_facts::asked_facts_key(
                &format!("{key}.delta"),
                &"1".repeat(32),
                meta.stored_size(),
                &LogicalFacts {
                    size: 7,
                    etag: "0".repeat(32),
                },
            )
            .expect("a facts key");
            fake.insert_object(&format!("b/{stale}"), &[], &[]);
        }
        let without = bucket_keyed_reader(&endpoint, "keyed-node-2");
        fake.clear();
        let page = list(&without, "rel/").await;
        assert_eq!(sizes(&page), [65_536; 3], "from one HEAD each");
        assert!(page.facts_missing_keys.is_empty(), "{page:?}");
        assert!(wait_stored(&fake, 6, "b/.dg/facts/").await, "facts kept");
        let other = bucket_keyed_reader(&endpoint, "keyed-node-3");
        fake.clear();
        let page = list(&other, "rel/").await;
        assert_eq!(sizes(&page), [65_536; 3]);
        assert_eq!(fake.requests().iter().filter(|r| delta_heads(r)).count(), 0);
    }

    /// Sealed deltas of bucket `b` with their `!!2` tokens and a planted
    /// `!!1` entry (size 7) for each: the writer's cache is not the
    /// reader's. Returns the number of deltas.
    async fn sealed_deltas_with_planted_facts(
        endpoint: &str,
        fake: &FakeS3,
        keep_tokens: bool,
        writer_config: crate::storage::EncryptionConfig,
    ) -> usize {
        let writer = wrapped(endpoint, "sealed-writer", writer_config);
        writer.create_bucket("b").await.unwrap();
        crate::deltaglider::store_deltas(&writer, "rel", 3).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while fake.stored_count("b/.dg/facts/") < 3 {
            assert!(tokio::time::Instant::now() < deadline, "tokens written");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if !keep_tokens {
            fake.remove_prefix("b/.dg/facts/");
        }
        plant_facts(endpoint, fake).await
    }

    fn sizes_of(p: &crate::deltaglider::ListObjectsPage) -> Vec<u64> {
        p.objects.iter().map(|(_, m)| m.file_size).collect()
    }

    /// A bucket that stopped encrypting keeps its key as a legacy key, so it
    /// may hold sealed objects: a planted `!!1` entry next to a sealed delta
    /// with a valid token lists the token's size, not the planted one.
    #[tokio::test]
    async fn a_planted_facts_object_never_sizes_a_sealed_delta() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let n =
            sealed_deltas_with_planted_facts(&endpoint, &fake, true, bucket_b_config(false)).await;
        assert_eq!(n, 3);
        let reader = wrapped(&endpoint, "stopped-reader", bucket_b_config(true));
        let page = list(&reader, "rel/").await;
        assert_eq!(sizes_of(&page), [65_536; 3]);
    }

    /// A `!!2` token beats a `!!1` entry for the same stored object on the
    /// raw backend too, whatever its caller trusts.
    #[tokio::test]
    async fn a_token_beats_planted_facts_on_the_raw_backend() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        sealed_deltas_with_planted_facts(&endpoint, &fake, true, bucket_b_config(false)).await;
        let raw = reader(&endpoint, "raw-reader");
        let page = list(&raw, "rel/").await;
        assert!(
            sizes_of(&page).iter().all(|s| *s != 7),
            "{:?}",
            sizes_of(&page)
        );
    }

    /// A bucket that may hold sealed objects ignores `!!1` entries even
    /// where no token exists: the entry cannot size the sealed delta.
    #[tokio::test]
    async fn a_bucket_that_stopped_encrypting_ignores_planted_facts() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        sealed_deltas_with_planted_facts(&endpoint, &fake, false, bucket_b_config(false)).await;
        let reader = wrapped(&endpoint, "stopped-reader-2", bucket_b_config(true));
        let page = list(&reader, "rel/").await;
        assert!(
            sizes_of(&page).iter().all(|s| *s != 7),
            "{:?}",
            sizes_of(&page)
        );
    }

    /// The backend key `kid` (no override); with `mode_none`, bucket `b`
    /// also has a plaintext override (`encryption: {mode: none}`: no key,
    /// no legacy key id of its own).
    fn backend_key_config(mode_none: bool) -> crate::storage::EncryptionConfig {
        use crate::storage::{BucketKeys, EncryptionConfig, EncryptionKey};
        let key = EncryptionKey::from_hex(&"12".repeat(32)).unwrap();
        EncryptionConfig {
            key: Some(key.clone()),
            key_id: Some("kid".into()),
            buckets: if mode_none {
                std::collections::HashMap::from([("b".to_string(), BucketKeys::default())])
            } else {
                Default::default()
            },
            keyring: std::collections::HashMap::from([("kid".to_string(), key)]),
            ..Default::default()
        }
    }

    /// A bucket that inherited the backend key, sealed deltas under it, and
    /// then got `encryption: {mode: none}` has no key of its own, yet it
    /// may hold sealed objects: a planted `!!1` entry next to a sealed delta
    /// without a token sizes nothing.
    #[tokio::test]
    async fn a_bucket_that_left_the_backend_key_ignores_planted_facts() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        sealed_deltas_with_planted_facts(&endpoint, &fake, false, backend_key_config(false)).await;
        let reader = wrapped(&endpoint, "left-reader", backend_key_config(true));
        let page = list(&reader, "rel/").await;
        assert!(
            sizes_of(&page).iter().all(|s| *s != 7),
            "{:?}",
            sizes_of(&page)
        );
    }

    /// The leader of a shared HEAD ends (its client left): a LIST that
    /// waited for it sends the HEAD itself and lists the original size.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_follower_heads_itself_when_the_leader_is_dropped() {
        let (engine, fake, _) = deltas_without_facts("rel", 3).await;
        let engine = Arc::new(engine);
        fake.set_delay_ms("HEAD", 600);
        let leader = {
            let engine = engine.clone();
            tokio::spawn(async move { list(&engine, "rel/").await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let follower = {
            let engine = engine.clone();
            tokio::spawn(async move { list(&engine, "rel/").await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        leader.abort();
        let page = tokio::time::timeout(std::time::Duration::from_secs(10), follower)
            .await
            .expect("the follower answered")
            .unwrap();
        assert_eq!(sizes_of(&page), [65_536; 3]);
    }

    /// A walk (a folder listing, a scan, a job) sends no inline HEAD: its
    /// misses list their stored size and are healed in the background. A
    /// client LIST of the same page lists the original sizes.
    #[tokio::test]
    async fn a_walk_sends_no_inline_head_but_a_client_list_does() {
        let (engine, fake, _) = deltas_without_facts("rel", 6).await;
        fake.set_delay_ms("HEAD", 300);
        fake.clear();
        let started = std::time::Instant::now();
        let page = walk(&engine, "rel/").await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(250),
            "a walk page waited for HEADs"
        );
        assert!(sizes_of(&page).iter().all(|s| *s < 65_536));
        // The background healer repairs them; a client LIST is right.
        fake.set_delay_ms("HEAD", 0);
        let healed = async {
            loop {
                let page = list(&engine, "rel/").await;
                if sizes_of(&page) == [65_536; 6] {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), healed)
            .await
            .expect("a client LIST is right once the heal ran");
    }

    /// LISTs of one page at the same time share the HEAD of each miss: one
    /// HEAD per stored object, however many LISTs wait for it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_lists_share_one_head_per_object() {
        const N: usize = 40;
        let (engine, fake, _) = deltas_without_facts("rel", N).await;
        fake.set_delay_ms("HEAD", 100);
        fake.clear();
        let pages = futures::future::join_all((0..5).map(|_| list(&engine, "rel/"))).await;
        for page in &pages {
            assert_eq!(sizes_of(page), [65_536; N]);
        }
        let heads = fake.requests().iter().filter(|r| delta_heads(r)).count();
        assert_eq!(heads, N, "one HEAD per stored object");
    }

    /// A backend that throttles the HEADs of a LIST does not fail the LIST:
    /// the entries list their stored size, the healer pauses, and a later
    /// LIST is right.
    #[tokio::test]
    async fn a_throttled_head_does_not_fail_the_list() {
        let (_, fake, endpoint) = deltas_without_facts("rel", 3).await;
        let mut s3 = test_support::for_test_endpoint(&endpoint);
        s3.list_cache_scope = test_support::test_scope(&endpoint, "throttled-node");
        let healer = s3.healer.clone();
        let engine = engine_on(s3);
        fake.fail("HEAD", ".delta", 503, "SlowDown", u32::MAX);
        let page = list(&engine, "rel/").await;
        assert_eq!(page.objects.len(), 3);
        assert!(page.objects.iter().all(|(_, m)| m.file_size < 65_536));
        assert!(healer.state.paused(), "a failed HEAD pauses the healer");
        // The backend recovers and the pause ends: the same page is right,
        // so the failed HEADs were not marked tried.
        fake.clear_faults();
        *healer.state.paused_until.lock().unwrap() = None;
        let page = list(&engine, "rel/").await;
        assert!(page.objects.iter().all(|(_, m)| m.file_size == 65_536));
    }

    /// The HEADs of a LIST run at most INLINE_CONCURRENCY at once, one per
    /// stored object.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn healing_is_bounded_and_heads_each_object_once() {
        let (engine, fake, _) = deltas_without_facts("rel", 24).await;
        fake.set_delay_ms("HEAD", 30);
        fake.clear();
        list(&engine, "rel/").await;
        assert!(
            wait_count(&fake, 24, delta_heads).await,
            "{:#?}",
            fake.requests()
        );
        assert!(fake.peak_in_flight("HEAD") <= INLINE_CONCURRENCY);
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
    ///
    /// The cleanup's first LIST is stalled and never answers until the end
    /// of the test: the cleanup cannot finish, whatever the host load, so
    /// facts that land can not have waited for it. (A wall-clock bound on
    /// "at once" failed on a loaded host.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn facts_converge_while_a_long_delete_cleanup_runs() {
        const N: usize = 20;
        let (_, fake, endpoint) = deltas_without_facts("rel", N).await;
        let mut s3 = test_support::for_test_endpoint(&endpoint);
        s3.list_cache_scope = test_support::test_scope(&endpoint, "busy-node");
        fake.stall("LIST", "prefix=.dg%2Ffacts%2Fe2e");
        // 40 deleted keys, one per directory: 40 facts LISTs, the first
        // one never answered.
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
        let page = list_with_heads(&busy, "rel/").await;
        assert!(page.objects.iter().all(|(_, m)| m.file_size == 65_536));
        let landed = wait_stored(&fake, N, "b/.dg/facts/rel/").await;
        assert!(landed, "{N} facts PUTs wait behind the cleanup");
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|r| r.contains("prefix=.dg%2Ffacts%2Fe2e"))
                .count(),
            0,
            "the cleanup moved on: it was not stalled"
        );
        let other = reader(&endpoint, "fresh-node");
        fake.clear();
        let page = list(&other, "rel/").await;
        assert!(page.objects.iter().all(|(_, m)| m.file_size == 65_536));
        assert!(page.facts_missing_keys.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(fake.requests().iter().filter(|r| delta_heads(r)).count(), 0);
        fake.release_stalls();
    }

    /// Prod (beshu, v2.0.9): deltas written by an older toolchain carry no
    /// `dg-md5`. A HEAD and a GET send an empty ETag (`""`) for them, and
    /// `facts_key` refused an empty logical ETag, so they never got facts:
    /// 180k facts misses and 164k HEADs in 2 hours, 58 facts PUTs. One
    /// heal now writes their facts with the ETag the HEAD sends, and a
    /// fresh node lists them like the HEAD shows them, with no HEAD.
    #[tokio::test]
    async fn a_delta_without_md5_heals_with_the_etag_its_head_sends() {
        const N: usize = 3;
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.insert_object("b/rel/reference.bin", &[7u8; 4096], &[]);
        for i in 0..N {
            let name = format!("app-1.7.{i}.zip");
            let size = (100_000 + i).to_string();
            let sha = format!("{i:064}");
            // Exactly the metadata set of the prod deltas: no dg-md5.
            let meta = [
                ("dg-created-at", "2024-03-01T10:00:00.000000Z"),
                ("dg-delta-cmd", "xdelta3 -e -9 -s reference.bin"),
                ("dg-delta-size", "333"),
                ("dg-file-sha256", sha.as_str()),
                ("dg-file-size", size.as_str()),
                ("dg-original-name", name.as_str()),
                ("dg-ref-key", "rel/reference.bin"),
                ("dg-ref-sha256", &"e".repeat(64)),
                ("dg-tool", "deltaglider/5.0.0"),
            ];
            fake.insert_object(&format!("b/rel/{name}.delta"), &[i as u8; 333], &meta);
        }
        let first = reader(&endpoint, "md5-node-1");
        fake.clear();
        let page = list(&first, "rel/").await;
        assert_eq!(page.objects.len(), N);
        assert!(wait_count(&fake, N, delta_heads).await);
        assert!(
            wait_stored(&fake, N, "b/.dg/facts/rel/").await,
            "no facts written: {:?}",
            fake.requests()
        );
        let fresh = reader(&endpoint, "md5-node-2");
        fake.clear();
        let page = list(&fresh, "rel/").await;
        assert!(page.facts_missing_keys.is_empty(), "{page:?}");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(fake.requests().iter().filter(|r| delta_heads(r)).count(), 0);
        for (key, listed) in &page.objects {
            let head = fresh.head("b", key).await.unwrap();
            assert_eq!(listed.file_size, head.file_size, "{key} size");
            assert_eq!(listed.etag(), head.etag(), "{key} etag");
        }
        assert_eq!(
            page.objects[0].1.etag(),
            "\"\"",
            "the ETag clients get today"
        );
    }

    /// Issue #120: a sealed delta has no `!!1` facts, and its HEAD learns
    /// none. The heal HEAD marks it sealed and writes nothing; the next LIST
    /// reads no facts for it and queues no HEAD, also after the one-minute
    /// "missing" window.
    #[tokio::test]
    async fn a_sealed_delta_is_headed_once_and_gets_no_facts() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.insert_object("b/sealed/reference.bin", &[7u8; 4096], &[]);
        let sha = "e".repeat(64);
        let meta = [
            ("dg-created-at", "2026-03-01T10:00:00.000000Z"),
            ("dg-delta-size", "333"),
            ("dg-file-sha256", sha.as_str()),
            ("dg-file-size", "1000"),
            ("dg-original-name", "app.zip"),
            ("dg-ref-path", "reference.bin"),
            ("dg-ref-sha256", sha.as_str()),
            ("dg-tool", "deltaglider/6.0.0"),
            ("user-dg-sealed", "2.opaque"),
        ];
        fake.insert_object("b/sealed/app.zip.delta", &[1u8; 333], &meta);
        let (engine, scope) = reader_and_scope(&endpoint, "sealed-node");
        fake.clear();
        list(&engine, "sealed/").await;
        assert!(wait_count(&fake, 1, delta_heads).await);
        let stored_etag = format!("{:x}", <md5::Md5 as md5::Digest>::digest([1u8; 333]));
        let id = StoredObjectId {
            scope: &scope,
            bucket: "b",
            key: "sealed/app.zip.delta",
            etag: &stored_etag,
            size: 333,
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !list_size_cache::known_sealed(&id) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(list_size_cache::known_sealed(&id));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !fake
                .requests()
                .iter()
                .any(|r| r.starts_with("PUT /b/.dg/facts/")),
            "{:?}",
            fake.requests()
        );
        fake.clear();
        list(&engine, "sealed/").await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(facts_lists(&fake), 0, "{:?}", fake.requests());
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

    /// An engine whose backend holds a key for bucket `enc` only: bucket `b`
    /// (and any other) writes plaintext. Its own listing-cache scope.
    fn bucket_keyed_reader(endpoint: &str, node: &str) -> DynEngine {
        use crate::storage::{BucketKeys, EncryptingBackend, EncryptionConfig, EncryptionKey};
        let mut s3 = test_support::for_test_endpoint(endpoint);
        s3.list_cache_scope = test_support::test_scope(endpoint, node);
        let key = EncryptionKey::from_hex(&"cd".repeat(32)).unwrap();
        let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(EncryptionConfig {
            buckets: std::collections::HashMap::from([(
                "enc".to_string(),
                BucketKeys {
                    key: Some(key.clone()),
                    key_id: Some("enc-id".into()),
                    ..Default::default()
                },
            )]),
            keyring: std::collections::HashMap::from([("enc-id".to_string(), key)]),
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

    /// An engine with a proxy-encryption key on the fake, with its own
    /// listing-cache scope.
    fn encrypted_reader(endpoint: &str, node: &str) -> DynEngine {
        use crate::storage::{EncryptingBackend, EncryptionConfig, EncryptionKey};
        let mut s3 = test_support::for_test_endpoint(endpoint);
        s3.list_cache_scope = test_support::test_scope(endpoint, node);
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

    /// Q2, under #120: with an encryption key, a LIST does not trust
    /// `!!1` facts (a backend writer can forge them), so it neither reads
    /// nor heals them. A plain object stored before the key lists its
    /// stored size and ETag (its logical ones), and no LIST, on any node,
    /// sends a background HEAD or writes facts for it.
    #[tokio::test]
    async fn plain_objects_under_an_encryption_key_send_no_background_requests() {
        const N: usize = 5;
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let writer = engine_on(test_support::for_test_endpoint(&endpoint));
        writer.create_bucket("b").await.unwrap();
        let mut sizes = Vec::new();
        for i in 0..N {
            let body = format!("plain object {i}");
            sizes.push(body.len() as u64);
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
        for node in ["enc-node-1", "enc-node-2"] {
            let reader = encrypted_reader(&endpoint, node);
            fake.clear();
            let page = list(&reader, "old/").await;
            assert_eq!(page.objects.len(), N);
            let listed: Vec<u64> = page.objects.iter().map(|(_, m)| m.file_size).collect();
            assert_eq!(listed, sizes, "{node}");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert!(
                fake.requests()
                    .iter()
                    .all(|r| !r.starts_with("HEAD ") && !r.starts_with("PUT /b/.dg/facts/")),
                "{node}: {:?}",
                fake.requests()
            );
        }
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

    /// THE decision of both healers for a delta without facts: a HEAD while
    /// it may, else the stored size and a background heal; never "no heal".
    #[test]
    fn decide_delta_miss_truth_table() {
        assert_eq!(decide_delta_miss(true), MissDecision::HeadNow);
        assert_eq!(decide_delta_miss(false), MissDecision::Keep { heal: true });
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

// SPDX-License-Identifier: BUSL-1.1

//! Background upkeep of the S3 listing facts (`storage::listing_facts`): the
//! facts writes of stored objects, and the requests that remove the entries
//! of deleted objects.
//!
//! A PUT does not write its facts object in its own request path (storage-8):
//! it queues the write, and the drain task below sends it. Until then a LIST
//! reports the stored size. The entries of an overwritten object are left to
//! the facts GC (`gc_bucket`), which deletes every entry that does not
//! describe the live object.
//!
//! A delete does not clean up in its own request path either. It queues the stored
//! key, and one background task per backend drains the queue in batches: it
//! reads each directory's facts range ONCE and deletes the matching entries
//! with batched `DeleteObjects` requests. A 1000-key `DeleteObjects` from a
//! client (or a lifecycle, replication or admin bulk delete, which delete key
//! by key) then costs a few requests, not 1000 LISTs. Cleanup is best effort
//! by design: an entry that is left behind never matches a later object.

use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use aws_sdk_s3::Client;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::listing_facts::{self, CleanupRead};
use super::s3::{put_facts_object, NativeEncryptionConfig, LISTING_FACTS_REQUESTS};

/// How long the task waits for more deletes before it flushes a batch.
const DEBOUNCE: Duration = Duration::from_millis(200);
/// Most stored keys in one flush.
const MAX_BATCH: usize = 10_000;
/// `DeleteObjects` takes at most 1000 keys.
const DELETE_CHUNK: usize = 1000;
/// Facts objects that one drain batch writes at once.
const FACTS_WRITE_CONCURRENCY: usize = 32;

/// A facts key with the backend's `LastModified` as `(secs, nanos)`.
type Listed = (String, Option<(i64, u32)>);

fn listed_of(o: &aws_sdk_s3::types::Object) -> Option<Listed> {
    Some((
        o.key()?.to_string(),
        o.last_modified().map(|t| (t.secs(), t.subsec_nanos())),
    ))
}

/// Delete facts keys, in `DeleteObjects` batches; one key at a time where
/// the backend refuses the batch request.
pub(super) async fn delete_facts_keys(client: &Client, bucket: &str, keys: Vec<String>) {
    for chunk in keys.chunks(DELETE_CHUNK) {
        let ids: Vec<ObjectIdentifier> = chunk
            .iter()
            .filter_map(|k| ObjectIdentifier::builder().key(k).build().ok())
            .collect();
        let Ok(delete) = Delete::builder().set_objects(Some(ids)).quiet(true).build() else {
            continue;
        };
        LISTING_FACTS_REQUESTS.with_label_values(&["delete"]).inc();
        let batched = client
            .delete_objects()
            .bucket(bucket)
            .delete(delete)
            .send()
            .await;
        if let Err(e) = batched {
            debug!("batched facts delete on {bucket} refused, deleting per key: {e:?}");
            for key in chunk {
                LISTING_FACTS_REQUESTS.with_label_values(&["delete"]).inc();
                if let Err(e) = client.delete_object().bucket(bucket).key(key).send().await {
                    debug!("stale listing facts {bucket}/{key} not deleted: {e:?}");
                }
            }
        }
    }
}

/// Every facts entry of one stored key (one prefix LIST).
async fn entries_of(client: &Client, bucket: &str, stored_key: &str) -> Vec<Listed> {
    let prefix = format!("{}!!", listing_facts::stored_prefix(stored_key));
    LISTING_FACTS_REQUESTS.with_label_values(&["list"]).inc();
    match client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(&prefix)
        .max_keys(100)
        .send()
        .await
    {
        Ok(r) => r.contents().iter().filter_map(listed_of).collect(),
        Err(e) => {
            debug!("listing facts of {bucket}/{stored_key} not read: {e:?}");
            Vec::new()
        }
    }
}

/// The stored object at a deleted key when the flush runs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LiveNow {
    /// Not read (no entry needs it, or the HEAD failed): keep what is unsure.
    Unknown,
    Gone,
    /// Written again (by any node): `(stored ETag, stored size)`.
    Is(String, u64),
}

/// The entries to delete for one deleted stored key. A key that was written
/// again after its delete was queued keeps its newest entry. With the
/// server time of the delete (`deleted_at`, the `Date` of its response), an
/// entry the server stored BEFORE that second goes. An entry of that second
/// or later may be a peer's write after the delete (another node, so no
/// local rewrite mark): it goes only when it does not describe the object
/// that is there now (`live`).
fn doomed(
    entries: &[Listed],
    rewritten: bool,
    deleted_at: Option<i64>,
    live: &LiveNow,
) -> Vec<String> {
    let newest = entries.iter().filter_map(|(_, t)| *t).max();
    entries
        .iter()
        .filter(|(_, t)| !(rewritten && t.is_some() && *t == newest))
        .filter(|(k, t)| match (deleted_at, t) {
            (Some(at), Some((secs, _))) if *secs >= at => match live {
                LiveNow::Unknown => false,
                LiveNow::Gone => true,
                LiveNow::Is(etag, size) => listing_facts::parse_facts_key(k).is_none_or(|e| {
                    e.stored_etag != etag.trim_matches('"') || e.stored_size != *size
                }),
            },
            _ => true,
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// Does any entry of this key need the live object to decide?
fn needs_live(entries: &[Listed], deleted_at: Option<i64>) -> bool {
    deleted_at.is_some_and(|at| entries.iter().any(|(_, t)| t.is_some_and(|(s, _)| s >= at)))
}

/// HEAD the stored key once.
async fn live_now(client: &Client, bucket: &str, stored_key: &str) -> LiveNow {
    super::s3::BACKEND_HEAD_REQUESTS.inc();
    match client
        .head_object()
        .bucket(bucket)
        .key(stored_key)
        .send()
        .await
    {
        Ok(h) => LiveNow::Is(
            h.e_tag().unwrap_or_default().to_string(),
            h.content_length().unwrap_or(0).max(0) as u64,
        ),
        Err(e)
            if crate::config_db_sync::is_object_absent(
                &crate::coordination::cas::sdk_error_signal(&e),
            ) =>
        {
            LiveNow::Gone
        }
        Err(_) => LiveNow::Unknown,
    }
}

/// One flush: the facts of every queued stored key of one bucket.
async fn flush_bucket(
    client: &Client,
    bucket: &str,
    deleted: HashMap<String, Option<i64>>,
    rewritten: &HashSet<String>,
) {
    let stored_keys: Vec<String> = deleted.keys().cloned().collect();
    let mut by_key: HashMap<String, Vec<Listed>> = HashMap::new();
    let mut per_key: Vec<String> = Vec::new();
    for read in listing_facts::plan_cleanup(stored_keys) {
        match read {
            CleanupRead::PerKey(keys) => per_key.extend(keys),
            CleanupRead::Range { scan, keys, pages } => {
                let wanted: HashSet<&str> = keys.iter().map(String::as_str).collect();
                let mut token: Option<String> = None;
                let mut done = false;
                for _ in 0..pages {
                    let mut request = client
                        .list_objects_v2()
                        .bucket(bucket)
                        .prefix(&scan.prefix)
                        .set_delimiter(scan.delimiter.map(String::from));
                    request = match &token {
                        Some(t) => request.continuation_token(t),
                        None => request.start_after(&scan.start_after),
                    };
                    LISTING_FACTS_REQUESTS.with_label_values(&["list"]).inc();
                    let Ok(resp) = request.send().await else {
                        break;
                    };
                    for o in resp.contents() {
                        let Some(listed) = listed_of(o) else { continue };
                        if scan.is_past(&listed.0) {
                            done = true;
                            break;
                        }
                        if let Some(e) = listing_facts::parse_facts_key(&listed.0) {
                            if wanted.contains(e.stored_key.as_str()) {
                                by_key.entry(e.stored_key).or_default().push(listed);
                            }
                        }
                    }
                    match resp.next_continuation_token() {
                        Some(t) if !done && resp.is_truncated().unwrap_or(false) => {
                            token = Some(t.to_string())
                        }
                        _ => {
                            done = true;
                            break;
                        }
                    }
                }
                if !done {
                    // Page budget spent (many foreign entries in between):
                    // the rest per key, never unbounded.
                    per_key.extend(keys.into_iter().filter(|k| !by_key.contains_key(k)));
                }
            }
        }
    }
    for k in per_key {
        let entries = entries_of(client, bucket, &k).await;
        by_key.insert(k, entries);
    }
    let mut delete = Vec::new();
    for (k, entries) in by_key {
        let at = deleted.get(&k).copied().flatten();
        let live = if needs_live(&entries, at) {
            live_now(client, bucket, &k).await
        } else {
            LiveNow::Unknown
        };
        delete.extend(doomed(&entries, rewritten.contains(&k), at, &live));
    }
    if !delete.is_empty() {
        delete_facts_keys(client, bucket, delete).await;
    }
}

/// Queued `(bucket, stored key)` pairs, and those of them that were written
/// again while their delete cleanup was queued.
#[derive(Default)]
struct QueueState {
    pending: HashSet<(String, String)>,
    rewritten: HashSet<(String, String)>,
}

type Rewritten = Arc<Mutex<QueueState>>;

/// One job of the drain task.
enum Queued {
    /// `(bucket, stored key, server time of the delete)`.
    Deleted(String, String, Option<i64>),
    /// Write the facts object `facts_key` into `bucket`.
    Write { bucket: String, facts_key: String },
}

/// Queue of facts writes, and of deleted stored keys whose facts the
/// background task removes.
pub(super) struct FactsCleanupQueue {
    tx: mpsc::UnboundedSender<Queued>,
    rewritten: Rewritten,
    /// The GC task never ends on its own: the queue owns it and aborts it
    /// on drop, else every engine rebuild leaked one with the old client.
    gc: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for FactsCleanupQueue {
    fn drop(&mut self) {
        if let Some(gc) = self.gc.take() {
            gc.abort();
        }
    }
}

impl FactsCleanupQueue {
    /// Start the drain task. Needs a tokio runtime (the S3 backend is built
    /// in one). The task ends when the queue is dropped, after a last flush.
    pub(super) fn start(client: Client, native: NativeEncryptionConfig) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let rewritten: Rewritten = Default::default();
        tokio::spawn(drain(client.clone(), native, rx, rewritten.clone()));
        let gc = crate::config::env_bool(GC_ENV, true).then(|| tokio::spawn(gc_loop(client)));
        Self { tx, rewritten, gc }
    }

    /// The object `stored_key` is gone (deleted at `deleted_at` on the
    /// server clock, when known): drop its facts soon.
    pub(super) fn enqueue(&self, bucket: &str, stored_key: &str, deleted_at: Option<i64>) {
        let pair = (bucket.to_string(), stored_key.to_string());
        if let Ok(mut st) = self.rewritten.lock() {
            st.rewritten.remove(&pair);
            st.pending.insert(pair.clone());
        }
        if self
            .tx
            .send(Queued::Deleted(pair.0, pair.1, deleted_at))
            .is_err()
        {
            debug!("facts cleanup queue closed; {bucket}/{stored_key} keeps its entries");
        }
    }

    /// `stored_key` was written again, with the facts object `facts_key`:
    /// write it in the background, and a queued delete cleanup of the key
    /// keeps its newest entry. The drain sends the writes of a batch before
    /// its delete cleanup, so that entry exists when the cleanup lists.
    pub(super) fn write(&self, bucket: &str, stored_key: &str, facts_key: String) {
        if let Ok(mut st) = self.rewritten.lock() {
            let pair = (bucket.to_string(), stored_key.to_string());
            if st.pending.contains(&pair) {
                st.rewritten.insert(pair);
            }
        }
        let job = Queued::Write {
            bucket: bucket.to_string(),
            facts_key,
        };
        if self.tx.send(job).is_err() {
            debug!("facts queue closed; {bucket}/{stored_key} lists its stored size");
        }
    }
}

async fn drain(
    client: Client,
    native: NativeEncryptionConfig,
    mut rx: mpsc::UnboundedReceiver<Queued>,
    rewritten: Rewritten,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(x) => batch.push(x),
                Err(mpsc::error::TryRecvError::Disconnected) => break,
                Err(mpsc::error::TryRecvError::Empty) => {
                    match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                        Ok(Some(x)) => batch.push(x),
                        _ => break,
                    }
                }
            }
        }
        let (writes, deletes): (Vec<Queued>, Vec<Queued>) = batch
            .into_iter()
            .partition(|q| matches!(q, Queued::Write { .. }));
        // Concurrently, one write per key: one task serves every client, so
        // a sequential loop capped the facts rate at one S3 round-trip each.
        let writes: HashSet<(String, String)> = writes
            .into_iter()
            .filter_map(|q| match q {
                Queued::Write { bucket, facts_key } => Some((bucket, facts_key)),
                Queued::Deleted(..) => None,
            })
            .collect();
        futures::stream::iter(writes)
            .for_each_concurrent(FACTS_WRITE_CONCURRENCY, |(bucket, facts_key)| {
                let (client, native) = (&client, &native);
                async move {
                    if let Err(e) = put_facts_object(client, native, &bucket, &facts_key).await {
                        warn!("listing facts {bucket}/{facts_key} not written: {e}");
                    }
                }
            })
            .await;
        let batch: Vec<(String, String, Option<i64>)> = deletes
            .into_iter()
            .filter_map(|q| match q {
                Queued::Deleted(b, k, at) => Some((b, k, at)),
                Queued::Write { .. } => None,
            })
            .collect();
        if batch.is_empty() {
            continue;
        }
        // A write after this point is not seen by the flush: its new entry
        // can be deleted, which only makes a LIST report the stored size
        // until a HEAD backfills it.
        let rewritten_now: HashSet<(String, String)> = match rewritten.lock() {
            Ok(mut st) => batch
                .iter()
                .map(|(b, k, _)| (b.clone(), k.clone()))
                .filter(|p| {
                    st.pending.remove(p);
                    st.rewritten.remove(p)
                })
                .collect(),
            Err(_) => HashSet::new(),
        };
        let mut by_bucket: HashMap<String, HashMap<String, Option<i64>>> = HashMap::new();
        for (bucket, key, at) in batch {
            // The same key deleted twice: the later delete rules.
            let slot = by_bucket
                .entry(bucket)
                .or_default()
                .entry(key)
                .or_insert(at);
            *slot = (*slot).max(at);
        }
        for (bucket, keys) in by_bucket {
            let rw: HashSet<String> = rewritten_now
                .iter()
                .filter(|(b, _)| *b == bucket)
                .map(|(_, k)| k.clone())
                .collect();
            flush_bucket(&client, &bucket, keys, &rw).await;
        }
    }
}

/// How often the garbage collection runs, and how much of each bucket's
/// facts namespace one run reads (it resumes there next time).
const GC_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const GC_FACTS_PAGES_PER_RUN: usize = 20;
/// Object-listing pages that may verify one facts page.
const GC_VERIFY_PAGES: usize = 5;
/// An entry younger than this is never collected (its PUT may be in flight).
const GC_GRACE_SECS: i64 = 3600;
/// `false` turns the garbage collection off (default on).
pub(super) const GC_ENV: &str = "DGP_LISTING_FACTS_GC";

/// Periodic garbage collection of facts entries whose object is gone
/// (`listing_facts::gc_doomed`). Every node runs it; deletes are idempotent,
/// and the grace keeps a fresh entry of any node.
async fn gc_loop(client: Client) {
    let mut cursors: HashMap<String, String> = HashMap::new();
    let mut tick = tokio::time::interval(GC_INTERVAL);
    tick.tick().await; // not at boot
    loop {
        tick.tick().await;
        let Ok(resp) = client.list_buckets().send().await else {
            continue;
        };
        for bucket in resp.buckets().iter().filter_map(|b| b.name()) {
            let from = cursors.remove(bucket);
            let now = chrono::Utc::now().timestamp();
            if let Some(next) = gc_bucket(
                &client,
                bucket,
                from,
                now,
                GC_FACTS_PAGES_PER_RUN,
                GC_GRACE_SECS,
            )
            .await
            {
                cursors.insert(bucket.to_string(), next);
            }
        }
    }
}

/// One GC pass over at most `pages` facts pages of `bucket`, from `from`
/// (a facts key). Returns where the next pass resumes, `None` at the end.
pub(super) async fn gc_bucket(
    client: &Client,
    bucket: &str,
    from: Option<String>,
    now: i64,
    pages: usize,
    grace_secs: i64,
) -> Option<String> {
    let mut cursor = from;
    for _ in 0..pages {
        LISTING_FACTS_REQUESTS.with_label_values(&["list"]).inc();
        let resp = client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(listing_facts::FACTS_ROOT)
            .set_start_after(cursor.clone())
            .send()
            .await
            .ok()?;
        let candidates: Vec<listing_facts::GcCandidate> = resp
            .contents()
            .iter()
            .filter_map(listed_of)
            .map(|(key, t)| listing_facts::GcCandidate {
                entry: listing_facts::parse_facts_key(&key),
                modified: t.map(|(secs, _)| secs),
                key,
            })
            .collect();
        let last_key = candidates.last()?.key.clone();
        let stored: Vec<&str> = candidates
            .iter()
            .filter_map(|c| c.entry.as_ref().map(|e| e.stored_key.as_str()))
            .collect();
        if let (Some(first), Some(last)) = (stored.iter().min(), stored.iter().max()) {
            let (live, through) = live_objects(client, bucket, first, last).await;
            let doomed =
                listing_facts::gc_doomed(&candidates, &live, through.as_deref(), now, grace_secs);
            if !doomed.is_empty() {
                debug!(
                    "facts GC on {bucket}: {} entries of gone objects",
                    doomed.len()
                );
                delete_facts_keys(client, bucket, doomed).await;
            }
        }
        if !resp.is_truncated().unwrap_or(false) {
            return None;
        }
        cursor = Some(last_key);
    }
    cursor
}

/// The stored objects from `first` to `last` (key -> (ETag, size)), and
/// the key the listing is complete through (`None`: nothing verified).
async fn live_objects(
    client: &Client,
    bucket: &str,
    first: &str,
    last: &str,
) -> (HashMap<String, (String, u64)>, Option<String>) {
    let mut live = HashMap::new();
    let mut token: Option<String> = None;
    let mut read_through: Option<String> = None;
    let mut start_after = listing_facts::start_before(first);
    for _ in 0..GC_VERIFY_PAGES {
        let mut request = client.list_objects_v2().bucket(bucket);
        request = match token.take() {
            Some(t) => request.continuation_token(t),
            None => request.start_after(&start_after),
        };
        let Ok(resp) = request.send().await else {
            return (live, None);
        };
        for o in resp.contents() {
            let Some(k) = o.key() else { continue };
            if k > last {
                return (live, Some(last.to_string()));
            }
            live.insert(
                k.to_string(),
                (
                    o.e_tag().unwrap_or_default().to_string(),
                    o.size().unwrap_or(0).max(0) as u64,
                ),
            );
            read_through = Some(k.to_string());
        }
        match resp.next_continuation_token() {
            Some(t) if resp.is_truncated().unwrap_or(false) => {
                // A page that ends in the facts namespace jumps past it.
                let page_last = resp.contents().last().and_then(|o| o.key());
                match page_last.and_then(listing_facts::skip_past_facts) {
                    Some(past) => start_after = past.to_string(),
                    None => token = Some(t.to_string()),
                }
            }
            // The bucket ends here: everything up to `last` is read.
            _ => return (live, Some(last.to_string())),
        }
    }
    // Page budget spent below `last`: only what was read is verified.
    (live, read_through)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// storage-3: an engine rebuild drops the S3 backend and its queue. The
    /// GC task must end with it, or every rebuild leaks one task that keeps
    /// the old client and credentials.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_queue_ends_its_background_tasks() {
        let conf = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "a", "b", None, None, "t",
            ))
            .endpoint_url("http://127.0.0.1:9")
            .build();
        let client = Client::from_conf(conf);
        let metrics = tokio::runtime::Handle::current().metrics();
        let before = metrics.num_alive_tasks();
        for _ in 0..5 {
            drop(FactsCleanupQueue::start(
                client.clone(),
                NativeEncryptionConfig::None,
            ));
        }
        // Aborted and closed tasks end on their next poll.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while metrics.num_alive_tasks() > before && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        assert_eq!(metrics.num_alive_tasks(), before, "tasks leaked");
    }

    /// storage-8 moved the facts write off the PUT path into this one drain
    /// task. It wrote a batch one object at a time, so every client's facts
    /// queued behind one S3 round-trip each: on a busy node the unbounded
    /// queue grew and listings showed stored sizes for longer and longer.
    /// A batch is written concurrently now, one write per key.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_batch_of_facts_writes_runs_concurrently() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.set_put_delay_ms(100);
        let conf = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "a", "b", None, None, "t",
            ))
            .force_path_style(true)
            .endpoint_url(endpoint)
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
            .build();
        let queue = FactsCleanupQueue::start(Client::from_conf(conf), NativeEncryptionConfig::None);
        const N: usize = 32;
        let started = std::time::Instant::now();
        for i in 0..N {
            queue.write("b", &format!("p/k{i}"), format!(".dg/facts/p/k{i}"));
        }
        // The same key twice in one batch is written once.
        queue.write("b", "p/k0", ".dg/facts/p/k0".to_string());
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let puts = || {
            fake.requests()
                .iter()
                .filter(|r| r.starts_with("PUT /b/.dg/facts/p/"))
                .count()
        };
        while puts() < N && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Let a second write of the repeated key show up if the drain sent one.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(puts(), N, "every key written, a repeated key once");
        assert!(
            fake.peak_puts_in_flight() > 1,
            "facts writes ran one at a time"
        );
        // One at a time would take N x 100 ms = 3.2 s.
        assert!(
            started.elapsed() < Duration::from_millis(1600),
            "{:?} for {N} writes",
            started.elapsed()
        );
    }

    #[test]
    fn a_rewritten_key_keeps_its_newest_entry() {
        let e = |k: &str, t: i64| (k.to_string(), Some((t, 0)));
        let entries = vec![e("a", 1), e("b", 3), e("c", 2)];
        let unknown = LiveNow::Unknown;
        assert_eq!(doomed(&entries, false, None, &unknown), vec!["a", "b", "c"]);
        assert_eq!(doomed(&entries, true, None, &unknown), vec!["a", "c"]);
    }

    /// C3: node A deletes a key and queues its cleanup; node B writes the
    /// key again before A's flush. B's entry is newer than the delete (on
    /// the server clock), so A must keep it. A saw no local rewrite and
    /// deleted every entry: B's object then lists with its stored size.
    #[test]
    fn a_peer_write_after_the_delete_keeps_its_entry() {
        let e = |k: &str, t: i64| (k.to_string(), Some((t, 0)));
        let entries = vec![e("old", 1), e("peer-new", 5)];
        assert_eq!(
            doomed(&entries, false, Some(3), &LiveNow::Unknown),
            vec!["old"]
        );
        assert!(needs_live(&entries, Some(3)));
        assert!(!needs_live(&entries, Some(6)));
    }

    /// An entry of the delete's second (or later) goes when the object is
    /// gone, or when it does not describe the object that is there now.
    #[test]
    fn a_recent_entry_is_judged_by_the_live_object() {
        let logical = super::super::list_size_cache::LogicalFacts {
            size: 40,
            etag: "abc".into(),
        };
        let mine = listing_facts::facts_key("d/k.delta", "e1", 4, &logical).unwrap();
        let peer = listing_facts::facts_key("d/k.delta", "e2", 5, &logical).unwrap();
        let entries = vec![(mine.clone(), Some((3, 0))), (peer.clone(), Some((3, 0)))];
        assert_eq!(
            doomed(&entries, false, Some(3), &LiveNow::Gone),
            vec![mine.clone(), peer.clone()]
        );
        assert_eq!(
            doomed(&entries, false, Some(3), &LiveNow::Is("\"e2\"".into(), 5)),
            vec![mine]
        );
        assert!(doomed(&entries, false, Some(3), &LiveNow::Unknown).is_empty());
    }
}

// SPDX-License-Identifier: BUSL-1.1

//! Cleanup of the S3 listing facts (`storage::listing_facts`): the requests
//! that remove the entries of overwritten and deleted objects.
//!
//! A delete does not clean up in its own request path. It queues the stored
//! key, and one background task per backend drains the queue in batches: it
//! reads each directory's facts range ONCE and deletes the matching entries
//! with batched `DeleteObjects` requests. A 1000-key `DeleteObjects` from a
//! client (or a lifecycle, replication or admin bulk delete, which delete key
//! by key) then costs a few requests, not 1000 LISTs. Cleanup is best effort
//! by design: an entry that is left behind never matches a later object.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use aws_sdk_s3::Client;
use tokio::sync::mpsc;
use tracing::debug;

use super::listing_facts::{self, CleanupRead};
use super::s3::LISTING_FACTS_REQUESTS;

/// How long the task waits for more deletes before it flushes a batch.
const DEBOUNCE: Duration = Duration::from_millis(200);
/// Most stored keys in one flush.
const MAX_BATCH: usize = 10_000;
/// `DeleteObjects` takes at most 1000 keys.
const DELETE_CHUNK: usize = 1000;

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

/// After a PUT wrote `mine`: delete the entries of the same stored key that
/// the backend stored before it (`listing_facts::stale_after_write`). A newer
/// entry from a concurrent overwrite on another node survives.
pub(super) async fn cleanup_after_write(
    client: &Client,
    bucket: &str,
    stored_key: &str,
    mine: &str,
) {
    let listed = entries_of(client, bucket, stored_key).await;
    let stale = listing_facts::stale_after_write(&listed, mine);
    if !stale.is_empty() {
        delete_facts_keys(client, bucket, stale).await;
    }
}

/// The entries to delete for one deleted stored key. A key that was written
/// again after its delete was queued keeps its newest entry. With the
/// server time of the delete (`deleted_at`, the `Date` of its response),
/// only entries the server stored BEFORE that second go: an entry of a
/// peer's write after the delete (another node, so no local rewrite mark)
/// stays. An entry of the same second stays too; it is garbage at worst.
fn doomed(entries: &[Listed], rewritten: bool, deleted_at: Option<i64>) -> Vec<String> {
    let newest = entries.iter().filter_map(|(_, t)| *t).max();
    entries
        .iter()
        .filter(|(_, t)| !(rewritten && t.is_some() && *t == newest))
        .filter(|(_, t)| match (deleted_at, t) {
            (Some(at), Some((secs, _))) => *secs < at,
            _ => true,
        })
        .map(|(k, _)| k.clone())
        .collect()
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
        delete.extend(doomed(&entries, rewritten.contains(&k), at));
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

/// `(bucket, stored key, server time of the delete)`.
type Queued = (String, String, Option<i64>);

/// Queue of deleted stored keys whose facts the background task removes.
pub(super) struct FactsCleanupQueue {
    tx: mpsc::UnboundedSender<Queued>,
    rewritten: Rewritten,
}

impl FactsCleanupQueue {
    /// Start the drain task. Needs a tokio runtime (the S3 backend is built
    /// in one). The task ends when the queue is dropped, after a last flush.
    pub(super) fn start(client: Client) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let rewritten: Rewritten = Default::default();
        tokio::spawn(drain(client, rx, rewritten.clone()));
        Self { tx, rewritten }
    }

    /// The object `stored_key` is gone (deleted at `deleted_at` on the
    /// server clock, when known): drop its facts soon.
    pub(super) fn enqueue(&self, bucket: &str, stored_key: &str, deleted_at: Option<i64>) {
        let pair = (bucket.to_string(), stored_key.to_string());
        if let Ok(mut st) = self.rewritten.lock() {
            st.rewritten.remove(&pair);
            st.pending.insert(pair.clone());
        }
        if self.tx.send((pair.0, pair.1, deleted_at)).is_err() {
            debug!("facts cleanup queue closed; {bucket}/{stored_key} keeps its entries");
        }
    }

    /// `stored_key` got a new facts entry: a queued delete cleanup of it
    /// keeps the newest entry.
    pub(super) fn note_rewrite(&self, bucket: &str, stored_key: &str) {
        if let Ok(mut st) = self.rewritten.lock() {
            let pair = (bucket.to_string(), stored_key.to_string());
            if st.pending.contains(&pair) {
                st.rewritten.insert(pair);
            }
        }
    }
}

async fn drain(
    client: Client,
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
            let slot = by_bucket.entry(bucket).or_default().entry(key).or_insert(at);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewritten_key_keeps_its_newest_entry() {
        let e = |k: &str, t: i64| (k.to_string(), Some((t, 0)));
        let entries = vec![e("a", 1), e("b", 3), e("c", 2)];
        assert_eq!(doomed(&entries, false, None), vec!["a", "b", "c"]);
        assert_eq!(doomed(&entries, true, None), vec!["a", "c"]);
    }

    /// C3: node A deletes a key and queues its cleanup; node B writes the
    /// key again before A's flush. B's entry is newer than the delete (on
    /// the server clock), so A must keep it. A saw no local rewrite and
    /// deleted every entry: B's object then lists with its stored size.
    #[test]
    fn a_peer_write_after_the_delete_keeps_its_entry() {
        let e = |k: &str, t: i64| (k.to_string(), Some((t, 0)));
        let entries = vec![e("old", 1), e("peer-new", 5)];
        assert_eq!(doomed(&entries, false, Some(3)), vec!["old"]);
    }
}

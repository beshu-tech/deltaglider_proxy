// SPDX-License-Identifier: BUSL-1.1

//! Mirror deletes: remove the destination copies of source keys that are
//! gone, many at once. Shared by the event consumer (a drained batch of
//! delete events) and the reconcile walk (a finished folder).
//!
//! Before a destination key goes, its source key is checked once more: a
//! key re-created at the source since the event (or the walk's listing) is
//! kept. The check reads a listing of the source FOLDER, from just before
//! its first key, instead of one HEAD per key (two or three requests each
//! on S3). The deletes then go through `DeltaGliderEngine::delete_batch`,
//! which checks each destination folder once for a reference.bin to
//! reclaim, instead of once per key.

use crate::deltaglider::{DeleteItem, DeleteOutcome, DynEngine, EngineError};
use crate::transfer::CopyClass;
use futures::StreamExt;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Source folders re-checked at the same time.
const RECHECK_CONCURRENCY: usize = 4;
/// Keys per re-check listing page.
const RECHECK_PAGE: u32 = 1000;

/// One destination key to delete because its source key is gone.
#[derive(Debug, Clone)]
pub(crate) struct MirrorDelete {
    /// The source key, as a client names it.
    pub src_key: String,
    /// The destination key, as a client names it.
    pub dest_key: String,
}

/// Why one [`MirrorDelete`] did not happen.
#[derive(Debug, Clone)]
pub(crate) struct DeleteFailure {
    pub class: CopyClass,
    pub message: String,
    /// The key itself can never be stored or deleted as written (an invalid
    /// key): a retry cannot help.
    pub key_unusable: bool,
}

impl DeleteFailure {
    fn engine(context: &str, e: &EngineError) -> Self {
        use crate::storage::StorageError;
        let key_unusable = matches!(
            e,
            EngineError::InvalidArgument(_)
                | EngineError::Storage(StorageError::InvalidKey(_))
                | EngineError::Storage(StorageError::MetadataTooLarge(_))
        );
        Self {
            class: CopyClass::of_engine(e),
            message: format!("{context}: {e}"),
            key_unusable,
        }
    }
}

/// What happened to one [`MirrorDelete`].
#[derive(Debug, Clone)]
pub(crate) enum MirrorOutcome {
    /// The destination key is gone; `existed`: this call deleted it.
    Deleted { existed: bool },
    /// The source holds the key again: the destination copy stays.
    SourceBack,
    /// The re-check or the delete failed; the destination key may remain.
    Failed(DeleteFailure),
}

/// The folder a key lives in: everything up to its last `/` (`""` at the
/// bucket root). The engine's delimited listing of it holds the key.
fn folder_of(key: &str) -> &str {
    key.rfind('/').map_or("", |i| &key[..=i])
}

/// A listing token that starts a listing just BEFORE `key`: the key minus
/// its last character, a proper prefix, so it sorts below `key` and only
/// keys that sort between the two come first. `None` at the very start of
/// the listing.
fn start_before(key: &str) -> Option<&str> {
    let cut = key.char_indices().next_back().map_or(0, |(i, _)| i);
    (cut > 0).then(|| &key[..cut])
}

/// Which of `keys` (all directly in `folder`) the source holds now, read
/// from delimited listings of `folder`. Each listing starts just before the
/// first key not yet answered and answers every key up to the end of its
/// page; at most `max_pages` of them. Returns the keys the source holds and
/// the keys no listing reached (the caller HEADs those).
async fn listed_presence(
    engine: &DynEngine,
    bucket: &str,
    folder: &str,
    keys: &BTreeSet<String>,
    max_pages: usize,
) -> Result<(BTreeSet<String>, BTreeSet<String>), EngineError> {
    let mut pending: BTreeSet<String> = keys.clone();
    let mut present = BTreeSet::new();
    let mut pages = 0;
    while pages < max_pages {
        let Some(first) = pending.first().cloned() else {
            break;
        };
        if let Some(m) = engine.metrics() {
            m.replication_list_calls_total.inc();
        }
        let page = engine
            .list_objects(
                bucket,
                folder,
                Some("/"),
                RECHECK_PAGE,
                start_before(&first),
                false,
            )
            .await?;
        pages += 1;
        for (key, _) in &page.objects {
            if pending.remove(key) {
                present.insert(key.clone());
            }
        }
        match page.next_continuation_token {
            // Everything up to the token was listed: what is still pending
            // there is absent.
            Some(reached) if page.is_truncated => pending.retain(|k| k.as_str() > reached.as_str()),
            // The folder's end: everything still pending is absent.
            _ => pending.clear(),
        }
    }
    Ok((present, pending))
}

/// Which of `keys` the source holds now: listings first, a HEAD for each
/// key the listings did not reach. `Err`: the listing failed (nothing is
/// known); a failed HEAD fails only its key.
async fn source_presence(
    engine: &DynEngine,
    bucket: &str,
    folder: &str,
    keys: &BTreeSet<String>,
) -> Result<HashMap<String, Result<bool, DeleteFailure>>, DeleteFailure> {
    // Never more listing pages than about 1 per 100 keys: a few deletes
    // spread over a huge folder HEAD their keys instead.
    let max_pages = 1 + keys.len() / 100;
    let (present, unreached) = listed_presence(engine, bucket, folder, keys, max_pages)
        .await
        .map_err(|e| DeleteFailure::engine("source re-check listing failed", &e))?;
    let mut out: HashMap<String, Result<bool, DeleteFailure>> = keys
        .iter()
        .filter(|k| !unreached.contains(*k))
        .map(|k| (k.clone(), Ok(present.contains(k))))
        .collect();
    for key in unreached {
        if let Some(m) = engine.metrics() {
            m.replication_head_calls_total.inc();
        }
        let held = match engine.head(bucket, &key).await {
            Ok(_) => Ok(true),
            Err(e) if e.is_not_found() => Ok(false),
            Err(e) => Err(DeleteFailure::engine("source re-check head failed", &e)),
        };
        out.insert(key, held);
    }
    Ok(out)
}

/// Delete the destination copies of `items` whose source key is still gone,
/// re-checked folder by folder. One outcome per item, in input order.
pub(crate) async fn delete_mirrored(
    engine: &DynEngine,
    src_bucket: &str,
    dst_bucket: &str,
    items: &[MirrorDelete],
) -> Vec<MirrorOutcome> {
    let mut folders: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for item in items {
        folders
            .entry(folder_of(&item.src_key))
            .or_default()
            .insert(item.src_key.clone());
    }
    // The futures are made up front: a closure inside the stream made the
    // future of a spawned caller fail rustc's `Send` check.
    let checks: Vec<_> = folders
        .into_iter()
        .map(|(folder, keys)| async move {
            let presence = source_presence(engine, src_bucket, folder, &keys).await;
            (keys, presence)
        })
        .collect();
    let checked: Vec<_> = futures::stream::iter(checks)
        .buffer_unordered(RECHECK_CONCURRENCY)
        .collect()
        .await;
    let mut verdict: HashMap<String, Result<bool, DeleteFailure>> = HashMap::new();
    for (keys, presence) in checked {
        match presence {
            Ok(map) => verdict.extend(map),
            Err(failure) => verdict.extend(keys.into_iter().map(|k| (k, Err(failure.clone())))),
        }
    }

    let mut outcomes: Vec<Option<MirrorOutcome>> = vec![None; items.len()];
    let mut to_delete: Vec<usize> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        match verdict.get(&item.src_key) {
            Some(Ok(false)) => to_delete.push(i),
            Some(Ok(true)) => outcomes[i] = Some(MirrorOutcome::SourceBack),
            Some(Err(failure)) => outcomes[i] = Some(MirrorOutcome::Failed(failure.clone())),
            None => {
                outcomes[i] = Some(MirrorOutcome::Failed(DeleteFailure {
                    class: CopyClass::Transient,
                    message: "source re-check gave no answer".to_string(),
                    key_unusable: false,
                }))
            }
        }
    }
    let batch: Vec<DeleteItem<'_>> = to_delete
        .iter()
        .map(|&i| DeleteItem::new(items[i].dest_key.clone()))
        .collect();
    let deleted = engine
        .delete_batch(dst_bucket, batch, Default::default())
        .await;
    for (&i, outcome) in to_delete.iter().zip(deleted) {
        outcomes[i] = Some(match outcome {
            DeleteOutcome::Deleted(_) => MirrorOutcome::Deleted { existed: true },
            DeleteOutcome::NotFound => MirrorOutcome::Deleted { existed: false },
            DeleteOutcome::Failed(e) => {
                MirrorOutcome::Failed(DeleteFailure::engine("destination delete failed", &e))
            }
            // No condition, no hook: neither happens.
            DeleteOutcome::Changed | DeleteOutcome::Skipped => {
                MirrorOutcome::Failed(DeleteFailure {
                    class: CopyClass::Transient,
                    message: "destination delete not done".to_string(),
                    key_unusable: false,
                })
            }
        });
    }
    outcomes
        .into_iter()
        .map(|o| {
            o.unwrap_or(MirrorOutcome::Failed(DeleteFailure {
                class: CopyClass::Transient,
                message: "no outcome".to_string(),
                key_unusable: false,
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_and_start_token() {
        assert_eq!(folder_of("a/b/c.bin"), "a/b/");
        assert_eq!(folder_of("c.bin"), "");
        assert_eq!(start_before("a/b/c.bin"), Some("a/b/c.bi"));
        assert_eq!(start_before("a/x"), Some("a/"));
        assert_eq!(start_before("x"), None);
        // A multi-byte last character is cut whole.
        assert_eq!(start_before("a/é"), Some("a/"));
        // The token sorts below its key, never above.
        for k in ["a/b/c.bin", "a/x", "a/é", "z"] {
            assert!(start_before(k).is_none_or(|t| t < k), "{k}");
        }
    }
}

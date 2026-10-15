// SPDX-License-Identifier: BUSL-1.1

//! S3 ListObjects: prefix filtering, delimiter collapsing, pagination, and the
//! deltaspace reference scan.

use super::*;
use futures::StreamExt;

/// Reference-metadata reads (HEADs on S3) in flight at once in
/// [`DeltaGliderEngine::list_deltaspace_references`]: below the S3
/// backend's own HEAD bound, so one savings scan never bursts a backend.
const REFERENCE_HEAD_CONCURRENCY: usize = 8;

/// Apply continuation-token filtering and max-keys truncation to a sorted list.
/// Returns `(is_truncated, next_continuation_token)`.
fn paginate_sorted<T>(
    items: &mut Vec<T>,
    max_keys: u32,
    continuation_token: Option<&str>,
    sort_key: impl Fn(&T) -> &String,
) -> (bool, Option<String>) {
    if let Some(token) = continuation_token {
        items.retain(|item| sort_key(item).as_str() > token);
    }
    let max = max_keys as usize;
    let is_truncated = items.len() > max;
    if is_truncated {
        items.truncate(max);
    }
    let next_token = if is_truncated {
        items.last().map(|item| sort_key(item).clone())
    } else {
        None
    };
    (is_truncated, next_token)
}

/// Result of interleaving objects and common prefixes with pagination.
pub(crate) struct InterleavedPage<O> {
    pub objects: Vec<(String, O)>,
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

/// Interleave objects and common prefixes into a single sorted list, apply
/// continuation-token filtering and max-keys pagination, then split back.
///
/// S3 ListObjectsV2 counts both objects and common prefixes toward max-keys
/// and requires lexicographic ordering across both sets. This function is the
/// single source of truth for that logic (used by engine, S3 backend, and
/// filesystem backend).
pub(crate) fn interleave_and_paginate<O>(
    objects: Vec<(String, O)>,
    common_prefixes: Vec<String>,
    max_keys: u32,
    continuation_token: Option<&str>,
) -> InterleavedPage<O> {
    enum Entry<T> {
        Obj(String, T),
        Prefix(String),
    }

    let mut entries: Vec<(String, Entry<O>)> =
        Vec::with_capacity(objects.len() + common_prefixes.len());
    for (key, obj) in objects {
        entries.push((key.clone(), Entry::Obj(key, obj)));
    }
    for cp in common_prefixes {
        entries.push((cp.clone(), Entry::Prefix(cp)));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Apply continuation_token: skip entries <= token.
    if let Some(token) = continuation_token {
        entries.retain(|e| e.0.as_str() > token);
    }

    let max = max_keys as usize;
    let is_truncated = entries.len() > max;
    if entries.len() > max {
        entries.truncate(max);
    }
    let next_token = if is_truncated {
        entries.last().map(|(key, _)| key.clone())
    } else {
        None
    };

    let mut final_objects = Vec::new();
    let mut final_prefixes = Vec::new();
    for (_, entry) in entries {
        match entry {
            Entry::Obj(key, obj) => final_objects.push((key, obj)),
            Entry::Prefix(p) => final_prefixes.push(p),
        }
    }

    InterleavedPage {
        objects: final_objects,
        common_prefixes: final_prefixes,
        is_truncated,
        next_continuation_token: next_token,
    }
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// Does a `metadata=true` LIST entry need a HEAD? Only when the listing
    /// does not know its logical size: a facts miss (`StoredOnly`: a delta
    /// stub, or a ciphertext on an encrypted backend, whose durable listing
    /// facts are missing). Every other entry lists its storage facts
    /// (`dg-note`, `dg-delta-size`) from the listing alone: the stored form
    /// is in the listed key (`k.delta` or `k`), and a delta's stored size is
    /// the listed size of its `.delta` object. A delta-eligible key stored
    /// as a passthrough object needs no HEAD either: the listing proves its
    /// form. A sealed delta's plaintext size comes from its listing token
    /// with its logical facts. Pure; unit-tested.
    pub(super) fn list_entry_needs_head(size: crate::storage::list_size_cache::ListedSize) -> bool {
        !size.is_known()
    }

    /// A delta entry built from LIST data alone (no HEAD) — the `delta_stub`
    /// shape with empty `ref_sha256`. Its `file_size` is the STORED (delta)
    /// size, not the original, so it must never be cached as authoritative.
    pub(super) fn is_unresolved_delta_stub(meta: &FileMetadata) -> bool {
        meta.is_unresolved_delta_stub()
    }

    /// Returns `true` if a local prefix (bucket-relative) could contain keys
    /// matching the given user prefix.
    #[cfg(test)]
    pub(super) fn local_prefix_could_match(local_prefix: &str, prefix: &str) -> bool {
        if prefix.is_empty() {
            return true;
        }
        if local_prefix.is_empty() {
            // Root-level keys are bare filenames (no '/'). They can only match
            // a prefix that doesn't contain '/' (e.g. prefix="app" matches "app.zip").
            return !prefix.contains('/');
        }
        let lp_slash = format!("{}/", local_prefix);
        // Include if: the local prefix starts with the user prefix (prefix is broader),
        // OR the user prefix drills into this local prefix (prefix is narrower/equal).
        lp_slash.starts_with(prefix) || prefix.starts_with(&lp_slash)
    }

    /// S3 ListObjects — the single owner of prefix filtering, delimiter collapsing,
    /// and pagination. All three are coupled (CommonPrefixes count toward max-keys
    /// and must be deduplicated across pages), so they must live in one place.
    /// `list_objects_lite` lists the page, `finish_listed_page` completes its
    /// sizes (and its metadata when `metadata`).
    #[instrument(skip(self))]
    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys_raw: u32,
        continuation_token: Option<&str>,
        metadata: bool,
    ) -> Result<ListObjectsPage, EngineError> {
        let mut page = self
            .list_objects_lite(bucket, prefix, delimiter, max_keys_raw, continuation_token)
            .await?;
        self.finish_listed_page(bucket, &mut page, metadata).await?;
        Ok(page)
    }

    /// One [`Self::list_objects`] page as the storage lists it: STORED sizes
    /// (no listing-size cache, no listing facts, no HEAD), and an empty
    /// `facts_missing_keys`. A caller that drops most entries (the filtered
    /// LIST of a restricted user) reads pages with this and completes only
    /// the page it returns with `finish_listed_page`.
    pub(crate) async fn list_objects_lite(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys_raw: u32,
        continuation_token: Option<&str>,
    ) -> Result<ListObjectsPage, EngineError> {
        Ok(self
            .list_objects_lite_counted(bucket, prefix, delimiter, max_keys_raw, continuation_token)
            .await?
            .0)
    }

    /// [`Self::list_objects_lite`], and the requests that the page sent to
    /// the backend (`DelegatedListResult::upstream_requests`; 0 from a
    /// backend that sends none, and from the in-memory paging fallback).
    pub(crate) async fn list_objects_lite_counted(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys_raw: u32,
        continuation_token: Option<&str>,
    ) -> Result<(ListObjectsPage, u32), EngineError> {
        let (page, upstream, _) = self
            .list_lite_with_forms(bucket, prefix, delimiter, max_keys_raw, continuation_token)
            .await?;
        Ok((page, upstream))
    }

    /// One page of every user object of `bucket` for a maintenance job:
    /// the keys as the storage lists them (stored sizes, no HEAD, no
    /// listing facts), the one stored form of each key that the page
    /// proved ([`ListedPageForms`]), and the deltaspaces whose
    /// reference.bin it read ([`crate::storage::DelegatedListResult::reference_prefixes`]).
    pub(crate) async fn list_job_page(
        &self,
        bucket: &str,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<JobPage, EngineError> {
        let (page, _, listed) = self
            .list_lite_with_forms(bucket, "", None, max_keys, continuation_token)
            .await?;
        let forms = ListedPageForms::of_page(&page.objects, &listed.forms);
        Ok(JobPage {
            page,
            forms,
            reference_prefixes: listed.reference_prefixes,
        })
    }

    /// [`Self::list_objects_lite_counted`], and what the storage reported
    /// besides the page (`Unreported` / `None` from the in-memory paging
    /// fallback).
    async fn list_lite_with_forms(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys_raw: u32,
        continuation_token: Option<&str>,
    ) -> Result<(ListObjectsPage, u32, ListedExtras), EngineError> {
        // S3 requires max-keys >= 1; clamp to prevent pagination invariant violations.
        let max_keys = max_keys_raw.max(1);
        // `delimiter=` (mc, rclone, restic send it) is no delimiter, as on
        // AWS. Every key contains "" at offset 0, so it would collapse the
        // whole listing into one CommonPrefix.
        let delimiter = delimiter.filter(|d| !d.is_empty());

        ObjectKey::validate_prefix(prefix)
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;

        // Fast path: delegate listing to the storage backend (S3 pages
        // natively — with OR without a delimiter — so we never materialise
        // the whole prefix just to cut one page out of it).
        let page = if let Some(result) = self
            .storage
            .list_objects_delegated(bucket, prefix, delimiter, max_keys, continuation_token)
            .await?
        {
            let page = ListObjectsPage {
                objects: result.objects,
                common_prefixes: result.common_prefixes,
                is_truncated: result.is_truncated,
                next_continuation_token: result.next_continuation_token,
                facts_missing_keys: Vec::new(),
            };
            let extras = ListedExtras {
                forms: result.forms,
                reference_prefixes: result.reference_prefixes,
            };
            (page, result.upstream_requests, extras)
        } else {
            // Backend doesn't support delegated listing for this shape — fall
            // through to the generic bulk_list + in-memory paging path.
            let page = self
                .list_objects_bulk(bucket, prefix, delimiter, max_keys, continuation_token)
                .await?;
            let extras = ListedExtras {
                forms: crate::storage::ListedForms::Unreported,
                reference_prefixes: None,
            };
            (page, 0, extras)
        };
        Ok(page)
    }

    /// Complete a page of `list_objects_lite`: the logical sizes, and with
    /// `metadata` the storage facts (a HEAD only where
    /// [`Self::list_entry_needs_head`] says so).
    pub(crate) async fn finish_listed_page(
        &self,
        bucket: &str,
        page: &mut ListObjectsPage,
        metadata: bool,
    ) -> Result<(), EngineError> {
        let completion = if metadata {
            Completion::StorageFacts
        } else {
            Completion::Sizes
        };
        self.complete_listed_page(bucket, page, completion).await
    }

    /// [`Self::finish_listed_page`] with the HEAD metadata of every delta
    /// and delta-eligible entry (its `dg-created-at`, its user metadata),
    /// as `metadata=true` gave it before D1. The lifecycle scan judges an
    /// object's age by that creation time, which a migrate or a metadata
    /// backfill keeps older than the backend's LastModified.
    pub(crate) async fn finish_listed_page_with_heads(
        &self,
        bucket: &str,
        page: &mut ListObjectsPage,
    ) -> Result<(), EngineError> {
        self.complete_listed_page(bucket, page, Completion::HeadMetadata)
            .await
    }

    async fn complete_listed_page(
        &self,
        bucket: &str,
        page: &mut ListObjectsPage,
        completion: Completion,
    ) -> Result<(), EngineError> {
        // Transparency without a HEAD per key: a lite LIST on an S3 backend
        // reports the STORED object (a delta's `.delta`, a ciphertext). Swap
        // in the logical size and ETag of each exact stored object (same key,
        // ETag and size), from this process's listing-size cache or from the
        // durable listing facts (one more LIST per page, any node, after a
        // restart; see `storage::listing_facts`). A miss keeps the stored
        // size: a client LIST never sends a HEAD (issue #82). Never fails.
        let sizes = if page.objects.is_empty() {
            Vec::new()
        } else {
            self.storage
                .resolve_listed_sizes(
                    bucket,
                    &mut page.objects,
                    crate::storage::list_size_cache::ResolveListed {
                        caller_heads_misses: completion != Completion::Sizes,
                        ..Default::default()
                    },
                )
                .await
        };
        page.facts_missing_keys = stored_size_only_keys(&page.objects, &sizes);

        // `metadata=true` (D1): every entry lists its storage facts from the
        // listing. Only a facts miss is HEADed, unless the metadata cache
        // knows it.
        if completion != Completion::Sizes && !page.objects.is_empty() {
            let mut known = Vec::with_capacity(page.objects.len());
            let mut misses = Vec::new();
            for ((key, meta), size) in
                std::mem::take(&mut page.objects)
                    .into_iter()
                    .zip(sizes.into_iter().chain(std::iter::repeat(
                        crate::storage::list_size_cache::ListedSize::Listed,
                    )))
            {
                let needs_head = match completion {
                    Completion::HeadMetadata => {
                        !size.is_known()
                            || meta.is_delta()
                            || self.file_router.is_delta_eligible(&key)
                    }
                    _ => Self::list_entry_needs_head(size),
                };
                // The lifecycle scan takes the cached HEAD metadata of any
                // entry (its `dg-created-at`), as before D1.
                let cached = (needs_head || completion == Completion::HeadMetadata)
                    .then(|| self.metadata_cache.get(bucket, &key))
                    .flatten();
                if let Some(cached) = cached {
                    known.push((key, cached));
                } else if !needs_head {
                    known.push((key, meta));
                } else {
                    misses.push((key, meta));
                }
            }

            if !misses.is_empty() {
                let enriched = self.storage.enrich_list_metadata(bucket, misses).await?;
                // Cache ONLY genuinely HEAD-resolved metadata. When a HEAD
                // sweep aborts under backend throttling, enrich_list_metadata
                // returns unresolved delta STUBS (empty ref_sha256, delta_size
                // = stored size) as a serviceable listing fallback — but those
                // must NOT poison the cache, or a later HEAD/GET would serve
                // the stub's wrong (stored, not original) size. A stub is
                // identifiable by an empty ref_sha256 on a Delta entry. An
                // entry whose seal did not open is not cached either: a
                // HEAD must apply the read rule, not answer from the list.
                for (key, meta) in &enriched {
                    if !Self::is_unresolved_delta_stub(meta) && !meta.is_unopened_seal() {
                        self.metadata_cache.insert(bucket, key, meta.clone());
                    }
                }
                known.extend(enriched);
            }

            // Re-sort by key to maintain S3 lexicographic ordering
            known.sort_by(|a, b| a.0.cmp(&b.0));
            // A HEAD resolved the missing ones, except a stub that a
            // throttled HEAD sweep left.
            page.facts_missing_keys.retain(|k| {
                known
                    .binary_search_by(|(key, _)| key.as_str().cmp(k))
                    .is_ok_and(|i| Self::is_unresolved_delta_stub(&known[i].1))
            });
            page.objects = known;
        }
        Ok(())
    }

    /// Return the `reference.bin` metadata for every deltaspace whose
    /// prefix begins with `scope_prefix` in the given bucket, plus a
    /// `truncated` flag set when the scan hit `limit` references and more
    /// remain.
    ///
    /// `list_objects` deliberately hides references from S3-compatible
    /// callers (a `reference.bin` is an implementation detail, not a
    /// user-visible object). Anything reporting "true storage cost" or
    /// "honest savings" — the admin dashboard, the CLI `stats` command,
    /// the SPA's per-prefix savings chip — must add reference bytes to
    /// the on-disk total. This helper is the supported way to do that
    /// without re-implementing per-backend listing details at the call
    /// sites.
    ///
    /// Cost: one listing of the scope
    /// ([`StorageBackend::list_reference_prefixes`]), then one
    /// `get_reference_metadata` (a HEAD on S3) per reference found,
    /// `REFERENCE_HEAD_CONCURRENCY` at a time. A directory without a
    /// reference costs no request. (It once listed the whole bucket and
    /// sent a HEAD to every directory in scope, one at a time: a folder of
    /// screenshots sent hundreds of 404 HEADs per savings request.)
    ///
    /// `scope_prefix == ""` returns every reference in the bucket
    /// (bounded by `limit`).
    /// `limit: None` means "no cap"; `limit: Some(n)` stops after n
    /// references, in prefix order, and sets `truncated: true` when more
    /// remain. The constant [`REFERENCE_SCAN_LIMIT`](super::REFERENCE_SCAN_LIMIT)
    /// is the recommended cap for latency-sensitive paths.
    ///
    /// Errors from `get_reference_metadata` for individual deltaspaces
    /// are logged and skipped — a missing or unreadable reference for
    /// one prefix should not poison the entire scan.
    pub async fn list_deltaspace_references(
        &self,
        bucket: &str,
        scope_prefix: &str,
        limit: Option<usize>,
    ) -> Result<ReferenceScan, EngineError> {
        // Storage backends name deltaspaces WITHOUT a trailing slash (e.g.
        // `releases/v1`), but callers using the S3 convention pass
        // `releases/v1/` here: the scope is `releases/v1` and everything
        // below it. An empty scope means "everything".
        let scope = scope_prefix.trim_end_matches('/');
        let mut prefixes = self.storage.list_reference_prefixes(bucket, scope).await?;
        prefixes.sort();
        let candidates = prefixes.len();
        let storage = &self.storage;
        let mut heads = futures::stream::iter(prefixes)
            .map(|prefix| async move {
                let meta = storage.get_reference_metadata(bucket, &prefix).await;
                (prefix, meta)
            })
            .buffered(REFERENCE_HEAD_CONCURRENCY);
        let mut references = Vec::new();
        let mut truncated = false;
        let mut consumed = 0usize;
        loop {
            if limit.is_some_and(|n| references.len() >= n) {
                truncated = consumed < candidates;
                if truncated {
                    tracing::info!(
                        "list_deltaspace_references: hit cap {:?} for bucket={bucket} scope={scope_prefix} \
                         — caller should treat totals as a lower bound and surface `truncated` to the UI.",
                        limit,
                    );
                }
                break;
            }
            let Some((prefix, meta)) = heads.next().await else {
                break;
            };
            consumed += 1;
            match meta {
                Ok(meta) => references.push((prefix, meta)),
                // Listed, then deleted before its HEAD: a concurrent delete
                // reclaimed it, so there is nothing to count.
                Err(StorageError::NotFound(_)) => {
                    tracing::debug!(
                        "list_deltaspace_references: {bucket}/{prefix} reference gone since the listing"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "list_deltaspace_references: skipping {}/{} ({}). \
                         Savings totals for this scope will undercount the \
                         reference bytes for this deltaspace.",
                        bucket,
                        prefix,
                        e,
                    );
                }
            }
        }
        Ok(ReferenceScan {
            references,
            truncated,
        })
    }

    /// Internal: build a ListObjectsPage from bulk_list_objects + in-memory
    /// delimiter collapsing and pagination.
    async fn list_objects_bulk(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<ListObjectsPage, EngineError> {
        // Single-pass listing: replaces list_deltaspaces + scan_deltaspace×N
        let bulk = self.storage.bulk_list_objects(bucket, prefix).await?;

        // Dedup by key, keeping latest version (shared logic with S3 backend)
        let mut items = crate::types::dedup_keep_latest(bulk);

        if !prefix.is_empty() {
            items.retain(|(key, _meta)| key.starts_with(prefix));
        }

        // --- Delimiter collapsing + pagination as a single operation ---
        //
        // When a delimiter is present, objects whose key (after the prefix)
        // contains the delimiter are collapsed into CommonPrefixes. Each
        // CommonPrefix counts as one entry toward max-keys, and is emitted
        // exactly once across all pages.

        if let Some(delim) = delimiter {
            // Collapse objects into CommonPrefixes where the key contains the delimiter
            let mut collapsed_objects = Vec::new();
            let mut seen_prefixes = std::collections::BTreeSet::new();

            for (key, meta) in items {
                let after = &key[prefix.len()..];
                if let Some(pos) = after.find(delim) {
                    let cp = format!("{}{}{}", prefix, &after[..pos], delim);
                    seen_prefixes.insert(cp);
                } else {
                    collapsed_objects.push((key, meta));
                }
            }

            let collapsed_prefixes: Vec<String> = seen_prefixes.into_iter().collect();
            let page = interleave_and_paginate(
                collapsed_objects,
                collapsed_prefixes,
                max_keys,
                continuation_token,
            );

            Ok(ListObjectsPage {
                objects: page.objects,
                common_prefixes: page.common_prefixes,
                is_truncated: page.is_truncated,
                next_continuation_token: page.next_continuation_token,
                facts_missing_keys: Vec::new(),
            })
        } else {
            // No delimiter — paginate raw objects
            let (is_truncated, next_token) =
                paginate_sorted(&mut items, max_keys, continuation_token, |(k, _)| k);

            Ok(ListObjectsPage {
                objects: items,
                common_prefixes: Vec::new(),
                is_truncated,
                next_continuation_token: next_token,
                facts_missing_keys: Vec::new(),
            })
        }
    }
}

/// What a storage listing reported besides its page.
struct ListedExtras {
    forms: crate::storage::ListedForms,
    reference_prefixes: Option<Vec<String>>,
}

/// One page of a maintenance job's listing ([`DeltaGliderEngine::list_job_page`]).
pub(crate) struct JobPage {
    pub page: ListObjectsPage,
    /// The one stored form of each key that the page proved.
    pub forms: ListedPageForms,
    /// The deltaspaces whose reference.bin the page read; `None`: not
    /// reported.
    pub reference_prefixes: Option<Vec<String>>,
}

/// What [`DeltaGliderEngine::complete_listed_page`] adds to a lite page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Completion {
    /// Logical sizes only.
    Sizes,
    /// Sizes and storage facts: a HEAD only for a facts miss (D1).
    StorageFacts,
    /// Sizes and the HEAD metadata of every delta and delta-eligible entry.
    HeadMetadata,
}

/// Keys of a listing page whose entry shows only its stored size (see
/// [`ListObjectsPage::facts_missing_keys`]).
pub(crate) fn stored_size_only_keys(
    objects: &[(String, FileMetadata)],
    sizes: &[crate::storage::list_size_cache::ListedSize],
) -> Vec<String> {
    objects
        .iter()
        .zip(sizes)
        .filter(|(_, size)| !size.is_known())
        .map(|((key, _), _)| key.clone())
        .collect()
}

#[cfg(test)]
mod stored_size_only_tests {
    use super::*;
    use crate::storage::list_size_cache::ListedSize;

    #[test]
    fn only_stored_only_entries_are_listed() {
        let meta = |k: &str| {
            FileMetadata::new_passthrough(k.into(), String::new(), String::new(), 1, None)
        };
        let objects: Vec<(String, FileMetadata)> = ["a", "b", "c"]
            .iter()
            .map(|k| (k.to_string(), meta(k)))
            .collect();
        let sizes = [
            ListedSize::Listed,
            ListedSize::StoredOnly,
            ListedSize::Cached,
        ];
        assert_eq!(
            stored_size_only_keys(&objects, &sizes),
            vec!["b".to_string()]
        );
        assert!(stored_size_only_keys(&objects, &[]).is_empty());
    }
}

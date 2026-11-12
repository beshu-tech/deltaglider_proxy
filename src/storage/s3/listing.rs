// SPDX-License-Identifier: BUSL-1.1

//! Listing: classification of listed keys, HEAD enrichment and the
//! delegated-listing anchor algebra.

use super::*;

/// The last key of one upstream LIST page, before any filtering.
pub(super) fn last_listed_key(contents: Option<&[aws_sdk_s3::types::Object]>) -> Option<String> {
    contents?.last()?.key.clone()
}

/// Lightweight object info from ListObjectsV2 (no HEAD requests needed)
pub(super) struct S3ListedObject {
    pub(super) key: String,
    pub(super) size: u64,
    pub(super) last_modified: Option<DateTime<Utc>>,
    pub(super) etag: Option<String>,
}

impl S3ListedObject {
    /// Convert an AWS SDK `Object` from a ListObjectsV2 response into our
    /// lightweight representation.  Returns `None` if the object has no key.
    pub(super) fn from_s3_object(object: aws_sdk_s3::types::Object) -> Option<Self> {
        let key = object.key?;
        // Listing facts and the `.dg` directory are internal, never objects.
        if listing_facts::is_hidden_key(&key) {
            return None;
        }
        let last_modified = object.last_modified.and_then(|dt| {
            DateTime::parse_from_rfc3339(&dt.to_string())
                .ok()
                .map(|d| d.with_timezone(&Utc))
        });
        Some(Self {
            key,
            size: object.size.unwrap_or(0) as u64,
            last_modified,
            etag: object.e_tag.map(|e| e.trim_matches('"').to_string()),
        })
    }
}

/// Output of `S3Backend::classify_listed_objects`.
pub(super) struct ClassifiedListing {
    pub(super) classified: Vec<ClassifiedObject>,
    pub(super) dir_markers: Vec<(String, FileMetadata)>,
    /// `(stored key, stored size)` of each `reference.bin`.
    pub(super) baselines: Vec<(String, u64)>,
}

/// An S3 listed object classified into a user-visible key, with enough info
/// to decide whether a HEAD call is needed for full metadata.
pub(super) struct ClassifiedObject {
    pub(super) user_key: String,
    pub(super) s3_key: String,
    pub(super) listing_meta: S3ListedObject,
}

impl S3Backend {
    /// Max concurrent HEAD requests to avoid S3 503 SlowDown throttling.
    /// See `bounded_head_calls()` for rationale.
    // 10, down from 50: fifty concurrent HEADs is a burst that trips Ceph
    // qos (Hetzner 503 SlowDown RCA, 2026-07-09); ten keeps a 1000-key page
    // enrich fast on a healthy backend while quartering the instantaneous
    // pressure, and doubles as the throttle-breaker batch size.
    pub(super) const MAX_CONCURRENT_HEADS: usize = 10;
}

impl S3Backend {
    /// The listing-size-cache resolution of one listed entry. Pure apart from
    /// the cache lookup; see `StorageBackend::resolve_listed_sizes`.
    /// What a listed entry's size is without any listing facts: the
    /// listing's own for a directory marker, full metadata or a
    /// passthrough, `StoredOnly` for a delta stub.
    pub(super) fn listed_size_without_facts(user_key: &str, meta: &FileMetadata) -> ListedSize {
        if !user_key.ends_with('/') && meta.is_unresolved_delta_stub() {
            ListedSize::StoredOnly
        } else {
            ListedSize::Listed
        }
    }

    pub(super) fn resolve_one_listed(
        scope: &str,
        bucket: &str,
        user_key: &str,
        meta: &mut FileMetadata,
    ) -> ListedSize {
        if user_key.ends_with('/') {
            return ListedSize::Listed; // directory marker
        }
        let stub = meta.is_unresolved_delta_stub();
        if meta.is_delta() && !stub {
            return ListedSize::Listed; // already full metadata
        }
        let stored_key = if stub {
            format!("{user_key}.delta")
        } else {
            user_key.to_string()
        };
        let id = StoredObjectId {
            scope,
            bucket,
            key: &stored_key,
            etag: &meta.md5,
            size: meta.stored_size(),
        };
        match list_size_cache::lookup(&id) {
            Some(facts) => {
                list_size_cache::apply(meta, &facts);
                ListedSize::Cached
            }
            None if stub => ListedSize::StoredOnly,
            None => ListedSize::Listed,
        }
    }

    /// Classify a batch of S3 listed objects into user-visible entries,
    /// directory markers, and delta baselines. A baseline (`reference.bin`)
    /// is never user-visible: it comes back separately as
    /// `(stored key, stored size)` so a caller that reports stored bytes can
    /// count it without another request.
    pub(super) fn classify_listed_objects(objects: Vec<S3ListedObject>) -> ClassifiedListing {
        let mut classified = Vec::new();
        let mut dir_markers = Vec::new();
        let mut baselines = Vec::new();

        for obj in objects {
            let filename = obj.key.rsplit('/').next().unwrap_or(&obj.key);

            // Directory marker: zero-byte key ending with '/'
            if obj.key.ends_with('/') && obj.size == 0 {
                let mut marker = FileMetadata::directory_marker(&obj.key);
                marker.stored_last_modified = obj.last_modified;
                dir_markers.push((obj.key.clone(), marker));
                continue;
            }

            // Internal deltaspace file: never a user-visible object.
            if crate::types::is_reference_key(&obj.key) {
                baselines.push((obj.key.clone(), obj.size));
                continue;
            }

            let key_prefix = if obj.key.contains('/') {
                &obj.key[..obj.key.len() - filename.len() - 1]
            } else {
                ""
            };

            let is_delta = filename.ends_with(".delta");
            let original_name = if is_delta {
                filename.trim_end_matches(".delta").to_string()
            } else {
                filename.to_string()
            };

            let user_key = if key_prefix.is_empty() {
                original_name
            } else {
                format!("{}/{}", key_prefix, original_name)
            };

            classified.push(ClassifiedObject {
                user_key,
                s3_key: obj.key.clone(),
                listing_meta: obj,
            });
        }

        ClassifiedListing {
            classified,
            dir_markers,
            baselines,
        }
    }

    /// Fire bounded parallel HEAD calls for a set of S3 keys, returning metadata
    /// for each key that responded successfully.
    ///
    /// PERF: Uses `buffer_unordered(MAX_CONCURRENT_HEADS)` instead of `join_all()`
    /// to avoid blasting thousands of concurrent HEADs at S3 (which triggers 503
    /// SlowDown throttling). Do NOT replace with `join_all()`.
    ///
    /// LIFETIME SUBTLETY: Keys and bucket are cloned into owned Strings and futures
    /// are collected into a Vec BEFORE streaming. Without this, the async closures
    /// capture `&self` and `&str` which can't satisfy the `'static` bound that
    /// `buffer_unordered` requires.
    /// Returns the successful lookups plus a count of throttle rejections
    /// (503 SlowDown), so callers can stop a HEAD sweep against a backend
    /// that is actively shedding load instead of grinding through it.
    pub(super) async fn bounded_head_calls<'a, I>(
        &self,
        bucket: &str,
        keys: I,
    ) -> (HashMap<String, FileMetadata>, usize)
    where
        I: Iterator<Item = &'a str>,
    {
        let head_futs: Vec<_> = keys
            .map(|key| {
                let key = key.to_string();
                let bucket = bucket.to_string();
                async move {
                    let meta_result = self.get_object_metadata(&bucket, &key).await;
                    (key, meta_result)
                }
            })
            .collect();
        futures::stream::iter(head_futs)
            .buffer_unordered(Self::MAX_CONCURRENT_HEADS)
            .fold(
                (HashMap::new(), 0usize),
                |(mut ok, throttled), (key, result)| async move {
                    match result {
                        Ok(meta) => {
                            ok.insert(key, meta);
                            (ok, throttled)
                        }
                        Err(StorageError::Throttled(_)) => (ok, throttled + 1),
                        Err(_) => (ok, throttled),
                    }
                },
            )
            .await
    }

    /// Resolve classified objects to `(user_key, FileMetadata)` pairs using
    /// listing data only (no HEAD calls). Deduplicates by user key, keeping
    /// the latest version.
    pub(super) fn resolve_classified_lite(
        classified: Vec<ClassifiedObject>,
        mut seed_results: Vec<(String, FileMetadata)>,
    ) -> Vec<(String, FileMetadata)> {
        let classified_pairs: Vec<(String, FileMetadata)> = classified
            .into_iter()
            .map(|entry| {
                let is_delta = entry.s3_key.ends_with(".delta");
                let storage_info = if is_delta {
                    StorageInfo::delta_stub(entry.listing_meta.size)
                } else {
                    StorageInfo::Passthrough
                };
                let meta = Self::fallback_metadata_from_listing(
                    &entry.listing_meta,
                    &entry.user_key,
                    storage_info,
                );
                (entry.user_key, meta)
            })
            .collect();

        seed_results.extend(classified_pairs);
        crate::types::dedup_keep_latest(seed_results)
    }

    /// Build a best-effort FileMetadata from S3 listing info alone (no HEAD).
    /// Used when HEAD fails or isn't needed (passthrough files).
    pub(super) fn fallback_metadata_from_listing(
        obj: &S3ListedObject,
        user_key: &str,
        storage_info: StorageInfo,
    ) -> FileMetadata {
        let mut meta = FileMetadata::fallback(
            user_key.rsplit('/').next().unwrap_or(user_key).to_string(),
            obj.size,
            obj.etag.clone().unwrap_or_default(),
            obj.last_modified.unwrap_or_else(Utc::now),
            None,
            storage_info,
        );
        meta.stored_last_modified = obj.last_modified;
        meta
    }

    /// LIST a deltaspace and return only the entries at the prefix level
    /// itself (not from subdirectories). Shared between
    /// [`scan_deltaspace`] and [`scan_deltaspace_lite`].
    ///
    /// The LIST of `prefix/` has delimiter `/`, so a child deltaspace is one
    /// CommonPrefix and none of its keys is read: a child's deltas use the
    /// child's own reference.bin, and without the delimiter a root folder
    /// read the whole bucket. (Counted, a child's objects stopped the
    /// reclaim of the parent's reference.bin, and the delta-efficiency scan
    /// counted them in the parent too.)
    pub(super) async fn list_deltaspace_eligible(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<S3ListedObject>, StorageError> {
        let search_prefix = level_prefix(prefix);
        let mut eligible = Vec::new();
        let mut token = None;
        loop {
            let (objects, next) = self
                .list_level_page(bucket, &search_prefix, 1000, token)
                .await?;
            // The delimiter keeps sub-folder keys out; this filter is for
            // a backend that ignores it.
            eligible.extend(objects.into_iter().filter(|obj| {
                obj.key
                    .strip_prefix(&search_prefix)
                    .is_some_and(|name| !name.contains('/'))
            }));
            match next {
                Some(t) => token = Some(t),
                None => return Ok(eligible),
            }
        }
    }

    /// [`StorageBackend::holds_only_reference`] on S3: LIST the level of
    /// `prefix/` (delimiter `/`) and stop at the first key that is not
    /// reference.bin. The first page is small: in the common case it holds
    /// an object and answers the question. Only a level that starts with
    /// sub-folders reads more pages.
    pub(super) async fn holds_only_reference_listed(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<bool, StorageError> {
        /// Entries of the first page: enough to pass reference.bin and
        /// a few sub-folders.
        const FIRST_PAGE_KEYS: i32 = 8;
        let search_prefix = level_prefix(prefix);
        let reference = self.reference_key(prefix);
        let mut found_reference = false;
        let (mut max_keys, mut token) = (FIRST_PAGE_KEYS, None);
        loop {
            let (objects, next) = self
                .list_level_page(bucket, &search_prefix, max_keys, token)
                .await?;
            for obj in objects {
                if obj.key != reference {
                    return Ok(false);
                }
                found_reference = true;
            }
            match next {
                Some(t) => (max_keys, token) = (1000, Some(t)),
                None => return Ok(found_reference),
            }
        }
    }

    /// One ListObjectsV2 page of the level `search_prefix` with delimiter
    /// `/`: its objects, and the token of the next page (`None`: the last).
    async fn list_level_page(
        &self,
        bucket: &str,
        search_prefix: &str,
        max_keys: i32,
        token: Option<String>,
    ) -> Result<(Vec<S3ListedObject>, Option<String>), StorageError> {
        let response = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(search_prefix)
            .delimiter("/")
            .max_keys(max_keys)
            .set_continuation_token(token)
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))?;
        let next = response
            .is_truncated
            .unwrap_or(false)
            .then_some(response.next_continuation_token)
            .flatten();
        let objects = response
            .contents
            .unwrap_or_default()
            .into_iter()
            .filter_map(S3ListedObject::from_s3_object)
            .collect();
        Ok((objects, next))
    }

    /// Build a no-HEAD FileMetadata for a listed object. For deltas the
    /// resulting `file_size` is the on-disk delta size, not the original.
    /// Suitable only for callers that explicitly opt into the lite shape.
    pub(super) fn lite_metadata_from_listed(obj: &S3ListedObject) -> FileMetadata {
        let filename = obj.key.rsplit('/').next().unwrap_or(&obj.key);
        let is_delta = filename.ends_with(".delta");
        let is_reference = crate::types::is_reference_key(&obj.key);
        let original_name = filename.trim_end_matches(".delta").to_string();
        let storage_info = if is_delta {
            StorageInfo::delta_stub(obj.size)
        } else if is_reference {
            StorageInfo::Reference {
                source_name: String::new(),
            }
        } else {
            StorageInfo::Passthrough
        };
        let mut meta = FileMetadata::fallback(
            original_name,
            obj.size,
            obj.etag.clone().unwrap_or_default(),
            obj.last_modified.unwrap_or_else(Utc::now),
            None,
            storage_info,
        );
        meta.stored_last_modified = obj.last_modified;
        meta
    }

    /// List objects with a prefix in a specific bucket (keys only)
    pub(super) async fn list_objects_with_prefix(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<String>, StorageError> {
        let objects = self.list_objects_full(bucket, prefix).await?;
        Ok(objects.into_iter().map(|o| o.key).collect())
    }

    /// List objects with a prefix, returning full listing info (size, last_modified, etag)
    pub(super) async fn list_objects_full(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<S3ListedObject>, StorageError> {
        let mut results = Vec::new();
        let mut continuation_token: Option<String> = None;

        let mut skip_to: Option<&'static str> = None;

        loop {
            let mut request = self.client.list_objects_v2().bucket(bucket).prefix(prefix);

            if let Some(past) = skip_to.take() {
                request = request.start_after(past);
            } else if let Some(token) = continuation_token {
                request = request.continuation_token(token);
            }

            let response = request
                .send()
                .await
                .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))?;

            let page_last = last_listed_key(response.contents.as_deref());
            if let Some(contents) = response.contents {
                results.extend(
                    contents
                        .into_iter()
                        .filter_map(S3ListedObject::from_s3_object),
                );
            }

            if response.is_truncated.unwrap_or(false) {
                continuation_token = response.next_continuation_token;
                skip_to = page_last
                    .as_deref()
                    .and_then(listing_facts::skip_past_facts);
                if continuation_token.is_none() && skip_to.is_none() {
                    return Err(truncated_without_token(bucket));
                }
            } else {
                break;
            }
        }

        Ok(results)
    }
}

/// The LIST prefix of the deltaspace `prefix`: `prefix/`, or `""` for
/// the bucket root.
fn level_prefix(prefix: &str) -> String {
    if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    }
}

/// Early-exit decision for the delegated-listing fetch loop.
///
/// Given the raw keys + common prefixes fetched so far, returns the anchor —
/// the (max_keys+1)-th distinct user-visible key past the continuation token
/// (its existence also proves `is_truncated`) — or `None` when more
/// candidates are still needed. The fetch loop stops as soon as the last raw
/// key read sorts above the anchor: everything the page can still be missing
/// at that point is the bounded set `confirmable_candidates` enumerates, and
/// the caller confirms those with one exact request each. Raw sort order is
/// NOT user sort order for `.delta` keys ("v1" < "v1.2" but "v1.delta" >
/// "v1.2.delta"), which is why stopping at the anchor alone would drop keys
/// (see `late_candidates_cover_the_prefix_chain_regression`).
///
/// Distinct counting matters too: `k` and `k.delta` dedup into one entry, so
/// counting duplicates could stop the fetch with an under-filled page and a
/// false `is_truncated=false`.
pub(super) fn list_anchor<'a>(
    raw_keys: impl Iterator<Item = &'a str>,
    common_prefixes: impl Iterator<Item = &'a str>,
    max_keys: u32,
    continuation_token: Option<&str>,
) -> Option<String> {
    let token = continuation_token.unwrap_or("");
    let mut candidates: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for key in raw_keys {
        if crate::types::is_reference_key(key) {
            continue;
        }
        // Strip ALL trailing `.delta` (not just one) so this matches the
        // classification path (`trim_end_matches` in `classify_listed_objects`
        // and the lite-metadata builder). Otherwise a foreign pair `b.delta` +
        // `b.delta.delta` counts as TWO distinct candidates here but dedups to
        // ONE served entry, over-counting the anchor and risking a dropped
        // page tail.
        let user_key: &str = key.trim_end_matches(".delta");
        if user_key > token {
            candidates.insert(user_key);
        }
    }
    for cp in common_prefixes {
        if cp > token {
            candidates.insert(cp);
        }
    }
    // Need max_keys+1 distinct entries: max_keys fill the page, the extra one
    // proves truncation and anchors the completeness horizon.
    candidates
        .iter()
        .nth(max_keys as usize)
        .map(|s| (*s).to_string())
}

/// Raw keys that can sort ABOVE `anchor` and still map to a user key BELOW
/// it — the only keys the fetch loop can still be missing once it has passed
/// the anchor.
///
/// Such a key must be `p + ".delta"` for some PROPER prefix `p` of `anchor`.
/// If the user key `u` is not a prefix of the anchor, the first differing
/// character already decides the order, so `u + ".delta"` stays below the
/// anchor as well and has therefore already arrived. The full-anchor form
/// `anchor + ".delta"` is deliberately excluded: its user key IS the anchor,
/// which is already represented (the anchor was derived from a raw key or
/// common prefix the loop has read), so probing it is a guaranteed no-op.
/// The set is never larger than the anchor is long, and every member is an
/// EXACT key, so each one costs a single cheap request to confirm.
///
/// This replaces the old horizon, which was the MAXIMUM of this same set. That
/// maximum is correct but unreachable for hierarchical keys: for the anchor
/// `ror/builds/1.0.1/app.zip` it is `ror/builds/1.delta`, which sorts after the
/// complete `ror/builds/1.*` subtree, so the loop read the whole subtree before
/// it could stop (issue #82).
///
/// This is pure candidate arithmetic; `confirmable_candidates` applies the
/// request-scope filters before any probe is issued.
pub(super) fn late_delta_candidates(anchor: &str) -> Vec<String> {
    (1..anchor.len())
        .filter(|&n| anchor.is_char_boundary(n))
        .map(|n| format!("{}.delta", &anchor[..n]))
        .filter(|candidate| candidate.as_str() > anchor)
        .collect()
}

/// The late-delta candidates that are actually worth a confirmation probe for
/// THIS request. Pure — the whole eligibility decision is unit-testable.
///
/// Three filters on top of `late_delta_candidates`:
///
/// 1. **Request-prefix scope.** The fetch loop only ever reads keys under
///    `request_prefix`, but a candidate built from a prefix of the anchor
///    SHORTER than the request prefix names a key OUTSIDE the listing scope
///    (e.g. anchor `app-v1/b-2/x` yields `app.delta`). Probing it would
///    inject a foreign object into the page, violating the S3 Prefix
///    contract.
/// 2. **Delimiter collapse.** With a delimiter, a candidate whose user key
///    contains the delimiter beyond the request prefix lives inside a
///    collapsed subtree: upstream reports it as a CommonPrefix (always the
///    anchor's own covering prefix, already collected), never as Contents.
///    Probing it would serve the same name as both an object and a
///    CommonPrefix.
/// 3. **Already read.** The loop breaks at an upstream page boundary, so raw
///    keys up to `last_read` are all in hand. If a candidate sorts below
///    `last_read` and is not a prefix of it, every key extending the
///    candidate also sorts below `last_read` — the probe cannot find
///    anything new. (When the candidate IS a prefix of `last_read`, foreign
///    multi-suffix forms may still lie beyond it, so the probe stays.)
pub(super) fn confirmable_candidates(
    anchor: &str,
    request_prefix: &str,
    delimiter: Option<&str>,
    last_read: Option<&str>,
) -> Vec<String> {
    late_delta_candidates(anchor)
        .into_iter()
        .filter(|c| c.starts_with(request_prefix))
        .filter(|c| match delimiter {
            Some(d) if !d.is_empty() => {
                // `c` = user key + ".delta" by construction (single suffix).
                let user = c.strip_suffix(".delta").unwrap_or(c);
                !user
                    .get(request_prefix.len().min(user.len())..)
                    .unwrap_or("")
                    .contains(d)
            }
            _ => true,
        })
        .filter(|c| match last_read {
            Some(last) => c.as_str() >= last || last.starts_with(c.as_str()),
            None => true,
        })
        .collect()
}

/// Does a key returned by a `prefix(candidate)` probe serve the candidate's
/// user key? True for the exact candidate and for foreign multi-suffix forms
/// (`candidate + ".delta"`, `candidate + ".delta.delta"`, …), which the
/// classification path (`trim_end_matches(".delta")`) maps to the same user
/// key. Anything else under the prefix (`p.deltafoo`, `p.delta/x`) is a
/// different user key and must not be injected here.
pub(super) fn probe_hit_serves_candidate(key: &str, candidate: &str) -> bool {
    key.strip_prefix(candidate).is_some_and(|rest| {
        rest.len() % ".delta".len() == 0
            && rest
                .as_bytes()
                .chunks(".delta".len())
                .all(|c| c == b".delta")
    })
}

/// S3 answers at most this many entries per ListObjectsV2 request.
const UPSTREAM_MAX_KEYS: u32 = 1000;

/// Entries a delegated listing asks upstream for beyond the client's page:
/// room for baselines and the two forms of a key, so the anchor of a small
/// page is on the first upstream page.
const UPSTREAM_SLACK: u32 = 100;

/// A backend answered `IsTruncated=true` without a continuation token: the
/// next request could only start the listing again from its first page.
pub(super) fn truncated_without_token(bucket: &str) -> StorageError {
    StorageError::Other(format!(
        "the backend listing of bucket '{bucket}' is truncated but has no continuation token"
    ))
}

/// Keys per request of a probe.
const PROBE_PAGE: i32 = 16;
/// Requests one probe of a prefix may send, over both of its ranges.
const PROBE_REQUESTS_MAX: u32 = 6;
/// Prefixes one LIST page probes: the extra requests of a page are at most
/// `PROBES_PER_PAGE x PROBE_REQUESTS_MAX` (the common case is 2 for the root
/// `.dg/`, 0 once it is remembered).
pub(super) const PROBES_PER_PAGE: u32 = 8;
/// How long an [`UnwritablePrefix`] with no listed key is remembered. No
/// client write adds a key below it (every write path refuses a `.dg`
/// directory), so only an older release writing at the same time could make
/// the answer stale, and it shows in at most this long.
const OWN_ONLY_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// [`UnwritablePrefix`]es (`scope\0bucket\0prefix`) found to hold only the
/// proxy's own files. Nothing else is stored here: see `holds_listed_key`.
static OWN_ONLY_PREFIXES: std::sync::LazyLock<moka::sync::Cache<String, ()>> =
    std::sync::LazyLock::new(|| {
        moka::sync::Cache::builder()
            .max_capacity(10_000)
            .time_to_live(OWN_ONLY_TTL)
            .build()
    });

/// What a range of a probe found.
enum ProbeVerdict {
    /// A key that a LIST shows.
    Listed,
    /// No such key in the range.
    Nothing,
    /// The request cap was spent: the prefix is shown.
    GaveUp,
}

impl S3Backend {
    /// Does a key that a LIST shows exist under `prefix`? Asked for a
    /// CommonPrefix that may hold only the proxy's own files
    /// ([`listing_facts::may_hold_only_own_keys`]), and bounded:
    ///
    /// * `PROBE_REQUESTS_MAX` requests at most for one prefix, and the page
    ///   grants `PROBES_PER_PAGE` prefixes at most (`budget`). Past either
    ///   limit the prefix is shown: a stray prefix is better than an
    ///   unbounded LIST, as an entry past the HEAD budget of a page keeps its
    ///   stored size.
    /// * A prefix that is a leading part of the facts namespace is read in
    ///   two ranges, past the facts (`start_after = PAST_FACTS`, one request
    ///   when nothing lies there) and up to the first facts key. The facts
    ///   (hundreds, in a busy bucket) are never paged through.
    /// * A prefix with no listed key is remembered for `OWN_ONLY_TTL` ONLY
    ///   when it is an [`UnwritablePrefix`] (below a `.dg` directory, where
    ///   no client write adds a key), so the next LIST of the level sends no
    ///   probe and the answer cannot go stale. Any other prefix (`a/r`, `.`)
    ///   is probed each time: a client can write a key under it at any moment.
    /// * A probe that fails (the backend refuses the prefix: MinIO answers
    ///   `XMinioInvalidResourceName` for `prefix=.`) never fails the LIST: the
    ///   prefix is shown, as past the budget.
    ///
    /// `requests` counts what this sends (the page's total).
    async fn holds_listed_key(
        &self,
        bucket: &str,
        prefix: &str,
        requests: &mut u32,
        budget: &mut u32,
    ) -> bool {
        let remembered = listing_facts::UnwritablePrefix::of(prefix)
            .map(|u| format!("{}\0{bucket}\0{}", self.list_cache_scope, u.as_str()));
        if remembered
            .as_ref()
            .is_some_and(|key| OWN_ONLY_PREFIXES.contains_key(key))
        {
            return false;
        }
        if *budget == 0 {
            return true;
        }
        *budget -= 1;
        let mut sent = 0u32;
        let ranges: &[(Option<&str>, bool)] = if listing_facts::FACTS_ROOT.starts_with(prefix) {
            // (start_after, stop at the first facts key)
            &[(Some(listing_facts::PAST_FACTS), false), (None, true)]
        } else {
            &[(None, false)]
        };
        for (start_after, stop_at_facts) in ranges {
            match self
                .probe_range(bucket, prefix, *start_after, *stop_at_facts, &mut sent)
                .await
            {
                Ok(ProbeVerdict::Listed) | Ok(ProbeVerdict::GaveUp) => {
                    *requests += sent;
                    return true;
                }
                Ok(ProbeVerdict::Nothing) => {}
                Err(e) => {
                    *requests += sent;
                    debug!("probe of prefix {prefix:?} in {bucket} failed, showing it: {e}");
                    return true;
                }
            }
        }
        *requests += sent;
        if let Some(key) = remembered {
            OWN_ONLY_PREFIXES.insert(key, ());
        }
        false
    }

    /// One range of [`Self::holds_listed_key`]: pages of `PROBE_PAGE` keys
    /// from `start_after` (or the start of `prefix`), until a listed key, the
    /// end, the first facts key (`stop_at_facts`) or the request cap.
    async fn probe_range(
        &self,
        bucket: &str,
        prefix: &str,
        start_after: Option<&str>,
        stop_at_facts: bool,
        sent: &mut u32,
    ) -> Result<ProbeVerdict, StorageError> {
        let mut token: Option<String> = None;
        let mut first = true;
        loop {
            if *sent >= PROBE_REQUESTS_MAX {
                return Ok(ProbeVerdict::GaveUp);
            }
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .max_keys(PROBE_PAGE);
            if first {
                request = request.set_start_after(start_after.map(String::from));
                first = false;
            } else if let Some(t) = token.take() {
                request = request.continuation_token(t);
            }
            *sent += 1;
            let response = request
                .send()
                .await
                .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))?;
            let keys: Vec<&str> = response.contents().iter().filter_map(|o| o.key()).collect();
            if keys.iter().any(|k| listing_facts::shows_in_listing(k)) {
                return Ok(ProbeVerdict::Listed);
            }
            if stop_at_facts && keys.iter().any(|k| listing_facts::is_facts_key(k)) {
                return Ok(ProbeVerdict::Nothing);
            }
            if !response.is_truncated.unwrap_or(false) {
                return Ok(ProbeVerdict::Nothing);
            }
            token = response.next_continuation_token.clone();
            if token.is_none() {
                return Err(truncated_without_token(bucket));
            }
        }
    }

    /// The delegated listing (`StorageBackend::list_objects_delegated`):
    /// upstream S3 pages (and, with a delimiter, collapses) the listing, and
    /// the proxy reads only as far as the page needs.
    ///
    /// A page is complete up to an ANCHOR: an entry that sorts at or below
    /// the last key read. Every entry below it is then read, except the late
    /// `.delta` keys that `confirmable_candidates` names, which one exact
    /// LIST each confirms. A full page anchors at its `max_keys + 1`-th entry.
    /// When one upstream page does not hold that many, the page is served
    /// SHORT, up to the last entry the upstream page proves: S3 allows a
    /// truncated page below `max_keys`, and reading the next upstream page
    /// for one more entry made the next client page (which starts at the
    /// page's last key) read that upstream page again. So a page costs one
    /// upstream LIST, plus a probe when a late `.delta` key can sort into it.
    pub(super) async fn list_delegated_page(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<DelegatedListResult, StorageError> {
        let mut all_common_prefixes = std::collections::BTreeSet::new();
        let mut raw_objects: Vec<S3ListedObject> = Vec::new();
        let mut upstream_token: Option<String> = None;
        let mut first_page = true;
        // Set when the loop stops at an anchor; `short` when the page stops
        // below `max_keys` at the end of the upstream page.
        let mut settled_anchor: Option<String> = None;
        let mut short = false;
        let mut skip_to: Option<&'static str> = None;
        // Every request this call sends: its pages and its probes.
        let mut upstream_requests = 0u32;
        // The prefixes this call may still probe (see `holds_listed_key`).
        let mut probe_budget = PROBES_PER_PAGE;
        let upstream_max_keys = max_keys
            .saturating_add(1 + UPSTREAM_SLACK)
            .min(UPSTREAM_MAX_KEYS);

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .max_keys(upstream_max_keys as i32)
                .set_delimiter(delimiter.map(String::from));

            // The client's continuation token is a user-visible key: the first
            // upstream page starts after it; later pages follow the upstream
            // token (or jump past the facts namespace).
            if first_page {
                if let Some(sa) = continuation_token {
                    request = request.start_after(sa);
                }
                first_page = false;
            } else if let Some(past) = skip_to.take() {
                request = request.start_after(past);
            } else if let Some(ref token) = upstream_token {
                request = request.continuation_token(token);
            }

            upstream_requests += 1;
            let response = request
                .send()
                .await
                .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))?;
            DELEGATED_LIST_UPSTREAM_PAGES.inc();

            // Collect CommonPrefixes. One that holds only the proxy's own
            // files is not shown: the facts namespace outright, and a prefix
            // that may cover them (`.dg/`, or `.` with delimiter `.`) when a
            // look under it finds no key that a LIST shows (L2, #171).
            // Deliberately narrow: an earlier version skipped ANY dot-prefixed
            // segment, which also hid legitimate folders like `.well-known/`.
            if let Some(cps) = response.common_prefixes {
                for cp in cps {
                    if let Some(p) = cp.prefix {
                        if listing_facts::is_internal_common_prefix(&p) {
                            continue;
                        }
                        if listing_facts::may_hold_only_own_keys(&p)
                            && !self
                                .holds_listed_key(
                                    bucket,
                                    &p,
                                    &mut upstream_requests,
                                    &mut probe_budget,
                                )
                                .await
                        {
                            continue;
                        }
                        all_common_prefixes.insert(p);
                    }
                }
            }

            let page_last = last_listed_key(response.contents.as_deref());
            if let Some(contents) = response.contents {
                raw_objects.extend(
                    contents
                        .into_iter()
                        .filter_map(S3ListedObject::from_s3_object),
                );
            }

            if !response.is_truncated.unwrap_or(false) {
                break;
            }
            upstream_token = response.next_continuation_token;
            skip_to = page_last
                .as_deref()
                .and_then(listing_facts::skip_past_facts);
            if upstream_token.is_none() && skip_to.is_none() {
                return Err(truncated_without_token(bucket));
            }

            let Some(read) = read_horizon(
                raw_objects.last().map(|o| o.key.as_str()),
                all_common_prefixes.last().map(String::as_str),
            ) else {
                continue;
            };
            let raw_keys = || raw_objects.iter().map(|o| o.key.as_str());
            let cps = || all_common_prefixes.iter().map(String::as_str);
            // A full page: past its (max_keys+1)-th entry.
            if let Some(anchor) = list_anchor(raw_keys(), cps(), max_keys, continuation_token) {
                if anchor.as_str() <= read {
                    settled_anchor = Some(anchor);
                    break;
                }
            }
            // Else a short page, up to what this upstream page proves.
            if let Some(anchor) = short_page_anchor(raw_keys(), cps(), continuation_token, read) {
                settled_anchor = Some(anchor);
                short = true;
                break;
            }
        }

        // The loop stopped at the anchor, so a raw key of the form
        // `p + ".delta"` (p a proper prefix of the anchor) that sorts above the
        // anchor may not have been read yet, and each one still belongs on the
        // page. Confirm that bounded set with one exact request each — reading
        // forward to their maximum instead is what made a delimiter-less
        // listing walk the whole subtree (issue #82). `confirmable_candidates`
        // owns the eligibility decision (request-prefix scope, delimiter
        // collapse, already-read skip) so it stays unit-testable. A candidate
        // at or below the continuation token belongs to an earlier page.
        if let Some(anchor) = &settled_anchor {
            let read = read_horizon(
                raw_objects.last().map(|o| o.key.as_str()),
                all_common_prefixes.last().map(String::as_str),
            )
            .map(str::to_string);
            let token = continuation_token.unwrap_or("");
            let candidates: Vec<String> =
                confirmable_candidates(anchor, prefix, delimiter, read.as_deref())
                    .into_iter()
                    .filter(|c| c.strip_suffix(".delta").unwrap_or(c) > token)
                    .collect();
            upstream_requests += candidates.len() as u32;
            // Probes are independent; run them concurrently (bounded, same
            // doctrine as `bounded_head_calls` — never blast the backend).
            // A probe error fails the listing closed, deliberately: silently
            // skipping a candidate would drop a user-visible key, and the
            // probes are the same failure class as the page fetches above.
            let probes = candidates.into_iter().map(|candidate| {
                let client = self.client.clone();
                let bucket = bucket.to_string();
                async move {
                    DELEGATED_LIST_PROBE_REQUESTS.inc();
                    // `max_keys(3)`: the exact key sorts first among keys
                    // sharing its prefix, and the next slots catch foreign
                    // multi-suffix forms (`candidate + ".delta"…`) that
                    // classification also maps to the candidate's user key.
                    let found = client
                        .list_objects_v2()
                        .bucket(&bucket)
                        .prefix(&candidate)
                        .max_keys(3)
                        .send()
                        .await
                        .map_err(|e| self.classify(&bucket, &e, S3Op::ListObjects))?;
                    let hits: Vec<S3ListedObject> = found
                        .contents
                        .into_iter()
                        .flatten()
                        .filter(|obj| {
                            obj.key
                                .as_deref()
                                .is_some_and(|k| probe_hit_serves_candidate(k, &candidate))
                        })
                        .filter_map(S3ListedObject::from_s3_object)
                        .collect();
                    Ok::<_, StorageError>(hits)
                }
            });
            let results: Vec<Result<Vec<S3ListedObject>, StorageError>> =
                futures::stream::iter(probes)
                    .buffer_unordered(Self::MAX_CONCURRENT_HEADS)
                    .collect()
                    .await;
            for result in results {
                // No ordering fix-up needed: `dedup_keep_latest` keys by user
                // key and `interleave_and_paginate` sorts every entry itself —
                // ordering (and duplicate absorption) is owned downstream.
                raw_objects.extend(result?);
            }
        }

        // Classify and build lite metadata (no HEAD calls — same as bulk_list_objects).
        let listing = Self::classify_listed_objects(raw_objects);
        let ambiguous = ambiguous_forms(&listing);
        // Every raw key from the token to the read horizon went through
        // this page, so a delimiter-less walk reads every reference.bin.
        let reference_prefixes = delimiter.is_none().then(|| {
            listing
                .baselines
                .iter()
                .filter_map(|(k, _)| Self::deltaspace_of_reference_key(k))
                .map(str::to_string)
                .collect()
        });
        let mut objects: Vec<(String, FileMetadata)> =
            Self::resolve_classified_lite(listing.classified, listing.dir_markers);
        let mut common_prefixes: Vec<String> = all_common_prefixes.into_iter().collect();
        if let (true, Some(anchor)) = (short, &settled_anchor) {
            // The anchor and what follows it start the next page.
            objects.retain(|(k, _)| k < anchor);
            common_prefixes.retain(|p| p < anchor);
        }

        // Apply max_keys across both objects and common_prefixes (interleaved)
        let mut page = crate::deltaglider::interleave_and_paginate(
            objects,
            common_prefixes,
            max_keys,
            continuation_token,
        );
        if short && !page.is_truncated {
            page.is_truncated = true;
            page.next_continuation_token = page
                .objects
                .last()
                .map(|(k, _)| k)
                .into_iter()
                .chain(page.common_prefixes.last())
                .max()
                .cloned();
        }

        debug!(
            "Delegated list: {} objects + {} prefixes in {}/{}{}",
            page.objects.len(),
            page.common_prefixes.len(),
            bucket,
            prefix,
            if short { " (short page)" } else { "" }
        );

        Ok(DelegatedListResult {
            objects: page.objects,
            common_prefixes: page.common_prefixes,
            is_truncated: page.is_truncated,
            next_continuation_token: page.next_continuation_token,
            upstream_requests,
            forms: crate::storage::ListedForms::Reported { ambiguous },
            reference_prefixes,
        })
    }
}

/// Pure: the user keys of a classified page whose stored forms are not
/// exactly one of `key` and `key.delta`: both forms (or a folder marker
/// and a `.delta` under one key), or a stored name that maps to the key
/// but is neither form (`a.delta.delta`). Sorted.
pub(super) fn ambiguous_forms(listing: &ClassifiedListing) -> Vec<String> {
    let mut seen: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    let mut odd = std::collections::BTreeSet::new();
    for (key, _) in &listing.dir_markers {
        *seen.entry(key.as_str()).or_default() += 1;
    }
    for c in &listing.classified {
        *seen.entry(c.user_key.as_str()).or_default() += 1;
        let exact =
            c.s3_key == c.user_key || c.s3_key.strip_suffix(".delta") == Some(c.user_key.as_str());
        if !exact {
            odd.insert(c.user_key.as_str());
        }
    }
    seen.into_iter()
        .filter(|(k, n)| *n > 1 || odd.contains(k))
        .map(|(k, _)| k.to_string())
        .collect()
}

/// The last entry a listing read so far: its last key, or its last
/// CommonPrefix when that sorts higher (upstream returns keys and prefixes
/// in one order, and every key under a returned prefix is read too).
pub(super) fn read_horizon<'a>(
    last_key: Option<&'a str>,
    last_prefix: Option<&'a str>,
) -> Option<&'a str> {
    last_key.into_iter().chain(last_prefix).max()
}

/// The anchor of a SHORT page (see `S3Backend::list_delegated_page`): the
/// highest entry after `continuation_token` and at or below `read` (the
/// last entry read), when at least one more entry after the token sorts
/// below it, so the page is not empty. Pure; keys map to user keys and
/// baselines drop out as in `list_anchor`.
pub(super) fn short_page_anchor<'a>(
    raw_keys: impl Iterator<Item = &'a str>,
    common_prefixes: impl Iterator<Item = &'a str>,
    continuation_token: Option<&str>,
    read: &str,
) -> Option<String> {
    let token = continuation_token.unwrap_or("");
    if read <= token {
        return None;
    }
    let mut entries: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for key in raw_keys {
        if crate::types::is_reference_key(key) {
            continue;
        }
        entries.insert(key.trim_end_matches(".delta"));
    }
    entries.extend(common_prefixes);
    let mut proven = entries.range::<str, _>((
        std::ops::Bound::Excluded(token),
        std::ops::Bound::Included(read),
    ));
    let anchor = proven.next_back()?;
    proven.next()?;
    Some(anchor.to_string())
}

/// What a delegated listing costs in upstream requests, and how it ends.
#[cfg(test)]
mod delegated_cost_tests {
    use super::*;
    use crate::storage::FakeS3;

    async fn backend_with_keys(keys: &[String]) -> (S3Backend, std::sync::Arc<FakeS3>) {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let http = reqwest::Client::new();
        futures::stream::iter(keys)
            .for_each_concurrent(32, |k| {
                let req = http.put(format!("{endpoint}/b/{k}")).body("x");
                async move {
                    req.send().await.unwrap();
                }
            })
            .await;
        fake.clear();
        (test_support::for_test_endpoint(&endpoint), fake)
    }

    fn lists(fake: &FakeS3) -> usize {
        fake.requests()
            .iter()
            .filter(|r| r.starts_with("GET /b") && r.contains("list-type=2"))
            .count()
    }

    /// A recursive listing of 3,000 keys sends at most two upstream LISTs
    /// per client page. It sent three: the anchor of a full page (key 1001)
    /// is on the second upstream page, the next client page read that page
    /// again, and every page probed for a late `.delta` key that sorts
    /// before the page.
    #[tokio::test]
    async fn a_recursive_listing_sends_at_most_two_lists_per_page() {
        let keys: Vec<String> = (0..3000).map(|i| format!("d/img-{i:05}.jpg")).collect();
        let (s3, fake) = backend_with_keys(&keys).await;
        let mut got: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        let mut per_page = Vec::new();
        loop {
            let before = lists(&fake);
            let page = s3
                .list_objects_delegated("b", "d/", None, 1000, token.as_deref())
                .await
                .unwrap()
                .unwrap();
            per_page.push(lists(&fake) - before);
            got.extend(page.objects.into_iter().map(|(k, _)| k));
            if !page.is_truncated {
                break;
            }
            token = page.next_continuation_token;
            assert_eq!(token.as_ref(), got.last());
        }
        assert_eq!(got, keys, "every key once, in order");
        assert!(
            per_page.iter().all(|n| *n <= 2),
            "upstream LISTs per client page: {per_page:?}"
        );
    }

    /// P6.1: a page reports which keys it saw in more than one stored form
    /// (or under a stored name that is neither form), also when the
    /// `.delta` form sorts past the page and only the probe reads it.
    #[tokio::test]
    async fn a_page_reports_the_keys_it_saw_in_both_forms() {
        let mut keys: Vec<String> = ["a", "a.delta", "b.delta", "c", "e.delta.delta"]
            .iter()
            .map(|k| format!("d/{k}"))
            .collect();
        // `d/k.delta` sorts after every `d/k-…` key: past a 3-key page.
        keys.extend(["d/k".to_string(), "d/k.delta".to_string()]);
        keys.extend((0..5).map(|i| format!("d/k-{i}")));
        let (s3, _fake) = backend_with_keys(&keys).await;
        let page = s3
            .list_objects_delegated("b", "d/", None, 1000, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            page.forms,
            crate::storage::ListedForms::Reported {
                ambiguous: vec!["d/a".into(), "d/e".into(), "d/k".into()]
            }
        );
        let page = s3
            .list_objects_delegated("b", "d/", None, 6, None)
            .await
            .unwrap()
            .unwrap();
        let listed: Vec<&str> = page.objects.iter().map(|(k, _)| k.as_str()).collect();
        assert!(listed.contains(&"d/k"), "{listed:?}");
        assert_eq!(
            page.forms,
            crate::storage::ListedForms::Reported {
                ambiguous: vec!["d/a".into(), "d/e".into(), "d/k".into()]
            }
        );
        // The one form of a single-form key is the form of its entry.
        let (_, b) = page.objects.iter().find(|(k, _)| k == "d/b").unwrap();
        assert_eq!(
            page.forms.single_form("d/b", b),
            Some(crate::storage::ObjectVariant::Delta)
        );
        let (_, c) = page.objects.iter().find(|(k, _)| k == "d/c").unwrap();
        assert_eq!(
            page.forms.single_form("d/c", c),
            Some(crate::storage::ObjectVariant::Passthrough)
        );
        assert_eq!(page.forms.single_form("d/a", c), None);
    }

    /// P6.4: a walk of every page of a delimiter-less listing from its start
    /// reads every reference.bin of the prefix, at any page size: the
    /// pages' reports add up to `list_reference_prefixes`.
    #[tokio::test]
    async fn a_walk_reports_every_reference() {
        let mut keys: Vec<String> = vec!["reference.bin".into(), "a.txt".into()];
        for d in ["d", "d/e", "f", "z"] {
            keys.push(format!("{d}/reference.bin"));
            for i in 0..3 {
                keys.push(format!("{d}/k-{i}.zip.delta"));
            }
        }
        keys.push("g/only.txt".into());
        let (s3, _fake) = backend_with_keys(&keys).await;
        let want: std::collections::BTreeSet<String> = s3
            .list_reference_prefixes("b", "")
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(want.len(), 5, "{want:?}");
        for max_keys in [1, 2, 3, 1000] {
            let mut seen = std::collections::BTreeSet::new();
            let mut token: Option<String> = None;
            loop {
                let page = s3
                    .list_objects_delegated("b", "", None, max_keys, token.as_deref())
                    .await
                    .unwrap()
                    .unwrap();
                seen.extend(page.reference_prefixes.expect("reported"));
                if !page.is_truncated {
                    break;
                }
                token = page.next_continuation_token;
            }
            assert_eq!(seen, want, "max_keys {max_keys}");
        }
        // A delimiter page does not read the subtree: not reported.
        let page = s3
            .list_objects_delegated("b", "", Some("/"), 1000, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.reference_prefixes, None);
    }

    /// L1 (#171, S3 backend): keys that an older release stored under a
    /// `.dg` directory are ordinary keys, listed flat and with a delimiter,
    /// and the recursive delimiter walk equals the flat listing at every
    /// page size. The facts and the baselines are listed nowhere. Unicode and
    /// a dot-dir stay ordinary keys.
    #[tokio::test]
    async fn a_legacy_dg_key_is_listed_the_same_way_flat_and_with_a_delimiter() {
        let keys: Vec<String> = [
            ".dg/facts/x!!1.e.1.2.e",
            ".dg/foo",
            ".dg/reference.bin",
            "a/.dg/bar",
            "a/.dg/reference.bin",
            "a/x",
            "a-b/y",
            "a_b/z",
            ".dgx/k",
            ".well-known/w",
            "é/u",
            "top",
        ]
        .map(String::from)
        .to_vec();
        let (s3, _fake) = backend_with_keys(&keys).await;
        let want: Vec<String> = [
            ".dg/foo",
            ".dgx/k",
            ".well-known/w",
            "a-b/y",
            "a/.dg/bar",
            "a/x",
            "a_b/z",
            "top",
            "é/u",
        ]
        .map(String::from)
        .to_vec();
        for max_keys in [1u32, 2, 3, 1000] {
            let mut flat = Vec::new();
            let mut token: Option<String> = None;
            loop {
                let page = s3
                    .list_objects_delegated("b", "", None, max_keys, token.as_deref())
                    .await
                    .unwrap()
                    .unwrap();
                flat.extend(page.objects.into_iter().map(|(k, _)| k));
                if !page.is_truncated {
                    break;
                }
                token = page.next_continuation_token;
            }
            assert_eq!(flat, want, "flat, max_keys {max_keys}");
            let mut walked = Vec::new();
            let mut todo = vec![String::new()];
            while let Some(prefix) = todo.pop() {
                let mut token: Option<String> = None;
                loop {
                    let page = s3
                        .list_objects_delegated("b", &prefix, Some("/"), max_keys, token.as_deref())
                        .await
                        .unwrap()
                        .unwrap();
                    walked.extend(page.objects.into_iter().map(|(k, _)| k));
                    todo.extend(page.common_prefixes);
                    if !page.is_truncated {
                        break;
                    }
                    token = page.next_continuation_token;
                }
            }
            walked.sort();
            assert_eq!(walked, want, "walk, max_keys {max_keys}");
        }
        // Asked for by name, the proxy's directory lists the legacy key,
        // never the facts or a baseline.
        let page = s3
            .list_objects_delegated("b", ".dg/", None, 1000, None)
            .await
            .unwrap()
            .unwrap();
        let names: Vec<_> = page.objects.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, [".dg/foo"]);
    }

    /// L2 (#171): a CommonPrefix is shown only when a key that LIST shows
    /// lies under it, whatever the delimiter. The model builds the prefixes
    /// from the visible keys; the proxy's own files (the facts, the
    /// baselines) make none. Every page size.
    #[tokio::test]
    async fn the_common_prefixes_of_any_delimiter_come_from_visible_keys_only() {
        // `.dg/facts/..` exists in every bucket; the rest are the shapes
        // that made phantoms.
        let key_sets: [&[&str]; 6] = [
            &[".dg/facts/x!!1.e.1.2.e", "top", "a/x"],
            // The baselines of today (`<folder>/reference.bin`).
            &[".dg/facts/x!!1.e.1.2.e", "a/x.txt", "a/reference.bin"],
            &[".dg/facts/x!!1.e.1.2.e", "top", "reference.bin", "a/b/c"],
            &[
                ".dg/facts/x!!1.e.1.2.e",
                "a/reference.bin",
                "a/b",
                ".well-known/w",
            ],
            &[
                ".dg/facts/x!!1.e.1.2.e",
                ".dg/old.txt",
                "a/.dg/bar",
                "a/.dg/reference.bin",
                ".dgx/k",
                ".dg-x",
            ],
            &[".dg/facts/x!!1.e.1.2.e", ".dg/facts/y!!1.e.1.2.e"],
        ];
        for keys in key_sets {
            let owned: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
            let (s3, _fake) = backend_with_keys(&owned).await;
            let visible: Vec<&str> = keys
                .iter()
                .copied()
                .filter(|k| listing_facts::shows_in_listing(k))
                .collect();
            for prefix in ["", "a/", ".dg/", ".d", "a/.", ".dg/f"] {
                for delimiter in ["/", ".", "d", "g", "x", "-", "r", "f"] {
                    let mut want_objects = Vec::new();
                    let mut want_prefixes = std::collections::BTreeSet::new();
                    for k in visible.iter().filter(|k| k.starts_with(prefix)) {
                        let rest = &k[prefix.len()..];
                        match rest.find(delimiter) {
                            Some(i) => {
                                want_prefixes.insert(format!("{prefix}{}{delimiter}", &rest[..i]));
                            }
                            None => want_objects.push(k.to_string()),
                        }
                    }
                    want_objects.sort();
                    for max_keys in [1u32, 1000] {
                        let mut objects = Vec::new();
                        let mut prefixes = std::collections::BTreeSet::new();
                        let mut token: Option<String> = None;
                        loop {
                            let page = s3
                                .list_objects_delegated(
                                    "b",
                                    prefix,
                                    Some(delimiter),
                                    max_keys,
                                    token.as_deref(),
                                )
                                .await
                                .unwrap()
                                .unwrap();
                            objects.extend(page.objects.into_iter().map(|(k, _)| k));
                            prefixes.extend(page.common_prefixes);
                            if !page.is_truncated {
                                break;
                            }
                            token = page.next_continuation_token;
                        }
                        let ctx = format!(
                            "{keys:?} prefix {prefix:?} delimiter {delimiter:?} max {max_keys}"
                        );
                        assert_eq!(objects, want_objects, "objects: {ctx}");
                        assert_eq!(prefixes, want_prefixes, "prefixes: {ctx}");
                    }
                }
            }
        }
    }

    /// Every page of a delegated LIST: its objects (in order) and its
    /// CommonPrefixes.
    async fn walk_pages<B: crate::storage::StorageBackend>(
        backend: &B,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
    ) -> (Vec<String>, Vec<String>) {
        let mut objects = Vec::new();
        let mut prefixes = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..200 {
            let page = backend
                .list_objects_delegated("b", prefix, delimiter, max_keys, token.as_deref())
                .await
                .unwrap()
                .unwrap();
            objects.extend(page.objects.into_iter().map(|(k, _)| k));
            prefixes.extend(page.common_prefixes);
            if !page.is_truncated {
                return (objects, prefixes);
            }
            token = page.next_continuation_token;
        }
        panic!("the listing does not end: {prefix:?} {delimiter:?} {max_keys}");
    }

    /// The guard of THE hiding rule (`listing_facts::is_own_file`): the same
    /// keys give the same objects and the same CommonPrefixes on the S3
    /// backend (fake) and on the filesystem backend, for every delimiter and
    /// page size. The S3 side holds the listing facts too (the filesystem has
    /// none); both hold the baselines of today (`<folder>/reference.bin`) and
    /// of the legacy layout (`<folder>/.dg/reference.bin`), and legacy client
    /// keys below `.dg`. A folder that holds ONLY a baseline is the one case
    /// they differ in (`an_s3_listing_shows_a_folder_with_only_a_baseline`).
    #[tokio::test]
    async fn s3_and_the_filesystem_list_the_same_keys() {
        let sets: &[&[&str]] = &[
            &["a/x.txt", "a/reference.bin"],
            &["top", "reference.bin", "a/b/c", "a/b/reference.bin"],
            &["a/reference.bin", "a/b", ".well-known/w", "é/u"],
            &[
                ".dg/old.txt",
                "a/.dg/bar",
                "a/.dg/reference.bin",
                ".dgx/k",
                ".dg-x",
                "a-b/y",
                "a_b/z",
            ],
            &["x.zip", "d/y", "d/reference.bin"],
        ];
        for keys in sets {
            assert_same_listings(keys).await;
        }
    }

    /// Both backends hold `keys` (S3 also the listing facts that every bucket
    /// has, the filesystem raw files listed from their stat); every prefix,
    /// delimiter (one or several characters) and page size lists the same.
    ///
    /// KNOWN LIMIT, kept OUT of these key sets: a `.delta` stored name. A
    /// delimiter that occurs inside the suffix `.delta` makes S3 return a
    /// prefix of the STORED name (`d/y.delta` with `.` gives `d/y.`; a
    /// delimiter `l` hides `a/x.zip.delta` behind `a/x.zip.del`), where the
    /// filesystem lists the key. That is older than this work and is not
    /// fixed here.
    async fn assert_same_listings(keys: &[&str]) {
        let mut s3_keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
        s3_keys.push(".dg/facts/x!!1.e.1.2.e".into());
        s3_keys.push(".dg/facts/y!!1.e.1.2.e".into());
        let (s3, _fake) = backend_with_keys(&s3_keys).await;
        let dir = tempfile::tempdir().unwrap();
        let fs = crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap();
        fs.create_bucket("b").await.unwrap();
        let root = dir.path().join("b").join("deltaspaces");
        for key in keys.iter() {
            let path = root.join(key);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }
        for prefix in [
            "", "a/", "a/.", ".dg/", ".d", "d/", "a/b/", "r", "a/r", "ref",
        ] {
            for delimiter in [
                None,
                Some("/"),
                Some("."),
                Some("r"),
                Some("e"),
                Some("-"),
                Some("_"),
                Some("ref"),
                Some("/."),
                Some("re"),
                Some(".d"),
                Some("dg"),
                Some("/r"),
                Some("/ref"),
            ] {
                for max_keys in [1u32, 2, 3, 1000] {
                    let ctx = format!("{keys:?} {prefix:?} {delimiter:?} {max_keys}");
                    let (s3_objects, s3_prefixes) =
                        walk_pages(&s3, prefix, delimiter, max_keys).await;
                    let (fs_objects, fs_prefixes) =
                        walk_pages(&fs, prefix, delimiter, max_keys).await;
                    assert_eq!(s3_objects, fs_objects, "objects: {ctx}");
                    assert_eq!(s3_prefixes, fs_prefixes, "prefixes: {ctx}");
                }
            }
        }
    }

    /// The same, over generated key sets: names that make the folders, files
    /// and delimiters meet (`r`, `ref`, `re`, `.dg`, `reference.bin`, a dot
    /// file). A set where a folder holds ONLY a baseline is skipped (the one
    /// known difference, `an_s3_listing_shows_a_folder_with_only_a_baseline`).
    #[tokio::test]
    async fn s3_and_the_filesystem_list_generated_key_sets_the_same() {
        let dirs = ["a", "b", ".dg", "dg", "r", "ref"];
        let files = ["x", "reference.bin", "y.txt", ".k", "r1", "re"];
        let mut seed: u64 = 0x1234_5678_9abc_def1;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut sets = 0;
        for _ in 0..30 {
            let mut keys: std::collections::BTreeSet<String> = Default::default();
            for _ in 0..3 + (next() % 7) as usize {
                let depth = (next() % 3) as usize;
                let mut parts: Vec<&str> = (0..depth)
                    .map(|_| dirs[(next() % dirs.len() as u64) as usize])
                    .collect();
                parts.push(files[(next() % files.len() as u64) as usize]);
                keys.insert(parts.join("/"));
            }
            // A key that is also the folder of another key cannot exist.
            let all: Vec<String> = keys.iter().cloned().collect();
            let keys: Vec<String> = all
                .iter()
                .filter(|k| !all.iter().any(|o| o.starts_with(&format!("{k}/"))))
                .cloned()
                .collect();
            let only_a_baseline = keys.iter().any(|k| {
                crate::types::is_reference_key(k) && {
                    let folder = k.trim_end_matches(crate::types::REFERENCE_FILE_NAME);
                    !folder.is_empty()
                        && !keys
                            .iter()
                            .any(|o| o.starts_with(folder) && !crate::types::is_reference_key(o))
                }
            });
            if only_a_baseline {
                continue;
            }
            sets += 1;
            let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            assert_same_listings(&refs).await;
        }
        assert!(
            sets >= 10,
            "only {sets} of 30 generated sets were comparable"
        );
    }

    /// A KNOWN LIMIT older than this work, pinned so that a fix is a decision
    /// and the compatibility page stays true: a delimiter inside the suffix
    /// `.delta` of a stored name gives a prefix of the stored name.
    #[tokio::test]
    async fn a_delimiter_inside_the_delta_suffix_gives_a_prefix_of_the_stored_name() {
        let keys = vec!["d/y.delta".to_string(), "a/x.zip.delta".to_string()];
        let (s3, _fake) = backend_with_keys(&keys).await;
        let (objects, prefixes) = walk_pages(&s3, "", Some("."), 1000).await;
        assert!(objects.is_empty(), "{objects:?}");
        assert!(prefixes.contains(&"d/y.".to_string()), "{prefixes:?}");
        let (objects, prefixes) = walk_pages(&s3, "a/", Some("l"), 1000).await;
        assert!(objects.is_empty(), "{objects:?}");
        assert_eq!(prefixes, ["a/x.zip.del"]);
    }

    /// The one case where the backends differ: a folder that holds ONLY a
    /// baseline (an orphan that DeleteBucket reclaims). The filesystem knows
    /// it for free; on S3 it takes a request per folder, which a LIST page
    /// does not send. Pinned so that a change is a decision.
    #[tokio::test]
    async fn an_s3_listing_shows_a_folder_with_only_a_baseline() {
        let keys = vec!["ba/reference.bin".to_string(), "top".to_string()];
        let (s3, _fake) = backend_with_keys(&keys).await;
        let (_, prefixes) = walk_pages(&s3, "", Some("/"), 1000).await;
        assert_eq!(prefixes, ["ba/"]);
        let dir = tempfile::tempdir().unwrap();
        let fs = crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap();
        fs.create_bucket("b").await.unwrap();
        let ba = dir.path().join("b").join("deltaspaces").join("ba");
        std::fs::create_dir_all(&ba).unwrap();
        std::fs::write(ba.join("reference.bin"), b"x").unwrap();
        let (_, prefixes) = walk_pages(&fs, "", Some("/"), 1000).await;
        assert!(prefixes.is_empty(), "{prefixes:?}");
    }

    /// Request cost of the root `.dg/` CommonPrefix (the listing facts): the
    /// probe jumps past the facts, so it costs two requests (past the facts,
    /// and up to the first facts key) however many facts there are, and none
    /// once the answer is remembered. A legacy key next to the facts is found.
    #[tokio::test]
    async fn a_root_delimiter_list_probes_past_the_facts() {
        for facts in [3usize, 40, 400] {
            let mut keys: Vec<String> = (0..facts)
                .map(|i| format!(".dg/facts/k{i:04}!!1.e.1.2.e"))
                .collect();
            keys.extend(["top".to_string(), "a/x".to_string()]);
            let (s3, fake) = backend_with_keys(&keys).await;
            let page = s3
                .list_objects_delegated("b", "", Some("/"), 1000, None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(page.common_prefixes, ["a/"], "{facts} facts");
            assert_eq!(lists(&fake), 3, "{facts} facts: {:?}", fake.requests());
            assert_eq!(page.upstream_requests, 3, "{facts} facts");
            // Remembered: the next LIST of the level is the one request.
            fake.clear();
            s3.list_objects_delegated("b", "", Some("/"), 1000, None)
                .await
                .unwrap();
            assert_eq!(lists(&fake), 1, "{facts} facts, again");
        }
        // A legacy key after the facts (`.dg/z`) is found by the first range,
        // one before them (`.dg/a`) by the second.
        for legacy in [".dg/z", ".dg/a"] {
            let keys = vec![
                ".dg/facts/k0!!1.e.1.2.e".to_string(),
                legacy.to_string(),
                "top".to_string(),
            ];
            let (s3, fake) = backend_with_keys(&keys).await;
            let page = s3
                .list_objects_delegated("b", "", Some("/"), 1000, None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(page.common_prefixes, [".dg/"], "{legacy}");
            assert_eq!(
                lists(&fake),
                if legacy == ".dg/z" { 2 } else { 3 },
                "{legacy}: {:?}",
                fake.requests()
            );
        }
    }

    /// A page probes at most `PROBES_PER_PAGE` prefixes: 12 folders that each
    /// hold a baseline (so the delimiter `r` makes a prefix `dN/r` of only a
    /// baseline) send 8 probes; the other 4 prefixes are shown, unchecked.
    #[tokio::test]
    async fn a_page_probes_a_bounded_number_of_prefixes() {
        let mut keys = Vec::new();
        for i in 0..12 {
            keys.push(format!("d{i:02}/reference.bin"));
            keys.push(format!("d{i:02}/x"));
        }
        let (s3, fake) = backend_with_keys(&keys).await;
        let page = s3
            .list_objects_delegated("b", "", Some("r"), 1000, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.objects.len(), 12, "the x keys");
        assert_eq!(
            page.common_prefixes.len(),
            12 - PROBES_PER_PAGE as usize,
            "{:?}",
            page.common_prefixes
        );
        assert_eq!(
            lists(&fake),
            1 + PROBES_PER_PAGE as usize,
            "{:?}",
            fake.requests()
        );
    }

    /// The listing a client must see: the objects and CommonPrefixes of the
    /// LISTED keys (not the proxy's own files) for `prefix` and `delimiter`.
    fn model_listing(
        keys: &[&str],
        prefix: &str,
        delimiter: &str,
    ) -> (Vec<String>, std::collections::BTreeSet<String>) {
        let mut objects = Vec::new();
        let mut prefixes = std::collections::BTreeSet::new();
        for k in keys
            .iter()
            .filter(|k| listing_facts::shows_in_listing(k) && k.starts_with(prefix))
        {
            let rest = &k[prefix.len()..];
            match rest.find(delimiter) {
                Some(i) => {
                    prefixes.insert(format!("{prefix}{}{delimiter}", &rest[..i]));
                }
                None => objects.push(k.to_string()),
            }
        }
        objects.sort();
        (objects, prefixes)
    }

    /// A LIST must not depend on what an earlier LIST found: a client writes
    /// a key under a prefix that held only the proxy's own files a moment
    /// ago, and the next LIST shows it. (A remembered answer is kept only for
    /// a prefix below `.dg`, where no client write adds a key.)
    #[tokio::test]
    async fn a_key_written_after_a_probe_shows_in_the_next_list() {
        let facts = ".dg/facts/x!!1.e.1.2.e";
        // (keys before, prefix, delimiter, key written, name)
        let cases: &[(&[&str], &str, &str, &str)] = &[
            (&[facts, "a/reference.bin", "a/x"], "", "r", "a/readme"),
            (&[facts, "a/reference.bin", "a/x"], "a/", "r", "a/readme"),
            (&[facts, "top"], "", ".", ".env"),
            (&[facts, "top"], "", ".", ".well-known/x"),
            (&[facts, "a/reference.bin", "a/x"], "a/", ".", "a/.hidden"),
            (&[facts, "reference.bin", "top"], "", "r", "readme"),
        ];
        for (keys, prefix, delimiter, written) in cases {
            let owned: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
            let (endpoint, _fake) = crate::storage::fake_s3::start().await;
            let http = reqwest::Client::new();
            for k in &owned {
                http.put(format!("{endpoint}/b/{k}"))
                    .body("x")
                    .send()
                    .await
                    .unwrap();
            }
            let s3 = test_support::for_test_endpoint(&endpoint);
            let before = walk_pages(&s3, prefix, Some(delimiter), 1000).await;
            let ctx = format!("{keys:?} {prefix:?} {delimiter:?} then {written:?}");
            let (want_objects, want_prefixes) = model_listing(keys, prefix, delimiter);
            assert_eq!(before.0, want_objects, "before: {ctx}");
            assert_eq!(
                before
                    .1
                    .iter()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>(),
                want_prefixes,
                "before: {ctx}"
            );
            http.put(format!("{endpoint}/b/{written}"))
                .body("x")
                .send()
                .await
                .unwrap();
            let mut all: Vec<&str> = keys.to_vec();
            all.push(written);
            let (want_objects, want_prefixes) = model_listing(&all, prefix, delimiter);
            let after = walk_pages(&s3, prefix, Some(delimiter), 1000).await;
            assert_eq!(after.0, want_objects, "after: {ctx}");
            assert_eq!(
                after
                    .1
                    .iter()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>(),
                want_prefixes,
                "after: {ctx}"
            );
        }
    }

    /// A probe that fails never fails the LIST: the prefix is shown. MinIO
    /// answers `XMinioInvalidResourceName` for a probe of `prefix=.`.
    #[tokio::test]
    async fn a_failed_probe_shows_the_prefix_and_not_an_error() {
        let keys = vec![
            ".dg/facts/x!!1.e.1.2.e".to_string(),
            "a/reference.bin".to_string(),
            "a/x".to_string(),
        ];
        let (s3, fake) = backend_with_keys(&keys).await;
        // The probes ask for 16 keys; the page itself for more.
        fake.fail(
            "LIST",
            "max-keys=16",
            400,
            "XMinioInvalidResourceName",
            u32::MAX,
        );
        let (objects, prefixes) = walk_pages(&s3, "", Some("r"), 1000).await;
        assert_eq!(objects, ["a/x"]);
        assert_eq!(prefixes, ["a/r"], "shown: the probe could not tell");
        let (_, prefixes) = walk_pages(&s3, "", Some("."), 1000).await;
        assert_eq!(prefixes, [".", "a/reference."], "{prefixes:?}");
        assert!(
            fake.faulted("LIST", "max-keys=16") > 0,
            "the probes ran: {:?}",
            fake.requests()
        );
    }

    /// A page reports the requests it sent (its pages and its probes), so
    /// the scan budget of a filtered LIST can count them.
    #[tokio::test]
    async fn a_page_reports_the_requests_it_sent() {
        // `d/a.delta` sorts after every `d/a-…` key: the first page probes it.
        let mut keys: Vec<String> = (0..300).map(|i| format!("d/a-{i:04}")).collect();
        keys.extend((0..3000).map(|i| format!("d/img-{i:05}.jpg")));
        let (s3, fake) = backend_with_keys(&keys).await;
        let mut token: Option<String> = None;
        let mut reported = Vec::new();
        let mut sent = Vec::new();
        for max_keys in [2, 1000, 1000, 1000, 1000] {
            let before = lists(&fake);
            let page = s3
                .list_objects_delegated("b", "d/", None, max_keys, token.as_deref())
                .await
                .unwrap()
                .unwrap();
            reported.push(page.upstream_requests as usize);
            sent.push(lists(&fake) - before);
            token = page.next_continuation_token;
        }
        assert_eq!(reported, sent);
        assert_eq!(sent[0], 2, "the first page probes `d/a.delta`");
    }

    /// A page of a few keys is served from one upstream LIST.
    #[tokio::test]
    async fn a_small_page_costs_one_list() {
        let keys: Vec<String> = (0..300).map(|i| format!("d/img-{i:05}.jpg")).collect();
        let (s3, fake) = backend_with_keys(&keys).await;
        let page = s3
            .list_objects_delegated("b", "d/", None, 100, Some("d/img-00099.jpg"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.objects.len(), 100);
        assert_eq!(page.objects[0].0, "d/img-00100.jpg");
        assert!(page.is_truncated);
        assert_eq!(lists(&fake), 1, "{:?}", fake.requests());
    }

    /// A backend that answers `IsTruncated=true` with no continuation
    /// token: both listing loops fail. They sent the next request with no
    /// token, got the first page again, and looped for ever.
    #[tokio::test]
    async fn a_truncated_page_without_a_token_fails_the_listing() {
        let app = axum::Router::new().fallback(|| async {
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Name>b</Name><Prefix></Prefix><MaxKeys>1000</MaxKeys>\
             <IsTruncated>true</IsTruncated><Contents><Key>a</Key>\
             <LastModified>2025-01-01T00:00:00.000Z</LastModified><ETag>\"e\"</ETag>\
             <Size>1</Size><StorageClass>STANDARD</StorageClass></Contents>\
             </ListBucketResult>"
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let s3 = test_support::for_test_endpoint(&endpoint);
        let limit = std::time::Duration::from_secs(5);
        let full = tokio::time::timeout(limit, s3.list_objects_full("b", ""))
            .await
            .expect("list_objects_full looped");
        assert!(full.is_err());
        let delegated =
            tokio::time::timeout(limit, s3.list_objects_delegated("b", "", None, 1000, None))
                .await
                .expect("list_objects_delegated looped");
        assert!(delegated.is_err());
    }

    #[test]
    fn a_short_page_anchors_at_the_last_entry_read() {
        let keys = ["d/a", "d/b.delta", "d/b", "d/c", "d/reference.bin", "d/e"];
        let cps = ["d/c/", "d/z/"];
        let anchor =
            |token, read| short_page_anchor(keys.iter().copied(), cps.iter().copied(), token, read);
        // Entries: d/a d/b d/c d/c/ d/e d/z/ (the baseline drops out).
        assert_eq!(anchor(None, "d/e").as_deref(), Some("d/e"));
        assert_eq!(anchor(None, "d/d").as_deref(), Some("d/c/"));
        assert_eq!(
            anchor(Some("d/a"), "d/c").as_deref(),
            Some("d/c"),
            "d/c/ > d/c"
        );
        assert_eq!(anchor(Some("d/b"), "d/c"), None, "d/c alone");
        assert_eq!(anchor(None, "d/z/").as_deref(), Some("d/z/"));
        // One entry after the token: no page below it.
        assert_eq!(anchor(Some("d/c/"), "d/e"), None);
        assert_eq!(
            anchor(Some("d/e"), "d/e"),
            None,
            "nothing read after the token"
        );
        assert_eq!(anchor(Some("d/z"), "d/e"), None, "read is below the token");
        assert_eq!(read_horizon(Some("d/e"), Some("d/c/")), Some("d/e"));
        assert_eq!(read_horizon(Some("d/a"), Some("d/c/")), Some("d/c/"));
        assert_eq!(read_horizon(None, None), None);
    }

    /// Every upstream LIST loop that follows a continuation token stops
    /// with an error when a truncated page has none, instead of asking for
    /// the first page again.
    #[test]
    fn every_token_loop_stops_without_a_token() {
        for (file, text) in crate::source_scan::prod_sources("src/storage/s3") {
            let lines: Vec<(usize, &str)> = crate::source_scan::prod_lines(&text);
            for (i, (n, line)) in lines.iter().enumerate() {
                if !line.contains("= response.next_continuation_token;") {
                    continue;
                }
                let handled = lines[i..lines.len().min(i + 8)]
                    .iter()
                    .any(|(_, l)| l.contains("truncated_without_token("));
                assert!(
                    handled,
                    "{file}:{n}: a truncated page without a token loops"
                );
            }
        }
    }
}

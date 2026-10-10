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
        // Listing facts are internal index entries, never objects.
        if listing_facts::is_facts_key(&key) {
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
                dir_markers.push((obj.key.clone(), FileMetadata::directory_marker(&obj.key)));
                continue;
            }

            // Internal deltaspace file: never a user-visible object.
            if filename == "reference.bin" {
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
        FileMetadata::fallback(
            user_key.rsplit('/').next().unwrap_or(user_key).to_string(),
            obj.size,
            obj.etag.clone().unwrap_or_default(),
            obj.last_modified.unwrap_or_else(Utc::now),
            None,
            storage_info,
        )
    }

    /// LIST a deltaspace and return only the entries at the prefix level
    /// itself (not from subdirectories). Shared between
    /// [`scan_deltaspace`] and [`scan_deltaspace_lite`].
    ///
    /// A LIST of `prefix/` returns the whole subtree, so the entries of
    /// child deltaspaces are dropped here, as the filesystem backend skips
    /// child directories. They are not this deltaspace's objects: a child's
    /// deltas use the child's own reference.bin. (Kept, they stopped the
    /// reclaim of the parent's reference.bin, and the delta-efficiency scan
    /// counted a child's objects in the parent too.)
    pub(super) async fn list_deltaspace_eligible(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<S3ListedObject>, StorageError> {
        let search_prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", prefix)
        };
        let listed = self.list_objects_full(bucket, &search_prefix).await?;
        let eligible: Vec<S3ListedObject> = listed
            .into_iter()
            .filter(|obj| {
                obj.key
                    .strip_prefix(&search_prefix)
                    .is_some_and(|name| !name.contains('/'))
            })
            .collect();
        Ok(eligible)
    }

    /// Build a no-HEAD FileMetadata for a listed object. For deltas the
    /// resulting `file_size` is the on-disk delta size, not the original.
    /// Suitable only for callers that explicitly opt into the lite shape.
    pub(super) fn lite_metadata_from_listed(obj: &S3ListedObject) -> FileMetadata {
        let filename = obj.key.rsplit('/').next().unwrap_or(&obj.key);
        let is_delta = filename.ends_with(".delta");
        let is_reference = filename == "reference.bin";
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
        FileMetadata::fallback(
            original_name,
            obj.size,
            obj.etag.clone().unwrap_or_default(),
            obj.last_modified.unwrap_or_else(Utc::now),
            None,
            storage_info,
        )
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
        let filename = key.rsplit('/').next().unwrap_or(key);
        if filename == "reference.bin" {
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

impl S3Backend {
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

            let response = request
                .send()
                .await
                .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))?;
            DELEGATED_LIST_UPSTREAM_PAGES.inc();

            // Collect CommonPrefixes, skipping ONLY the `.dg/` internal deltaspace
            // directory (never a user-visible key). Deliberately narrow: an
            // earlier version skipped ANY dot-prefixed segment, which also hid
            // legitimate folders like `.well-known/` from user-facing listings.
            if let Some(cps) = response.common_prefixes {
                for cp in cps {
                    if let Some(p) = cp.prefix {
                        let last_seg = p.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                        if last_seg == ".dg" || listing_facts::is_internal_common_prefix(&p) {
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
        })
    }
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
        if key.rsplit('/').next() == Some("reference.bin") {
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

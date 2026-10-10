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

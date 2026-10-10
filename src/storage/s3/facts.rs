// SPDX-License-Identifier: BUSL-1.1

//! Durable listing facts of stored objects (`storage::listing_facts`).

use super::*;

/// Facts LIST pages one listing page may read. The facts of one page fit in
/// one or two; more means stale entries, and the rest keep stored sizes.
pub(super) const MAX_FACTS_PAGES: usize = 4;

impl S3Backend {
    /// Record, in the listing-size cache, what a client must see for the
    /// stored object `key` (see `list_size_cache`). Called wherever the
    /// backend learns the logical metadata anyway (the PUT it sends, the HEAD
    /// it sends), so it costs no request. Baselines are never listed to
    /// clients, so they get no entry.
    pub(super) fn remember_listed_facts(
        &self,
        bucket: &str,
        key: &str,
        stored_etag: &str,
        stored_size: u64,
        meta: &FileMetadata,
    ) {
        if key.rsplit('/').next() == Some("reference.bin") {
            return;
        }
        list_size_cache::record(
            &StoredObjectId {
                scope: &self.list_cache_scope,
                bucket,
                key,
                etag: stored_etag,
                size: stored_size,
            },
            LogicalFacts::of(meta),
        );
    }

    /// Queue the durable listing facts of the stored object `key` (see
    /// `storage::listing_facts`) for the background writer (storage-8: no
    /// facts request on the client's PUT path). Best effort: the object is
    /// stored already; until the facts object lands, a LIST reports the
    /// stored size. Older entries of the key are left to the facts GC.
    pub(super) fn persist_listing_facts(
        &self,
        bucket: &str,
        key: &str,
        stored_etag: &str,
        stored_size: u64,
        meta: &FileMetadata,
    ) {
        if key.rsplit('/').next() == Some("reference.bin") {
            return;
        }
        let Some(facts_key) =
            listing_facts::facts_key(key, stored_etag, stored_size, &LogicalFacts::of(meta))
        else {
            return;
        };
        self.facts_cleanup.write(bucket, key, facts_key);
    }

    /// Lazy backfill: a HEAD learned the facts of a stored object that a LIST
    /// found without durable facts (an object stored before they existed, or
    /// whose facts write failed). Write them in the background, once, through
    /// the bounded facts drain (`FACTS_WRITE_CONCURRENCY` writes at once): a
    /// task per HEAD put about 200 PUTs at once on a slow backend.
    pub(super) fn backfill_listing_facts(
        &self,
        bucket: &str,
        key: &str,
        stored_etag: &str,
        stored_size: u64,
        meta: &FileMetadata,
    ) {
        let id = StoredObjectId {
            scope: &self.list_cache_scope,
            bucket,
            key,
            etag: stored_etag,
            size: stored_size,
        };
        if !list_size_cache::take_missing_facts(&id) {
            return;
        }
        let Some(facts_key) =
            listing_facts::facts_key(key, stored_etag, stored_size, &LogicalFacts::of(meta))
        else {
            return;
        };
        self.facts_cleanup.write(bucket, key, facts_key);
    }

    /// Delete every listing-facts object of `bucket` when nothing else is
    /// stored in it (the keys before and after the facts namespace are
    /// checked first). A bucket with objects keeps its facts, and
    /// DeleteBucket then fails as it must.
    pub(super) async fn purge_listing_facts_if_only_ones(
        &self,
        bucket: &str,
    ) -> Result<(), StorageError> {
        let first_key = |start_after: Option<&str>| {
            let request = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .max_keys(1)
                .set_start_after(start_after.map(String::from));
            async move {
                request
                    .send()
                    .await
                    .map(|r| r.contents().first().and_then(|o| o.key()).map(String::from))
                    .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))
            }
        };
        match first_key(None).await? {
            Some(k) if listing_facts::is_facts_key(&k) => {}
            _ => return Ok(()),
        }
        // Past every key under `.dg/facts/` (`0` follows `/`).
        let after_facts = format!("{}0", listing_facts::FACTS_ROOT.trim_end_matches('/'));
        if first_key(Some(&after_facts)).await?.is_some() {
            return Ok(());
        }
        let mut token: Option<String> = None;
        loop {
            let resp = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(listing_facts::FACTS_ROOT)
                .set_continuation_token(token.take())
                .send()
                .await
                .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))?;
            // One DeleteObjects per page, not a DELETE per facts object. A
            // key left behind makes DeleteBucket fail, as it must.
            let keys: Vec<String> = resp
                .contents()
                .iter()
                .filter_map(|o| o.key().map(str::to_string))
                .collect();
            super::super::facts_cleanup::delete_facts_keys(&self.client, bucket, keys).await;
            match resp.next_continuation_token() {
                Some(t) if resp.is_truncated().unwrap_or(false) => token = Some(t.to_string()),
                _ => return Ok(()),
            }
        }
    }

    /// Read the durable listing facts for the unresolved entries of one
    /// listing page and apply them. `candidates` are indices into `objects`
    /// with their stored keys. Returns the indices it resolved.
    pub(super) async fn apply_durable_listing_facts(
        &self,
        bucket: &str,
        objects: &mut [(String, FileMetadata)],
        candidates: &[(usize, String)],
    ) -> Vec<usize> {
        let Some(scan) = listing_facts::plan_facts_scan(candidates.iter().map(|(_, k)| k.as_str()))
        else {
            return Vec::new();
        };
        let mut entries: HashMap<String, Vec<listing_facts::FactsEntry>> = HashMap::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_FACTS_PAGES {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(&scan.prefix)
                .set_delimiter(scan.delimiter.map(String::from));
            request = match &token {
                Some(t) => request.continuation_token(t),
                None => request.start_after(&scan.start_after),
            };
            LISTING_FACTS_REQUESTS.with_label_values(&["list"]).inc();
            let resp = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        "listing facts of {bucket} not read: {}",
                        self.classify(bucket, &e, S3Op::ListObjects)
                    );
                    break;
                }
            };
            let mut past = false;
            for key in resp.contents().iter().filter_map(|o| o.key()) {
                if scan.is_past(key) {
                    past = true;
                    break;
                }
                if let Some(e) = listing_facts::parse_facts_key(key) {
                    entries.entry(e.stored_key.clone()).or_default().push(e);
                }
            }
            if past || !resp.is_truncated().unwrap_or(false) {
                break;
            }
            match resp.next_continuation_token() {
                Some(t) => token = Some(t.to_string()),
                None => break,
            }
        }
        let mut resolved = Vec::new();
        for (i, stored_key) in candidates {
            let meta = &mut objects[*i].1;
            let found = entries
                .get(stored_key)
                .and_then(|e| listing_facts::facts_for(e.iter(), &meta.md5, meta.stored_size()));
            let id = StoredObjectId {
                scope: &self.list_cache_scope,
                bucket,
                key: stored_key,
                etag: &meta.md5,
                size: meta.stored_size(),
            };
            match found {
                Some(facts) => {
                    list_size_cache::record(&id, facts.clone());
                    list_size_cache::apply(meta, &facts);
                    resolved.push(*i);
                }
                None => {
                    LISTING_FACTS_MISSES.inc();
                    list_size_cache::mark_missing_facts(&id);
                }
            }
        }
        resolved
    }
}

/// PUT one zero-byte facts object (native SSE headers as for any object: a
/// bucket policy may require them).
pub(in crate::storage) async fn put_facts_object(
    client: &Client,
    native: &NativeEncryptionConfig,
    bucket: &str,
    facts_key: &str,
) -> Result<(), StorageError> {
    let mut request = client
        .put_object()
        .bucket(bucket)
        .key(facts_key)
        .content_length(0)
        .body(ByteStream::from(Vec::new()));
    if let Some(marker) = native.marker() {
        request = request.metadata("dg-encrypted-native", marker);
    }
    request = apply_native_encryption(request, native);
    LISTING_FACTS_REQUESTS.with_label_values(&["put"]).inc();
    request
        .send()
        .await
        .map_err(|e| S3Backend::classify_s3_error(bucket, &e, S3Op::PutObject))?;
    Ok(())
}

/// The lazy facts backfill goes through the bounded facts drain.
#[cfg(test)]
mod backfill_tests {
    use super::*;

    /// 200 HEADs that each learn the missing facts of an object send at
    /// most `FACTS_WRITE_CONCURRENCY` (32) facts PUTs at once. Each one was
    /// its own task: on a slow backend about 200 PUTs ran at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn backfill_writes_go_through_the_bounded_drain() {
        let (s3, fake) = crate::storage::fake_s3_backend().await;
        fake.set_put_delay_ms(200);
        let stored_etag = "0123456789abcdef0123456789abcdef";
        let meta = FileMetadata::new_delta(
            "k".into(),
            "a".repeat(64),
            "fedcba9876543210fedcba9876543210".into(),
            100,
            "reference.bin".into(),
            "b".repeat(64),
            10,
            None,
        );
        const N: usize = 200;
        for i in 0..N {
            let key = format!("p/k{i:03}.delta");
            list_size_cache::mark_missing_facts(&StoredObjectId {
                scope: &s3.list_cache_scope,
                bucket: "b",
                key: &key,
                etag: stored_etag,
                size: 10,
            });
            s3.backfill_listing_facts("b", &key, stored_etag, 10, &meta);
        }
        let puts = || {
            fake.requests()
                .iter()
                .filter(|r| r.starts_with("PUT /b/.dg/facts/"))
                .count()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while puts() < N && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(puts(), N, "every missing facts object written once");
        assert!(
            fake.peak_puts_in_flight() <= 32,
            "{} facts PUTs at once",
            fake.peak_puts_in_flight()
        );
    }

    /// A facts object is written only by the bounded drain: every other
    /// path queues it (`FactsCleanupQueue::write`).
    #[test]
    fn only_the_drain_writes_facts_objects() {
        let writers: Vec<String> = crate::source_scan::prod_sources("src")
            .into_iter()
            .filter(|(_, text)| crate::source_scan::prod_text(text).contains("put_facts_object("))
            .map(|(file, _)| file)
            .collect();
        assert_eq!(
            writers,
            ["src/storage/facts_cleanup.rs", "src/storage/s3/facts.rs"],
            "the drain and the definition"
        );
        let defined =
            crate::source_scan::prod_text(&crate::source_scan::read("src/storage/s3/facts.rs"))
                .matches("put_facts_object(")
                .count();
        assert_eq!(defined, 1, "facts.rs only defines it");
    }
}

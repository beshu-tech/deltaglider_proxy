// SPDX-License-Identifier: BUSL-1.1

//! Best-effort bucket usage counter updates.
//!
//! The rule: a write records its counter change right after the storage
//! write that commits it, before the next await. A future dropped after the
//! commit (a request timeout, a client disconnect) then cannot leave a
//! stored object uncounted.

use super::metadata::StoredForms;
use super::*;

/// What the usage counter knows about the object a write replaces. Read
/// BEFORE the write ([`DeltaGliderEngine::prior_for_counter`]): S3 PUT is an
/// upsert, and an overwrite must net to +0 objects.
#[derive(Debug, Clone)]
pub enum CounterPrior {
    /// No counter, or a bucket that the counter skips (migration staging):
    /// the write records nothing, and no HEAD was sent for it.
    Untracked,
    /// The key held no object.
    Absent,
    /// The key held `object` (the newest stored form): the write
    /// subtracts it. `forms`: which stored forms (`k`, `k.delta`) the read
    /// found, so the write deletes only a form that is there.
    Present {
        object: Box<FileMetadata>,
        forms: StoredForms,
    },
    /// The read failed with an error other than NotFound (a timeout, 503):
    /// the net change of the write is unknown. The write records no object
    /// change and marks the bucket's row for a Refresh. Before, the error
    /// read as "absent", and each such overwrite added the object again.
    Unknown,
    /// A read taken BEFORE the key's object write lock, while other
    /// writers on this node (a client PUT) could still write the key
    /// ([`DeltaGliderEngine::head_with_counter_prior`]). It counts: the
    /// write nets against the inner read. It does not decide the drop: a
    /// write in the window may have stored the other form, so
    /// [`Self::may_hold`] says yes and the write deletes that form.
    ///
    /// The other variants come from a read under the object write lock
    /// (the store paths, a conditional write's precondition check), or
    /// behind the bucket's maintenance write gate (re-encrypt, migrate);
    /// the source guard `priors_outside_the_engine_are_read_ahead_or_gated`
    /// holds the list of the latter.
    ReadAhead(Box<CounterPrior>),
}

impl CounterPrior {
    /// The replaced object, for [`StoreResult::replaced`].
    pub fn replaced(&self) -> Option<FileMetadata> {
        match self {
            CounterPrior::Present { object, .. } => Some(FileMetadata::clone(object)),
            CounterPrior::ReadAhead(inner) => inner.replaced(),
            _ => None,
        }
    }

    /// `prior`, read before the object write lock: it counts, but it
    /// never skips the DELETE of the other form ([`Self::ReadAhead`]).
    pub fn read_ahead(prior: CounterPrior) -> CounterPrior {
        match prior {
            CounterPrior::Untracked => CounterPrior::Untracked,
            CounterPrior::ReadAhead(_) => prior,
            other => CounterPrior::ReadAhead(Box::new(other)),
        }
    }

    /// The read the counter nets against ([`Self::ReadAhead`] unwrapped).
    fn counted(&self) -> &CounterPrior {
        match self {
            CounterPrior::ReadAhead(inner) => inner.counted(),
            other => other,
        }
    }

    /// Whether the key may hold the stored form `form`, as the read saw
    /// it. Only a read that found the form absent says no. No read
    /// (`Untracked`), a failed one (`Unknown`) or one taken before the
    /// object write lock (`ReadAhead`) says yes.
    pub(super) fn may_hold(&self, form: crate::storage::ObjectVariant) -> bool {
        match self {
            CounterPrior::Absent => false,
            CounterPrior::Present { forms, .. } => forms.has(form),
            CounterPrior::Untracked | CounterPrior::Unknown | CounterPrior::ReadAhead(_) => true,
        }
    }
}

/// `transfer.rs` passes `counter_prior.as_ref()` to
/// [`DeltaGliderEngine::record_fast_path_copy`].
impl AsRef<CounterPrior> for CounterPrior {
    fn as_ref(&self) -> &CounterPrior {
        self
    }
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// Record a committed object write: `added` replaces what `prior` says
    /// the key held. Call it right after the storage write commits, before
    /// the next await (see the module doc).
    pub(super) fn record_commit(&self, bucket: &str, prior: &CounterPrior, added: &FileMetadata) {
        let Some(u) = &self.bucket_usage else { return };
        match prior.counted() {
            // `counted()` unwraps a ReadAhead: never one here.
            CounterPrior::Untracked | CounterPrior::ReadAhead(_) => {}
            CounterPrior::Absent => u.apply_net(bucket, None, Some(added), 0),
            CounterPrior::Present { object, .. } => {
                u.apply_net(bucket, Some(&**object), Some(added), 0)
            }
            CounterPrior::Unknown => u.mark_for_recount(bucket),
        }
    }

    /// Record a reference.bin that a write created (`+bytes`) or removed
    /// (`-bytes`), right after that storage write. The size is exact, so it
    /// is recorded also when the object's own change is unknown.
    pub(super) fn record_reference(&self, bucket: &str, bytes: i64) {
        let Some(u) = &self.bucket_usage else { return };
        u.apply_net(bucket, None, None, bytes);
    }

    /// Best-effort: fold a deleted object out of the bucket counter (-1), plus
    /// any reclaimed reference bytes (stored-only) so stored_bytes stays exact.
    pub(super) fn record_delete(
        &self,
        bucket: &str,
        meta: &FileMetadata,
        reclaimed_ref_bytes: u64,
    ) {
        let Some(u) = &self.bucket_usage else { return };
        u.apply_net(bucket, Some(meta), None, -(reclaimed_ref_bytes as i64));
    }

    /// A change whose size the counter cannot know (the deleted object's
    /// facts did not open): the bucket is recounted.
    pub(super) fn record_unknown_change(&self, bucket: &str) {
        if let Some(u) = &self.bucket_usage {
            u.mark_for_recount(bucket);
        }
    }

    /// Resolve the prior object at `bucket/key` for overwrite-net accounting.
    /// Sends no request without a counter or for a bucket that the counter
    /// skips (`is_transient_bucket`): migration staging paid 2 HEADs per
    /// copy whose result the counter threw away.
    pub(super) async fn prior_for_counter(&self, bucket: &str, key: &str) -> CounterPrior {
        if self.bucket_usage.is_none() || crate::bucket_usage::is_transient_bucket(bucket) {
            return CounterPrior::Untracked;
        }
        let Ok((obj_key, deltaspace_id)) = self.validated_key(bucket, key) else {
            return CounterPrior::Absent;
        };
        match self
            .resolve_stored_forms(bucket, &deltaspace_id, &obj_key.filename)
            .await
        {
            Ok(Some((meta, forms))) => CounterPrior::Present {
                object: Box::new(meta),
                forms,
            },
            Ok(None) => CounterPrior::Absent,
            Err(e) => {
                warn!(
                    "usage counter: the object that {bucket}/{key} replaces is unknown ({e}); \
                     the write is not counted and the bucket needs a Refresh"
                );
                CounterPrior::Unknown
            }
        }
    }

    /// Snapshot the destination's PRIOR metadata for fast-path accounting
    /// (`transfer.rs`, which ships a `.delta` verbatim via `put_delta_raw`
    /// and so bypasses the store pipeline). MUST be called BEFORE the
    /// fast-path write: read after it, the prior is the just-written delta
    /// and an overwrite nets to zero.
    pub async fn fast_path_prior(&self, bucket: &str, dest_key: &str) -> CounterPrior {
        self.prior_for_counter(bucket, dest_key).await
    }

    /// HEAD `bucket/key` for a caller that may write the key next (the
    /// replication event consumer reads the destination before it
    /// copies): the metadata (`None` = absent), and the same read as the
    /// `CounterPrior` of that write, so the copy does not read the key a
    /// second time. The read is taken before the object write lock, so the
    /// prior is a `CounterPrior::ReadAhead`: a write by someone else
    /// between this read and the copy skews the usage counter (the next
    /// usage scan corrects it), and the copy still deletes the other
    /// stored form, which that write may have stored.
    pub async fn head_with_counter_prior(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(Option<FileMetadata>, CounterPrior), EngineError> {
        let tracked =
            self.bucket_usage.is_some() && !crate::bucket_usage::is_transient_bucket(bucket);
        match self.head_with_forms(bucket, key).await {
            Ok((meta, forms)) => {
                let prior = if tracked {
                    CounterPrior::read_ahead(CounterPrior::Present {
                        object: Box::new(meta.clone()),
                        forms,
                    })
                } else {
                    CounterPrior::Untracked
                };
                Ok((Some(meta), prior))
            }
            Err(EngineError::NotFound(_)) => Ok((
                None,
                if tracked {
                    CounterPrior::read_ahead(CounterPrior::Absent)
                } else {
                    CounterPrior::Untracked
                },
            )),
            Err(e) => Err(e),
        }
    }

    /// Counter update of a reference.bin that a fast-path copy seeded. Call
    /// it right after the reference write returns, before the delta ships:
    /// a ship that fails leaves the reference in place.
    pub fn record_seeded_reference(&self, bucket: &str, bytes: u64) {
        self.record_reference(bucket, bytes as i64);
    }

    /// Counter update of a fast-path copy: overwrite-aware, plus any
    /// reference the copy seeded.
    pub fn record_fast_path_copy(
        &self,
        bucket: &str,
        prior: &CounterPrior,
        delta_meta: &FileMetadata,
        seeded_reference_bytes: u64,
    ) {
        self.record_commit(bucket, prior, delta_meta);
        self.record_reference(bucket, seeded_reference_bytes as i64);
    }
}

/// Usage-counter accounting at the commit, on a fake S3: the prior-object
/// read, a write whose future the caller drops, the metrics of a failed
/// commit, and the legacy-reference migration.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket_usage::{usage_delta_for, BucketUsage, BucketUsageRow};
    use crate::deltaglider::savings::SavingsTotals;
    use crate::metrics::Metrics;
    use crate::storage::{DynStorageBackend, FakeS3, FilesystemBackend, StorageBackendExt};
    use md5::Md5;
    use sha2::Sha256;

    /// An engine on a fake S3 with bucket `b` and a usage counter.
    async fn counted_s3_engine(
        metrics: Option<Arc<Metrics>>,
    ) -> (Arc<DynEngine>, Arc<FakeS3>, Arc<BucketUsage>) {
        let (s3, fake) = crate::storage::fake_s3_backend().await;
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), metrics)
                .with_bucket_usage(Some(usage.clone()));
        engine.create_bucket("b").await.unwrap();
        (Arc::new(engine), fake, usage)
    }

    fn row(usage: &BucketUsage) -> BucketUsageRow {
        usage.flush_pending();
        usage.read("b").unwrap().unwrap_or(BucketUsageRow {
            object_count: 0,
            logical_bytes: 0,
            stored_bytes: 0,
            last_scan_at: None,
        })
    }

    fn noise(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn heads(fake: &FakeS3) -> usize {
        fake.requests()
            .iter()
            .filter(|r| r.starts_with("HEAD "))
            .count()
    }

    /// A prior-object HEAD that fails with 503 is not "no prior object":
    /// read as absent, the overwrite counted +1 object and its full size,
    /// and never subtracted the prior. The write goes on, the counter is
    /// left alone, and the row asks for a Refresh.
    #[tokio::test]
    async fn a_failed_prior_head_skips_the_counter_and_marks_a_recount() {
        let (engine, fake, usage) = counted_s3_engine(None).await;
        engine
            .store("b", "img/k.jpg", &[1u8; 100], None, Default::default())
            .await
            .unwrap();
        let mut totals = SavingsTotals::default();
        totals.accumulate(&engine.head("b", "img/k.jpg").await.unwrap());
        usage
            .overwrite_from_scan(usage.begin_scan("b"), &totals, 1_000)
            .unwrap();

        fake.fail("HEAD", "k.jpg", 503, "SlowDown", u32::MAX);
        engine
            .store("b", "img/k.jpg", &[2u8; 300], None, Default::default())
            .await
            .expect("a failed prior read must not fail the write");
        fake.clear_faults();

        let r = row(&usage);
        assert_eq!(
            r.object_count, 1,
            "the overwrite was counted as a new object"
        );
        assert_eq!(r.last_scan_at, None, "the row is not marked for a recount");
    }

    /// Migration staging buckets are never counted, so their writes send no
    /// prior-object HEAD (2 per write before).
    #[tokio::test]
    async fn transient_buckets_send_no_prior_head() {
        let (engine, fake, _usage) = counted_s3_engine(None).await;
        fake.clear();
        let _ = engine
            .prior_for_counter("__dgmigrate_t__src", "p/k.zip")
            .await;
        assert_eq!(heads(&fake), 0, "a transient bucket paid a prior HEAD");
        let _ = engine.prior_for_counter("b", "p/k.zip").await;
        assert!(heads(&fake) > 0, "a counted bucket reads its prior");
    }

    /// The counter is recorded where the object commits. Before, it was
    /// recorded after the cleanup DELETE that follows the commit: a request
    /// timeout or a client disconnect there dropped the future and the
    /// stored object was never counted. The write is an overwrite (of a
    /// shorter version) in the OTHER stored form: a write of a new key, or
    /// an overwrite in the same form, sends no cleanup DELETE.
    #[tokio::test]
    async fn a_put_dropped_after_its_commit_is_still_counted() {
        let no_delta = || HashMap::from([("dg-no-delta".to_string(), "true".to_string())]);
        // (key, the first write is passthrough on request; else the second)
        for (key, first_passthrough) in [("rel/app.zip", false), ("rel/other.zip", true)] {
            let (engine, fake, usage) = counted_s3_engine(None).await;
            let data = noise(7, 64 * 1024);
            let first = &data[..data.len() - 4096];
            let (first_meta, second_meta) = if first_passthrough {
                (no_delta(), HashMap::new())
            } else {
                (HashMap::new(), no_delta())
            };
            engine
                .store("b", key, first, None, first_meta)
                .await
                .unwrap();
            fake.set_delete_delay_ms(30_000);
            let task = {
                let (engine, data) = (engine.clone(), data.clone());
                tokio::spawn(async move { engine.store("b", key, &data, None, second_meta).await })
            };
            assert!(
                fake.wait_for(|r| r.starts_with("DELETE ")).await,
                "{key}: no cleanup DELETE"
            );
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled(), "{key}");
            fake.set_delete_delay_ms(0);

            let head = engine.head("b", key).await.expect("the object committed");
            // The delta write's body is the folder's baseline.
            let reference = if first_passthrough {
                data.len() as i64
            } else {
                first.len() as i64
            };
            let r = row(&usage);
            assert_eq!(
                r.object_count, 1,
                "{key}: the committed object is not counted"
            );
            assert_eq!(r.logical_bytes, data.len() as u64, "{key}");
            assert_eq!(
                r.stored_bytes as i64,
                usage_delta_for(&head, 1).2 + reference,
                "{key}"
            );
        }
    }

    /// `deltaglider_delta_bytes_saved_total` and the decision counters move
    /// only for a commit that happened. Before, a failed delta PUT counted
    /// its savings, a `delta` decision and a `reference` for the baseline
    /// that the failure then removed.
    #[tokio::test]
    async fn a_failed_delta_commit_counts_no_savings_and_no_decision() {
        let metrics = Arc::new(Metrics::new());
        let (engine, fake, usage) = counted_s3_engine(Some(metrics.clone())).await;
        // Every attempt: the S3 backend retries a 503 PUT itself.
        fake.fail("PUT", ".delta", 503, "SlowDown", u32::MAX);
        let data = noise(9, 64 * 1024);
        assert!(engine
            .store("b", "rel/app.zip", &data, None, Default::default())
            .await
            .is_err());
        fake.clear_faults();
        assert_eq!(metrics.delta_bytes_saved_total.get(), 0, "bytes saved");
        for d in ["delta", "reference", "passthrough"] {
            assert_eq!(
                metrics.delta_decisions_total.with_label_values(&[d]).get(),
                0,
                "decision {d}"
            );
        }
        let r = row(&usage);
        assert_eq!(
            (r.object_count, r.stored_bytes),
            (0, 0),
            "the removed baseline is still counted"
        );
        // The next PUT commits: its metrics count once.
        engine
            .store("b", "rel/app.zip", &data, None, Default::default())
            .await
            .unwrap();
        assert_eq!(
            metrics
                .delta_decisions_total
                .with_label_values(&["delta"])
                .get(),
            1
        );
        assert_eq!(
            metrics
                .delta_decisions_total
                .with_label_values(&["reference"])
                .get(),
            1
        );
        assert!(metrics.delta_bytes_saved_total.get() > 0);
    }

    /// The legacy-reference migration writes a user-visible delta: it is
    /// counted like any other new object (before: not at all, and its later
    /// delete drove the row negative).
    #[tokio::test]
    async fn legacy_reference_migration_counts_the_delta_it_creates() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .unwrap(),
        );
        backend.create_bucket("b").await.unwrap();
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let engine = DeltaGliderEngine::new_with_backend(backend.clone(), &Config::default(), None)
            .with_bucket_usage(Some(usage.clone()));
        let v1 = noise(4, 200_000);
        let legacy = FileMetadata::new_reference(
            "a.zip".into(),
            "a.zip".into(),
            hex::encode(Sha256::digest(&v1)),
            hex::encode(Md5::digest(&v1)),
            v1.len() as u64,
            None,
        );
        backend
            .put_reference(
                "b",
                "rel",
                bytes::Bytes::copy_from_slice(&v1),
                &legacy,
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(
            engine.migrate_legacy_references("b").await.unwrap(),
            (1, 0, 0)
        );
        let head = engine.head("b", "rel/a.zip").await.unwrap();
        let r = row(&usage);
        assert_eq!(
            (r.object_count, r.logical_bytes, r.stored_bytes as i64),
            (1, v1.len() as u64, usage_delta_for(&head, 1).2),
        );
    }

    /// The storage writes in `store.rs` that commit an object or change a
    /// reference.bin.
    const COMMITS: &[&str] = &[
        ".put_passthrough(",
        ".put_passthrough_file(",
        ".put_passthrough_chunked(",
        ".put_passthrough_parts(",
        ".put_delta(",
        ".put_directory_marker(",
        ".complete_multipart_upload(",
        ".put_reference(",
        ".put_reference_from_file(",
    ];

    /// The commits in `src` whose counter change is not recorded between
    /// the commit's own `.await` and the next `.await` (a commit in another
    /// match arm does not count as "next"). A commit that is the condition
    /// of `if let Err(..) = ...` is checked after its error block. A
    /// `// counter: unchanged` comment just above a commit exempts it.
    fn unrecorded_commits(src: &str) -> Vec<usize> {
        let awaits: Vec<usize> = src.match_indices(".await").map(|(i, _)| i).collect();
        let commits: Vec<usize> = COMMITS
            .iter()
            .flat_map(|c| src.match_indices(c).map(|(i, _)| i))
            .collect();
        // The await that ends the call starting at `c`.
        let own_await = |c: usize| *awaits.iter().find(|&&a| a > c).unwrap();
        let closes_a_commit = |a: usize| commits.iter().any(|&c| own_await(c) == a);
        let mut missing = Vec::new();
        for &c in &commits {
            let line_start = src[..c].rfind('\n').unwrap_or(0);
            let above = &src[src[..line_start].rfind("\n\n").unwrap_or(0)..line_start];
            if above.contains("// counter: unchanged") {
                continue;
            }
            let mut from = own_await(c) + ".await".len();
            let stmt = src[..c].rfind([';', '{', '}']).unwrap_or(0);
            if src[stmt..c].contains("if let Err(") {
                // Skip the error block: `{ ...; return ... }`.
                let open = from + src[from..].find('{').unwrap();
                let mut depth = 0;
                for (i, ch) in src[open..].char_indices() {
                    match ch {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    if depth == 0 {
                        from = open + i;
                        break;
                    }
                }
            }
            let next = awaits
                .iter()
                .copied()
                .find(|&a| a > from && !closes_a_commit(a))
                .unwrap_or(src.len());
            let window = &src[from..next];
            if !window.contains("record_commit(") && !window.contains("record_reference(") {
                missing.push(c);
            }
        }
        missing
    }

    /// Fix the class: every storage write in `store.rs` that commits an
    /// object or a reference.bin records the counter before the next await,
    /// so no dropped future can skip it.
    #[test]
    fn store_commits_record_the_counter_before_the_next_await() {
        let src = include_str!("store.rs");
        let production = &src[..src.find("#[cfg(test)]").unwrap()];
        let missing: Vec<String> = unrecorded_commits(production)
            .into_iter()
            .map(|i| {
                let line = production[..i].lines().count();
                format!(
                    "store.rs:{line}: {}",
                    production[i..].lines().next().unwrap()
                )
            })
            .collect();
        assert!(
            missing.is_empty(),
            "commits without a counter record before the next await: {missing:#?}"
        );
        // The guard flags the shape the old code had: the record after the
        // cleanup DELETE that follows the commit.
        let old = "
            self.storage.put_passthrough(b, d, f, data, &m).await?;
            self.delete_delta_idempotent(b, d, f).await;
            self.record_store(bucket, &result);";
        assert_eq!(unrecorded_commits(old).len(), 1);
        let new = "
            self.storage.put_passthrough(b, d, f, data, &m).await?;
            self.record_commit(bucket, prior, &m);
            self.delete_delta_idempotent(b, d, f).await;";
        assert!(unrecorded_commits(new).is_empty());
    }

    /// A prior that code outside the engine builds by hand decides which
    /// stored form the write deletes. Outside the engine that is right
    /// only behind the bucket's maintenance write gate, where no writer on
    /// this node writes the key between the read and the write. Every
    /// other read outside the object write lock is a `ReadAhead`
    /// (`head_with_counter_prior`): the replication event consumer read
    /// the destination before the lock, a client PUT in that window stored
    /// the other form, and the copy left both forms.
    #[test]
    fn priors_outside_the_engine_are_read_ahead_or_gated() {
        // Each file builds the prior of a copy into a bucket that its
        // maintenance job holds behind the write gate.
        const GATED: &[&str] = &["src/maintenance/worker.rs", "src/maintenance/migrate.rs"];
        let mut sites = Vec::new();
        for (file, text) in crate::source_scan::prod_sources("src") {
            if file.starts_with("src/deltaglider/engine/") {
                continue;
            }
            for (n, line) in crate::source_scan::prod_lines(&text) {
                if (line.contains("CounterPrior::Present") || line.contains("CounterPrior::Absent"))
                    && !line.trim_start().starts_with("//")
                {
                    sites.push((file.clone(), n));
                }
            }
        }
        let ungated: Vec<_> = sites
            .iter()
            .filter(|(f, _)| !GATED.contains(&f.as_str()))
            .collect();
        assert!(
            ungated.is_empty(),
            "a prior built outside the engine and outside a write gate: {ungated:?} \
             (use head_with_counter_prior, which returns a ReadAhead)"
        );
        assert!(!sites.is_empty(), "the scan found the gated sites");
    }
}

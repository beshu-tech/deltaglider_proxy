// SPDX-License-Identifier: BUSL-1.1

//! Conditional client writes: the per-object write lock and the S3 write
//! preconditions (`If-Match` / `If-None-Match`).
//!
//! Every write of one object takes the same per-object lock: the client
//! writes (PutObject, CompleteMultipartUpload, CopyObject, form POST), the
//! admin bulk copy, every engine-routed copy (replication, lifecycle,
//! maintenance), the delete, and the legacy-reference migration. So a
//! conditional check and the store that follows it are one step for that
//! key on this process, and two writes of one key never interleave their
//! stored forms. Other instances are not covered.
//!
//! The store methods TAKE the guard ([`ObjectWriteGuard`]) as a parameter:
//! only [`DeltaGliderEngine::lock_object_write`] makes one, so no write path
//! can skip the lock. [`DeltaGliderEngine::store`] (and the delete) take the
//! lock themselves; a caller that holds the guard must not call them for
//! the same key (the lock is not re-entrant; source guard
//! `a_lock_holder_never_calls_a_self_locking_writer`).
//!
//! Lock order, outermost first: the object write lock, then the
//! per-deltaspace prefix lock, then the cross-instance reference lock. A
//! holder of the prefix lock never takes an object write lock.

use super::*;
use crate::storage::StorageBackend;
use s3s::dto::ETagCondition;

/// The S3 write preconditions of one request. Empty = an unconditional write.
#[derive(Debug, Clone, Default)]
pub struct Precondition {
    pub if_match: Option<ETagCondition>,
    pub if_none_match: Option<ETagCondition>,
}

impl Precondition {
    pub fn none() -> Self {
        Self::default()
    }

    fn is_empty(&self) -> bool {
        self.if_match.is_none() && self.if_none_match.is_none()
    }

    /// Pure: the PutObject conditional rules of S3 (s3surface-13), judged
    /// against the current object (`None` = absent). `If-Match` on a missing
    /// key is `NoSuchKey`; it compares by strong ETag, so a weak `W/"…"`
    /// never matches. `If-None-Match` supports only `*`: another value is
    /// `NotImplemented`, as on S3, not a silent ETag compare.
    pub fn check(&self, existing: Option<&FileMetadata>) -> Result<(), ConditionalError> {
        if self.if_none_match.as_ref().is_some_and(|c| !c.is_any()) {
            return Err(ConditionalError::NotImplemented(
                "If-None-Match on PutObject supports only '*'",
            ));
        }
        if let Some(cond) = &self.if_match {
            let Some(meta) = existing else {
                return Err(ConditionalError::NoSuchKey);
            };
            let current = s3s::dto::ETag::parse_http_header(meta.etag().as_bytes())
                .map_err(|_| ConditionalError::PreconditionFailed)?;
            let matches = cond.is_any()
                || cond
                    .as_etag()
                    .is_some_and(|wanted| wanted.strong_cmp(&current));
            if !matches {
                return Err(ConditionalError::PreconditionFailed);
            }
        }
        if self.if_none_match.is_some() && existing.is_some() {
            return Err(ConditionalError::PreconditionFailed);
        }
        Ok(())
    }
}

/// Why a conditional write did not store.
#[derive(Debug)]
pub enum ConditionalError {
    /// A precondition does not hold (412).
    PreconditionFailed,
    /// `If-Match` on a key that does not exist (404 NoSuchKey, as on S3).
    NoSuchKey,
    /// A precondition shape S3 does not support (501).
    NotImplemented(&'static str),
    /// The engine failed (the existence check or the store).
    Engine(EngineError),
}

impl From<EngineError> for ConditionalError {
    fn from(e: EngineError) -> Self {
        ConditionalError::Engine(e)
    }
}

impl From<ConditionalError> for crate::api::S3Error {
    fn from(e: ConditionalError) -> Self {
        match e {
            ConditionalError::PreconditionFailed => crate::api::S3Error::PreconditionFailed,
            ConditionalError::NoSuchKey => crate::api::S3Error::NoSuchKey(String::new()),
            ConditionalError::NotImplemented(msg) => {
                crate::api::S3Error::NotImplemented(msg.to_string())
            }
            ConditionalError::Engine(e) => e.into(),
        }
    }
}

/// Held for one write of one object; see [`DeltaGliderEngine::lock_object_write`].
/// The proof that a store method asks for: its fields are private, so only
/// the lock makes one.
pub struct ObjectWriteGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    /// The lock entry, for the write counter that `Drop` bumps.
    lock: Arc<ObjectWriteLock>,
    /// The object this guard locks, as the engine parses its key.
    object: ObjectKey,
    /// The object as the precondition check read it under this lock
    /// (`Some(None)`: absent). `None`: no precondition, nothing read.
    checked: Option<Option<(FileMetadata, super::metadata::StoredForms)>>,
}

impl ObjectWriteGuard {
    /// `Ok` when this guard locks the object `deltaspace_id`/`filename` of
    /// `bucket`. A store checks it before it writes, so a guard of one key
    /// cannot stand in for another.
    pub(super) fn covers(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<(), EngineError> {
        if self.object.bucket == bucket
            && self.object.prefix == deltaspace_id
            && self.object.filename == filename
        {
            return Ok(());
        }
        Err(EngineError::Storage(StorageError::Other(format!(
            "the write of {bucket}/{deltaspace_id}/{filename} holds the write lock of {}/{}",
            self.object.bucket,
            self.object.full_key()
        ))))
    }

    /// What the key holds, for the store's usage counter, when the
    /// precondition check read it: no PUT of this process can change it
    /// while the lock is held, so the store needs no read of its own.
    fn known_prior(&self) -> Option<super::usage::CounterPrior> {
        use super::usage::CounterPrior;
        self.checked.as_ref().map(|found| match found {
            Some((meta, forms)) => CounterPrior::Present {
                object: Box::new(meta.clone()),
                forms: *forms,
            },
            None => CounterPrior::Absent,
        })
    }
}

impl Drop for ObjectWriteGuard {
    /// A write that ends is a write event too: a read that overlapped it
    /// sees the counter move (see `WriteActivity`).
    fn drop(&mut self) {
        self.lock
            .events
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// One key's write lock and its write counter (bumped when a write takes
/// the lock and when it lets go).
struct ObjectWriteLock {
    mutex: Arc<tokio::sync::Mutex<()>>,
    events: std::sync::atomic::AtomicU64,
}

type ObjectWriteLocks = DashMap<String, Arc<ObjectWriteLock>>;

/// What a reader saw of a key's writes: the lock entry, its counter, and
/// whether a write held the lock. Reads take no lock. A write that
/// switches the stored form (writes the new form, then deletes the old)
/// between a read's two requests makes the object look absent, so a read
/// that finds nothing compares two snapshots and reads again when a write
/// ran in between ([`DeltaGliderEngine::retry_missing_during_writes`]).
pub(super) struct WriteActivity {
    lock: Option<Arc<ObjectWriteLock>>,
    events: u64,
    held: bool,
}

impl WriteActivity {
    /// Pure: a write ran between `before` and `after` (or held the lock at
    /// either end).
    fn overlapped(before: &WriteActivity, after: &WriteActivity) -> bool {
        let same_entry = match (&before.lock, &after.lock) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        before.held || after.held || !same_entry || before.events != after.events
    }
}

/// THE process-wide object-write-lock map (like `shared_prefix_locks`): a
/// rebuilt engine and the one it replaces must exclude each other.
fn shared_object_write_locks() -> &'static ObjectWriteLocks {
    static SHARED: std::sync::OnceLock<ObjectWriteLocks> = std::sync::OnceLock::new();
    SHARED.get_or_init(DashMap::new)
}

/// Pure: the write-lock key of an object. The engine trims leading `/`
/// (`//a` and `a` are one object), so the lock must too, or two
/// create-only PUTs through the two spellings both pass the check.
/// `storage`: the storage identity of the bucket, so two alias names of one
/// real bucket share the lock of a key (as they share its deltaspace lock).
fn object_write_lock_key(storage: &str, key: &ObjectKey) -> String {
    format!("{storage}\0{}", key.full_key())
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// The per-object write lock (review C4). Always taken BEFORE the
    /// per-deltaspace prefix lock (the store takes that one). Idle entries
    /// are pruned like the prefix locks. Not re-entrant: a task that holds
    /// the guard of a key and asks for it again waits forever.
    pub async fn lock_object_write(&self, bucket: &str, key: &str) -> ObjectWriteGuard {
        const CLEANUP_THRESHOLD: usize = 1024;
        let locks = shared_object_write_locks();
        if locks.len() > CLEANUP_THRESHOLD {
            locks.retain(|_, m| Arc::strong_count(m) > 1);
        }
        let object = ObjectKey::parse(bucket, key);
        let mutex = locks
            .entry(object_write_lock_key(
                &self.storage.storage_identity(bucket),
                &object,
            ))
            .or_insert_with(|| {
                Arc::new(ObjectWriteLock {
                    mutex: Arc::new(tokio::sync::Mutex::new(())),
                    events: std::sync::atomic::AtomicU64::new(0),
                })
            })
            .clone();
        let guard = mutex.mutex.clone().lock_owned().await;
        mutex
            .events
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ObjectWriteGuard {
            _guard: guard,
            lock: mutex,
            object,
            checked: None,
        }
    }

    /// A snapshot of the key's writes on this node, for
    /// [`Self::retry_missing_during_writes`]. Creates no lock entry.
    pub(super) fn write_activity(&self, bucket: &str, key: &str) -> WriteActivity {
        let object = ObjectKey::parse(bucket, key);
        let lock = shared_object_write_locks()
            .get(&object_write_lock_key(
                &self.storage.storage_identity(bucket),
                &object,
            ))
            .map(|e| e.value().clone());
        match lock {
            Some(l) => {
                let held = l.mutex.try_lock().is_err();
                WriteActivity {
                    events: l.events.load(std::sync::atomic::Ordering::SeqCst),
                    held,
                    lock: Some(l),
                }
            }
            None => WriteActivity {
                lock: None,
                events: 0,
                held: false,
            },
        }
    }

    /// Run the read `op`, and run it again (at most twice more) when it
    /// finds no object while a write of the key ran on this node: that
    /// write can switch the stored form between the read's requests. A
    /// read of a missing key with no write in flight sends nothing more.
    /// A caller that holds the key's write lock gets the retries too (the
    /// lock reads as held); it can only cost requests, never a deadlock,
    /// because a read takes no lock.
    pub(super) async fn retry_missing_during_writes<T, F, Fut>(
        &self,
        bucket: &str,
        key: &str,
        mut op: F,
    ) -> Result<T, EngineError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, EngineError>>,
    {
        const RETRIES: usize = 2;
        let mut attempt = 0;
        loop {
            let before = self.write_activity(bucket, key);
            match op().await {
                Err(e)
                    if e.is_not_found()
                        && attempt < RETRIES
                        && WriteActivity::overlapped(
                            &before,
                            &self.write_activity(bucket, key),
                        ) =>
                {
                    attempt += 1;
                    tracing::debug!(
                        "{bucket}/{key}: not found during a write of the key, reading again"
                    );
                }
                r => return r,
            }
        }
    }

    /// Take the object's write lock and judge `precondition` under it. The
    /// caller stores while it holds the returned guard.
    pub async fn lock_and_check(
        &self,
        bucket: &str,
        key: &str,
        precondition: &Precondition,
    ) -> Result<ObjectWriteGuard, ConditionalError> {
        let mut guard = self.lock_object_write(bucket, key).await;
        if !precondition.is_empty() {
            // Fail closed (review C4): only NotFound means "absent". Any
            // other HEAD error used to read as absent, so `If-None-Match: *`
            // overwrote.
            let existing = match self.head_with_forms(bucket, key).await {
                Ok(found) => Some(found),
                Err(EngineError::NotFound(_)) => None,
                Err(e) => return Err(e.into()),
            };
            precondition.check(existing.as_ref().map(|(meta, _)| meta))?;
            guard.checked = Some(existing);
        }
        Ok(guard)
    }

    /// A client write of a whole body: lock, judge the precondition, store.
    /// `data` is the body as the S3 adapter collected it: the store shares
    /// the buffer down to the storage write, it never copies it.
    /// `known_sha256`: the body's SHA-256 when the caller proved it (a
    /// verified SigV4 payload hash); `None` hashes the body here.
    #[allow(clippy::too_many_arguments)]
    pub async fn store_conditional(
        &self,
        bucket: &str,
        key: &str,
        data: Bytes,
        known_sha256: Option<super::store::KnownSha256>,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        precondition: &Precondition,
    ) -> Result<StoreResult, ConditionalError> {
        let guard = self.lock_and_check(bucket, key, precondition).await?;
        Ok(self
            .store_client_body(
                &guard,
                bucket,
                key,
                data,
                known_sha256,
                content_type,
                user_metadata,
                guard.known_prior(),
            )
            .await?)
    }

    /// Store a client body that is already in memory. A large body that tries
    /// a delta goes through the streaming spool store, so the delta encode and
    /// the ratio decision run with bounded memory; the others are stored
    /// buffered. `known_prior`: what the precondition check read, if it ran.
    #[allow(clippy::too_many_arguments)]
    async fn store_client_body(
        &self,
        lock: &ObjectWriteGuard,
        bucket: &str,
        key: &str,
        data: Bytes,
        known_sha256: Option<super::store::KnownSha256>,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        known_prior: Option<super::usage::CounterPrior>,
    ) -> Result<StoreResult, EngineError> {
        let size = data.len() as u64;
        if size > self.spool_threshold()
            && self.write_tries_delta(bucket, key, &user_metadata, size)
        {
            let spool = self.spool_acquire(size).await?;
            // Hash from RAM while the spool is written: the spooled store
            // then does not read the body back to hash it.
            let hashing = super::store::BodyHashes::compute(&data, known_sha256);
            let (written, hashes) = tokio::join!(tokio::fs::write(spool.path(), &data), hashing);
            written.map_err(|e| EngineError::Storage(StorageError::from(e)))?;
            let hashes = hashes?;
            self.store_spooled_delta_inner(
                lock,
                bucket,
                key,
                &spool,
                size,
                content_type,
                user_metadata,
                None,
                known_prior,
                Some(hashes),
            )
            .await
        } else {
            self.store_inner(
                lock,
                bucket,
                key,
                &data,
                content_type,
                user_metadata,
                None,
                known_prior,
                known_sha256,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_write_lock_key_matches_the_engine_key() {
        let k = |b: &str, key: &str| object_write_lock_key(b, &ObjectKey::parse(b, key));
        assert_eq!(k("b", "//a"), k("b", "a"));
        assert_eq!(k("b", "/x/y"), k("b", "x/y"));
        assert_ne!(k("b", "x/y"), k("b", "xy"));
        assert_ne!(k("b", "a/x"), k("c", "a/x"));
        // The bucket part cannot run into the key: `a` + `b/c` is not
        // `a/b` + `c`.
        assert_ne!(k("a", "b/c"), k("a/b", "c"));
    }

    /// A guard proves the write of its own key only.
    #[test]
    fn a_write_overlaps_a_read_when_anything_moved() {
        use std::sync::atomic::AtomicU64;
        let entry = || {
            Arc::new(ObjectWriteLock {
                mutex: Arc::new(tokio::sync::Mutex::new(())),
                events: AtomicU64::new(0),
            })
        };
        let (a, b) = (entry(), entry());
        let snap = |lock: Option<&Arc<ObjectWriteLock>>, events, held| WriteActivity {
            lock: lock.cloned(),
            events,
            held,
        };
        let quiet = |x: WriteActivity, y: WriteActivity| !WriteActivity::overlapped(&x, &y);
        assert!(
            quiet(snap(None, 0, false), snap(None, 0, false)),
            "no writes"
        );
        assert!(quiet(snap(Some(&a), 4, false), snap(Some(&a), 4, false)));
        // A write began, ended, held the lock at either end, or its entry
        // appeared, went, or was replaced.
        assert!(!quiet(snap(Some(&a), 4, false), snap(Some(&a), 5, false)));
        assert!(!quiet(snap(Some(&a), 4, true), snap(Some(&a), 4, false)));
        assert!(!quiet(snap(Some(&a), 4, false), snap(Some(&a), 4, true)));
        assert!(!quiet(snap(None, 0, false), snap(Some(&a), 1, false)));
        assert!(!quiet(snap(Some(&a), 2, false), snap(None, 0, false)));
        assert!(!quiet(snap(Some(&a), 2, false), snap(Some(&b), 2, false)));
    }

    #[tokio::test]
    async fn a_guard_covers_its_own_key_only() {
        let (engine, _fake, _usage) = counted_s3_engine().await;
        let guard = engine.lock_object_write("b", "//d/a.txt").await;
        assert!(guard.covers("b", "d", "a.txt").is_ok());
        assert!(guard.covers("b", "d", "b.txt").is_err());
        assert!(guard.covers("b", "", "a.txt").is_err());
        assert!(guard.covers("c", "d", "a.txt").is_err());
    }

    #[test]
    fn put_conditionals_truth_table() {
        let meta = FileMetadata::new_passthrough(
            "k".into(),
            "sha".into(),
            "0123456789abcdef0123456789abcdef".into(),
            1,
            None,
        );
        let etag = meta.etag();
        let cond = |v: &str| ETagCondition::parse_http_header(v.as_bytes()).unwrap();
        let weak = format!("W/{etag}");
        // (object exists, If-Match, If-None-Match, error code)
        type Case<'a> = (bool, Option<&'a str>, Option<&'a str>, Option<&'a str>);
        let cases: &[Case] = &[
            (true, None, None, None),
            (false, None, None, None),
            (true, Some(&etag), None, None),
            (true, Some("*"), None, None),
            (true, Some("\"other\""), None, Some("PreconditionFailed")),
            (true, Some(&weak), None, Some("PreconditionFailed")),
            (false, Some(&etag), None, Some("NoSuchKey")),
            (false, Some("*"), None, Some("NoSuchKey")),
            (false, None, Some("*"), None),
            (true, None, Some("*"), Some("PreconditionFailed")),
            (true, None, Some("\"other\""), Some("NotImplemented")),
            (false, None, Some(&etag), Some("NotImplemented")),
        ];
        for &(exists, im, inm, want) in cases {
            let pre = Precondition {
                if_match: im.map(cond),
                if_none_match: inm.map(cond),
            };
            let got = pre
                .check(exists.then_some(&meta))
                .err()
                .map(|e| crate::api::S3Error::from(e).code());
            assert_eq!(
                got, want,
                "exists {exists}, If-Match {im:?}, If-None-Match {inm:?}"
            );
        }
    }

    /// An engine on a fake S3 with bucket `b` and a usage counter (each
    /// write reads the object it replaces).
    async fn counted_s3_engine() -> (
        DynEngine,
        Arc<crate::storage::FakeS3>,
        Arc<crate::bucket_usage::BucketUsage>,
    ) {
        use crate::storage::DynStorageBackend;
        let (s3, fake) = crate::storage::fake_s3_backend().await;
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
        let usage = Arc::new(crate::bucket_usage::BucketUsage::in_memory().unwrap());
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
                .with_bucket_usage(Some(usage.clone()));
        engine.create_bucket("b").await.unwrap();
        (engine, fake, usage)
    }

    /// Object HEADs (not HeadBucket, `HEAD /b/`).
    fn object_heads(fake: &crate::storage::FakeS3) -> usize {
        fake.requests()
            .iter()
            .filter(|r| {
                r.starts_with("HEAD /b/") && !r.starts_with("HEAD /b/?") && *r != "HEAD /b/"
            })
            .count()
    }

    /// A conditional PUT reads the object once, under its write lock: the
    /// precondition's HEAD also tells the usage counter what the write
    /// replaces. It read the object twice (2 HEADs each read: the delta
    /// and the passthrough form).
    #[tokio::test]
    async fn a_conditional_put_reads_the_object_once() {
        let (engine, fake, usage) = counted_s3_engine().await;
        let cond = |v: &str| ETagCondition::parse_http_header(v.as_bytes()).unwrap();
        let create_only = Precondition {
            if_match: None,
            if_none_match: Some(cond("*")),
        };
        fake.clear();
        let stored = engine
            .store_conditional(
                "b",
                "img/a.jpg",
                Bytes::from_static(b"one"),
                None,
                None,
                HashMap::new(),
                &create_only,
            )
            .await
            .unwrap();
        let heads = object_heads(&fake);
        eprintln!("If-None-Match: * PUT of a new key: {heads} HEADs");
        assert_eq!(heads, 2, "{:?}", fake.requests());

        // An If-Match overwrite: the HEAD found the object it replaces.
        let replace = Precondition {
            if_match: Some(cond(&stored.metadata.etag())),
            if_none_match: None,
        };
        fake.clear();
        engine
            .store_conditional(
                "b",
                "img/a.jpg",
                Bytes::from_static(b"two!"),
                None,
                None,
                HashMap::new(),
                &replace,
            )
            .await
            .unwrap();
        assert_eq!(object_heads(&fake), 2, "{:?}", fake.requests());
        usage.flush_pending();
        let row = usage.read("b").unwrap().unwrap();
        assert_eq!(
            (row.object_count, row.logical_bytes),
            (1, 4),
            "the overwrite nets the object it replaced"
        );
    }

    /// The production functions of every source under `dir`: `(file, first
    /// line, signature, body)`. A function runs from its `fn` line to the
    /// next `fn` line; the signature ends at the first line that ends in
    /// `{`. Rough, but rustfmt keeps it exact enough for these guards.
    fn prod_fns(dir: &str) -> Vec<(String, usize, String, String)> {
        let starts_fn = |l: &str| {
            let t = l.trim_start();
            let t = t
                .strip_prefix("pub(crate) ")
                .or_else(|| t.strip_prefix("pub(super) "))
                .or_else(|| t.strip_prefix("pub "))
                .unwrap_or(t);
            let t = t.strip_prefix("async ").unwrap_or(t);
            t.starts_with("fn ")
        };
        let mut out = Vec::new();
        for (file, text) in crate::source_scan::prod_sources(dir) {
            let lines = crate::source_scan::prod_lines(&text);
            let heads: Vec<usize> = (0..lines.len())
                .filter(|&i| starts_fn(lines[i].1))
                .collect();
            for (n, &h) in heads.iter().enumerate() {
                let end = heads.get(n + 1).copied().unwrap_or(lines.len());
                let chunk: Vec<&str> = lines[h..end].iter().map(|(_, l)| *l).collect();
                let sig_end = chunk
                    .iter()
                    .position(|l| l.trim_end().ends_with('{') || l.trim_end().ends_with(';'))
                    .unwrap_or(0);
                out.push((
                    file.clone(),
                    lines[h].0,
                    chunk[..=sig_end].join("\n"),
                    chunk.join("\n"),
                ));
            }
        }
        out
    }

    /// Every engine function that writes a stored form of an object, or
    /// drops the other form, has the object's write lock in its signature
    /// (the guard, or the `PutObject` / `PassthroughWrite` that carries
    /// it). A new write path that skips the lock fails here: without it, a
    /// passthrough write and a delta write of one key can each drop the
    /// form the other wrote, and the object is gone.
    #[test]
    fn every_object_form_write_holds_the_object_write_lock() {
        const FORM_WRITES: &[&str] = &[
            ".put_passthrough(",
            ".put_passthrough_file(",
            ".put_passthrough_chunked(",
            ".put_passthrough_parts(",
            ".put_delta(",
            ".copy_within(",
            ".create_multipart_upload(",
            ".complete_multipart_upload(",
            ".drop_other_form(",
            ".delete_passthrough_idempotent(",
        ];
        // The reference-write module forwards a delta write that its caller
        // made under both locks (`RefWriteProof`); the helper that deletes
        // a form is the delete's and the drop's.
        const FORWARDERS: &[&str] = &[
            "async fn put_delta<",
            "async fn delete_passthrough_idempotent(",
        ];
        let proofs = ["ObjectWriteGuard", "PutObject", "PassthroughWrite"];
        let mut offenders = Vec::new();
        for (file, line, sig, body) in prod_fns("src/deltaglider/engine") {
            if FORWARDERS.iter().any(|f| sig.contains(f)) {
                continue;
            }
            let writes = FORM_WRITES.iter().any(|w| body.contains(w));
            if writes
                && !proofs.iter().any(|p| sig.contains(p))
                && !body.contains("lock_object_write(")
            {
                offenders.push(format!("{file}:{line}"));
            }
        }
        assert!(
            offenders.is_empty(),
            "an engine write of an object form without the object's write lock: {offenders:?}"
        );
    }

    /// The object write lock is not re-entrant. A function that takes it
    /// must not call a writer that takes it again (the store that locks
    /// itself, a delete, an engine-routed copy): for the same key that
    /// waits forever. Such a function passes its guard to a method that
    /// asks for one.
    #[test]
    fn a_lock_holder_never_calls_a_self_locking_writer() {
        const TAKES: &[&str] = &[
            "lock_object_write(",
            "lock_and_check(",
            "store_client_multipart_admit(",
        ];
        const SELF_LOCKING: &[&str] = &[
            ".store(",
            ".store_conditional(",
            ".delete(",
            ".delete_if(",
            ".delete_batch(",
            "copy_object_known(",
            "copy_object_from(",
            "copy_object_with_retries(",
            ".migrate_legacy_references(",
        ];
        let mut offenders = Vec::new();
        let mut holders = 0;
        for (file, line, _sig, body) in prod_fns("src") {
            let takes = TAKES.iter().filter(|t| body.contains(*t)).count();
            if takes == 0 {
                continue;
            }
            holders += 1;
            for call in SELF_LOCKING {
                if body.contains(call) {
                    offenders.push(format!("{file}:{line} calls {call}"));
                }
            }
        }
        assert!(holders >= 8, "the scan found the lock holders ({holders})");
        assert!(
            offenders.is_empty(),
            "a holder of the object write lock calls a writer that takes it again: {offenders:?}"
        );
    }

    /// The lock order is object, then deltaspace. A writer that waits for
    /// an object's lock holds no deltaspace lock while it waits, so the
    /// other keys of the folder (writes, deletes, the reference reclaim)
    /// go on. A path that took the deltaspace lock first and then waited
    /// for the object lock would block them all, and with a second writer
    /// of the other order, deadlock: the timeouts fail this test then.
    #[tokio::test]
    async fn a_writer_waiting_for_an_object_lock_holds_no_deltaspace_lock() {
        use std::time::Duration;
        let (engine, _fake, _usage) = counted_s3_engine().await;
        let engine = Arc::new(engine);
        engine
            .store("b", "d/other.jpg", b"x", None, HashMap::new())
            .await
            .unwrap();
        let held = engine.lock_object_write("b", "d/k.jpg").await;
        // Writers of the held key wait: a store, a delete, a copy into it.
        let waiting = {
            let e = engine.clone();
            tokio::spawn(async move { e.store("b", "d/k.jpg", b"one", None, HashMap::new()).await })
        };
        let deleting = {
            let e = engine.clone();
            tokio::spawn(async move { e.delete("b", "d/k.jpg").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished() && !deleting.is_finished());
        // Meanwhile the folder's other keys go on, deltaspace lock and all.
        let deadline = Duration::from_secs(10);
        tokio::time::timeout(deadline, async {
            engine
                .store("b", "d/a.jpg", b"a", None, HashMap::new())
                .await
                .unwrap();
            engine.delete("b", "d/other.jpg").await.unwrap();
            let lock = engine.lock_object_write("b", "d/c.zip").await;
            engine
                .store_known_prior(
                    &lock,
                    "b",
                    "d/c.zip",
                    &Bytes::from_static(b"delta-eligible"),
                    None,
                    HashMap::new(),
                    None,
                    None,
                )
                .await
                .unwrap();
        })
        .await
        .expect("a writer waiting for one key's lock blocked its folder");
        drop(held);
        tokio::time::timeout(deadline, waiting)
            .await
            .expect("the store never got the lock")
            .unwrap()
            .unwrap();
        tokio::time::timeout(deadline, deleting)
            .await
            .expect("the delete never got the lock")
            .unwrap()
            .unwrap();
    }

    #[test]
    fn conditional_errors_keep_their_s3_codes() {
        let code = |e: ConditionalError| crate::api::S3Error::from(e).code();
        assert_eq!(
            code(ConditionalError::PreconditionFailed),
            "PreconditionFailed"
        );
        assert_eq!(code(ConditionalError::NoSuchKey), "NoSuchKey");
        assert_eq!(
            code(ConditionalError::NotImplemented("x")),
            "NotImplemented"
        );
    }
}

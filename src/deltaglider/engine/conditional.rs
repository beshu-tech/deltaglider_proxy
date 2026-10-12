// SPDX-License-Identifier: BUSL-1.1

//! Conditional client writes: the per-object write lock and the S3 write
//! preconditions (`If-Match` / `If-None-Match`).
//!
//! Every client write of one object (PutObject, CompleteMultipartUpload,
//! CopyObject, form POST, admin bulk copy) takes the same per-object lock,
//! so a conditional check and the store that follows it are one step for
//! that key on this process. Other instances are not covered.

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

/// Held for one client write of one object; see [`DeltaGliderEngine::lock_object_write`].
pub struct ObjectWriteGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    /// The object as the precondition check read it under this lock
    /// (`Some(None)`: absent). `None`: no precondition, nothing read.
    checked: Option<Option<(FileMetadata, super::metadata::StoredForms)>>,
}

impl ObjectWriteGuard {
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

type ObjectWriteLocks = DashMap<String, Arc<tokio::sync::Mutex<()>>>;

/// THE process-wide object-write-lock map (like `shared_prefix_locks`): a
/// rebuilt engine and the one it replaces must exclude each other.
fn shared_object_write_locks() -> &'static ObjectWriteLocks {
    static SHARED: std::sync::OnceLock<ObjectWriteLocks> = std::sync::OnceLock::new();
    SHARED.get_or_init(DashMap::new)
}

/// Pure: the write-lock key of an object. The engine trims leading `/`
/// (`//a` and `a` are one object), so the lock must too, or two
/// create-only PUTs through the two spellings both pass the check.
fn object_write_lock_key(bucket: &str, key: &str) -> String {
    format!("{bucket}/{}", ObjectKey::parse(bucket, key).full_key())
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// The per-object write lock (review C4). Always taken BEFORE the
    /// per-deltaspace prefix lock (the store takes that one). Idle entries
    /// are pruned like the prefix locks.
    pub async fn lock_object_write(&self, bucket: &str, key: &str) -> ObjectWriteGuard {
        const CLEANUP_THRESHOLD: usize = 1024;
        let locks = shared_object_write_locks();
        if locks.len() > CLEANUP_THRESHOLD {
            locks.retain(|_, m| Arc::strong_count(m) > 1);
        }
        let mutex = locks
            .entry(object_write_lock_key(bucket, key))
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        ObjectWriteGuard {
            _guard: mutex.lock_owned().await,
            checked: None,
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
    pub async fn store_conditional(
        &self,
        bucket: &str,
        key: &str,
        data: Bytes,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        precondition: &Precondition,
    ) -> Result<StoreResult, ConditionalError> {
        let guard = self.lock_and_check(bucket, key, precondition).await?;
        Ok(self
            .store_client_body(
                bucket,
                key,
                data,
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
    async fn store_client_body(
        &self,
        bucket: &str,
        key: &str,
        data: Bytes,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        known_prior: Option<super::usage::CounterPrior>,
    ) -> Result<StoreResult, EngineError> {
        let size = data.len() as u64;
        if size > self.spool_threshold() && self.write_tries_delta(bucket, key, &user_metadata) {
            let spool = self.spool_acquire(size).await?;
            tokio::fs::write(spool.path(), &data)
                .await
                .map_err(|e| EngineError::Storage(StorageError::from(e)))?;
            self.store_spooled_delta_inner(
                bucket,
                key,
                &spool,
                size,
                content_type,
                user_metadata,
                None,
                known_prior,
            )
            .await
        } else {
            self.store_inner(
                bucket,
                key,
                &data,
                content_type,
                user_metadata,
                None,
                known_prior,
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
        assert_eq!(
            object_write_lock_key("b", "//a"),
            object_write_lock_key("b", "a")
        );
        assert_eq!(
            object_write_lock_key("b", "/x/y"),
            object_write_lock_key("b", "x/y")
        );
        assert_ne!(
            object_write_lock_key("b", "x/y"),
            object_write_lock_key("b", "xy")
        );
        assert_ne!(
            object_write_lock_key("b", "a/x"),
            object_write_lock_key("c", "a/x")
        );
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

// SPDX-License-Identifier: BUSL-1.1

use super::*;
use crate::coordination::LeaseError;
use crate::storage::FilesystemBackend;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// A lock that always grants, and whose renew answer the test sets.
struct ScriptedLock {
    acquires: AtomicUsize,
    renews: AtomicUsize,
    renew_ok: AtomicBool,
    grant: bool,
    ttl_secs: i64,
    interval: Duration,
}

impl ScriptedLock {
    fn new(renew_ok: bool, interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            acquires: AtomicUsize::new(0),
            renews: AtomicUsize::new(0),
            renew_ok: AtomicBool::new(renew_ok),
            grant: true,
            ttl_secs: 120,
            interval,
        })
    }
}

#[async_trait]
impl crate::coordination::ReferenceLock for ScriptedLock {
    async fn try_acquire(&self, _: &str, _: &str, _: i64) -> Result<bool, LeaseError> {
        self.acquires.fetch_add(1, Ordering::SeqCst);
        Ok(self.grant)
    }
    async fn release(&self, _: &str, _: &str) -> Result<(), LeaseError> {
        Ok(())
    }
    async fn renew(&self, _: &str, _: &str, _: i64) -> Result<(), LeaseError> {
        self.renews.fetch_add(1, Ordering::SeqCst);
        LeaseError::from_renewal(Ok::<_, LeaseError>(self.renew_ok.load(Ordering::SeqCst)))
    }
    fn ttl_secs(&self) -> i64 {
        self.ttl_secs
    }
    fn renew_interval(&self) -> Duration {
        self.interval
    }
    fn acquire_timeout(&self) -> Duration {
        Duration::from_millis(50)
    }
}

async fn engine_with(
    lock: Arc<ScriptedLock>,
) -> (tempfile::TempDir, DeltaGliderEngine<FilesystemBackend>) {
    let tmp = tempfile::tempdir().unwrap();
    let backend = FilesystemBackend::new(tmp.path().to_path_buf())
        .await
        .unwrap();
    backend.create_bucket("releases").await.unwrap();
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
        .with_reference_lock(Some(lock));
    (tmp, engine)
}

#[test]
fn hold_check_truth_table() {
    let every = Duration::from_secs(30);
    assert_eq!(
        hold_check(false, Duration::from_secs(1), every),
        HoldCheck::Trust
    );
    assert_eq!(hold_check(false, every, every), HoldCheck::Confirm);
    assert_eq!(hold_check(true, Duration::ZERO, every), HoldCheck::Lost);
}

/// A holder whose lock lapsed (renew says "not yours") must not write
/// reference.bin. Interval zero: every commit re-confirms.
#[tokio::test]
async fn lost_lock_refuses_the_reference_write() {
    let lock = ScriptedLock::new(false, Duration::ZERO);
    let (_tmp, engine) = engine_with(lock.clone()).await;
    let err = engine
        .store(
            "releases",
            "v1/app.zip",
            &vec![7u8; 4096],
            None,
            HashMap::new(),
        )
        .await
        .expect_err("a lost lock must fail the PUT");
    assert!(err.to_string().contains("lapsed"), "{err}");
    assert!(lock.renews.load(Ordering::SeqCst) >= 1);
    assert!(
        !engine
            .storage
            .has_reference("releases", "v1")
            .await
            .unwrap(),
        "no reference.bin may be written without the lock"
    );
}

/// While held, the heartbeat renews the lock; a commit after that finds
/// it still ours.
#[tokio::test]
async fn heartbeat_renews_while_held() {
    let lock = ScriptedLock::new(true, Duration::from_millis(20));
    let (_tmp, engine) = engine_with(lock.clone()).await;
    let guard = engine
        .acquire_reference_lock("releases", "v1")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        lock.renews.load(Ordering::SeqCst) >= 2,
        "heartbeat must renew"
    );
    guard.ensure_held().await.expect("still held");
    // The heartbeat sees the lock lost → the next commit refuses at once.
    lock.renew_ok.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(guard.ensure_held().await.is_err());
}

/// Deleting the last object reclaims reference.bin: a reference write,
/// so it takes the cross-instance lock. It once took only the in-process
/// one, and a peer's fresh delta could lose its reference.
#[tokio::test]
async fn delete_reclaim_and_sweep_reclaim_take_the_lock() {
    let lock = ScriptedLock::new(true, Duration::from_secs(30));
    let (_tmp, engine) = engine_with(lock.clone()).await;
    engine
        .store(
            "releases",
            "v1/app.zip",
            &vec![7u8; 4096],
            None,
            HashMap::new(),
        )
        .await
        .unwrap();
    let after_store = lock.acquires.load(Ordering::SeqCst);
    engine.delete("releases", "v1/app.zip").await.unwrap();
    assert!(
        lock.acquires.load(Ordering::SeqCst) > after_store,
        "reclaim-on-delete must take the lock"
    );
    assert!(!engine
        .storage
        .has_reference("releases", "v1")
        .await
        .unwrap());

    engine
        .store(
            "releases",
            "v2/app.zip",
            &vec![8u8; 4096],
            None,
            HashMap::new(),
        )
        .await
        .unwrap();
    engine
        .delete_in_sweep("releases", "v2/app.zip")
        .await
        .unwrap();
    let before_sweep = lock.acquires.load(Ordering::SeqCst);
    engine
        .reclaim_empty_deltaspace("releases", "v2")
        .await
        .unwrap();
    assert!(lock.acquires.load(Ordering::SeqCst) > before_sweep);
    assert!(!engine
        .storage
        .has_reference("releases", "v2")
        .await
        .unwrap());
}

/// The replication fast path seeds a reference inside
/// `with_dest_prefix_lock`: it must hold the cross-instance lock, and a
/// lock it cannot take must stop the closure.
#[tokio::test]
async fn fast_path_seed_holds_the_lock() {
    let lock = ScriptedLock::new(true, Duration::from_secs(30));
    let (_tmp, engine) = engine_with(lock.clone()).await;
    let meta = FileMetadata::new_reference(
        "__reference__".into(),
        "v1/app.zip".into(),
        "00".repeat(32),
        "00".repeat(16),
        3,
        None,
    );
    let res: Result<(), EngineError> = engine
        .with_dest_prefix_lock("releases", "v1", || async {
            Ok(engine
                .put_reference_raw("releases", "v1", b"abc", &meta)
                .await?)
        })
        .await;
    res.unwrap();
    assert_eq!(lock.acquires.load(Ordering::SeqCst), 1);

    // Outside the scope, multi-instance refuses the raw write.
    assert!(engine
        .put_reference_raw("releases", "v1", b"abc", &meta)
        .await
        .is_err());
    // The same for a raw delta (review 4 coordination-5): outside the
    // lock it would be an unfenced write against an unheld reference.
    let delta_meta = FileMetadata::new_delta(
        "app.zip".into(),
        "00".repeat(32),
        "00".repeat(16),
        3,
        "v1/reference.bin".into(),
        "00".repeat(32),
        3,
        None,
    );
    let err = engine
        .put_delta_raw("releases", "v1", "app.zip", b"abc", &delta_meta)
        .await
        .expect_err("a raw delta outside the deltaspace lock is refused");
    assert!(
        err.to_string().contains("outside the deltaspace lock"),
        "{err}"
    );
    assert_eq!(lock.acquires.load(Ordering::SeqCst), 1, "no lock taken");

    // A lock that cannot be taken: the closure never runs.
    let busy = Arc::new(ScriptedLock {
        grant: false,
        ..Arc::try_unwrap(ScriptedLock::new(true, Duration::from_secs(30)))
            .ok()
            .unwrap()
    });
    let (_tmp2, engine2) = engine_with(busy).await;
    let ran = AtomicBool::new(false);
    let res: Result<(), EngineError> = engine2
        .with_dest_prefix_lock("releases", "v1", || async {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert!(res.is_err());
    assert!(!ran.load(Ordering::SeqCst));
}

/// A backend reference or delta write needs a `RefWriteProof`, which
/// only the guard's `reference_writes` module makes; the compiler
/// enforces that. `RefWriteProof::unguarded` exists for integration
/// tests only: no production file calls it.
#[test]
fn production_code_never_writes_unguarded() {
    let needle = ["RefWriteProof::", "unguarded("].concat();
    let offenders: Vec<String> = crate::source_scan::prod_sources("src")
        .into_iter()
        .filter(|(_, text)| crate::source_scan::prod_text(text).contains(needle.as_str()))
        .map(|(rel, _)| rel)
        .collect();
    assert!(
        offenders.is_empty(),
        "unguarded reference writes: {offenders:?}"
    );
}

// ── review second pass (failing tests for findings) ──────────────────

/// A lock that models `S3ReferenceLock::renew`: read the etag, one round
/// trip, then an If-Match PUT; a changed etag is a 412 → `Ok(false)`.
struct CasLock {
    state: parking_lot::Mutex<(u64, Option<String>)>, // (etag, owner)
    rtt: Duration,
    interval: Duration,
}

#[async_trait]
impl crate::coordination::ReferenceLock for CasLock {
    async fn try_acquire(&self, _: &str, owner: &str, _: i64) -> Result<bool, LeaseError> {
        let mut s = self.state.lock();
        if s.1.is_some() {
            return Ok(false);
        }
        s.0 += 1;
        s.1 = Some(owner.to_string());
        Ok(true)
    }
    async fn release(&self, _: &str, owner: &str) -> Result<(), LeaseError> {
        let mut s = self.state.lock();
        if s.1.as_deref() == Some(owner) {
            s.0 += 1;
            s.1 = None;
        }
        Ok(())
    }
    async fn renew(&self, _: &str, owner: &str, _: i64) -> Result<(), LeaseError> {
        let seen = {
            let s = self.state.lock();
            if s.1.as_deref() != Some(owner) {
                return Err(LeaseError::Lost);
            }
            s.0
        };
        tokio::time::sleep(self.rtt).await; // the If-Match PUT round trip
        let mut s = self.state.lock();
        if s.0 != seen {
            return Err(LeaseError::Lost); // 412 -> put_lock Ok(false)
        }
        s.0 += 1;
        Ok(())
    }
    fn ttl_secs(&self) -> i64 {
        120
    }
    fn renew_interval(&self) -> Duration {
        self.interval
    }
    fn acquire_timeout(&self) -> Duration {
        Duration::from_millis(50)
    }
}

/// Review-2 (2d4fd1ac): the heartbeat and `ensure_held` (Confirm path)
/// renew the SAME lock with the same owner. Confirm fires when the last
/// confirmation is `renew_interval` old, i.e. exactly while the
/// heartbeat's renew is in flight. Both PUT with If-Match on one etag;
/// the loser's 412 reads as "lost", so the commit is refused and the
/// hold is marked lost although the lock is still ours.
#[tokio::test]
async fn review2_commit_racing_the_heartbeat_renew_keeps_the_lock() {
    let lock = Arc::new(CasLock {
        state: parking_lot::Mutex::new((0, None)),
        rtt: Duration::from_millis(40),
        interval: Duration::from_millis(100),
    });
    let tmp = tempfile::tempdir().unwrap();
    let backend = FilesystemBackend::new(tmp.path().to_path_buf())
        .await
        .unwrap();
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
        .with_reference_lock(Some(lock.clone()));
    let guard = engine
        .acquire_reference_lock("review2-releases", "v1")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(110)).await;
    let commit = guard.ensure_held().await;
    let next = guard.ensure_held().await;
    assert!(lock.state.lock().1.is_some(), "the lock is still ours");
    assert!(commit.is_ok(), "commit refused: {}", commit.unwrap_err());
    assert!(next.is_ok(), "hold marked lost: {}", next.unwrap_err());
}

/// A lock that models the S3 wire: a renew's If-Match PUT, once sent,
/// lands even when the caller's future is dropped; release reads the
/// etag, then one round trip later DELETEs with If-Match.
struct WireLock {
    state: Arc<parking_lot::Mutex<(u64, Option<String>)>>, // (etag, owner)
    rtt: Duration,
    interval: Duration,
}

#[async_trait]
impl crate::coordination::ReferenceLock for WireLock {
    async fn try_acquire(&self, _: &str, owner: &str, _: i64) -> Result<bool, LeaseError> {
        let mut s = self.state.lock();
        if s.1.is_some() {
            return Ok(false);
        }
        *s = (s.0 + 1, Some(owner.to_string()));
        Ok(true)
    }
    async fn release(&self, _: &str, owner: &str) -> Result<(), LeaseError> {
        let seen = self.state.lock().clone();
        tokio::time::sleep(self.rtt).await;
        let mut s = self.state.lock();
        if s.0 == seen.0 && s.1.as_deref() == Some(owner) {
            *s = (s.0 + 1, None);
        }
        Ok(())
    }
    async fn renew(&self, _: &str, owner: &str, _: i64) -> Result<(), LeaseError> {
        let seen = self.state.lock().clone();
        if seen.1.as_deref() != Some(owner) {
            return Err(LeaseError::Lost);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (state, rtt) = (self.state.clone(), self.rtt);
        tokio::spawn(async move {
            tokio::time::sleep(rtt).await; // the PUT is on the wire
            let mut s = state.lock();
            let ok = s.0 == seen.0;
            if ok {
                s.0 += 1;
            }
            let _ = tx.send(ok);
        });
        LeaseError::from_renewal(Ok::<_, LeaseError>(rx.await.unwrap_or(false)))
    }
    fn ttl_secs(&self) -> i64 {
        120
    }
    fn renew_interval(&self) -> Duration {
        self.interval
    }
    fn acquire_timeout(&self) -> Duration {
        Duration::from_millis(50)
    }
}

/// The guard dropped while a heartbeat renew is on the wire: the renew
/// lands between the release's read and its If-Match DELETE, the DELETE
/// fails, and the lock stays until its TTL (every PUT to the deltaspace
/// waits, then fails). The release must wait out the renew.
#[tokio::test]
async fn release_does_not_race_a_heartbeat_renew() {
    let state = Arc::new(parking_lot::Mutex::new((0, None)));
    let lock = Arc::new(WireLock {
        state: state.clone(),
        rtt: Duration::from_millis(30),
        interval: Duration::from_millis(20),
    });
    let tmp = tempfile::tempdir().unwrap();
    let backend = FilesystemBackend::new(tmp.path().to_path_buf())
        .await
        .unwrap();
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
        .with_reference_lock(Some(lock));
    let guard = engine
        .acquire_reference_lock("release-race", "v1")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(28)).await; // renew on the wire
    drop(guard);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(state.lock().1.is_none(), "the lock was never released");
}

/// Review-2 (b22f6ff8 incomplete): only the reclaim CHECK became
/// best-effort. `delete_reference(..).await?` still fails the DELETE
/// after the object itself is already gone (lost lock, transient error),
/// so the client gets a 500 for a delete that happened.
#[tokio::test]
async fn review2_lost_lock_during_reclaim_does_not_fail_the_delete() {
    let lock = ScriptedLock::new(true, Duration::ZERO); // every commit re-confirms
    let (_tmp, engine) = engine_with(lock.clone()).await;
    engine
        .store(
            "releases",
            "v9/app.zip",
            &vec![7u8; 4096],
            None,
            HashMap::new(),
        )
        .await
        .unwrap();
    lock.renew_ok.store(false, Ordering::SeqCst);
    let res = engine.delete("releases", "v9/app.zip").await;
    let gone = engine.head("releases", "v9/app.zip").await.is_err();
    assert!(gone, "precondition: the object is deleted");
    assert!(
        res.is_ok(),
        "the object is already deleted; DELETE must not fail: {}",
        res.err().map(|e| e.to_string()).unwrap_or_default()
    );
}

// ── Fencing: the reference write is conditional on what the lock saw ──

use crate::storage::{RefFence, RefWrite};
use crate::types::FileMetadata as Meta;
use futures::stream::BoxStream;

/// A filesystem backend with S3-style conditional reference writes: a
/// version per reference plays the ETag. `peer_race` makes a peer write
/// its own reference.bin right before our next reference write lands
/// (the lock lapsed under us and the peer stole it).
struct FencingFs {
    inner: FilesystemBackend,
    versions: parking_lot::Mutex<HashMap<String, u64>>,
    peer_race: AtomicBool,
    /// review3: the peer's baseline lands right before our next DELTA
    /// write (not a reference write) lands.
    peer_race_on_delta: AtomicBool,
}

impl FencingFs {
    fn slot(b: &str, p: &str) -> String {
        format!("{b}/{p}")
    }
    fn bump(&self, b: &str, p: &str) {
        *self.versions.lock().entry(Self::slot(b, p)).or_insert(0) += 1;
    }
    async fn maybe_peer_write(&self, b: &str, p: &str) {
        if self.peer_race.swap(false, Ordering::SeqCst) {
            let meta = Meta::new_reference(
                "reference.bin".into(),
                "peer.zip".into(),
                "0".repeat(64),
                "0".repeat(32),
                4,
                None,
            );
            self.inner
                .put_reference(
                    b,
                    p,
                    b"PEER",
                    &meta,
                    crate::deltaglider::RefWriteProof::for_tests(),
                )
                .await
                .unwrap();
            self.bump(b, p);
        }
    }
}

#[async_trait]
impl crate::storage::StorageBackend for FencingFs {
    async fn create_bucket(&self, b: &str) -> Result<(), StorageError> {
        self.inner.create_bucket(b).await
    }
    async fn delete_bucket(&self, b: &str) -> Result<(), StorageError> {
        self.inner.delete_bucket(b).await
    }
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        self.inner.list_buckets().await
    }
    async fn head_bucket(&self, b: &str) -> Result<bool, StorageError> {
        self.inner.head_bucket(b).await
    }
    async fn get_reference(&self, b: &str, p: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.get_reference(b, p).await
    }
    async fn put_reference(
        &self,
        b: &str,
        p: &str,
        d: &[u8],
        m: &Meta,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.maybe_peer_write(b, p).await;
        self.inner.put_reference(b, p, d, m, proof).await?;
        self.bump(b, p);
        Ok(())
    }
    async fn put_reference_metadata(
        &self,
        b: &str,
        p: &str,
        m: &Meta,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.inner.put_reference_metadata(b, p, m, proof).await
    }
    async fn get_reference_metadata(&self, b: &str, p: &str) -> Result<Meta, StorageError> {
        self.inner.get_reference_metadata(b, p).await
    }
    async fn has_reference(&self, b: &str, p: &str) -> Result<bool, StorageError> {
        self.inner.has_reference(b, p).await
    }
    async fn flush_pending(&self) -> Result<(), StorageError> {
        self.inner.flush_pending().await
    }
    async fn delete_reference(
        &self,
        b: &str,
        p: &str,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.inner.delete_reference(b, p, proof).await?;
        self.versions.lock().remove(&Self::slot(b, p));
        Ok(())
    }
    async fn reference_fence(&self, b: &str, p: &str) -> Result<RefFence, StorageError> {
        Ok(match self.versions.lock().get(&Self::slot(b, p)) {
            Some(v) => RefFence::ETag(format!("v{v}")),
            None => RefFence::Absent,
        })
    }
    async fn write_reference_fenced(
        &self,
        b: &str,
        p: &str,
        op: RefWrite<'_>,
        fence: &RefFence,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<RefFence, StorageError> {
        self.maybe_peer_write(b, p).await;
        let now = self.reference_fence(b, p).await?;
        if *fence != RefFence::Unfenced && *fence != now {
            return Err(crate::storage::reference_fence_lost(b, p));
        }
        match op {
            RefWrite::Put { data, metadata } => {
                self.inner
                    .put_reference(b, p, data, metadata, proof)
                    .await?
            }
            RefWrite::PutFile { path, metadata } => {
                self.inner
                    .put_reference_from_file(b, p, path, metadata, proof)
                    .await?
            }
            RefWrite::Metadata { metadata } => {
                self.inner
                    .put_reference_metadata(b, p, metadata, proof)
                    .await?
            }
            RefWrite::Delete => {
                self.inner.delete_reference(b, p, proof).await?;
                self.versions.lock().remove(&Self::slot(b, p));
                return Ok(RefFence::Absent);
            }
        }
        self.bump(b, p);
        self.reference_fence(b, p).await
    }
    async fn get_delta(&self, b: &str, p: &str, f: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.get_delta(b, p, f).await
    }
    async fn put_delta(
        &self,
        b: &str,
        p: &str,
        f: &str,
        d: &[u8],
        m: &Meta,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        if self.peer_race_on_delta.swap(false, Ordering::SeqCst) {
            self.peer_race.store(true, Ordering::SeqCst);
            self.maybe_peer_write(b, p).await;
        }
        self.inner.put_delta(b, p, f, d, m, proof).await
    }
    async fn get_delta_metadata(&self, b: &str, p: &str, f: &str) -> Result<Meta, StorageError> {
        self.inner.get_delta_metadata(b, p, f).await
    }
    async fn delete_delta(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
        self.inner.delete_delta(b, p, f).await
    }
    async fn get_passthrough(&self, b: &str, p: &str, f: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.get_passthrough(b, p, f).await
    }
    async fn put_passthrough(
        &self,
        b: &str,
        p: &str,
        f: &str,
        d: &[u8],
        m: &Meta,
    ) -> Result<(), StorageError> {
        self.inner.put_passthrough(b, p, f, d, m).await
    }
    async fn get_passthrough_metadata(
        &self,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<Meta, StorageError> {
        self.inner.get_passthrough_metadata(b, p, f).await
    }
    async fn delete_passthrough(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
        self.inner.delete_passthrough(b, p, f).await
    }
    async fn open_object(
        &self,
        b: &str,
        p: &str,
        o: crate::storage::StoredObject<'_>,
    ) -> Result<
        (crate::storage::ByteStream, crate::types::FileMetadata),
        crate::storage::StorageError,
    > {
        crate::storage::open_object_by_parts(self, b, p, o).await
    }
    async fn get_passthrough_stream(
        &self,
        b: &str,
        p: &str,
        f: &str,
    ) -> Result<BoxStream<'static, Result<bytes::Bytes, StorageError>>, StorageError> {
        self.inner.get_passthrough_stream(b, p, f).await
    }
    async fn get_passthrough_stream_range(
        &self,
        b: &str,
        p: &str,
        f: &str,
        start: u64,
        end: u64,
    ) -> Result<(BoxStream<'static, Result<bytes::Bytes, StorageError>>, u64), StorageError> {
        self.inner
            .get_passthrough_stream_range(b, p, f, start, end)
            .await
    }
    async fn scan_deltaspace(&self, b: &str, p: &str) -> Result<Vec<Meta>, StorageError> {
        self.inner.scan_deltaspace(b, p).await
    }
    async fn list_deltaspaces(&self, b: &str) -> Result<Vec<String>, StorageError> {
        self.inner.list_deltaspaces(b).await
    }
    async fn total_size(&self, b: Option<&str>) -> Result<u64, StorageError> {
        self.inner.total_size(b).await
    }
    async fn bulk_list_objects(
        &self,
        b: &str,
        p: &str,
    ) -> Result<Vec<(String, Meta)>, StorageError> {
        self.inner.bulk_list_objects(b, p).await
    }
}

async fn fencing_engine(
    lock: Arc<ScriptedLock>,
) -> (tempfile::TempDir, DeltaGliderEngine<FencingFs>) {
    let tmp = tempfile::tempdir().unwrap();
    let inner = FilesystemBackend::new(tmp.path().to_path_buf())
        .await
        .unwrap();
    inner.create_bucket("releases").await.unwrap();
    let backend = FencingFs {
        inner,
        versions: parking_lot::Mutex::new(HashMap::new()),
        peer_race: AtomicBool::new(false),
        peer_race_on_delta: AtomicBool::new(false),
    };
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
        .with_reference_lock(Some(lock));
    (tmp, engine)
}

/// The lock lapsed and a peer wrote reference.bin between our lock-time
/// observation ("absent") and our baseline write. The write must fail
/// retryably (503 SlowDown), and the peer's baseline must stay.
#[tokio::test]
async fn a_peer_baseline_written_under_us_is_never_overwritten() {
    let lock = ScriptedLock::new(true, Duration::from_secs(60));
    let (_tmp, engine) = fencing_engine(lock).await;
    engine.storage.peer_race.store(true, Ordering::SeqCst);
    let err = engine
        .store(
            "releases",
            "v1/app.zip",
            &vec![7u8; 4096],
            None,
            HashMap::new(),
        )
        .await
        .expect_err("a lost fence must fail the PUT");
    assert!(
        matches!(err, EngineError::Storage(StorageError::Throttled(_))),
        "a lost fence must be retryable (SlowDown), got {err:?}"
    );
    assert_eq!(
        engine
            .storage
            .get_reference("releases", "v1")
            .await
            .unwrap(),
        b"PEER",
        "the peer's reference.bin was overwritten"
    );
}

/// The fence covers reference.bin writes only. A delta PUT against an
/// existing reference writes no reference, so when the lock lapsed and a
/// peer re-created the baseline from other bytes, our delta (encoded
/// against the old baseline) still lands and the PUT answers 200: the
/// object is unreadable afterwards.
#[tokio::test]
async fn review3_a_delta_against_a_replaced_baseline_is_not_acknowledged() {
    let lock = ScriptedLock::new(true, Duration::from_secs(60));
    let (_tmp, engine) = fencing_engine(lock).await;
    let a = vec![7u8; 4096];
    let mut b = a.clone();
    b[100] = 9;
    engine
        .store("releases", "v1/a.zip", &a, None, HashMap::new())
        .await
        .expect("baseline PUT");
    engine
        .storage
        .peer_race_on_delta
        .store(true, Ordering::SeqCst);
    let stored = engine
        .store("releases", "v1/b.zip", &b, None, HashMap::new())
        .await;
    assert!(
        matches!(
            stored,
            Err(EngineError::Storage(StorageError::Throttled(_)))
        ),
        "a delta against a replaced baseline must fail retryably, got {stored:?}"
    );
    assert!(
        engine
            .storage
            .get_delta("releases", "v1", "b.zip")
            .await
            .is_err(),
        "the unfenced delta must not stay behind"
    );
    if stored.is_ok() {
        // Another node (no in-process reference cache) reads it back.
        let peer =
            DeltaGliderEngine::new_with_backend(engine.storage.clone(), &Config::default(), None);
        let got = peer.retrieve("releases", "v1/b.zip").await;
        assert!(
            got.as_ref().is_ok_and(|(data, _)| *data == b),
            "the PUT answered 200 but the object does not read back: {:?}",
            got.err()
        );
    }
}

/// Without a race the fenced baseline write goes through, and the second
/// PUT (existing reference, fence = its ETag) stores a delta.
#[tokio::test]
async fn fenced_writes_pass_without_a_race() {
    let lock = ScriptedLock::new(true, Duration::from_secs(60));
    let (_tmp, engine) = fencing_engine(lock).await;
    engine
        .store(
            "releases",
            "v1/a.zip",
            &vec![7u8; 4096],
            None,
            HashMap::new(),
        )
        .await
        .expect("first PUT");
    engine
        .store(
            "releases",
            "v1/b.zip",
            &vec![8u8; 4096],
            None,
            HashMap::new(),
        )
        .await
        .expect("second PUT");
    assert!(engine
        .storage
        .has_reference("releases", "v1")
        .await
        .unwrap());
}

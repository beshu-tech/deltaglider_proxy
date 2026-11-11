// SPDX-License-Identifier: BUSL-1.1

//! `deltaglider::ingest` on an engine over the filesystem backend: the
//! medium each body takes, the RAM peak, and that every budget is whole
//! again after a success and after each failure.

use super::*;
use crate::deltaglider::ingest::{ingest, IngestError, IngestPlan, IngestedBody};
use crate::deltaglider::ingest_rig::{body, IngestRig, RigConfig};
use crate::deltaglider::memory_budget::MemoryBudget;
use md5::{Digest, Md5};
use sha2::Sha256;
use std::sync::atomic::{AtomicBool, Ordering};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const THRESHOLD: u64 = 64 * KIB;
const SPOOL_MIB: u64 = 8;

struct Rig {
    base: IngestRig,
    engine: DynEngine,
}

impl Rig {
    async fn new(memory: MemoryBudget) -> Self {
        Self::with_encryption(memory, false).await
    }

    /// `encrypted`: the backend is wrapped by the proxy's encryption, so
    /// a stored body needs a ciphertext file in the spool too.
    async fn with_encryption(memory: MemoryBudget, encrypted: bool) -> Self {
        let (base, engine) = IngestRig::build(RigConfig {
            threshold: THRESHOLD,
            spool_bytes: SPOOL_MIB * MIB,
            memory,
            max_object_size: None,
            encrypted,
            quotas: vec![],
            buckets: vec![],
        })
        .await;
        Self { base, engine }
    }

    async fn ingest<S>(
        &self,
        stream: S,
        plan: IngestPlan,
    ) -> Result<IngestedBody, IngestError<std::io::Error>>
    where
        S: futures::Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
    {
        ingest(&self.engine, "b", stream, plan).await
    }

    fn bodies(&self, medium: &str, reason: &str) -> u64 {
        self.base.bodies(medium, reason)
    }

    /// Nothing held: the RAM budget, the spool budget and the spool dir.
    fn assert_restored(&self, when: &str) {
        self.base.assert_restored(&self.engine, when);
    }
}

fn plan(declared: u64) -> IngestPlan {
    IngestPlan {
        declared,
        limit: 64 * MIB,
        sha256: crate::deltaglider::ingest::Sha256Need::Compute,
        held_spool_mib: 0,
        strict_length: true,
        destination: crate::deltaglider::ingest::Destination::Object,
    }
}

/// `data` in chunks of `chunk` bytes, the way a client body arrives.
fn chunks(
    data: &[u8],
    chunk: usize,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Unpin {
    let pieces: Vec<_> = data
        .chunks(chunk)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    futures::stream::iter(pieces)
}

fn assert_hashes(got: &IngestedBody, data: &[u8]) {
    assert_eq!(got.size(), data.len() as u64);
    assert_eq!(got.hashes().sha256(), hex::encode(Sha256::digest(data)));
    assert_eq!(got.hashes().md5(), hex::encode(Md5::digest(data)));
}

#[tokio::test]
async fn a_small_body_is_read_into_memory_on_its_permit() {
    let rig = Rig::new(MemoryBudget::new(THRESHOLD * 2)).await;
    let data = body(40 * KIB as usize);
    let got = rig
        .ingest(chunks(&data, 1000), plan(data.len() as u64))
        .await
        .unwrap();
    match &got {
        IngestedBody::Memory {
            data: d, permit, ..
        } => {
            assert_eq!(d.as_ref(), data.as_slice());
            assert_eq!(permit.bytes(), 40 * KIB);
        }
        other => panic!("a small body was spooled: {other:?}"),
    }
    assert_hashes(&got, &data);
    assert_eq!(rig.engine.memory_budget().held_bytes(), 40 * KIB);
    assert_eq!(rig.bodies("memory", "under_threshold"), 1);
    drop(got);
    rig.assert_restored("after a memory body");
}

/// A body above the threshold goes to a spool file from the first byte:
/// the RAM budget is never asked, so its peak stays at most the threshold.
#[tokio::test]
async fn a_large_body_is_spooled_and_never_held_in_memory() {
    let rig = Rig::new(MemoryBudget::unbounded()).await;
    let data = body(3 * THRESHOLD as usize);
    let got = rig
        .ingest(chunks(&data, 4096), plan(data.len() as u64))
        .await
        .unwrap();
    match &got {
        IngestedBody::Spooled { spool, size, .. } => {
            assert_eq!(*size, data.len() as u64);
            assert_eq!(std::fs::read(spool.path()).unwrap(), data);
        }
        other => panic!("a large body was read into memory: {other:?}"),
    }
    assert_hashes(&got, &data);
    assert!(rig.engine.memory_budget().peak_bytes() <= THRESHOLD);
    assert_eq!(rig.bodies("spool", "over_threshold"), 1);
    assert!(
        rig.engine.spool.free_mib() < SPOOL_MIB as usize,
        "the spool is reserved"
    );
    drop(got);
    rig.assert_restored("after a spooled body");
}

/// A small body that finds the RAM budget full goes to the spool, at once.
#[tokio::test]
async fn a_small_body_without_ram_takes_the_spool() {
    let rig = Rig::new(MemoryBudget::new(THRESHOLD)).await;
    let hog = rig.engine.memory_budget().try_reserve(THRESHOLD).unwrap();
    let data = body(10 * KIB as usize);
    let got = rig
        .ingest(chunks(&data, 999), plan(data.len() as u64))
        .await
        .unwrap();
    assert!(matches!(got, IngestedBody::Spooled { .. }), "{got:?}");
    assert_hashes(&got, &data);
    assert_eq!(rig.bodies("spool", "memory_budget"), 1);
    drop((got, hog));
    rig.assert_restored("after a small spooled body");

    // DGP_INGEST_MEMORY_MAX_BYTES=0: every body is spooled.
    let rig = Rig::new(MemoryBudget::new(0)).await;
    let got = rig
        .ingest(chunks(&data, 999), plan(data.len() as u64))
        .await
        .unwrap();
    assert!(matches!(got, IngestedBody::Spooled { .. }), "{got:?}");
}

/// A small body never waits for spool budget: no RAM and no free spool
/// is a retryable `Overloaded` at once, even with the acquire timeout.
#[tokio::test]
async fn a_small_body_never_queues_for_the_spool() {
    let rig = Rig::new(MemoryBudget::new(0)).await;
    let hog = rig.engine.spool.acquire(SPOOL_MIB * MIB).await.unwrap();
    let started = std::time::Instant::now();
    let err = rig.ingest(chunks(b"tiny", 4), plan(4)).await.unwrap_err();
    assert!(
        matches!(err, IngestError::Engine(EngineError::Overloaded(_))),
        "{err:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "it waited"
    );
    drop(hog);
    rig.assert_restored("after a refused small body");
}

/// A large body waits for its reservation BEFORE the body is polled.
#[tokio::test]
async fn a_large_body_waits_for_the_spool_before_reading() {
    let rig = Arc::new(Rig::new(MemoryBudget::unbounded()).await);
    let hog = rig.engine.spool.acquire(SPOOL_MIB * MIB).await.unwrap();
    let polled = Arc::new(AtomicBool::new(false));
    let data = body(2 * THRESHOLD as usize);
    let flag = polled.clone();
    let stream = futures::StreamExt::inspect(chunks(&data, 4096), move |_| {
        flag.store(true, Ordering::SeqCst)
    });
    let r = rig.clone();
    let len = data.len() as u64;
    let task = tokio::spawn(async move { r.ingest(stream, plan(len)).await.map(|b| b.size()) });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !polled.load(Ordering::SeqCst),
        "the body was read before the spool was reserved"
    );
    drop(hog);
    assert_eq!(task.await.unwrap().unwrap(), len);
    assert!(polled.load(Ordering::SeqCst));
    rig.assert_restored("after a waiting body");
}

#[tokio::test]
async fn the_declared_length_is_checked_before_a_byte_is_read() {
    let rig = Rig::new(MemoryBudget::unbounded()).await;
    let never = || {
        futures::stream::poll_fn(
            |_| -> std::task::Poll<Option<Result<Bytes, std::io::Error>>> {
                panic!("the body was polled")
            },
        )
    };
    let mut p = plan(10 * MIB);
    p.limit = MIB;
    let err = rig.ingest(never(), p).await.unwrap_err();
    assert!(matches!(err, IngestError::TooLarge { .. }), "{err:?}");
    let err = rig
        .ingest(never(), plan(SPOOL_MIB * MIB + 1))
        .await
        .unwrap_err();
    assert!(
        matches!(err, IngestError::SpoolTooSmall { max_bytes, .. } if max_bytes == SPOOL_MIB * MIB),
        "{err:?}"
    );
    rig.assert_restored("after refused lengths");
}

/// A bucket that the proxy encrypts needs the body AND its ciphertext
/// file in the spool: a body that fits the budget alone but not with its
/// encrypted copy is refused before a byte is read, with a message that
/// names DGP_SPOOL_MAX_BYTES and the encryption. Before, it was admitted,
/// its reservation clamped to the whole budget, and the write put about
/// twice the budget on the disk.
#[tokio::test]
async fn an_encrypted_body_is_admitted_with_its_ciphertext_file() {
    let rig = Rig::with_encryption(MemoryBudget::unbounded(), true).await;
    let never = || {
        futures::stream::poll_fn(
            |_| -> std::task::Poll<Option<Result<Bytes, std::io::Error>>> {
                panic!("the body was polled")
            },
        )
    };
    // 5 MiB fits the 8 MiB spool alone; with its 5 MiB copy it does not.
    let err = rig.ingest(never(), plan(5 * MIB)).await.unwrap_err();
    match &err {
        IngestError::SpoolTooSmall {
            declared,
            storage_extra,
            max_bytes,
        } => {
            assert_eq!(*declared, 5 * MIB);
            assert!(*storage_extra >= 5 * MIB, "{storage_extra}");
            assert_eq!(*max_bytes, SPOOL_MIB * MIB);
        }
        other => panic!("{other:?}"),
    }
    let msg = err.to_string();
    assert!(
        msg.contains("DGP_SPOOL_MAX_BYTES") && msg.contains("encrypted"),
        "{msg}"
    );
    rig.assert_restored("after a refused encrypted body");

    // 3 MiB with its copy fits: it is read and kept.
    let data = body(3 * MIB as usize);
    let got = rig
        .ingest(chunks(&data, 64 * KIB as usize), plan(3 * MIB))
        .await
        .unwrap();
    assert_eq!(got.size(), 3 * MIB);
    drop(got);
    rig.assert_restored("after an encrypted body");
}

/// Every failure in the middle of a body leaves nothing held, in memory
/// and in the spool.
#[tokio::test]
async fn every_failure_restores_the_budgets() {
    let rig = Rig::new(MemoryBudget::new(THRESHOLD)).await;
    for (medium, len) in [
        ("memory", 20 * KIB as usize),
        ("spool", 2 * THRESHOLD as usize),
    ] {
        let data = body(len);
        let declared = len as u64;

        // The source fails half way.
        let half = Bytes::copy_from_slice(&data[..len / 2]);
        let failing = futures::stream::iter(vec![
            Ok(half),
            Err(std::io::Error::other("connection reset")),
        ]);
        let err = rig.ingest(failing, plan(declared)).await.unwrap_err();
        assert!(matches!(err, IngestError::Source(_)), "{medium}: {err:?}");
        rig.assert_restored(&format!("{medium}: source error"));

        // Longer than declared: fails at the first byte past it.
        let err = rig
            .ingest(chunks(&data, 777), plan(declared - 1))
            .await
            .unwrap_err();
        assert!(
            matches!(err, IngestError::TooLong { .. }),
            "{medium}: {err:?}"
        );
        rig.assert_restored(&format!("{medium}: overrun"));

        // Shorter than declared.
        let err = rig
            .ingest(chunks(&data, 777), plan(declared + 1))
            .await
            .unwrap_err();
        assert!(
            matches!(err, IngestError::Incomplete { received, .. } if received == declared),
            "{medium}: {err:?}"
        );
        rig.assert_restored(&format!("{medium}: underrun"));

        // A form file: the declared length is an upper bound.
        let mut upper = plan(declared + 100);
        upper.strict_length = false;
        let got = rig.ingest(chunks(&data, 777), upper).await.unwrap();
        assert_hashes(&got, &data);
        drop(got);
        rig.assert_restored(&format!("{medium}: short form file"));

        // The client goes away: the future is dropped mid-body.
        let first = Bytes::copy_from_slice(&data[..len / 2]);
        let stalled = futures::StreamExt::chain(
            futures::stream::iter(vec![Ok::<_, std::io::Error>(first)]),
            futures::stream::pending(),
        );
        let fut = rig.ingest(stalled, plan(declared));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), fut)
                .await
                .is_err(),
            "{medium}: a stalled body finished"
        );
        rig.assert_restored(&format!("{medium}: dropped future"));
    }
    assert!(rig.engine.memory_budget().peak_bytes() <= THRESHOLD);
}

/// A SHA-256 the caller proved is kept; MD5 is computed, on both media.
#[tokio::test]
async fn a_known_sha256_is_kept() {
    let rig = Rig::new(MemoryBudget::unbounded()).await;
    let proven = crate::deltaglider::KnownSha256::of(b"proven");
    for len in [100usize, 2 * THRESHOLD as usize] {
        let data = body(len);
        let mut p = plan(len as u64);
        p.sha256 = crate::deltaglider::ingest::Sha256Need::Known(proven);
        let got = rig.ingest(chunks(&data, 1000), p).await.unwrap();
        assert_eq!(got.hashes().sha256(), proven.to_hex(), "{len}");
        assert_eq!(got.hashes().md5(), hex::encode(Md5::digest(&data)), "{len}");
    }
}

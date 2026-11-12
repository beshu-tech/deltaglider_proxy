// SPDX-License-Identifier: BUSL-1.1

//! Ingest: a request body read ONCE, into memory or into a spool file,
//! with its hashes, before any lock is taken or any byte is stored.
//!
//! The medium is decided before the first byte, from the length the client
//! declared ([`medium`]):
//!
//! - a body of at most the spool threshold (`DGP_SPOOL_THRESHOLD_BYTES`)
//!   is read into one buffer of its declared size when the RAM budget
//!   (`DGP_INGEST_MEMORY_MAX_BYTES`, `memory_budget::MemoryBudget`) has room for it;
//! - a larger body takes a spool reservation, and may wait for it (up to
//!   `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS`) before the body is polled, so a
//!   client that sent `Expect: 100-continue` has sent nothing yet;
//! - a small body that finds no RAM takes free spool budget without
//!   waiting (it never queues behind a large reservation), else the request
//!   is a retryable 503 SlowDown;
//! - a multipart part ([`Destination::MultipartPart`]) always goes to a
//!   spool file, which the multipart store keeps as a relay part.
//!
//! The spool reservation of an object covers the body AND the temp files that the
//! storage write of that body needs (`file_put_spool_bytes`), so the write
//! never waits for budget under its locks. The fill is capped at the
//! declared length, which the reservation covers: disk use never exceeds
//! the budget. A body longer than declared fails at the first byte past
//! it; a shorter one fails, except where the plan allows it (a form file,
//! whose declared length is an upper bound). There is no spill from memory
//! to disk in the middle of a body. Every failure drops the spool file and
//! the permits (RAII), so nothing partial is kept.

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use md5::{Digest as _, Md5};
use sha2::Sha256;

use crate::deltaglider::memory_budget::MemoryPermit;
use crate::deltaglider::spool::{mib_ceil, Spool, SpoolFillError, SpoolReservation};
use crate::deltaglider::{
    DeltaGliderEngine, EngineError, KnownSha256, SpoolWait, BLOCKING_HASH_THRESHOLD,
};
use crate::storage::{StorageBackend, StorageError};

/// The hex SHA-256 and MD5 of a body. Only the code that saw the bytes
/// makes one (`BodyHasher`, `BodyHashes::compute` and the spool fill),
/// so a store that receives one does not hash the body again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyHashes {
    sha256: String,
    md5: String,
}

impl BodyHashes {
    /// Hash `data`; a `known` SHA-256 is used as is, so only the MD5 pass
    /// runs. CPU work: [`Self::compute`] moves a large body off the async
    /// runtime.
    pub(crate) fn of(data: &[u8], known: Option<KnownSha256>) -> Self {
        Self::with(data, known.into())
    }

    fn with(data: &[u8], sha256: Sha256Need) -> Self {
        let mut hasher = BodyHasher::for_need(sha256);
        hasher.update(data);
        hasher.finish()
    }

    /// The hashes of `data` as `sha256` asks, on a blocking thread when the
    /// body is above [`BLOCKING_HASH_THRESHOLD`]: a 16 MiB body takes about
    /// 10 ms per pass, which a tokio worker must not spend.
    pub(crate) async fn compute(data: &Bytes, sha256: Sha256Need) -> Result<Self, EngineError> {
        if data.len() <= BLOCKING_HASH_THRESHOLD {
            return Ok(Self::with(data, sha256));
        }
        let data = data.clone();
        tokio::task::spawn_blocking(move || Self::with(&data, sha256))
            .await
            .map_err(|e| EngineError::Storage(StorageError::Other(format!("hash task: {e}"))))
    }

    /// The lowercase hex SHA-256; empty when the ingest skipped it
    /// ([`Sha256Need::Skip`]: a multipart part, whose SHA-256 nothing
    /// reads). An empty value matches no digest, so a check fails closed.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The lowercase hex MD5.
    pub fn md5(&self) -> &str {
        &self.md5
    }

    /// `(sha256, md5)`.
    pub(crate) fn into_parts(self) -> (String, String) {
        (self.sha256, self.md5)
    }
}

/// The incremental form of [`BodyHashes::of`]: fed the chunks of a body in
/// order. The one place a body's SHA-256 and MD5 are computed together:
/// the ingest, the spool fill, `hash_spool_file` and the read of the relay
/// parts feed it. (The MD5 of one relay part and the `Content-MD5` of an
/// S3 file upload are single MD5 passes of their own.)
pub(crate) struct BodyHasher {
    sha256: Option<Sha256>,
    known: Option<KnownSha256>,
    md5: Md5,
}

impl BodyHasher {
    pub(crate) fn new(known: Option<KnownSha256>) -> Self {
        Self::for_need(known.into())
    }

    pub(crate) fn for_need(sha256: Sha256Need) -> Self {
        let (hasher, known) = match sha256 {
            Sha256Need::Known(k) => (None, Some(k)),
            Sha256Need::Compute => (Some(Sha256::new()), None),
            Sha256Need::Skip => (None, None),
        };
        Self {
            sha256: hasher,
            known,
            md5: Md5::new(),
        }
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        if let Some(h) = &mut self.sha256 {
            h.update(data);
        }
        self.md5.update(data);
    }

    pub(crate) fn finish(self) -> BodyHashes {
        BodyHashes {
            sha256: match (self.known, self.sha256) {
                (Some(k), _) => k.to_hex(),
                (None, Some(h)) => hex::encode(h.finalize()),
                (None, None) => String::new(),
            },
            md5: hex::encode(self.md5.finalize()),
        }
    }
}

/// A request body read once, with its hashes.
pub enum IngestedBody {
    /// At most the spool threshold, in one buffer; `permit` holds its
    /// share of the RAM budget until the body drops.
    Memory {
        data: Bytes,
        hashes: BodyHashes,
        permit: MemoryPermit,
    },
    /// In a spool file of `size` bytes. `storage`: the reservation the
    /// file was cut from; it also covers the temp files of the storage
    /// write of this body (`SpoolBudget`), so that write never waits.
    Spooled {
        spool: Spool,
        size: u64,
        hashes: BodyHashes,
        storage: SpoolReservation,
    },
}

impl std::fmt::Debug for IngestedBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let medium = match self {
            Self::Memory { .. } => "memory",
            Self::Spooled { .. } => "spool",
        };
        f.debug_struct("IngestedBody")
            .field("medium", &medium)
            .field("size", &self.size())
            .field("hashes", self.hashes())
            .finish()
    }
}

impl IngestedBody {
    /// The body's length in bytes.
    pub fn size(&self) -> u64 {
        match self {
            Self::Memory { data, .. } => data.len() as u64,
            Self::Spooled { size, .. } => *size,
        }
    }

    pub fn hashes(&self) -> &BodyHashes {
        match self {
            Self::Memory { hashes, .. } | Self::Spooled { hashes, .. } => hashes,
        }
    }

    /// A memory body of `data`, hashed, on an unbounded budget (tests that
    /// store a body without the HTTP path).
    #[cfg(test)]
    pub(crate) fn hashed_from_bytes(data: Bytes) -> Self {
        let permit = crate::deltaglider::memory_budget::MemoryBudget::unbounded()
            .try_reserve(data.len() as u64)
            .expect("an unbounded budget grants everything");
        Self::Memory {
            hashes: BodyHashes::of(&data, None),
            data,
            permit,
        }
    }

    /// A spooled body of `data` in `pool` (tests).
    #[cfg(test)]
    pub(crate) async fn spooled_for_test(
        pool: &crate::deltaglider::spool::SpoolDir,
        data: Bytes,
    ) -> Self {
        let len = data.len() as u64;
        let storage = pool.reserve_beside(0, len).await.unwrap();
        let spool = storage.file().unwrap();
        let mut stream = futures::stream::iter([Ok::<_, std::io::Error>(data)]);
        let fill = spool
            .fill_from_stream(&mut stream, len, Sha256Need::Compute)
            .await
            .unwrap();
        Self::Spooled {
            spool,
            size: fill.written,
            hashes: fill.hashes,
            storage,
        }
    }
}

/// Where a body is read to, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    /// Into memory: at most the threshold, and the RAM budget granted it.
    Memory,
    /// Into a spool file: larger than the threshold.
    SpoolOverThreshold,
    /// Into a spool file: small, but the RAM budget is full.
    SpoolMemoryBudget,
    /// Into a spool file that becomes a multipart relay part: a part is
    /// never held in memory, whatever its size.
    SpoolMultipartPart,
}

impl Medium {
    /// The `medium` and `reason` labels of
    /// `deltaglider_ingest_bodies_total`.
    pub fn labels(self) -> [&'static str; 2] {
        match self {
            Self::Memory => ["memory", "under_threshold"],
            Self::SpoolOverThreshold => ["spool", "over_threshold"],
            Self::SpoolMemoryBudget => ["spool", "memory_budget"],
            Self::SpoolMultipartPart => ["spool", "multipart_part"],
        }
    }
}

/// Pure: the medium of a body of `declared` bytes for `destination`. A
/// multipart part always goes to a spool file (it becomes a relay part).
/// `ram_granted`: the RAM budget gave an object body a permit (asked only
/// for one at most `threshold`).
pub fn medium(
    destination: Destination,
    declared: u64,
    threshold: u64,
    ram_granted: bool,
) -> Medium {
    if destination == Destination::MultipartPart {
        Medium::SpoolMultipartPart
    } else if declared > threshold {
        Medium::SpoolOverThreshold
    } else if ram_granted {
        Medium::Memory
    } else {
        Medium::SpoolMemoryBudget
    }
}

/// Pure: can the spool ever hold a body of `declared` bytes for an op
/// that holds `held_mib` already? Compared with the whole budget
/// (`max_bytes`) only: a backend's share clamps a reservation by design,
/// so refusing above it would make the largest PUT depend on the number
/// of backends. Counted in MiB, as the reservations are.
pub fn spool_admits(declared: u64, max_bytes: u64, held_mib: usize) -> bool {
    mib_ceil(declared) <= mib_ceil(max_bytes).max(1).saturating_sub(held_mib)
}

/// Pure: may a body of `plan.declared` bytes be read at all? Above the
/// request's limit, or above what the spool budget (`spool_max_bytes`)
/// could ever hold beside what the op holds, it is refused before a byte
/// is read. Every body is judged against the spool: one that finds no RAM
/// goes there too. `storage_extra`: the spool bytes that the storage write
/// of the body needs beside it (the ciphertext file of a bucket that the
/// proxy encrypts, `file_put_spool_bytes`), which the reservation covers
/// too; judged without it, a body that fits alone was admitted and its
/// reservation clamped below what the write then put on the disk.
pub fn admit_declared<E>(
    plan: &IngestPlan,
    spool_max_bytes: u64,
    storage_extra: u64,
) -> Result<(), IngestError<E>> {
    if plan.declared > plan.limit {
        return Err(IngestError::TooLarge {
            declared: plan.declared,
            limit: plan.limit,
        });
    }
    let needed = plan.declared.saturating_add(storage_extra);
    if !spool_admits(needed, spool_max_bytes, plan.held_spool_mib) {
        return Err(IngestError::SpoolTooSmall {
            declared: plan.declared,
            storage_extra,
            max_bytes: spool_max_bytes,
        });
    }
    Ok(())
}

/// What the ingest does about a body's SHA-256. MD5 is always computed:
/// it is the ETag of an object and of a multipart part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sha256Need {
    /// Proven by the caller (a payload hash that s3s verified against the
    /// bytes): used as is, not computed.
    Known(KnownSha256),
    /// Computed while the body is read.
    Compute,
    /// Not computed, because nothing reads it ([`BodyHashes::sha256`] is
    /// then empty).
    Skip,
}

impl From<Option<KnownSha256>> for Sha256Need {
    fn from(known: Option<KnownSha256>) -> Self {
        known.map_or(Self::Compute, Self::Known)
    }
}

impl Sha256Need {
    /// Pure: the SHA-256 work of a body. `verified`: the SHA-256 that s3s
    /// proved; `signed_hex_to_check`: the request signs a hex payload hash
    /// that the caller must compare with the body after it arrives.
    ///
    /// An object stores its SHA-256 in its metadata, so it always has
    /// one. A multipart part has its own: the store keeps only its MD5
    /// (the part ETag and the check of the relay file), and a part's
    /// `x-amz-checksum-sha256` is computed by the checksum plan. So a part
    /// computes it only to check a signed payload hash.
    pub fn decide(
        destination: Destination,
        verified: Option<KnownSha256>,
        signed_hex_to_check: bool,
    ) -> Self {
        match (verified, destination) {
            (Some(known), _) => Self::Known(known),
            (None, Destination::Object) => Self::Compute,
            (None, Destination::MultipartPart) if signed_hex_to_check => Self::Compute,
            (None, Destination::MultipartPart) => Self::Skip,
        }
    }
}

/// What [`ingest`] reads.
#[derive(Debug, Clone, Copy)]
pub struct IngestPlan {
    /// The length the request declares (`api::body_ingest::declared_len`); the
    /// reservation and the fill cap. With `strict_length: false`, an upper
    /// bound of the body.
    pub declared: u64,
    /// The largest body this request may store.
    pub limit: u64,
    /// The SHA-256 work ([`Sha256Need::decide`]).
    pub sha256: Sha256Need,
    /// Spool budget the op holds already (the other parts of a multipart
    /// upload); such an op never waits for more.
    pub held_spool_mib: usize,
    /// `true`: a body shorter than `declared` fails. `false`: `declared`
    /// is an upper bound (a form file), and a shorter body is the body.
    pub strict_length: bool,
    /// What the body becomes.
    pub destination: Destination,
}

/// What an ingested body becomes, which decides its medium and what its
/// spool reservation covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// An object, stored right after the ingest: memory or spool by size
    /// ([`medium`]), and the reservation also covers the temp files of its
    /// storage write.
    Object,
    /// A multipart part, kept as a relay file until the upload completes:
    /// always a spool file, and the reservation covers the file only (the
    /// completion reserves for its own storage write).
    MultipartPart,
}

/// Why [`ingest`] failed. Every variant leaves nothing behind.
#[derive(Debug)]
pub enum IngestError<E> {
    /// The body stream failed.
    Source(E),
    /// `declared` is above the request's limit (EntityTooLarge).
    TooLarge { declared: u64, limit: u64 },
    /// `declared`, with the `storage_extra` that its storage write needs
    /// (the encrypted copy), is above what the spool budget can ever hold
    /// (`DGP_SPOOL_MAX_BYTES`; EntityTooLarge).
    SpoolTooSmall {
        declared: u64,
        storage_extra: u64,
        max_bytes: u64,
    },
    /// The body is longer than `declared`.
    TooLong { declared: u64 },
    /// The body ended before `declared` bytes (IncompleteBody).
    Incomplete { declared: u64, received: u64 },
    /// No spool budget now (`Overloaded`, a 503 SlowDown) or a spool
    /// write failed.
    Engine(EngineError),
}

impl<E: std::fmt::Display> std::fmt::Display for IngestError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(e) => write!(f, "request body: {e}"),
            Self::TooLarge { declared, limit } => {
                write!(
                    f,
                    "a body of {declared} bytes is above the limit of {limit}"
                )
            }
            Self::SpoolTooSmall {
                declared,
                storage_extra: 0,
                max_bytes,
            } => write!(
                f,
                "a body of {declared} bytes does not fit the spool budget \
                 (DGP_SPOOL_MAX_BYTES = {max_bytes}, {})",
                super::spool::shared_budget_note()
            ),
            Self::SpoolTooSmall {
                declared,
                storage_extra,
                max_bytes,
            } => write!(
                f,
                "a body of {declared} bytes needs {} bytes of spool: the body and the \
                 {storage_extra} bytes of its encrypted copy (the bucket is encrypted by the \
                 proxy, so its uploads need about twice their size in spool space). That does \
                 not fit the spool budget (DGP_SPOOL_MAX_BYTES = {max_bytes}, {})",
                declared.saturating_add(*storage_extra),
                super::spool::shared_budget_note()
            ),
            Self::TooLong { declared } => {
                write!(f, "the body is longer than its declared {declared} bytes")
            }
            Self::Incomplete { declared, received } => write!(
                f,
                "the body ended after {received} of its declared {declared} bytes"
            ),
            Self::Engine(e) => e.fmt(f),
        }
    }
}

/// Read `stream` once, into memory or a spool file (see the module docs).
/// Callers that check a checksum observe the chunks by mapping `stream`.
pub async fn ingest<B, S, E>(
    engine: &DeltaGliderEngine<B>,
    bucket: &str,
    stream: S,
    plan: IngestPlan,
) -> Result<IngestedBody, IngestError<E>>
where
    B: StorageBackend,
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    // An object is stored right after: its reservation also covers the
    // temp files of its storage write. A part is stored at completion,
    // which reserves for its own write.
    let storage_extra = engine
        .storage_spool_extra(bucket, plan.destination, plan.declared)
        .await;
    admit_declared(&plan, engine.spool_max_bytes(), storage_extra)?;
    let threshold = engine.spool_threshold();
    let object = plan.destination == Destination::Object;
    let permit = (object && plan.declared <= threshold)
        .then(|| engine.memory_budget().try_reserve(plan.declared))
        .flatten();
    let medium = medium(plan.destination, plan.declared, threshold, permit.is_some());
    if let Some(m) = engine.metrics() {
        m.ingest_bodies_total
            .with_label_values(&medium.labels())
            .inc();
    }
    match permit.filter(|_| medium == Medium::Memory) {
        Some(permit) => into_memory(stream, plan, permit).await,
        None => {
            let reserve = Reserve {
                bytes: plan.declared.saturating_add(storage_extra),
                wait: if plan.declared > threshold {
                    SpoolWait::Queue
                } else {
                    SpoolWait::Now
                },
            };
            into_spool(engine, stream, plan, reserve).await
        }
    }
}

async fn into_memory<S, E>(
    mut stream: S,
    plan: IngestPlan,
    permit: MemoryPermit,
) -> Result<IngestedBody, IngestError<E>>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    // `declared` is at most the threshold, which the permit covers: one
    // allocation, never a doubling.
    let mut buf = BytesMut::with_capacity(plan.declared as usize);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(IngestError::Source)?;
        if (buf.len() + chunk.len()) as u64 > plan.declared {
            return Err(IngestError::TooLong {
                declared: plan.declared,
            });
        }
        buf.extend_from_slice(&chunk);
    }
    check_length(plan, buf.len() as u64)?;
    let data = buf.freeze();
    let hashes = BodyHashes::compute(&data, plan.sha256)
        .await
        .map_err(IngestError::Engine)?;
    Ok(IngestedBody::Memory {
        data,
        hashes,
        permit,
    })
}

/// The spool reservation of a body: its `bytes` (the body and the temp
/// files of its storage write), and whether it may `wait` for them.
struct Reserve {
    bytes: u64,
    wait: SpoolWait,
}

async fn into_spool<B, S, E>(
    engine: &DeltaGliderEngine<B>,
    mut stream: S,
    plan: IngestPlan,
    reserve: Reserve,
) -> Result<IngestedBody, IngestError<E>>
where
    B: StorageBackend,
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    // A large body waits for its reservation, before the body is polled
    // (an op that holds spool budget already never waits). A small body
    // takes free budget now or is refused: it never queues behind a large
    // reservation.
    let storage = engine
        .reserve_ingest_spool(reserve.bytes, plan.held_spool_mib, reserve.wait)
        .await
        .map_err(IngestError::Engine)?;
    if storage.reserved_mib() < mib_ceil(plan.declared) {
        return Err(IngestError::Engine(EngineError::Overloaded(
            "spool budget: the reservation does not cover the body; retry shortly".into(),
        )));
    }
    let spool = storage
        .file()
        .map_err(|e| IngestError::Engine(EngineError::Storage(e.into())))?;
    // Capped at the declared length, which the reservation covers.
    let fill = spool
        .fill_from_stream(&mut stream, plan.declared, plan.sha256)
        .await
        .map_err(|e| match e {
            SpoolFillError::Source(e) => IngestError::Source(e),
            SpoolFillError::Write(e) => IngestError::Engine(EngineError::Storage(e.into())),
            SpoolFillError::Overrun { .. } => IngestError::TooLong {
                declared: plan.declared,
            },
        })?;
    check_length(plan, fill.written)?;
    Ok(IngestedBody::Spooled {
        spool,
        size: fill.written,
        hashes: fill.hashes,
        storage,
    })
}

/// [`ingest`] of a body that is in memory already (tests that store a
/// body through the ingest): one chunk, of exactly its length.
#[cfg(test)]
pub(crate) async fn ingest_bytes<B: StorageBackend>(
    engine: &DeltaGliderEngine<B>,
    bucket: &str,
    data: Bytes,
    limit: u64,
    known_sha256: Option<KnownSha256>,
) -> Result<IngestedBody, IngestError<std::convert::Infallible>> {
    let plan = IngestPlan {
        declared: data.len() as u64,
        limit,
        sha256: known_sha256.into(),
        held_spool_mib: 0,
        strict_length: true,
        destination: Destination::Object,
    };
    let stream = futures::stream::iter([Ok(data)]);
    ingest(engine, bucket, stream, plan).await
}

/// A body shorter than declared fails, unless the plan allows it.
fn check_length<E>(plan: IngestPlan, received: u64) -> Result<(), IngestError<E>> {
    if plan.strict_length && received < plan.declared {
        return Err(IngestError::Incomplete {
            declared: plan.declared,
            received,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn medium_truth_table() {
        let object = Destination::Object;
        assert_eq!(medium(object, 0, 16, true), Medium::Memory);
        assert_eq!(medium(object, 16, 16, true), Medium::Memory);
        assert_eq!(medium(object, 16, 16, false), Medium::SpoolMemoryBudget);
        assert_eq!(medium(object, 17, 16, true), Medium::SpoolOverThreshold);
        assert_eq!(medium(object, 17, 16, false), Medium::SpoolOverThreshold);
        // A part is a relay file at every size.
        for (declared, granted) in [(0, true), (16, true), (16, false), (17, false)] {
            assert_eq!(
                medium(Destination::MultipartPart, declared, 16, granted),
                Medium::SpoolMultipartPart
            );
        }
        assert_eq!(Medium::Memory.labels(), ["memory", "under_threshold"]);
        assert_eq!(
            Medium::SpoolOverThreshold.labels(),
            ["spool", "over_threshold"]
        );
        assert_eq!(
            Medium::SpoolMemoryBudget.labels(),
            ["spool", "memory_budget"]
        );
        assert_eq!(
            Medium::SpoolMultipartPart.labels(),
            ["spool", "multipart_part"]
        );
    }

    #[test]
    fn spool_admits_compares_with_the_whole_budget_minus_held() {
        assert!(spool_admits(16 * MIB, 16 * MIB, 0));
        assert!(!spool_admits(16 * MIB + 1, 16 * MIB, 0));
        assert!(spool_admits(12 * MIB, 16 * MIB, 4));
        assert!(!spool_admits(12 * MIB + 1, 16 * MIB, 4));
        assert!(spool_admits(0, 16 * MIB, 16));
        assert!(!spool_admits(1, 16 * MIB, 16));
        assert!(!spool_admits(1, 16 * MIB, 99), "held above max");
    }

    /// An object always has a SHA-256 (it is stored metadata); a part
    /// computes one only to check a signed hex payload hash; a SHA-256
    /// that s3s proved is used as is.
    #[test]
    fn sha256_need_truth_table() {
        use Destination::{MultipartPart, Object};
        let proven = KnownSha256::of(b"proven");
        let cases = [
            (Object, Some(proven), false, Sha256Need::Known(proven)),
            (Object, Some(proven), true, Sha256Need::Known(proven)),
            (Object, None, false, Sha256Need::Compute),
            (Object, None, true, Sha256Need::Compute),
            (MultipartPart, Some(proven), true, Sha256Need::Known(proven)),
            (
                MultipartPart,
                Some(proven),
                false,
                Sha256Need::Known(proven),
            ),
            (MultipartPart, None, true, Sha256Need::Compute),
            (MultipartPart, None, false, Sha256Need::Skip),
        ];
        for (destination, verified, signed, want) in cases {
            assert_eq!(
                Sha256Need::decide(destination, verified, signed),
                want,
                "{destination:?} {verified:?} {signed}"
            );
        }
    }

    /// A skipped SHA-256 is empty (it matches no digest), and MD5 is
    /// computed whatever the SHA-256 work.
    #[test]
    fn a_skipped_sha256_is_empty_and_md5_stays() {
        let data = b"part bytes";
        let md5 = hex::encode(Md5::digest(data));
        let skipped = BodyHashes::with(data, Sha256Need::Skip);
        assert_eq!(skipped.sha256(), "");
        assert_eq!(skipped.md5(), md5);
        let computed = BodyHashes::with(data, Sha256Need::Compute);
        assert_eq!(computed.sha256(), hex::encode(Sha256::digest(data)));
        assert_eq!(computed.md5(), md5);
    }

    #[test]
    fn admit_declared_truth_table() {
        let plan = |declared, limit, held| IngestPlan {
            declared,
            limit,
            sha256: Sha256Need::Compute,
            held_spool_mib: held,
            strict_length: true,
            destination: Destination::Object,
        };
        let verdict = |p: IngestPlan, max, extra| match admit_declared::<()>(&p, max, extra) {
            Ok(()) => "ok",
            Err(IngestError::TooLarge { .. }) => "too-large",
            Err(IngestError::SpoolTooSmall { .. }) => "spool",
            Err(_) => "other",
        };
        // (plan, spool max, storage extra) -> verdict
        let cases = [
            (plan(MIB, 2 * MIB, 0), 16 * MIB, 0, "ok"),
            (plan(2 * MIB, 2 * MIB, 0), 16 * MIB, 0, "ok"),
            (plan(2 * MIB + 1, 2 * MIB, 0), 16 * MIB, 0, "too-large"),
            // The limit first: a body above both is TooLarge.
            (plan(32 * MIB, 2 * MIB, 0), 16 * MIB, 0, "too-large"),
            (plan(17 * MIB, 64 * MIB, 0), 16 * MIB, 0, "spool"),
            (plan(13 * MIB, 64 * MIB, 4), 16 * MIB, 0, "spool"),
            (plan(0, 0, 0), 16 * MIB, 0, "ok"),
            // An encrypted bucket: the body and its ciphertext file must
            // fit together. 10 MiB alone fits 16; with its copy it does not.
            (
                plan(10 * MIB, 64 * MIB, 0),
                16 * MIB,
                10 * MIB + 4096,
                "spool",
            ),
            (plan(8 * MIB, 64 * MIB, 0), 16 * MIB, 8 * MIB, "ok"),
            (plan(8 * MIB, 64 * MIB, 0), 16 * MIB, 8 * MIB + 1, "spool"),
            (plan(6 * MIB, 64 * MIB, 4), 16 * MIB, 6 * MIB, "ok"),
            (plan(6 * MIB, 64 * MIB, 5), 16 * MIB, 6 * MIB, "spool"),
            // The limit still judges the body alone.
            (plan(2 * MIB, 2 * MIB, 0), 16 * MIB, 2 * MIB, "ok"),
        ];
        for (p, max, extra, want) in cases {
            assert_eq!(
                verdict(p, max, extra),
                want,
                "{p:?} max={max} extra={extra}"
            );
        }
    }

    proptest! {
        /// A body the spool admits always fits the reservation the spool
        /// grants it (the clamp of `SpoolDir::reserve_beside`), so the fill
        /// cap never exceeds the reservation.
        #[test]
        fn an_admitted_body_fits_its_clamped_reservation(
            declared in 0u64..64 * MIB,
            extra in 0u64..64 * MIB,
            max in 1u64..64 * MIB,
            held in 0usize..80,
        ) {
            if spool_admits(declared, max, held) {
                let max_mib = mib_ceil(max).max(1);
                let reserved = mib_ceil(declared + extra).max(1).min(max_mib - held);
                prop_assert!(reserved >= mib_ceil(declared));
            }
        }

        #[test]
        fn the_incremental_hasher_matches_the_one_shot(
            data in proptest::collection::vec(any::<u8>(), 0..2000),
            cut in any::<usize>(),
            known in any::<bool>(),
        ) {
            let known = known.then(|| KnownSha256::of(b"proof"));
            let cut = if data.is_empty() { 0 } else { cut % data.len() };
            let mut h = BodyHasher::new(known);
            h.update(&data[..cut]);
            h.update(&data[cut..]);
            let one = BodyHashes::of(&data, known);
            prop_assert_eq!(h.finish(), one.clone());
            prop_assert_eq!(one.md5(), hex::encode(Md5::digest(&data)));
            let sha = known.map_or_else(|| hex::encode(Sha256::digest(&data)), |k| k.to_hex());
            prop_assert_eq!(one.sha256(), sha);
        }
    }
}

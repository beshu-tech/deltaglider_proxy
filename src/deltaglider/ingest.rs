// SPDX-License-Identifier: BUSL-1.1

//! Ingest: a request body read ONCE, into memory or into a spool file,
//! with its hashes, before any lock is taken or any byte is stored.
//!
//! The medium is decided before the first byte, from the length the client
//! declared ([`medium`]):
//!
//! - a body of at most the spool threshold (`DGP_SPOOL_THRESHOLD_BYTES`)
//!   is read into one buffer of its declared size when the RAM budget
//!   (`DGP_INGEST_MEMORY_MAX_BYTES`, [`MemoryBudget`]) has room for it;
//! - a larger body takes a spool reservation, and may wait for it (up to
//!   `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS`) before the body is polled, so a
//!   client that sent `Expect: 100-continue` has sent nothing yet;
//! - a small body that finds no RAM takes free spool budget without
//!   waiting (it never queues behind a large reservation), else the request
//!   is a retryable 503 SlowDown.
//!
//! The spool reservation covers the body AND the temp files that the
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
use crate::deltaglider::{DeltaGliderEngine, EngineError, KnownSha256, BLOCKING_HASH_THRESHOLD};
use crate::storage::{StorageBackend, StorageError};

/// The hex SHA-256 and MD5 of a body. Only the code that saw the bytes
/// makes one ([`BodyHasher`], [`BodyHashes::compute`] and the spool fill),
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
        let mut hasher = BodyHasher::new(known);
        hasher.update(data);
        hasher.finish()
    }

    /// [`Self::of`], on a blocking thread when the body is above
    /// [`BLOCKING_HASH_THRESHOLD`]: a 16 MiB body takes about 10 ms per
    /// pass, which a tokio worker must not spend.
    pub(crate) async fn compute(
        data: &Bytes,
        known: Option<KnownSha256>,
    ) -> Result<Self, EngineError> {
        if data.len() <= BLOCKING_HASH_THRESHOLD {
            return Ok(Self::of(data, known));
        }
        let data = data.clone();
        tokio::task::spawn_blocking(move || Self::of(&data, known))
            .await
            .map_err(|e| EngineError::Storage(StorageError::Other(format!("hash task: {e}"))))
    }

    /// The lowercase hex SHA-256.
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
/// order. The one place a body's SHA-256 and MD5 are computed.
pub(crate) struct BodyHasher {
    sha256: Option<Sha256>,
    known: Option<KnownSha256>,
    md5: Md5,
}

impl BodyHasher {
    pub(crate) fn new(known: Option<KnownSha256>) -> Self {
        Self {
            sha256: known.is_none().then(Sha256::new),
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
                (None, None) => unreachable!("a hasher without a known SHA-256 computes one"),
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
            .fill_from_stream(&mut stream, len, None)
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
}

impl Medium {
    /// The `medium` and `reason` labels of
    /// `deltaglider_ingest_bodies_total`.
    pub fn labels(self) -> [&'static str; 2] {
        match self {
            Self::Memory => ["memory", "under_threshold"],
            Self::SpoolOverThreshold => ["spool", "over_threshold"],
            Self::SpoolMemoryBudget => ["spool", "memory_budget"],
        }
    }
}

/// Pure: the medium of a body of `declared` bytes. `ram_granted`: the RAM
/// budget gave it a permit (asked only for a body at most `threshold`).
pub fn medium(declared: u64, threshold: u64, ram_granted: bool) -> Medium {
    if declared > threshold {
        Medium::SpoolOverThreshold
    } else if ram_granted {
        Medium::Memory
    } else {
        Medium::SpoolMemoryBudget
    }
}

/// The request names no body length: neither `Content-Length` nor, for
/// an aws-chunked body, `x-amz-decoded-content-length`. S3 answers
/// `411 MissingContentLength`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingContentLength;

/// Pure: the length of the body that the request declares: the decoded
/// length of an aws-chunked body, else `Content-Length`.
pub fn declared_body_len(headers: &axum::http::HeaderMap) -> Result<u64, MissingContentLength> {
    if crate::api::aws_chunked::is_aws_chunked(headers) {
        return crate::api::aws_chunked::get_decoded_content_length(headers)
            .map(|n| n as u64)
            .ok_or(MissingContentLength);
    }
    headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .ok_or(MissingContentLength)
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
/// goes there too.
pub fn admit_declared<E>(plan: &IngestPlan, spool_max_bytes: u64) -> Result<(), IngestError<E>> {
    if plan.declared > plan.limit {
        return Err(IngestError::TooLarge {
            declared: plan.declared,
            limit: plan.limit,
        });
    }
    if !spool_admits(plan.declared, spool_max_bytes, plan.held_spool_mib) {
        return Err(IngestError::SpoolTooSmall {
            declared: plan.declared,
            max_bytes: spool_max_bytes,
        });
    }
    Ok(())
}

/// What [`ingest`] reads.
#[derive(Debug, Clone, Copy)]
pub struct IngestPlan {
    /// The length the request declares ([`declared_body_len`]); the
    /// reservation and the fill cap.
    pub declared: u64,
    /// The largest body this request may store.
    pub limit: u64,
    /// The body's SHA-256, proven by the caller (a verified SigV4 payload
    /// hash): only MD5 is computed.
    pub known_sha256: Option<KnownSha256>,
    /// Spool budget the op holds already (the other parts of a multipart
    /// upload); such an op never waits for more.
    pub held_spool_mib: usize,
    /// `true`: a body shorter than `declared` fails. `false`: `declared`
    /// is an upper bound (a form file), and a shorter body is the body.
    pub strict_length: bool,
}

/// Why [`ingest`] failed. Every variant leaves nothing behind.
#[derive(Debug)]
pub enum IngestError<E> {
    /// The body stream failed.
    Source(E),
    /// `declared` is above the request's limit (EntityTooLarge).
    TooLarge { declared: u64, limit: u64 },
    /// `declared` is above what the spool budget can ever hold
    /// (`DGP_SPOOL_MAX_BYTES`; EntityTooLarge).
    SpoolTooSmall { declared: u64, max_bytes: u64 },
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
                max_bytes,
            } => write!(
                f,
                "a body of {declared} bytes does not fit the spool budget \
                 (DGP_SPOOL_MAX_BYTES = {max_bytes})"
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
    admit_declared(&plan, engine.spool_max_bytes())?;
    let threshold = engine.spool_threshold();
    let permit = (plan.declared <= threshold)
        .then(|| engine.memory_budget().try_reserve(plan.declared))
        .flatten();
    let medium = medium(plan.declared, threshold, permit.is_some());
    if let Some(m) = engine.metrics() {
        m.ingest_bodies_total
            .with_label_values(&medium.labels())
            .inc();
    }
    match permit.filter(|_| medium == Medium::Memory) {
        Some(permit) => into_memory(stream, plan, permit).await,
        None => into_spool(engine, bucket, stream, plan, medium).await,
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
    let hashes = BodyHashes::compute(&data, plan.known_sha256)
        .await
        .map_err(IngestError::Engine)?;
    Ok(IngestedBody::Memory {
        data,
        hashes,
        permit,
    })
}

async fn into_spool<B, S, E>(
    engine: &DeltaGliderEngine<B>,
    bucket: &str,
    mut stream: S,
    plan: IngestPlan,
    medium: Medium,
) -> Result<IngestedBody, IngestError<E>>
where
    B: StorageBackend,
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    // A large body waits for its reservation, before the body is polled.
    // A small body that found no RAM takes free budget now or is refused:
    // it never queues behind a large reservation.
    let wait = medium == Medium::SpoolOverThreshold;
    let storage = engine
        .reserve_ingest_spool(bucket, plan.declared, plan.held_spool_mib, wait)
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
        .fill_from_stream(&mut stream, plan.declared, plan.known_sha256)
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

/// [`ingest`] of a body that is in memory already (a form POST's file, a
/// small copy source): one chunk, of exactly its length.
pub async fn ingest_bytes<B: StorageBackend>(
    engine: &DeltaGliderEngine<B>,
    bucket: &str,
    data: Bytes,
    limit: u64,
    known_sha256: Option<KnownSha256>,
) -> Result<IngestedBody, IngestError<std::convert::Infallible>> {
    let plan = IngestPlan {
        declared: data.len() as u64,
        limit,
        known_sha256,
        held_spool_mib: 0,
        strict_length: true,
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
        assert_eq!(medium(0, 16, true), Medium::Memory);
        assert_eq!(medium(16, 16, true), Medium::Memory);
        assert_eq!(medium(16, 16, false), Medium::SpoolMemoryBudget);
        assert_eq!(medium(17, 16, true), Medium::SpoolOverThreshold);
        assert_eq!(medium(17, 16, false), Medium::SpoolOverThreshold);
        assert_eq!(Medium::Memory.labels(), ["memory", "under_threshold"]);
        assert_eq!(
            Medium::SpoolOverThreshold.labels(),
            ["spool", "over_threshold"]
        );
        assert_eq!(
            Medium::SpoolMemoryBudget.labels(),
            ["spool", "memory_budget"]
        );
    }

    fn headers(pairs: &[(&'static str, &str)]) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn declared_length_truth_table() {
        let chunked = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";
        for (pairs, want) in [
            (vec![("content-length", "12")], Ok(12)),
            (vec![("content-length", "0")], Ok(0)),
            (vec![], Err(MissingContentLength)),
            (vec![("content-length", "x")], Err(MissingContentLength)),
            (vec![("content-length", "-1")], Err(MissingContentLength)),
            // aws-chunked: the decoded length, never the framed one.
            (
                vec![
                    ("x-amz-content-sha256", chunked),
                    ("content-length", "99"),
                    ("x-amz-decoded-content-length", "11"),
                ],
                Ok(11),
            ),
            (
                vec![("x-amz-content-sha256", chunked), ("content-length", "99")],
                Err(MissingContentLength),
            ),
            (
                vec![
                    ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
                    ("content-length", "5"),
                ],
                Ok(5),
            ),
        ] {
            assert_eq!(declared_body_len(&headers(&pairs)), want, "{pairs:?}");
        }
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

    #[test]
    fn admit_declared_truth_table() {
        let plan = |declared, limit, held| IngestPlan {
            declared,
            limit,
            known_sha256: None,
            held_spool_mib: held,
            strict_length: true,
        };
        let verdict = |p: IngestPlan, max| match admit_declared::<()>(&p, max) {
            Ok(()) => "ok",
            Err(IngestError::TooLarge { .. }) => "too-large",
            Err(IngestError::SpoolTooSmall { .. }) => "spool",
            Err(_) => "other",
        };
        assert_eq!(verdict(plan(MIB, 2 * MIB, 0), 16 * MIB), "ok");
        assert_eq!(verdict(plan(2 * MIB, 2 * MIB, 0), 16 * MIB), "ok");
        assert_eq!(
            verdict(plan(2 * MIB + 1, 2 * MIB, 0), 16 * MIB),
            "too-large"
        );
        // The limit first: a body above both is TooLarge.
        assert_eq!(verdict(plan(32 * MIB, 2 * MIB, 0), 16 * MIB), "too-large");
        assert_eq!(verdict(plan(17 * MIB, 64 * MIB, 0), 16 * MIB), "spool");
        assert_eq!(verdict(plan(13 * MIB, 64 * MIB, 4), 16 * MIB), "spool");
        assert_eq!(verdict(plan(0, 0, 0), 16 * MIB), "ok", "an empty body");
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

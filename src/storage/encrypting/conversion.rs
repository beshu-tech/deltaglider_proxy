// SPDX-License-Identifier: BUSL-1.1

//! The conversion marker of a bucket that starts to encrypt
//! (`docs/plan/encryption-conversion-marker.md`): a sealed object in the
//! bucket that names the window in which the bucket still serves its
//! plaintext history while the re-encrypt job converts it.
//!
//! # Body
//!
//! ```text
//! "dgc1." hex(fp) "." b64url( nonce (12) | AES-256-GCM-SIV(Kconv, fields, AD) )
//! fields = i64be(cutoff) | i64be(deadline) | run (16)
//! AD     = "dg-conversion" | format version | lp(storage identity)
//! ```
//!
//! `fp` (the key fingerprint) is in clear: it selects the ring entry, so the
//! marker opens with ANY key of the bucket's keyring (a key rotation keeps
//! the window). Kconv is in `v2.rs`. The storage identity is
//! `backend \0 real bucket`, so a marker copied to a bucket of the same name
//! on another backend does not open. A marker that does not open, that
//! names another identity, or whose deadline is past is a CLOSED window: a
//! backend writer can close a window (availability), never open one.
//!
//! # Writes
//!
//! Create with `If-None-Match: *`. Replace (deadline extension) and remove
//! (close) go BY IDENTITY: read, open, check the run id, then `If-Match`
//! the version just read; a lost condition reads again (bounded), and is
//! never taken for "gone".
//!
//! Pure, except the users of `MarkerIo`, which take the backend I/O as a
//! parameter.

use super::seal::Keyring;
use super::v2::{SiteKey, FP_LEN};
use crate::storage::{ClaimOutcome, ConversionMarker, MarkerRead, MarkerWrite, StorageError};
use base64::Engine as _;
use rand::RngCore;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The format version: in the body prefix and in the AD.
pub const FORMAT_VERSION: u8 = 1;

/// The body prefix of this format.
const BODY_PREFIX: &str = "dgc1.";

/// The AD label.
const AD_LABEL: &[u8] = b"dg-conversion";

/// Sealed fields: cutoff, deadline, run.
const FIELDS_LEN: usize = 8 + 8 + 16;

/// How many times an identity write reads again after a lost condition.
pub const RETRIES: usize = 3;

/// The longest window that a conversion may ask for.
pub const MAX_WINDOW_DAYS: u32 = 365;

/// The random id of one conversion run. Not a job id: job ids are
/// node-local, the run id is shared through the marker.
pub type RunId = [u8; 16];

/// A run id as text (hex).
pub fn run_label(run: &RunId) -> String {
    hex::encode(run)
}

/// A fresh random run id.
pub fn fresh_run() -> RunId {
    let mut run = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut run);
    run
}

/// The sealed fields of a marker. Times are unix seconds on the backend's
/// clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fields {
    /// Unsealed objects written before this time are history.
    pub cutoff: i64,
    /// The window is closed from this time on.
    pub deadline: i64,
    pub run: RunId,
}

/// The conversion window of a bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Window {
    /// No valid marker: an unsealed object is refused.
    Closed,
    /// A valid marker whose deadline is in the future.
    Open {
        cutoff: i64,
        deadline: i64,
        run: RunId,
        /// The marker's version (S3 ETag), for a write by identity.
        version: String,
    },
}

impl Window {
    /// This window on the backend clock `now`: closed once the deadline
    /// is reached.
    pub fn at(self, now: i64) -> Window {
        match self {
            Window::Open { deadline, .. } if now >= deadline => Window::Closed,
            w => w,
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self, Window::Open { .. })
    }
}

/// `b64url` without padding.
fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// The AD of a marker of `identity` (see the module doc).
fn ad(identity: &str) -> Vec<u8> {
    let mut ad = Vec::with_capacity(AD_LABEL.len() + 5 + identity.len());
    ad.extend_from_slice(AD_LABEL);
    ad.push(FORMAT_VERSION);
    ad.extend_from_slice(&(identity.len() as u32).to_be_bytes());
    ad.extend_from_slice(identity.as_bytes());
    ad
}

/// The marker body of `fields` for the storage `identity`, sealed with
/// `site`.
pub fn seal(site: &SiteKey, identity: &str, fields: &Fields) -> Result<Vec<u8>, StorageError> {
    let mut pt = [0u8; FIELDS_LEN];
    pt[..8].copy_from_slice(&fields.cutoff.to_be_bytes());
    pt[8..16].copy_from_slice(&fields.deadline.to_be_bytes());
    pt[16..].copy_from_slice(&fields.run);
    let sealed = site.seal_conversion(&ad(identity), &pt)?;
    Ok(format!(
        "{BODY_PREFIX}{}.{}",
        hex::encode(site.fingerprint()),
        b64().encode(sealed)
    )
    .into_bytes())
}

/// The fields of a marker body for the storage `identity`, opened with the
/// key of `ring` that its fingerprint names. `None`: the body is not a
/// marker of this format, names a key that the ring lacks, does not open
/// (forged, edited, another identity), or names a deadline that is not
/// after its cutoff. The deadline is NOT checked against a clock here.
pub fn open_fields(body: &[u8], ring: &Keyring, identity: &str) -> Option<Fields> {
    let text = std::str::from_utf8(body).ok()?;
    let (fp_hex, sealed) = text.strip_prefix(BODY_PREFIX)?.split_once('.')?;
    let mut fp = [0u8; FP_LEN];
    hex::decode_to_slice(fp_hex, &mut fp).ok()?;
    // One spelling only: an uppercase copy is an edit.
    if fp_hex != hex::encode(fp) {
        return None;
    }
    let sealed = b64().decode(sealed).ok()?;
    let entry = ring.get(&fp)?;
    let pt = entry.site.open_conversion(&ad(identity), &sealed)?;
    if pt.len() != FIELDS_LEN {
        return None;
    }
    let fields = Fields {
        cutoff: i64::from_be_bytes(pt[..8].try_into().ok()?),
        deadline: i64::from_be_bytes(pt[8..16].try_into().ok()?),
        run: pt[16..].try_into().ok()?,
    };
    (fields.deadline > fields.cutoff).then_some(fields)
}

/// THE window of a stored marker on the backend clock `now`: `Open` only
/// for a marker that opens with `ring` at `identity` and whose deadline is
/// after `now`; anything else is `Closed`.
pub fn open(marker: &ConversionMarker, ring: &Keyring, identity: &str, now: i64) -> Window {
    match open_fields(&marker.content, ring, identity) {
        Some(f) => Window::Open {
            cutoff: f.cutoff,
            deadline: f.deadline,
            run: f.run,
            version: marker.version.clone(),
        }
        .at(now),
        None => Window::Closed,
    }
}

/// The backend I/O of one bucket's marker.
pub(crate) trait MarkerIo {
    fn read(&self) -> impl std::future::Future<Output = Result<MarkerRead, StorageError>> + Send;
    fn write(
        &self,
        op: MarkerWrite<'_>,
    ) -> impl std::future::Future<Output = Result<ClaimOutcome, StorageError>> + Send;
}

/// The marker of `bucket` on a raw backend.
pub(crate) struct OnBackend<'a, B> {
    pub backend: &'a B,
    pub bucket: &'a str,
}

impl<B: crate::storage::StorageBackend + Sync> MarkerIo for OnBackend<'_, B> {
    async fn read(&self) -> Result<MarkerRead, StorageError> {
        self.backend.read_conversion_marker(self.bucket).await
    }
    async fn write(&self, op: MarkerWrite<'_>) -> Result<ClaimOutcome, StorageError> {
        self.backend.write_conversion_marker(self.bucket, op).await
    }
}

/// What opening a conversion found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenOutcome {
    /// This call wrote a new marker.
    Created(Fields, String),
    /// A valid marker was there already (another node, or an earlier
    /// apply): its window is kept, cutoff included.
    Reused(Fields, String),
    /// A marker of this identity is there, but its deadline is past: only
    /// a resume (extension by identity) may reopen it, with its original
    /// cutoff.
    Expired(Fields),
    /// A marker is there that this proxy cannot open (another key, another
    /// format, edited): this proxy never overwrites it.
    Foreign,
}

/// Open a conversion of the bucket behind `io`: reuse a valid marker, or
/// create one with `cutoff = now` on the backend clock and
/// `deadline = cutoff + window_secs`. Lost creates read again.
pub(crate) async fn open_or_reuse(
    io: &impl MarkerIo,
    ring: &Keyring,
    site: &SiteKey,
    identity: &str,
    window_secs: i64,
) -> Result<OpenOutcome, StorageError> {
    for _ in 0..RETRIES {
        let read = io.read().await?;
        let now = read.server_now.ok_or_else(|| {
            StorageError::Other(
                "the backend did not name its time (no Date header), so the proxy cannot set \
                 the conversion cutoff"
                    .into(),
            )
        })?;
        if let Some(m) = read.marker {
            return Ok(match open_fields(&m.content, ring, identity) {
                Some(f) if now < f.deadline => OpenOutcome::Reused(f, m.version),
                Some(f) => OpenOutcome::Expired(f),
                None => OpenOutcome::Foreign,
            });
        }
        let fields = Fields {
            cutoff: now,
            deadline: now.saturating_add(window_secs),
            run: fresh_run(),
        };
        let body = seal(site, identity, &fields)?;
        match io.write(MarkerWrite::Create(&body)).await? {
            ClaimOutcome::Written => {
                // The version a later identity write names: read it back.
                let version = match io.read().await?.marker {
                    Some(m) if m.content == body => m.version,
                    _ => String::new(),
                };
                return Ok(OpenOutcome::Created(fields, version));
            }
            ClaimOutcome::Lost => continue,
        }
    }
    Err(lost_three_times())
}

/// The result of a write by identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByIdentity {
    /// Written: the new fields (`None` after a remove).
    Done(Option<Fields>, String),
    /// No marker is there.
    Absent,
    /// The marker is not this run's (another run, or it does not open):
    /// never treated as gone.
    NotOurs,
    /// The marker changed under every attempt.
    Blocked,
}

/// Read the marker, check that it is `run`'s, and write `change(fields)`
/// (`Some(fields)`: replace with them; `None`: remove) on the version just
/// read. A lost condition reads again, up to [`RETRIES`] times.
pub(crate) async fn by_identity(
    io: &impl MarkerIo,
    ring: &Keyring,
    site: Option<&SiteKey>,
    identity: &str,
    run: &RunId,
    change: impl Fn(&Fields) -> Option<Fields>,
) -> Result<ByIdentity, StorageError> {
    for _ in 0..RETRIES {
        let Some(marker) = io.read().await?.marker else {
            return Ok(ByIdentity::Absent);
        };
        let Some(fields) = open_fields(&marker.content, ring, identity) else {
            return Ok(ByIdentity::NotOurs);
        };
        if fields.run != *run {
            return Ok(ByIdentity::NotOurs);
        }
        let next = change(&fields);
        let body =
            match (&next, site) {
                (Some(f), Some(site)) => Some(seal(site, identity, f)?),
                (Some(_), None) => return Err(StorageError::Encryption(
                    "the conversion marker cannot be rewritten without a key that encrypts the \
                     bucket"
                        .into(),
                )),
                (None, _) => None,
            };
        let op = match &body {
            Some(b) => MarkerWrite::Replace {
                body: b,
                version: &marker.version,
            },
            None => MarkerWrite::Remove {
                version: &marker.version,
            },
        };
        match io.write(op).await? {
            ClaimOutcome::Written => {
                let version = match (&body, next) {
                    (Some(b), Some(_)) => match io.read().await?.marker {
                        Some(m) if &m.content == b => m.version,
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                return Ok(ByIdentity::Done(next, version));
            }
            ClaimOutcome::Lost => continue,
        }
    }
    Ok(ByIdentity::Blocked)
}

fn lost_three_times() -> StorageError {
    StorageError::Throttled(format!(
        "the conversion marker changed {RETRIES} times while this proxy wrote it"
    ))
}

// ── Cache ──

/// How long a read window stays cached.
pub const TTL: Duration = Duration::from_secs(60);

/// How long a failed read is cached as `Closed`.
pub const FAILED_TTL: Duration = Duration::from_secs(5);

/// The shortest time between two re-reads that a refusal forces.
pub const REFUSAL_REFRESH: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Slot {
    state: parking_lot::Mutex<SlotState>,
    /// One marker read per bucket at a time.
    flight: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default)]
struct SlotState {
    /// The window, when it was read, and how long it stays.
    entry: Option<(Window, Instant, Duration)>,
    /// The backend clock minus the local clock at the read (seconds).
    skew: i64,
    last_refusal_refresh: Option<Instant>,
}

/// The windows of one wrapper's buckets: read at most once per [`TTL`],
/// with the deadline checked against the backend clock on every use.
#[derive(Default)]
pub struct WindowCache {
    slots: parking_lot::Mutex<HashMap<String, Arc<Slot>>>,
}

impl WindowCache {
    fn slot(&self, bucket: &str) -> Arc<Slot> {
        self.slots
            .lock()
            .entry(bucket.to_string())
            .or_default()
            .clone()
    }

    /// The single-flight lock of `bucket`'s marker reads.
    pub fn flight(&self, bucket: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.slot(bucket).flight.clone()
    }

    /// The cached window of `bucket` at local time `now` (wall clock
    /// `wall`, unix seconds), or `None` when it must be read.
    pub fn cached(&self, bucket: &str, now: Instant, wall: i64) -> Option<Window> {
        let slot = self.slot(bucket);
        let st = slot.state.lock();
        let (window, at, ttl) = st.entry.as_ref()?;
        (now.saturating_duration_since(*at) < *ttl)
            .then(|| window.clone().at(wall.saturating_add(st.skew)))
    }

    /// Record the window read at `now` (wall clock `wall`) with the
    /// backend's time `server_now`, kept for `ttl`.
    pub fn store(
        &self,
        bucket: &str,
        window: Window,
        now: Instant,
        wall: i64,
        server_now: Option<i64>,
        ttl: Duration,
    ) {
        let slot = self.slot(bucket);
        let mut st = slot.state.lock();
        if let Some(server) = server_now {
            st.skew = server.saturating_sub(wall);
        }
        st.entry = Some((window, now, ttl));
    }

    /// May a refused plaintext read of `bucket` read the marker again now?
    /// Yes when the cached window is not open and the last such re-read is
    /// [`REFUSAL_REFRESH`] old; a `true` records this one.
    pub fn take_refusal_refresh(&self, bucket: &str, now: Instant) -> bool {
        let slot = self.slot(bucket);
        let mut st = slot.state.lock();
        let open = st.entry.as_ref().is_some_and(|(w, _, _)| w.is_open());
        let due = st
            .last_refusal_refresh
            .is_none_or(|t| now.saturating_duration_since(t) >= REFUSAL_REFRESH);
        if open || !due {
            return false;
        }
        st.last_refusal_refresh = Some(now);
        true
    }
}

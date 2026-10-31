// SPDX-License-Identifier: BUSL-1.1

//! Listing-token misses of a sealed object on an S3 backend.
//!
//! A LIST shows the logical size and ETag of a sealed object from its
//! listing token (a `!!2` marker, see `storage::listing_facts`). An object
//! can have no token: its marker write failed, its marker key is too long
//! for S3 (a stored key longer than about 900 bytes), or it was stored
//! before tokens existed. A client LIST sends no HEAD per key (issue #82),
//! so such an entry used to list its stored size: the ciphertext size, or
//! for a delta the size of the delta (46 bytes for a 4 GB object).
//!
//! What a LIST does now, per miss ([`decide_miss`]):
//!
//! * A passthrough in a bucket that serves no plaintext is a DGE2 body, so
//!   its stored size gives its exact plaintext size
//!   ([`v2::plaintext_len_of`]) with no request. Its ETag stays the stored
//!   one until the token is back.
//! * A delta gets one HEAD on the LIST's path, at most
//!   [`INLINE_DELTA_HEADS`] per page: its plaintext size is not a function
//!   of the stored size. The HEAD gives size and ETag, and the token is
//!   kept (`StorageBackend::keep_listing_token`) so the next LIST needs no
//!   HEAD.
//! * A delta past that budget, and the ETag of an inverted passthrough,
//!   are healed in the background: one HEAD keeps the token, bounded as
//!   the S3 facts healer is (`storage::s3::heal::admit`).
//! * A passthrough in a bucket that serves plaintext (it may be plaintext)
//!   and a size that no DGE2 body has (it is not a ciphertext) list the
//!   stored size, with no request.
//!
//! Every miss is counted in [`LISTING_TOKEN_MISSES`].

use super::{listing_facts_of, seal, v2, EncryptionConfig};
use crate::storage::list_size_cache::bare_etag;
use crate::storage::s3::heal::{admit, Skip};
use crate::storage::traits::{ListedToken, StorageBackend, StorageError, StoredObject};
use crate::types::FileMetadata;
use arc_swap::ArcSwap;
use moka::sync::Cache;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Delta misses one listing page HEADs before it answers. The rest list
/// their stored size once and are healed in the background.
pub(super) const INLINE_DELTA_HEADS: usize = 8;
/// Background HEADs in flight per wrapper.
const HEAL_CONCURRENCY: usize = 4;
/// How long one stored object is not HEADed again.
const TRIED_TTL: Duration = Duration::from_secs(3600);
/// How long nothing is HEADed after a HEAD failed.
const PAUSE: Duration = Duration::from_secs(60);

/// Listed entries of an encrypted S3 backend without a listing token, by
/// what the LIST showed: `passthrough_inverted` (exact size from the
/// stored size, stored ETag), `delta_headed` (a HEAD on the LIST's path
/// gave size and ETag), `unresolved` (the stored size). `healed` counts the
/// background HEADs that kept a missing token.
pub static LISTING_TOKEN_MISSES: std::sync::LazyLock<prometheus::IntCounterVec> =
    std::sync::LazyLock::new(|| {
        prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "deltaglider_listing_token_misses_total",
                "Listed encrypted objects without a listing token, by how the LIST resolved them",
            ),
            &["kind"],
        )
        .expect("valid metric")
    });

pub(super) const PASSTHROUGH_INVERTED: &str = "passthrough_inverted";
pub(super) const DELTA_HEADED: &str = "delta_headed";
pub(super) const UNRESOLVED: &str = "unresolved";
pub(super) const HEALED: &str = "healed";

pub(super) fn count(kind: &str) {
    LISTING_TOKEN_MISSES.with_label_values(&[kind]).inc();
}

/// What a LIST does with one entry that has no listing token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MissDecision {
    /// List this plaintext size (exact; the ETag stays the stored one
    /// until a background HEAD keeps the token).
    Invert(u64),
    /// HEAD the stored object now.
    HeadNow,
    /// List the stored size. With `heal`, a background HEAD keeps the
    /// token for the next LIST.
    Keep { heal: bool },
}

/// THE listed-size decision for an entry without a listing token. Pure.
///
/// * `stored_size`: the size the LIST reported.
/// * `plaintext_ok`: the bucket's read rule serves unsealed objects (it
///   writes plaintext, `plaintext_reads: serve`, or its conversion window
///   is open: the wrapper treats an open window the same way). Then
///   a listed passthrough may be plaintext, and its stored size is its
///   size: inverting it would be wrong, and a HEAD per plaintext object
///   (a bucket that a re-encrypt job converts) would be a HEAD storm. Where
///   the rule refuses plaintext, every readable passthrough is a DGE2 body.
/// * `may_head`: the page has HEAD budget left and this stored object was
///   not tried a short time ago.
///
/// A size that no DGE2 body has is not a ciphertext: kept as stored, and
/// never HEADed (a plain object stored before the key).
pub(super) fn decide_miss(
    stored_size: u64,
    is_delta: bool,
    plaintext_ok: bool,
    may_head: bool,
) -> MissDecision {
    if is_delta {
        return if may_head {
            MissDecision::HeadNow
        } else {
            MissDecision::Keep { heal: true }
        };
    }
    if plaintext_ok {
        return MissDecision::Keep { heal: false };
    }
    match v2::plaintext_len_of(stored_size) {
        Some(n) => MissDecision::Invert(n),
        None => MissDecision::Keep { heal: false },
    }
}

/// One listed entry without a token, as the LIST reported it (before any
/// size was swapped in).
#[derive(Debug, Clone)]
pub(super) struct Miss {
    pub key: String,
    pub is_delta: bool,
    pub stored_etag: String,
    pub stored_size: u64,
}

impl Miss {
    pub(super) fn of(key: &str, listed: &FileMetadata) -> Self {
        Self {
            key: key.to_string(),
            is_delta: listed.is_delta(),
            stored_etag: listed.md5.clone(),
            stored_size: listed.stored_size(),
        }
    }

    fn stored_key(&self) -> String {
        if self.is_delta {
            format!("{}.delta", self.key)
        } else {
            self.key.clone()
        }
    }

    fn dedup_key(&self, bucket: &str) -> String {
        format!(
            "{bucket}\0{}\0{}\0{}",
            self.stored_key(),
            bare_etag(&self.stored_etag),
            self.stored_size
        )
    }
}

/// HEAD the stored object of `miss`, open its seal, and make and keep the
/// listing token of exactly the listed stored object. `Ok(None)`: nothing
/// to learn (not sealed, a key this proxy lacks, no ETag, or another
/// object than the listed one: its sealed body size disagrees).
pub(super) async fn heal_one<B: StorageBackend>(
    inner: &B,
    cfg: &EncryptionConfig,
    bucket: &str,
    miss: &Miss,
) -> Result<Option<ListedToken>, StorageError> {
    if bare_etag(&miss.stored_etag).is_empty() {
        return Ok(None);
    }
    let (prefix, name) = miss.key.rsplit_once('/').unwrap_or(("", miss.key.as_str()));
    let object = if miss.is_delta {
        StoredObject::Delta(name)
    } else {
        StoredObject::Passthrough(name)
    };
    let raw = inner.head_object(bucket, prefix, object).await?;
    let Ok(Some(sealed)) = seal::sealed_of(&raw) else {
        return Ok(None);
    };
    let at = seal::location(bucket, prefix, object);
    let Ok((facts, entry)) = seal::open(&raw, &sealed, &at, cfg.ring()) else {
        return Ok(None);
    };
    if v2::plaintext_len_of(miss.stored_size) != Some(seal::body_len(&facts)) {
        return Ok(None); // overwritten since the LIST
    }
    let Some(lf) = listing_facts_of(&facts) else {
        return Ok(None);
    };
    let stored_key = miss.stored_key();
    let token = entry.site.listing_token(
        &v2::ListedAt {
            bucket,
            stored_key: &stored_key,
            stored_etag: bare_etag(&miss.stored_etag),
            stored_size: miss.stored_size,
        },
        &lf,
    )?;
    let kept = ListedToken {
        token,
        stored_key,
        stored_etag: miss.stored_etag.clone(),
        stored_size: miss.stored_size,
    };
    inner.keep_listing_token(bucket, &kept).await;
    Ok(Some(kept))
}

struct State {
    permits: tokio::sync::Semaphore,
    pending: Mutex<HashSet<String>>,
    tried: Cache<String, ()>,
    paused_until: Mutex<Option<Instant>>,
}

/// The bounded token healer of one wrapper: dedup per stored object for
/// [`TRIED_TTL`], a pause after a failed HEAD, nothing during a shutdown.
#[derive(Clone)]
pub(super) struct TokenHealer {
    state: Arc<State>,
}

impl Default for TokenHealer {
    fn default() -> Self {
        Self {
            state: Arc::new(State {
                permits: tokio::sync::Semaphore::new(HEAL_CONCURRENCY),
                pending: Mutex::new(HashSet::new()),
                tried: Cache::builder()
                    .max_capacity(100_000)
                    .time_to_live(TRIED_TTL)
                    .build(),
                paused_until: Mutex::new(None),
            }),
        }
    }
}

impl TokenHealer {
    fn paused(&self) -> bool {
        self.state
            .paused_until
            .lock()
            .ok()
            .and_then(|p| *p)
            .is_some_and(|until| Instant::now() < until)
    }

    pub(super) fn pause(&self) {
        if let Ok(mut p) = self.state.paused_until.lock() {
            *p = Some(Instant::now() + PAUSE);
        }
    }

    /// May the LIST HEAD `miss` now? Takes the try when it may.
    pub(super) fn try_inline(&self, bucket: &str, miss: &Miss) -> bool {
        let key = miss.dedup_key(bucket);
        if crate::shutdown::is_shutting_down()
            || self.paused()
            || self.state.tried.contains_key(&key)
        {
            return false;
        }
        self.state.tried.insert(key, ());
        true
    }

    /// Queue one background heal of `miss`. Never waits.
    pub(super) fn queue<B: StorageBackend + 'static>(
        &self,
        inner: Arc<B>,
        config: Arc<ArcSwap<EncryptionConfig>>,
        bucket: &str,
        miss: Miss,
    ) -> Option<Skip> {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return Some(Skip::ShuttingDown);
        };
        let key = miss.dedup_key(bucket);
        {
            let Ok(mut pending) = self.state.pending.lock() else {
                return Some(Skip::Full);
            };
            let tried = pending.contains(&key) || self.state.tried.contains_key(&key);
            if let Some(skip) = admit(
                crate::shutdown::is_shutting_down(),
                self.paused(),
                pending.len(),
                tried,
            ) {
                return Some(skip);
            }
            pending.insert(key.clone());
        }
        let healer = self.clone();
        let bucket = bucket.to_string();
        runtime.spawn(async move {
            if let Ok(_permit) = healer.state.permits.acquire().await {
                if !crate::shutdown::is_shutting_down() && !healer.paused() {
                    healer.state.tried.insert(key.clone(), ());
                    let cfg = config.load_full();
                    match heal_one(&*inner, &cfg, &bucket, &miss).await {
                        Ok(Some(_)) => count(HEALED),
                        Ok(None) => {}
                        // Deleted since the LIST: nothing to heal.
                        Err(StorageError::NotFound(_)) => {}
                        Err(e) => {
                            tracing::debug!(
                                "token healer: HEAD {bucket}/{} failed, pausing: {e}",
                                miss.key
                            );
                            healer.pause();
                        }
                    }
                }
            }
            if let Ok(mut pending) = healer.state.pending.lock() {
                pending.remove(&key);
            }
        });
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_miss_truth_table() {
        let ct = v2::stored_len(1_000_000).unwrap();
        // A passthrough ciphertext in a bucket that refuses plaintext:
        // its exact plaintext size.
        assert_eq!(
            decide_miss(ct, false, false, true),
            MissDecision::Invert(1_000_000)
        );
        assert_eq!(
            decide_miss(ct, false, false, false),
            MissDecision::Invert(1_000_000),
            "no HEAD needed"
        );
        // Where plaintext may be served, a passthrough may be plaintext.
        assert_eq!(
            decide_miss(ct, false, true, true),
            MissDecision::Keep { heal: false }
        );
        // A size that no DGE2 body has (shorter than a header, or in the
        // gap after a full frame) is not a ciphertext.
        assert_eq!(
            decide_miss(10, false, false, true),
            MissDecision::Keep { heal: false }
        );
        let gap = v2::stored_len(v2::FRAME_PLAINTEXT as u64).unwrap() + 1;
        assert_eq!(v2::plaintext_len_of(gap), None);
        assert_eq!(
            decide_miss(gap, false, false, true),
            MissDecision::Keep { heal: false }
        );
        // A delta: a HEAD while the page has budget, else its stored size.
        assert_eq!(decide_miss(46, true, false, true), MissDecision::HeadNow);
        assert_eq!(decide_miss(46, true, true, true), MissDecision::HeadNow);
        assert_eq!(
            decide_miss(46, true, false, false),
            MissDecision::Keep { heal: true },
            "healed in the background"
        );
        // An empty object is a header and one empty frame.
        assert_eq!(
            decide_miss(v2::stored_len(0).unwrap(), false, false, false),
            MissDecision::Invert(0)
        );
    }

    proptest::proptest! {
        /// Every plaintext size round-trips through the stored size.
        #[test]
        fn inversion_is_exact(n in 0u64..(1 << 40)) {
            let stored = v2::stored_len(n).unwrap();
            proptest::prop_assert_eq!(
                decide_miss(stored, false, false, false),
                MissDecision::Invert(n)
            );
        }
    }

    #[test]
    fn a_stored_object_is_tried_inline_once() {
        let healer = TokenHealer::default();
        let miss = Miss {
            key: "rel/a.zip".into(),
            is_delta: true,
            stored_etag: "\"e1\"".into(),
            stored_size: 46,
        };
        assert!(healer.try_inline("b", &miss));
        assert!(!healer.try_inline("b", &miss), "tried");
        let overwritten = Miss {
            stored_etag: "e2".into(),
            ..miss.clone()
        };
        assert!(
            healer.try_inline("b", &overwritten),
            "another stored object"
        );
        healer.pause();
        let other = Miss {
            key: "rel/b.zip".into(),
            ..miss
        };
        assert!(!healer.try_inline("b", &other), "paused");
    }
}

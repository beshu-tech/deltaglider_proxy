// SPDX-License-Identifier: BUSL-1.1

//! Tests of the conversion marker (`conversion.rs`): the format's truth
//! table and proptests, the writes by identity against a scripted backend,
//! the window cache, and the wrapper on a filesystem and a fake S3.

use super::conversion::{
    self, by_identity, open, open_fields, open_or_reuse, seal, ByIdentity, Fields, MarkerIo,
    OpenOutcome, Window, WindowCache,
};
use super::seal::Keyring;
use super::*;
use crate::storage::{ClaimOutcome, ConversionMarker, MarkerRead, MarkerWrite};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const RELEASES: &str = "hetzner-fsn1\u{0}releases";

fn key(byte: u8) -> EncryptionKey {
    EncryptionKey([byte; 32])
}

/// A ring with `keys` (id, key byte); returns the ring and the site of the
/// first key.
fn ring(keys: &[(&str, u8)]) -> (Keyring, Arc<v2::SiteKey>) {
    let mut ring = Keyring::default();
    let mut first = None;
    for (id, b) in keys {
        let site = ring.insert(id, &key(*b));
        first.get_or_insert(site);
    }
    (ring, first.unwrap())
}

fn fields(cutoff: i64, deadline: i64, run: u8) -> Fields {
    Fields {
        cutoff,
        deadline,
        run: [run; 16],
    }
}

fn marker(body: Vec<u8>) -> ConversionMarker {
    ConversionMarker {
        content: body,
        version: "\"v\"".into(),
    }
}

fn open_window(f: Fields) -> Window {
    Window::Open {
        cutoff: f.cutoff,
        deadline: f.deadline,
        run: f.run,
        version: "\"v\"".into(),
    }
}

// ── Format: truth table ──

#[test]
fn open_truth_table() {
    let (ring_a, site_a) = ring(&[("releases-2026", 0xaa)]);
    let f = fields(1_000, 2_000, 7);
    let body = seal(&site_a, RELEASES, &f).unwrap();
    let m = marker(body.clone());

    // Valid, before the deadline.
    assert_eq!(open(&m, &ring_a, RELEASES, 1_500), open_window(f));
    assert_eq!(open(&m, &ring_a, RELEASES, 1_999), open_window(f));
    // The deadline itself and after: closed.
    assert_eq!(open(&m, &ring_a, RELEASES, 2_000), Window::Closed);
    assert_eq!(open(&m, &ring_a, RELEASES, 9_999), Window::Closed);
    // Same bucket name, other backend: another storage identity.
    assert_eq!(
        open(&m, &ring_a, "local-disk\u{0}releases", 1_500),
        Window::Closed
    );
    assert_eq!(open(&m, &ring_a, "releases", 1_500), Window::Closed);
    // A key that the ring lacks (unknown fingerprint).
    let (ring_b, _) = ring(&[("other", 0xbb)]);
    assert_eq!(open(&m, &ring_b, RELEASES, 1_500), Window::Closed);
    // The key held only as an older ring key (a rotation): opens.
    let (rotated, _) = ring(&[("releases-2027", 0xcc), ("releases-2026", 0xaa)]);
    assert_eq!(open(&m, &rotated, RELEASES, 1_500), open_window(f));
    // Empty and garbage bodies.
    for junk in [
        &b""[..],
        b"dgc1.",
        b"dgc1..",
        b"garbage",
        b"dgc1.zzzzzzzzzzzzzzzz.AAAA",
        b"fp-0123456789abcdef",
        &[0xff, 0xfe, 0x00],
    ] {
        assert_eq!(
            open(&marker(junk.to_vec()), &ring_a, RELEASES, 1_500),
            Window::Closed,
            "{junk:?}"
        );
    }
    // A sealed deadline that is not after the cutoff never opens.
    let bad = seal(&site_a, RELEASES, &fields(2_000, 2_000, 7)).unwrap();
    assert_eq!(open(&marker(bad), &ring_a, RELEASES, 0), Window::Closed);
    // The fingerprint is in clear and lowercase hex: an uppercase copy is
    // an edit, and closes.
    let text = String::from_utf8(body.clone()).unwrap();
    let (prefix, rest) = text.split_at(5);
    let (fp, sealed) = rest.split_once('.').unwrap();
    let upper = format!("{prefix}{}.{sealed}", fp.to_uppercase());
    assert_ne!(upper.as_bytes(), &body[..]);
    assert_eq!(
        open(&marker(upper.into_bytes()), &ring_a, RELEASES, 1_500),
        Window::Closed
    );
}

/// A replayed old marker opens only with its own sealed cutoff, run and
/// deadline: no edit of it gives another cutoff, and once its deadline is
/// past it is closed.
#[test]
fn a_replayed_marker_keeps_its_own_cutoff() {
    let (ring, site) = ring(&[("releases-2026", 0xaa)]);
    let old = fields(1_000, 2_000, 1);
    let new = fields(5_000, 9_000, 2);
    let old_body = seal(&site, RELEASES, &old).unwrap();
    let _new_body = seal(&site, RELEASES, &new).unwrap();
    // Replayed during its own window: its own cutoff, not the new one.
    assert_eq!(
        open(&marker(old_body.clone()), &ring, RELEASES, 1_500),
        open_window(old)
    );
    // Replayed after its deadline (while the new run is open): closed.
    assert_eq!(
        open(&marker(old_body.clone()), &ring, RELEASES, 6_000),
        Window::Closed
    );
    // Every single-byte edit closes it, so the cutoff cannot move.
    for i in 0..old_body.len() {
        for mask in [0x01u8, 0x02, 0x10, 0x20, 0x80] {
            let mut edited = old_body.clone();
            edited[i] ^= mask;
            assert_eq!(
                open_fields(&edited, &ring, RELEASES),
                None,
                "byte {i} ^ {mask:#x}"
            );
        }
    }
}

proptest::proptest! {
    /// Round trip for any fields with a deadline after the cutoff, under
    /// any identity; a different identity never opens.
    #[test]
    fn seal_opens_only_at_its_identity(
        cutoff in -1_000_000_000i64..4_000_000_000,
        len in 1i64..1_000_000_000,
        run in proptest::array::uniform16(proptest::num::u8::ANY),
        backend in "[a-z0-9-]{1,12}",
        bucket in "[a-z0-9.-]{3,20}",
        other in "[a-z0-9-]{1,12}",
    ) {
        let (ring, site) = ring(&[("k", 0x11)]);
        let f = Fields { cutoff, deadline: cutoff + len, run };
        let identity = format!("{backend}\u{0}{bucket}");
        let body = seal(&site, &identity, &f).unwrap();
        proptest::prop_assert_eq!(open_fields(&body, &ring, &identity), Some(f));
        let elsewhere = format!("{other}\u{0}{bucket}");
        if elsewhere != identity {
            proptest::prop_assert_eq!(open_fields(&body, &ring, &elsewhere), None);
        }
        // The deadline on the backend clock.
        proptest::prop_assert!(open(&marker(body.clone()), &ring, &identity, f.deadline - 1).is_open());
        proptest::prop_assert!(!open(&marker(body), &ring, &identity, f.deadline).is_open());
    }

    /// Forgery: any edit of a valid body (a changed byte, a cut, an added
    /// tail) never opens to fields other than the sealed ones, and a changed
    /// byte never opens at all.
    #[test]
    fn an_edited_marker_never_opens(
        at in 0usize..200,
        mask in 1u8..=255,
        cut in 0usize..200,
        tail in proptest::collection::vec(proptest::num::u8::ANY, 1..8),
    ) {
        let (ring, site) = ring(&[("k", 0x11)]);
        let f = fields(100, 200, 3);
        let body = seal(&site, RELEASES, &f).unwrap();
        let mut flipped = body.clone();
        let i = at % flipped.len();
        flipped[i] ^= mask;
        proptest::prop_assert_eq!(open_fields(&flipped, &ring, RELEASES), None);
        let cut = &body[..cut % body.len()];
        proptest::prop_assert_eq!(open_fields(cut, &ring, RELEASES), None);
        let mut longer = body.clone();
        longer.extend_from_slice(&tail);
        proptest::prop_assert_eq!(open_fields(&longer, &ring, RELEASES), None);
    }

    /// Arbitrary bytes never open.
    #[test]
    fn garbage_never_opens(body in proptest::collection::vec(proptest::num::u8::ANY, 0..120)) {
        let (ring, _) = ring(&[("k", 0x11)]);
        proptest::prop_assert_eq!(open_fields(&body, &ring, RELEASES), None);
    }
}

// ── Writes by identity, on a scripted backend ──

type Hook = Box<dyn FnOnce(&mut Option<(Vec<u8>, u64)>) + Send>;

/// One bucket's marker in memory, with S3's conditions. A hook runs just
/// before the next write is judged: a peer's write between our read and
/// our conditional write.
#[derive(Default)]
struct Scripted {
    stored: parking_lot::Mutex<Option<(Vec<u8>, u64)>>,
    next_version: std::sync::atomic::AtomicU64,
    hooks: parking_lot::Mutex<VecDeque<Hook>>,
    writes: std::sync::atomic::AtomicUsize,
    server_now: i64,
}

impl Scripted {
    fn at(server_now: i64) -> Self {
        Self {
            server_now,
            next_version: 100.into(),
            ..Default::default()
        }
    }
    fn holding(server_now: i64, body: Vec<u8>) -> Self {
        let s = Self::at(server_now);
        *s.stored.lock() = Some((body, 1));
        s
    }
    fn before_write(&self, hook: impl FnOnce(&mut Option<(Vec<u8>, u64)>) + Send + 'static) {
        self.hooks.lock().push_back(Box::new(hook));
    }
    fn body(&self) -> Option<Vec<u8>> {
        self.stored.lock().as_ref().map(|(b, _)| b.clone())
    }
    fn writes(&self) -> usize {
        self.writes.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl MarkerIo for Scripted {
    async fn read(&self) -> Result<MarkerRead, StorageError> {
        Ok(MarkerRead {
            marker: self.stored.lock().as_ref().map(|(b, v)| ConversionMarker {
                content: b.clone(),
                version: format!("\"{v}\""),
            }),
            server_now: Some(self.server_now),
        })
    }
    async fn write(&self, op: MarkerWrite<'_>) -> Result<ClaimOutcome, StorageError> {
        use std::sync::atomic::Ordering::SeqCst;
        self.writes.fetch_add(1, SeqCst);
        let mut stored = self.stored.lock();
        if let Some(hook) = self.hooks.lock().pop_front() {
            hook(&mut stored);
        }
        let current = stored.as_ref().map(|(_, v)| format!("\"{v}\""));
        let next = self.next_version.fetch_add(1, SeqCst);
        Ok(match op {
            MarkerWrite::Create(body) => {
                if stored.is_some() {
                    return Ok(ClaimOutcome::Lost);
                }
                *stored = Some((body.to_vec(), next));
                ClaimOutcome::Written
            }
            MarkerWrite::Replace { body, version } => {
                if current.as_deref() != Some(version) {
                    return Ok(ClaimOutcome::Lost);
                }
                *stored = Some((body.to_vec(), next));
                ClaimOutcome::Written
            }
            MarkerWrite::Remove { version } => {
                if current.as_deref() != Some(version) {
                    return Ok(ClaimOutcome::Lost);
                }
                *stored = None;
                ClaimOutcome::Written
            }
        })
    }
}

fn close(_: &Fields) -> Option<Fields> {
    None
}

/// A re-PUT of a foreign body before our close: NotOurs, and the foreign
/// marker stays (a lost condition is never "gone").
#[tokio::test]
async fn a_close_blocked_by_a_foreign_re_put_is_not_ours() {
    let (ring, site) = ring(&[("k", 0x11)]);
    let ours = seal(&site, RELEASES, &fields(100, 200, 1)).unwrap();
    let io = Scripted::holding(150, ours);
    io.before_write(|s| *s = Some((b"planted by a backend writer".to_vec(), 9)));
    let got = by_identity(&io, &ring, Some(&site), RELEASES, &[1; 16], close)
        .await
        .unwrap();
    assert_eq!(got, ByIdentity::NotOurs);
    assert_eq!(
        io.body().as_deref(),
        Some(&b"planted by a backend writer"[..])
    );
    assert_eq!(io.writes(), 1, "one conditional write, lost, then a read");

    // A foreign marker from the start: not even a write.
    let io = Scripted::holding(150, b"garbage".to_vec());
    let got = by_identity(&io, &ring, Some(&site), RELEASES, &[1; 16], close)
        .await
        .unwrap();
    assert_eq!((got, io.writes()), (ByIdentity::NotOurs, 0));
    assert!(io.body().is_some());

    // Another run's marker: NotOurs, kept.
    let theirs = seal(&site, RELEASES, &fields(100, 200, 2)).unwrap();
    let io = Scripted::holding(150, theirs.clone());
    let got = by_identity(&io, &ring, Some(&site), RELEASES, &[1; 16], close)
        .await
        .unwrap();
    assert_eq!((got, io.body()), (ByIdentity::NotOurs, Some(theirs)));
}

/// 412 then success: a peer rewrites OUR marker (same run) between the read
/// and the write; the close reads again and wins on the second write.
#[tokio::test]
async fn a_lost_condition_reads_again_and_retries() {
    let (ring, site) = ring(&[("k", 0x11)]);
    let ours = seal(&site, RELEASES, &fields(100, 200, 1)).unwrap();
    let io = Scripted::holding(150, ours.clone());
    io.before_write(move |s| *s = Some((ours, 7)));
    let got = by_identity(&io, &ring, Some(&site), RELEASES, &[1; 16], close)
        .await
        .unwrap();
    assert_eq!(got, ByIdentity::Done(None, String::new()));
    assert_eq!((io.writes(), io.body()), (2, None));
}

/// The condition lost on every attempt: Blocked, the marker stays.
#[tokio::test]
async fn a_condition_lost_every_time_is_blocked() {
    let (ring, site) = ring(&[("k", 0x11)]);
    let ours = seal(&site, RELEASES, &fields(100, 200, 1)).unwrap();
    let io = Scripted::holding(150, ours.clone());
    for v in 0..conversion::RETRIES as u64 {
        let body = ours.clone();
        io.before_write(move |s| *s = Some((body, 50 + v)));
    }
    let got = by_identity(&io, &ring, Some(&site), RELEASES, &[1; 16], close)
        .await
        .unwrap();
    assert_eq!(got, ByIdentity::Blocked);
    assert_eq!(io.writes(), conversion::RETRIES);
    assert!(io.body().is_some());
}

#[tokio::test]
async fn an_absent_marker_is_absent() {
    let (ring, site) = ring(&[("k", 0x11)]);
    let io = Scripted::at(150);
    let got = by_identity(&io, &ring, Some(&site), RELEASES, &[1; 16], close)
        .await
        .unwrap();
    assert_eq!((got, io.writes()), (ByIdentity::Absent, 0));
}

/// An extension keeps the cutoff and the run, and works after the
/// deadline (a resume of an expired window).
#[tokio::test]
async fn an_extension_keeps_cutoff_and_run() {
    let (ring, site) = ring(&[("k", 0x11)]);
    let f = fields(100, 200, 1);
    let io = Scripted::holding(500, seal(&site, RELEASES, &f).unwrap());
    let got = by_identity(&io, &ring, Some(&site), RELEASES, &f.run, |f| {
        Some(Fields {
            deadline: 900,
            ..*f
        })
    })
    .await
    .unwrap();
    let extended = fields(100, 900, 1);
    assert!(matches!(got, ByIdentity::Done(Some(g), _) if g == extended));
    assert_eq!(
        open_fields(&io.body().unwrap(), &ring, RELEASES),
        Some(extended)
    );
}

#[tokio::test]
async fn open_or_reuse_creates_once_and_reuses() {
    let (ring, site) = ring(&[("k", 0x11)]);
    let io = Scripted::at(1_000);
    let created = open_or_reuse(&io, &ring, &site, RELEASES, 600)
        .await
        .unwrap();
    let OpenOutcome::Created(f, _) = created else {
        panic!("{created:?}")
    };
    assert_eq!((f.cutoff, f.deadline), (1_000, 1_600), "server clock");
    let again = open_or_reuse(&io, &ring, &site, RELEASES, 600)
        .await
        .unwrap();
    assert!(
        matches!(again, OpenOutcome::Reused(g, _) if g == f),
        "{again:?}"
    );
    assert_eq!(io.writes(), 1);

    // A peer creates between our read and our create: we reuse theirs.
    let io = Scripted::at(1_000);
    let theirs = fields(990, 5_000, 4);
    let body = seal(&site, RELEASES, &theirs).unwrap();
    io.before_write(move |s| *s = Some((body, 3)));
    let got = open_or_reuse(&io, &ring, &site, RELEASES, 600)
        .await
        .unwrap();
    assert!(
        matches!(got, OpenOutcome::Reused(g, _) if g == theirs),
        "{got:?}"
    );

    // Past its deadline: Expired, never replaced (its cutoff must stay).
    let old = seal(&site, RELEASES, &fields(10, 20, 5)).unwrap();
    let io = Scripted::holding(1_000, old.clone());
    let got = open_or_reuse(&io, &ring, &site, RELEASES, 600)
        .await
        .unwrap();
    assert!(matches!(got, OpenOutcome::Expired(_)), "{got:?}");
    assert_eq!((io.writes(), io.body()), (0, Some(old)));

    // Foreign: never overwritten.
    let io = Scripted::holding(1_000, b"junk".to_vec());
    let got = open_or_reuse(&io, &ring, &site, RELEASES, 600)
        .await
        .unwrap();
    assert_eq!((got, io.writes()), (OpenOutcome::Foreign, 0));
}

// ── Cache ──

#[test]
fn the_cache_keeps_a_window_for_its_ttl_and_checks_the_deadline_each_use() {
    let cache = WindowCache::default();
    let t0 = Instant::now();
    assert_eq!(cache.cached("b", t0, 1_000), None);
    let w = open_window(fields(900, 1_100, 1));
    // The backend clock runs 50 s ahead of this node.
    cache.store("b", w.clone(), t0, 1_000, Some(1_050), conversion::TTL);
    assert_eq!(cache.cached("b", t0, 1_000), Some(w.clone()));
    // The deadline (1 100 on the backend clock) arrives at local 1 050.
    assert_eq!(cache.cached("b", t0, 1_049), Some(w));
    assert_eq!(cache.cached("b", t0, 1_050), Some(Window::Closed));
    // Past the TTL: read again.
    assert_eq!(cache.cached("b", t0 + conversion::TTL, 1_000), None);
    assert_eq!(cache.cached("other", t0, 1_000), None);
}

#[test]
fn a_refusal_rereads_a_closed_window_at_most_every_five_seconds() {
    let cache = WindowCache::default();
    let t0 = Instant::now();
    cache.store("b", Window::Closed, t0, 0, None, conversion::TTL);
    assert!(cache.take_refusal_refresh("b", t0));
    assert!(!cache.take_refusal_refresh("b", t0 + Duration::from_secs(4)));
    assert!(cache.take_refusal_refresh("b", t0 + conversion::REFUSAL_REFRESH));
    // An open window: a refusal is about the object, not the window.
    cache.store(
        "o",
        open_window(fields(0, i64::MAX, 1)),
        t0,
        0,
        None,
        conversion::TTL,
    );
    assert!(!cache.take_refusal_refresh("o", t0));
    // Nothing cached yet: due.
    assert!(cache.take_refusal_refresh("new", t0));
}

// ── The wrapper ──

struct Rig {
    dir: tempfile::TempDir,
    cfg: Arc<ArcSwap<EncryptionConfig>>,
}

impl Rig {
    async fn new(cfg: EncryptionConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fs = crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap();
        fs.create_bucket("releases").await.unwrap();
        Self {
            dir,
            cfg: Arc::new(ArcSwap::new(Arc::new(cfg))),
        }
    }

    /// A wrapper over the same directory, named `backend`.
    async fn wrapper(
        &self,
        backend: &str,
        cfg: Option<EncryptionConfig>,
    ) -> EncryptingBackend<crate::storage::FilesystemBackend> {
        let fs = crate::storage::FilesystemBackend::new(self.dir.path().to_path_buf())
            .await
            .unwrap();
        let cfg = match cfg {
            Some(c) => Arc::new(ArcSwap::new(Arc::new(c))),
            None => self.cfg.clone(),
        };
        EncryptingBackend::new(fs, cfg).with_witness(backend, Arc::new(WitnessGate::default()))
    }
}

fn encrypting(byte: u8, id: &str) -> EncryptionConfig {
    EncryptionConfig {
        key: Some(key(byte)),
        key_id: Some(id.into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_wrapper_opens_reuses_and_closes_by_identity() {
    let rig = Rig::new(encrypting(0xaa, "releases-2026")).await;
    let a = rig.wrapper("local-disk", None).await;
    assert_eq!(a.window_of("releases").await, Window::Closed);
    let run = a.open_conversion("releases", 7).await.unwrap();
    let Window::Open {
        cutoff, deadline, ..
    } = a.window_of("releases").await
    else {
        panic!("open after open_conversion")
    };
    assert_eq!(deadline - cutoff, 7 * 86_400);
    // Idempotent: a second open reuses the marker and its cutoff.
    assert_eq!(a.open_conversion("releases", 3).await.unwrap(), run);

    // A peer node (same backend name, fresh cache) sees the window.
    let b = rig.wrapper("local-disk", None).await;
    assert!(matches!(b.window_of("releases").await, Window::Open { run: r, .. } if r == run));
    // A rotated config (new primary, old key in the ring) still opens it.
    let mut rotated = encrypting(0xbb, "releases-2027");
    rotated.keyring.insert("releases-2026".into(), key(0xaa));
    let r = rig.wrapper("local-disk", Some(rotated)).await;
    assert!(r.window_of("releases").await.is_open());
    // The same directory under another backend name: another storage
    // identity, closed.
    let other = rig.wrapper("hetzner-fsn1", None).await;
    assert_eq!(other.window_of("releases").await, Window::Closed);

    // A close by another run id is refused.
    assert_eq!(
        b.close_conversion("releases", &[0; 16]).await.unwrap(),
        ByIdentity::NotOurs
    );
    assert!(b.window_of("releases").await.is_open());
    // The extension keeps the cutoff.
    let ext = a
        .extend_conversion("releases", &run, deadline + 100)
        .await
        .unwrap();
    assert!(
        matches!(ext, ByIdentity::Done(Some(f), _) if f.cutoff == cutoff && f.deadline == deadline + 100)
    );
    // Close by identity.
    assert!(matches!(
        a.close_conversion("releases", &run).await.unwrap(),
        ByIdentity::Done(None, _)
    ));
    assert_eq!(a.window_of("releases").await, Window::Closed);
    // `b` keeps its cached Open until the TTL (the plan accepts it: a
    // stale Open serves only pre-cutoff objects, which the job converted).
    // A refusal under an open window is about the object: no re-read.
    assert!(b.window_of("releases").await.is_open());
    assert!(b.refresh_after_refusal("releases").await.is_open());
    // A fresh node reads the close.
    let c = rig.wrapper("local-disk", None).await;
    assert_eq!(c.window_of("releases").await, Window::Closed);
}

/// A window opened on another node takes effect on a refused read even
/// while this node caches it as closed, at most once per five seconds.
#[tokio::test]
async fn a_refusal_picks_up_a_window_opened_elsewhere() {
    let rig = Rig::new(encrypting(0xaa, "releases-2026")).await;
    let a = rig.wrapper("local-disk", None).await;
    let b = rig.wrapper("local-disk", None).await;
    assert_eq!(b.window_of("releases").await, Window::Closed);
    a.open_conversion("releases", 1).await.unwrap();
    assert_eq!(b.window_of("releases").await, Window::Closed, "cached");
    assert!(b.refresh_after_refusal("releases").await.is_open());
}

#[tokio::test]
async fn a_bucket_that_writes_plaintext_has_no_conversion() {
    let rig = Rig::new(EncryptionConfig::default()).await;
    let a = rig.wrapper("local-disk", None).await;
    let e = a.open_conversion("releases", 7).await.unwrap_err();
    assert!(e.to_string().contains("writes plaintext"), "{e}");
    let e = a.open_conversion("releases", 0).await.unwrap_err();
    assert!(e.to_string().contains("1 to 365 days"), "{e}");
}

/// On S3 (the fake), the cutoff is the server's `Date`, the create is
/// conditional, and the marker key is in the facts namespace.
#[tokio::test]
async fn on_s3_the_marker_is_a_cas_object_with_the_server_clock() {
    let (ep, fake) = crate::storage::fake_s3::start().await;
    let s3 = crate::storage::s3_test_support::for_test_endpoint(&ep);
    let read = s3.read_conversion_marker("b").await.unwrap();
    assert_eq!(read.marker, None);
    let server = read.server_now.expect("the response Date");
    assert!((server - chrono::Utc::now().timestamp()).abs() < 5);
    assert_eq!(
        s3.write_conversion_marker("b", MarkerWrite::Create(b"one"))
            .await
            .unwrap(),
        ClaimOutcome::Written
    );
    assert_eq!(
        s3.write_conversion_marker("b", MarkerWrite::Create(b"two"))
            .await
            .unwrap(),
        ClaimOutcome::Lost
    );
    let m = s3
        .read_conversion_marker("b")
        .await
        .unwrap()
        .marker
        .unwrap();
    assert_eq!(m.content, b"one");
    assert_eq!(
        s3.write_conversion_marker("b", MarkerWrite::Remove { version: "\"x\"" })
            .await
            .unwrap(),
        ClaimOutcome::Lost
    );
    assert!(
        fake.requests().iter().all(|r| r.contains("/b/.dg/facts/")),
        "{:?}",
        fake.requests()
    );

    // Through the wrapper: open, then close by identity.
    let wrapper = EncryptingBackend::new(
        crate::storage::s3_test_support::for_test_endpoint(&ep),
        Arc::new(ArcSwap::new(Arc::new(encrypting(0xaa, "k")))),
    );
    assert_eq!(
        s3.write_conversion_marker(
            "b",
            MarkerWrite::Remove {
                version: &m.version
            }
        )
        .await
        .unwrap(),
        ClaimOutcome::Written
    );
    let run = wrapper.open_conversion("b", 7).await.unwrap();
    let Window::Open { cutoff, .. } = wrapper.window_of("b").await else {
        panic!("open")
    };
    assert!((cutoff - server).abs() < 5, "server clock");
    assert!(matches!(
        wrapper.close_conversion("b", &run).await.unwrap(),
        ByIdentity::Done(None, _)
    ));
    assert_eq!(s3.read_conversion_marker("b").await.unwrap().marker, None);
}

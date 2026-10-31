// SPDX-License-Identifier: BUSL-1.1

//! THE read rule for objects without a seal (`seal::read_verdict`) with
//! the conversion window and the two readers
//! (`docs/plan/encryption-conversion-marker.md`, P3): the truth table, a
//! proptest against an independent model, and the wrapper on a filesystem
//! (client reads inside and outside the window, the job's reads, sealed
//! objects, `plaintext_reads: serve`, a window opened on another node).

use super::conversion::{self, Window};
use super::seal::{read_verdict, PlaintextRule, Reader, Unsealed, CONVERSION_GRACE_SECS};
use super::*;
use crate::storage::traits::StoredObject;
use proptest::prelude::*;

const CUTOFF: i64 = 1_800_000_000;
const G: i64 = CONVERSION_GRACE_SECS;

fn open_at(cutoff: i64) -> Window {
    Window::Open {
        cutoff,
        deadline: cutoff + 86_400,
        run: [7; 16],
        version: String::new(),
    }
}

/// An unsealed object that the backend stored at `lm` (unix seconds).
fn plain(lm: Option<i64>, size: u64) -> FileMetadata {
    FileMetadata::new_passthrough("a.txt".into(), "x".into(), "e".into(), size, None)
        .stored_at_for_tests(lm)
}

fn marked(lm: Option<i64>) -> FileMetadata {
    let mut m = plain(lm, 3);
    m.user_metadata
        .insert(ENCRYPTION_MARKER_KEY.into(), seal::DISK_MARKER.into());
    m
}

// ── Truth table ──

#[test]
fn read_verdict_truth_table() {
    use Reader::{Client, Job};
    use Unsealed::{Adopted, Refuse, Serve, ServeInWindow};
    let open = open_at(CUTOFF);
    let closed = Window::Closed;
    let enc_open = PlaintextRule::Encrypts(&open);
    let enc_closed = PlaintextRule::Encrypts(&closed);
    let serves = PlaintextRule::Serves;
    let before = Some(CUTOFF - 3600);
    let in_grace = Some(CUTOFF + G - 1);
    let at_grace = Some(CUTOFF + G);
    let after = Some(CUTOFF + 3600);
    // (case, meta, folder, rule, reader, expected)
    let rows: Vec<(
        &str,
        FileMetadata,
        bool,
        PlaintextRule<'_>,
        Reader,
        Unsealed,
    )> = vec![
        // Window closed: clients refused, the job adopts.
        (
            "closed, before",
            plain(before, 3),
            false,
            enc_closed,
            Client,
            Refuse,
        ),
        (
            "closed, before, job",
            plain(before, 3),
            false,
            enc_closed,
            Job,
            Adopted,
        ),
        (
            "closed, missing",
            plain(None, 3),
            false,
            enc_closed,
            Client,
            Refuse,
        ),
        // Window open: history before cutoff + grace.
        (
            "open, before",
            plain(before, 3),
            false,
            enc_open,
            Client,
            ServeInWindow,
        ),
        (
            "open, before, job",
            plain(before, 3),
            false,
            enc_open,
            Job,
            ServeInWindow,
        ),
        (
            "open, at cutoff",
            plain(Some(CUTOFF), 3),
            false,
            enc_open,
            Client,
            ServeInWindow,
        ),
        (
            "open, inside grace",
            plain(in_grace, 3),
            false,
            enc_open,
            Client,
            ServeInWindow,
        ),
        (
            "open, grace boundary",
            plain(at_grace, 3),
            false,
            enc_open,
            Client,
            Refuse,
        ),
        (
            "open, grace boundary, job",
            plain(at_grace, 3),
            false,
            enc_open,
            Job,
            Adopted,
        ),
        (
            "open, after grace",
            plain(after, 3),
            false,
            enc_open,
            Client,
            Refuse,
        ),
        (
            "open, after grace, job",
            plain(after, 3),
            false,
            enc_open,
            Job,
            Adopted,
        ),
        // No stored time: never history.
        (
            "open, missing",
            plain(None, 3),
            false,
            enc_open,
            Client,
            Refuse,
        ),
        (
            "open, missing, job",
            plain(None, 3),
            false,
            enc_open,
            Job,
            Adopted,
        ),
        // The bucket writes plaintext, or `plaintext_reads: serve`.
        (
            "serves, after",
            plain(after, 3),
            false,
            serves,
            Client,
            Serve,
        ),
        (
            "serves, missing",
            plain(None, 3),
            false,
            serves,
            Client,
            Serve,
        ),
        ("serves, job", plain(after, 3), false, serves, Job, Serve),
        // A folder marker always reads; a folder key with bytes does not.
        ("folder", plain(None, 0), true, enc_closed, Client, Serve),
        (
            "folder with bytes",
            plain(None, 1),
            true,
            enc_closed,
            Client,
            Refuse,
        ),
        (
            "folder with bytes, open",
            plain(before, 1),
            true,
            enc_open,
            Client,
            ServeInWindow,
        ),
        // A marker without a seal: refused for every reader and rule.
        (
            "marker, open",
            marked(before),
            false,
            enc_open,
            Client,
            Refuse,
        ),
        ("marker, job", marked(before), false, enc_open, Job, Refuse),
        (
            "marker, serves",
            marked(before),
            false,
            serves,
            Client,
            Refuse,
        ),
        ("marker, folder", marked(None), true, serves, Job, Refuse),
    ];
    for (case, meta, folder, rule, reader, want) in rows {
        assert_eq!(read_verdict(&meta, folder, rule, reader, G), want, "{case}");
    }
}

/// One case of the proptest.
struct Case {
    has_marker: bool,
    folder: bool,
    size: u64,
    serves: bool,
    cutoff: Option<i64>,
    lm: Option<i64>,
    grace: i64,
    job: bool,
}

/// The model the proptest checks the rule against, written apart from it.
fn model(c: &Case) -> Unsealed {
    let Case {
        has_marker,
        folder,
        size,
        serves,
        cutoff,
        lm,
        grace,
        job,
    } = *c;
    if has_marker {
        Unsealed::Refuse
    } else if (folder && size == 0) || serves {
        Unsealed::Serve
    } else if matches!((cutoff, lm), (Some(c), Some(t)) if t < c + grace) {
        Unsealed::ServeInWindow
    } else if job {
        Unsealed::Adopted
    } else {
        Unsealed::Refuse
    }
}

proptest! {
    #[test]
    fn read_verdict_matches_the_model(
        has_marker in any::<bool>(),
        folder in any::<bool>(),
        size in 0u64..3,
        serves in any::<bool>(),
        cutoff in proptest::option::of(0i64..4_000_000_000),
        lm_off in proptest::option::of(-1000i64..1000),
        grace in 0i64..600,
        job in any::<bool>(),
    ) {
        let lm = lm_off.map(|o| cutoff.unwrap_or(2_000_000_000) + o);
        let mut meta = plain(lm, size);
        if has_marker {
            meta.user_metadata.insert(ENCRYPTION_MARKER_KEY.into(), "x".into());
        }
        let window = cutoff.map_or(Window::Closed, open_at);
        let rule = if serves { PlaintextRule::Serves } else { PlaintextRule::Encrypts(&window) };
        let reader = if job { Reader::Job } else { Reader::Client };
        let got = read_verdict(&meta, folder, rule, reader, grace);
        let case = Case { has_marker, folder, size, serves, cutoff, lm, grace, job };
        prop_assert_eq!(got, model(&case));
        // The invariants a reader should see at a glance.
        if reader == Reader::Client {
            prop_assert_ne!(got, Unsealed::Adopted, "a client never adopts");
        } else {
            prop_assert!(got != Unsealed::Refuse || has_marker, "the job refuses only a marker");
        }
        if got == Unsealed::ServeInWindow {
            prop_assert!(lm.is_some() && cutoff.is_some(), "history needs both times");
        }
        // The deadline is not the rule's: the caller passes an open window
        // only before it.
    }
}

// ── The wrapper ──

fn key(byte: u8) -> EncryptionKey {
    EncryptionKey([byte; 32])
}

fn encrypting(serve: bool) -> EncryptionConfig {
    EncryptionConfig {
        key: Some(key(0xaa)),
        key_id: Some("releases-2026".into()),
        serve_plaintext: serve,
        ..Default::default()
    }
}

struct Rig {
    dir: tempfile::TempDir,
}

impl Rig {
    /// A bucket `releases` with plaintext `old.txt` (stored an hour before
    /// now) and `new.txt` (stored an hour after now: after any cutoff
    /// opened now plus the grace), written without a key.
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fs = crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap();
        fs.create_bucket("releases").await.unwrap();
        let now = std::time::SystemTime::now();
        for (name, at) in [
            ("old.txt", now - std::time::Duration::from_secs(3600)),
            ("new.txt", now + std::time::Duration::from_secs(3600)),
        ] {
            let body = format!("plaintext {name}");
            let meta = FileMetadata::new_passthrough(
                name.into(),
                "0".repeat(64),
                "0".repeat(32),
                body.len() as u64,
                None,
            );
            fs.put_passthrough("releases", "p", name, Bytes::from(body), &meta)
                .await
                .unwrap();
            let path = dir
                .path()
                .join("releases")
                .join("deltaspaces")
                .join("p")
                .join(name);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(at)
                .unwrap();
        }
        Self { dir }
    }

    async fn wrapper(
        &self,
        cfg: EncryptionConfig,
    ) -> EncryptingBackend<crate::storage::FilesystemBackend> {
        let fs = crate::storage::FilesystemBackend::new(self.dir.path().to_path_buf())
            .await
            .unwrap();
        EncryptingBackend::new(fs, Arc::new(ArcSwap::new(Arc::new(cfg))))
            .with_witness("local-disk", Arc::new(WitnessGate::default()))
    }
}

async fn head(
    w: &EncryptingBackend<crate::storage::FilesystemBackend>,
    name: &str,
) -> Result<FileMetadata, StorageError> {
    w.head_object("releases", "p", StoredObject::Passthrough(name))
        .await
}

async fn get(
    w: &EncryptingBackend<crate::storage::FilesystemBackend>,
    name: &str,
) -> Result<Vec<u8>, StorageError> {
    w.get_object("releases", "p", StoredObject::Passthrough(name))
        .await
}

fn refused(r: Result<impl std::fmt::Debug, StorageError>) -> bool {
    matches!(&r, Err(e) if seal::is_plaintext_refusal(e))
}

fn reads(outcome: &str) -> u64 {
    conversion::PLAINTEXT_READS
        .with_label_values(&[outcome])
        .get()
}

#[tokio::test]
async fn clients_read_plaintext_history_only_inside_the_window() {
    let rig = Rig::new().await;
    let w = rig.wrapper(encrypting(false)).await;
    // No window: refused, with the conversion named.
    let e = head(&w, "old.txt").await.unwrap_err();
    assert!(seal::is_plaintext_refusal(&e), "{e}");
    assert!(e.to_string().contains("conversion"), "{e}");
    let refused_before = reads(conversion::REFUSED);
    assert!(refused(get(&w, "old.txt").await));
    assert!(reads(conversion::REFUSED) > refused_before);

    w.open_conversion("releases", 7).await.unwrap();
    // History: HEAD, GET and a range read.
    let served_before = reads(conversion::SERVED_IN_WINDOW);
    assert!(head(&w, "old.txt").await.is_ok());
    assert_eq!(get(&w, "old.txt").await.unwrap(), b"plaintext old.txt");
    let range = w
        .open_passthrough_range("releases", "p", "old.txt", 0, 4)
        .await
        .unwrap();
    assert_eq!(collect_all(range.stream).await, b"plain");
    assert!(reads(conversion::SERVED_IN_WINDOW) >= served_before + 3);
    // Stored after the cutoff plus the grace: refused.
    assert!(refused(head(&w, "new.txt").await));
    assert!(refused(get(&w, "new.txt").await));

    // The job reads both; only the new one would be adopted (the worker
    // classifies it with the same rule).
    let proof = crate::maintenance::worker::ConversionReadProof::for_tests();
    conversion::as_conversion_job(proof, async {
        assert!(head(&w, "new.txt").await.is_ok());
        assert_eq!(get(&w, "new.txt").await.unwrap(), b"plaintext new.txt");
    })
    .await;
    // Outside the scope, a client again.
    assert!(refused(head(&w, "new.txt").await));
    // A future spawned from the scope reads as a client.
    let w = Arc::new(w);
    let spawned = conversion::as_conversion_job(proof, {
        let w = w.clone();
        async move {
            tokio::spawn(async move { head(&w, "new.txt").await.is_ok() })
                .await
                .unwrap()
        }
    })
    .await;
    assert!(!spawned, "the job's scope does not follow a spawned task");
}

#[tokio::test]
async fn the_job_reads_every_plaintext_object_without_a_window() {
    let rig = Rig::new().await;
    let w = rig.wrapper(encrypting(false)).await;
    let proof = crate::maintenance::worker::ConversionReadProof::for_tests();
    conversion::as_conversion_job(proof, async {
        assert!(head(&w, "old.txt").await.is_ok());
        assert!(head(&w, "new.txt").await.is_ok());
    })
    .await;
}

#[tokio::test]
async fn serve_still_serves_every_plaintext_object() {
    let rig = Rig::new().await;
    let w = rig.wrapper(encrypting(true)).await;
    assert!(head(&w, "old.txt").await.is_ok());
    assert!(head(&w, "new.txt").await.is_ok());
    // A bucket that writes plaintext, too.
    let p = rig.wrapper(EncryptionConfig::default()).await;
    assert!(head(&p, "new.txt").await.is_ok());
}

#[tokio::test]
async fn sealed_objects_do_not_depend_on_the_window() {
    let rig = Rig::new().await;
    let w = rig.wrapper(encrypting(false)).await;
    let body = b"sealed body".to_vec();
    let meta = FileMetadata::new_passthrough(
        "s.txt".into(),
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&body)),
        hex::encode(<md5::Md5 as md5::Digest>::digest(&body)),
        body.len() as u64,
        None,
    );
    w.put_passthrough("releases", "p", "s.txt", Bytes::from(body.clone()), &meta)
        .await
        .unwrap();
    assert_eq!(get(&w, "s.txt").await.unwrap(), body, "window closed");
    w.open_conversion("releases", 7).await.unwrap();
    assert_eq!(get(&w, "s.txt").await.unwrap(), body, "window open");
    let proof = crate::maintenance::worker::ConversionReadProof::for_tests();
    conversion::as_conversion_job(proof, async {
        assert_eq!(get(&w, "s.txt").await.unwrap(), body, "the job");
    })
    .await;
}

/// A node that caches the window as closed serves history at once when
/// another node opened it: the refusal reads the marker again.
#[tokio::test]
async fn a_refused_read_picks_up_a_window_opened_elsewhere() {
    let rig = Rig::new().await;
    let a = rig.wrapper(encrypting(false)).await;
    let b = rig.wrapper(encrypting(false)).await;
    assert!(refused(head(&b, "old.txt").await), "caches Closed");
    a.open_conversion("releases", 7).await.unwrap();
    assert!(head(&b, "old.txt").await.is_ok());
}

/// A marker past its deadline is closed for clients; the job can still
/// extend it by identity, with its original cutoff, and clients read
/// again.
#[tokio::test]
async fn an_expired_window_refuses_until_a_resume_extends_it() {
    let rig = Rig::new().await;
    let w = rig.wrapper(encrypting(false)).await;
    let run = w.open_conversion_secs("releases", 1).await.unwrap();
    let conversion::MarkerState::Ours { fields, .. } =
        w.conversion_status("releases").await.unwrap()
    else {
        panic!("ours")
    };
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    let fresh = rig.wrapper(encrypting(false)).await;
    assert!(refused(head(&fresh, "old.txt").await), "past the deadline");
    let ext = fresh
        .extend_conversion("releases", &run, chrono::Utc::now().timestamp() + 600)
        .await
        .unwrap();
    assert!(
        matches!(ext, conversion::ByIdentity::Done(Some(f), _) if f.cutoff == fields.cutoff),
        "{ext:?}"
    );
    assert!(head(&fresh, "old.txt").await.is_ok());
    assert!(refused(head(&fresh, "new.txt").await));
}

async fn collect_all(stream: crate::storage::traits::ByteStream) -> Vec<u8> {
    use futures::TryStreamExt;
    let parts: Vec<Bytes> = stream.try_collect().await.unwrap();
    parts.concat()
}

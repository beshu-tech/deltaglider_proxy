// SPDX-License-Identifier: BUSL-1.1

//! The foreign-key sentinel at the wrapper (`sentinel.rs`): several proxies,
//! each a wrapper with its own config and sentinel over ONE data directory,
//! as two proxies with separate configs share one stored bucket. The pure
//! rules (claim action, TTLs, backoff) are unit-tested in `sentinel.rs`;
//! the HTTP answer (400 with the key label only) in `api::errors`; two live
//! proxies on MinIO in `tests/encryption_test.rs`.

use super::*;
use crate::storage::filesystem::fault::{Fault, FaultPoint, FaultyFs};
use crate::storage::filesystem::FilesystemBackend;
use std::path::Path;

type Proxy = EncryptingBackend<FaultyFs>;
type Cfg = Arc<ArcSwap<EncryptionConfig>>;

fn key(byte: u8) -> EncryptionKey {
    EncryptionKey::from_hex(&format!("{byte:02x}").repeat(32)).unwrap()
}

fn label(byte: u8) -> String {
    fingerprint_label(&key(byte))
}

/// A config that writes with key `k` (`None`: plaintext) and holds the
/// read-only keys `ring`.
fn config(k: Option<u8>, ring: &[u8]) -> EncryptionConfig {
    EncryptionConfig {
        key: k.map(key),
        key_id: k.map(|b| format!("k{b:02x}")),
        keyring: ring
            .iter()
            .map(|b| (format!("old{b:02x}"), key(*b)))
            .collect(),
        ..Default::default()
    }
}

/// A proxy with the sentinel over `dir`.
async fn proxy(dir: &Path, k: Option<u8>, ring: &[u8]) -> (Proxy, Cfg) {
    let cfg = Arc::new(ArcSwap::new(Arc::new(config(k, ring))));
    let fs = FaultyFs::new(FilesystemBackend::new(dir.to_path_buf()).await.unwrap());
    let p = EncryptingBackend::new(fs, cfg.clone()).with_sentinel(Arc::new(KeySentinel::default()));
    (p, cfg)
}

/// A one-byte passthrough write of `name` at the root of `bucket`.
async fn put(p: &Proxy, bucket: &str, name: &str) -> Result<(), StorageError> {
    let meta =
        FileMetadata::new_passthrough(name.into(), "ab".repeat(32), "cd".repeat(16), 1, None);
    p.put_passthrough(bucket, "", name, bytes::Bytes::from_static(b"x"), &meta)
        .await
}

fn claim_of(dir: &Path, bucket: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(bucket).join(".dg-key-claim")).ok()
}

/// The key that a refusal names, or a panic with the error.
fn refused_key(r: Result<(), StorageError>) -> (String, String) {
    match r {
        Err(StorageError::KeyNotHeld {
            key,
            write: true,
            detail,
        }) => (key, detail),
        other => panic!("expected a KeyNotHeld write refusal, got {other:?}"),
    }
}

/// The report that led to the sentinel: proxy A encrypts `releases`, proxy
/// B has no key. B's probe finds A's seal before B's first write and
/// refuses every write shape; B still deletes. Once B has A's key as a
/// read-only keyring entry, B writes again at once.
#[tokio::test]
async fn a_keyless_proxy_refuses_to_write_a_bucket_that_another_proxy_encrypts() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = proxy(dir.path(), Some(0xa1), &[]).await;
    a.create_bucket("releases").await.unwrap();
    a.create_bucket("downloads").await.unwrap();
    put(&a, "releases", "app.zip").await.unwrap();

    let (b, b_cfg) = proxy(dir.path(), None, &[]).await;
    let (k, detail) = refused_key(put(&b, "releases", "plain.txt").await);
    assert_eq!(k, label(0xa1));
    assert!(detail.contains("without proxy encryption"), "{detail}");
    assert!(b
        .get_passthrough_metadata("releases", "", "plain.txt")
        .await
        .is_err());
    refused_key(b.put_directory_marker("releases", "dir/").await);
    // A bucket without encrypted objects is written in plaintext as before.
    put(&b, "downloads", "plain.txt").await.unwrap();
    // A delete is not a write the sentinel stops.
    b.delete_passthrough("releases", "", "app.zip")
        .await
        .unwrap();

    b_cfg.store(Arc::new(config(None, &[0xa1])));
    put(&b, "releases", "plain.txt").await.unwrap();
}

/// A read of a sealed object is a sighting: a bucket whose sample held no
/// seal is refused once a read meets one.
#[tokio::test]
async fn a_read_of_a_foreign_seal_refuses_later_writes() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = proxy(dir.path(), Some(0xa1), &[]).await;
    a.create_bucket("releases").await.unwrap();
    let (b, _) = proxy(dir.path(), None, &[]).await;
    // The probe of an empty bucket finds nothing; no claim exists yet.
    put(&b, "releases", "first.txt").await.unwrap();
    // A claims and writes; B's cached verdict (no claim) still holds.
    put(&a, "releases", "sealed.bin").await.unwrap();
    put(&b, "releases", "second.txt").await.unwrap();
    let read = b
        .get_passthrough_metadata("releases", "", "sealed.bin")
        .await;
    assert!(
        matches!(read, Err(StorageError::KeyNotHeld { write: false, .. })),
        "{read:?}"
    );
    let (_, detail) = refused_key(put(&b, "releases", "third.txt").await);
    assert!(detail.contains("sealed.bin"), "{detail}");
}

/// Two proxies with different keys start on one EMPTY bucket at the same
/// moment. No object exists for a sample to find, so only the claim can
/// decide: exactly one of the two first writes succeeds.
#[tokio::test]
async fn two_keys_racing_on_an_empty_bucket_leave_one_key() {
    for _ in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let (a, _) = proxy(dir.path(), Some(0xa1), &[]).await;
        let (b, _) = proxy(dir.path(), Some(0xb2), &[]).await;
        a.create_bucket("releases").await.unwrap();
        let (ra, rb) = tokio::join!(put(&a, "releases", "a.bin"), put(&b, "releases", "b.bin"));
        assert!(ra.is_ok() != rb.is_ok(), "exactly one wins: {ra:?} {rb:?}");
        let (winner, loser, lost) = if ra.is_ok() {
            (0xa1, &b, rb)
        } else {
            (0xb2, &a, ra)
        };
        assert_eq!(claim_of(dir.path(), "releases").unwrap(), label(winner));
        assert_eq!(refused_key(lost).0, label(winner));
        assert_eq!(
            refused_key(put(loser, "releases", "again.bin").await).0,
            label(winner)
        );
    }
}

/// The claim does not depend on what a sample of the bucket finds: with
/// the encrypted objects gone, a proxy with another key and a proxy without
/// a key are still refused; one that holds the key read-only writes.
#[tokio::test]
async fn the_claim_refuses_without_any_encrypted_object_in_sight() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = proxy(dir.path(), Some(0xa1), &[]).await;
    a.create_bucket("releases").await.unwrap();
    put(&a, "releases", "sealed.bin").await.unwrap();
    a.delete_passthrough("releases", "", "sealed.bin")
        .await
        .unwrap();
    assert_eq!(claim_of(dir.path(), "releases").unwrap(), label(0xa1));

    for k in [Some(0xb2), None] {
        let (other, _) = proxy(dir.path(), k, &[]).await;
        let (key, detail) = refused_key(put(&other, "releases", "x.bin").await);
        assert_eq!(key, label(0xa1), "{k:?}");
        assert!(detail.contains("key claim"), "{detail}");
    }
    let (reader, _) = proxy(dir.path(), None, &[0xa1]).await;
    put(&reader, "releases", "x.bin").await.unwrap();
}

/// A key rotation moves the claim to the new key (the old one is still in
/// the ring); a proxy that has only the old key is then refused, because it
/// cannot read what the rotated proxy writes.
#[tokio::test]
async fn a_rotation_moves_the_claim_and_refuses_the_old_key_only_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let (a, a_cfg) = proxy(dir.path(), Some(0xa1), &[]).await;
    a.create_bucket("releases").await.unwrap();
    put(&a, "releases", "v1.bin").await.unwrap();

    a_cfg.store(Arc::new(config(Some(0xa2), &[0xa1])));
    put(&a, "releases", "v2.bin").await.unwrap();
    assert_eq!(claim_of(dir.path(), "releases").unwrap(), label(0xa2));

    let (old_only, _) = proxy(dir.path(), Some(0xa1), &[]).await;
    assert_eq!(
        refused_key(put(&old_only, "releases", "x.bin").await).0,
        label(0xa2)
    );
}

/// The owner turns encryption off (it keeps the key to read the history):
/// its next write removes the claim, and a bucket without a claim and
/// without encrypted objects is open to a proxy without a key.
#[tokio::test]
async fn the_owner_that_stops_encrypting_removes_the_claim() {
    let dir = tempfile::tempdir().unwrap();
    let (a, a_cfg) = proxy(dir.path(), Some(0xa1), &[]).await;
    a.create_bucket("releases").await.unwrap();
    put(&a, "releases", "sealed.bin").await.unwrap();
    a.delete_passthrough("releases", "", "sealed.bin")
        .await
        .unwrap();

    a_cfg.store(Arc::new(config(None, &[0xa1])));
    put(&a, "releases", "plain.bin").await.unwrap();
    assert_eq!(claim_of(dir.path(), "releases"), None);

    let (keyless, _) = proxy(dir.path(), None, &[]).await;
    put(&keyless, "releases", "other.bin").await.unwrap();
}

/// A writer never claims a bucket that already holds another key (an
/// earlier release encrypted it, so it has no claim): the sample refuses
/// first, and the bucket stays unclaimed.
#[tokio::test]
async fn a_writer_does_not_claim_a_bucket_that_holds_another_key() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = proxy(dir.path(), Some(0xa1), &[]).await;
    a.create_bucket("releases").await.unwrap();
    put(&a, "releases", "sealed.bin").await.unwrap();
    std::fs::remove_file(dir.path().join("releases").join(".dg-key-claim")).unwrap();

    let (b, _) = proxy(dir.path(), Some(0xb2), &[]).await;
    let (_, detail) = refused_key(put(&b, "releases", "x.bin").await);
    assert!(detail.contains("sealed.bin"), "{detail}");
    assert_eq!(claim_of(dir.path(), "releases"), None);
}

/// Review finding: a bucket in `WriteMode::Refuse` (proxy-AES without a
/// key) refused its folder markers and metadata rewrites through the
/// sentinel, and probed first. The sentinel leaves it alone.
#[tokio::test]
async fn a_bucket_in_refuse_mode_is_not_the_sentinels_business() {
    let dir = tempfile::tempdir().unwrap();
    let (p, cfg) = proxy(dir.path(), None, &[]).await;
    p.create_bucket("releases").await.unwrap();
    cfg.store(Arc::new(EncryptionConfig {
        write_mode: WriteMode::Refuse,
        ..Default::default()
    }));
    p.put_directory_marker("releases", "dir/").await.unwrap();
    let calls = |pt| p.inner.faults.calls(pt, "releases//");
    assert_eq!(calls(FaultPoint::ListObjectsDelegated), 0);
    assert_eq!(calls(FaultPoint::ReadKeyClaim), 0);
}

/// The background writers (replication, lifecycle, maintenance) ask
/// `write_refusal`: it reports the sentinel's cached verdict, so they defer
/// instead of failing object after object.
#[tokio::test]
async fn write_refusal_reports_the_sentinels_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = proxy(dir.path(), Some(0xa1), &[]).await;
    a.create_bucket("releases").await.unwrap();
    put(&a, "releases", "sealed.bin").await.unwrap();
    let (b, _) = proxy(dir.path(), None, &[]).await;
    assert_eq!(b.write_refusal("releases"), None, "no evidence yet");
    refused_key(put(&b, "releases", "x.bin").await);
    let why = b.write_refusal("releases").expect("cached verdict");
    assert!(why.contains(&label(0xa1)), "{why}");
}

/// The claim does not keep an emptied bucket, and a new bucket of the same
/// name starts without the old one's evidence or claim.
#[tokio::test]
async fn delete_bucket_removes_the_claim_and_forgets_the_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = proxy(dir.path(), Some(0xa1), &[]).await;
    a.create_bucket("releases").await.unwrap();
    put(&a, "releases", "sealed.bin").await.unwrap();
    let (b, _) = proxy(dir.path(), Some(0xb2), &[]).await;
    refused_key(put(&b, "releases", "x.bin").await);

    a.delete_passthrough("releases", "", "sealed.bin")
        .await
        .unwrap();
    b.delete_bucket("releases").await.unwrap();
    assert!(!dir.path().join("releases").exists());
    b.create_bucket("releases").await.unwrap();
    put(&b, "releases", "x.bin").await.unwrap();
    assert_eq!(claim_of(dir.path(), "releases").unwrap(), label(0xb2));
}

/// The cost of the sentinel: many writes to one bucket send ONE probe
/// listing, ONE claim read and ONE claim write, also when they start at
/// the same moment (one exchange per bucket at a time).
#[tokio::test]
async fn many_writes_cost_one_probe_and_one_claim_exchange() {
    let dir = tempfile::tempdir().unwrap();
    let (p, _) = proxy(dir.path(), Some(0xa1), &[]).await;
    p.create_bucket("releases").await.unwrap();
    let names: Vec<String> = (0..16).map(|i| format!("o{i}.bin")).collect();
    let writes = names.iter().map(|n| put(&p, "releases", n));
    for r in futures::future::join_all(writes).await {
        r.unwrap();
    }
    let calls = |pt| p.inner.faults.calls(pt, "releases//");
    assert_eq!(calls(FaultPoint::ListObjectsDelegated), 1);
    assert_eq!(calls(FaultPoint::ReadKeyClaim), 1);
    assert_eq!(calls(FaultPoint::WriteKeyClaim), 1);
}

/// Review finding: a claim write that fails (a bucket policy that refuses
/// it) was retried by every write. The write goes ahead (fail open) and
/// the claim is tried again only after the backoff.
#[tokio::test]
async fn a_failing_claim_write_is_not_retried_by_every_write() {
    let dir = tempfile::tempdir().unwrap();
    let (p, _) = proxy(dir.path(), Some(0xa1), &[]).await;
    p.create_bucket("releases").await.unwrap();
    p.inner
        .faults
        .arm(FaultPoint::WriteKeyClaim, "releases//", Fault::Throttled);
    for i in 0..5 {
        put(&p, "releases", &format!("o{i}.bin")).await.unwrap();
    }
    assert_eq!(
        p.inner
            .faults
            .fired(FaultPoint::WriteKeyClaim, "releases//"),
        1
    );
    assert_eq!(claim_of(dir.path(), "releases"), None);
}

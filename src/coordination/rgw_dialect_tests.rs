// SPDX-License-Identifier: BUSL-1.1

//! Conditional requests against a backend that answers them as Ceph RGW
//! does (Hetzner Object Storage, hel1, measured 2026-11-01): a PUT compares
//! `If-Match` with the BARE ETag (the correct ETag, quoted as HEAD returns
//! it, gets 412), and a DELETE ignores `If-Match`. The fake models it
//! ([`crate::storage::FakeS3::set_rgw_conditions`]).
//!
//! Before the dialect existed, the probe called such a backend non-CAS
//! (encrypted buckets read-only, the coordination bucket refused), and
//! every `If-Match` write on it reported a lost race.

use super::cas_probe::{probe_cas, ProbeResult};
use super::if_match::{ConditionalProfile, IfMatchDialect};
use super::lease::{CoordinationLease, LeaseSubsystem};
use super::reference_lock::{ReferenceLock, S3ReferenceLock};
use super::s3_lease::S3Lease;
use crate::config::BackendConfig;
use crate::storage::{FakeS3, ObjectVariant, StorageBackend};
use std::sync::Arc;

fn def(endpoint: &str) -> BackendConfig {
    BackendConfig::S3 {
        endpoint: Some(endpoint.to_string()),
        region: "us-east-1".into(),
        force_path_style: true,
        access_key_id: Some("k".into()),
        secret_access_key: Some("s".into()),
        allow_local: true,
        session_token: None,
    }
}

/// A fake that answers conditions as RGW does, and a client on it.
async fn rgw() -> (String, aws_sdk_s3::Client, Arc<FakeS3>) {
    let (endpoint, fake) = crate::storage::fake_s3::start().await;
    fake.set_rgw_conditions(true);
    let client = crate::config_db_sync::ConfigDbSync::build_client(&def(&endpoint))
        .await
        .unwrap();
    (endpoint, client, fake)
}

async fn put(client: &aws_sdk_s3::Client, key: &str, body: &'static [u8]) -> String {
    client
        .put_object()
        .bucket("b")
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from_static(body))
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string()
}

async fn exists(client: &aws_sdk_s3::Client, key: &str) -> bool {
    client
        .get_object()
        .bucket("b")
        .key(key)
        .send()
        .await
        .is_ok()
}

/// The probe tries the unquoted form after the quoted one refused the
/// correct ETag: CAS, unquoted, and DELETE does not enforce `If-Match`.
#[tokio::test]
async fn the_probe_finds_the_unquoted_dialect() {
    let (_, client, fake) = rgw().await;
    let got = probe_cas(&client, "b", "_probe").await;
    assert_eq!(
        got,
        Ok(ProbeResult {
            cas: true,
            conditional_delete: false,
            if_match: IfMatchDialect::Unquoted,
        })
    );
    // Four more than the quoted probe's seven: the GET that tells a refusal
    // from a lost response, two unquoted PUTs and a HEAD.
    assert_eq!(fake.requests().len(), 11, "{:?}", fake.requests());
    assert!(!exists(&client, "_probe").await, "the probe key is removed");
}

/// The capability verdict carries the dialect, and the backends built
/// from the definition use it: the re-encrypt job's conditional rewrite
/// writes at the version it read, and still refuses a changed object.
#[tokio::test]
async fn a_probed_backend_rewrites_at_its_version_and_refuses_a_change() {
    use crate::storage::{PutBody, RewriteTarget, StorageBackendExt, WriteIf};
    let (endpoint, _client, fake) = rgw().await;
    let group = super::capability::ClientWritableGroup {
        backend: def(&endpoint),
        buckets: vec![],
        probe_bucket: "b".into(),
    };
    let verdict =
        super::capability::establish_backend_verdict("hetzner-fsn1", &group, &Default::default())
            .await;
    assert_eq!(
        verdict,
        super::CapabilityVerdict::CasVerified {
            via: super::VerifiedVia::Probe,
            if_match: IfMatchDialect::Unquoted,
        }
    );
    let s3 = crate::storage::S3Backend::new(
        &def(&endpoint),
        crate::storage::NativeEncryptionConfig::None,
    )
    .await
    .unwrap()
    .with_health_name("hetzner-fsn1", &def(&endpoint));
    let meta = |body: &[u8]| {
        crate::types::FileMetadata::new_passthrough(
            "k.txt".into(),
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(body)),
            hex::encode(<md5::Md5 as md5::Digest>::digest(body)),
            body.len() as u64,
            None,
        )
    };
    s3.put_passthrough(
        "b",
        "",
        "k.txt",
        bytes::Bytes::from_static(b"old"),
        &meta(b"old"),
    )
    .await
    .unwrap();
    let version = s3
        .get_passthrough_metadata("b", "", "k.txt")
        .await
        .unwrap()
        .backend_version
        .unwrap();
    let rewrite = |version: String| {
        let s3 = &s3;
        let meta = meta(b"job");
        async move {
            s3.put_if_unchanged(
                "b",
                "",
                RewriteTarget::Passthrough { name: "k.txt" },
                PutBody::Bytes(bytes::Bytes::from_static(b"job")),
                &meta,
                &version,
            )
            .await
            .unwrap()
        }
    };
    assert_eq!(rewrite(version.clone()).await, WriteIf::Written);
    assert_eq!(s3.get_passthrough("b", "", "k.txt").await.unwrap(), b"job");

    // A client writes after the job's read: Changed, the client's bytes stay.
    let version = s3
        .get_passthrough_metadata("b", "", "k.txt")
        .await
        .unwrap()
        .backend_version
        .unwrap();
    fake.replace_body("b/k.txt", b"client");
    assert_eq!(rewrite(version).await, WriteIf::Changed);
    assert_eq!(
        s3.get_passthrough("b", "", "k.txt").await.unwrap(),
        b"client"
    );
}

/// The DELETE of a pinned version reads first where DELETE may ignore
/// `If-Match`: an object that changed since the version read stays.
#[tokio::test]
async fn a_pinned_delete_reads_first_and_keeps_a_changed_object() {
    let (endpoint, client, fake) = rgw().await;
    let s3 = crate::storage::s3_test_support::for_test_endpoint(&endpoint)
        .with_conditional_profile(ConditionalProfile {
            if_match: IfMatchDialect::Unquoted,
            conditional_delete: false,
        });
    let old = put(&client, "p/a.txt", b"v1").await;
    fake.replace_body("b/p/a.txt", b"v2");
    let deleted = s3
        .delete_variant_if("b", "p", "a.txt", ObjectVariant::Passthrough, &old)
        .await
        .unwrap();
    assert!(!deleted, "a changed object is not deleted");
    assert!(exists(&client, "p/a.txt").await, "the newer object stays");

    let now = s3
        .variant_version("b", "p", "a.txt", ObjectVariant::Passthrough)
        .await
        .unwrap()
        .unwrap();
    assert!(s3
        .delete_variant_if("b", "p", "a.txt", ObjectVariant::Passthrough, &now)
        .await
        .unwrap());
    assert!(
        !exists(&client, "p/a.txt").await,
        "the checked version goes"
    );
}

/// A backend never probed has no proven conditional DELETE either: the
/// delete reads first there too.
#[tokio::test]
async fn an_unprobed_backend_reads_before_a_pinned_delete() {
    let (endpoint, client, fake) = rgw().await;
    let s3 = crate::storage::s3_test_support::for_test_endpoint(&endpoint);
    let old = put(&client, "p/a.txt", b"v1").await;
    fake.replace_body("b/p/a.txt", b"v2");
    assert!(!s3
        .delete_variant_if("b", "p", "a.txt", ObjectVariant::Passthrough, &old)
        .await
        .unwrap());
    assert!(exists(&client, "p/a.txt").await);
}

/// THE conditional PUT of the lease, the lock and the encryption witness
/// writes on the ETag it kept, in the bucket's dialect.
#[tokio::test]
async fn the_coordination_put_replaces_by_the_kept_etag() {
    let (_, client, _) = rgw().await;
    let body = || aws_sdk_s3::primitives::ByteStream::from_static(b"{}");
    let first = super::cas::conditional_put_etag(
        &client,
        "b",
        "lease.json",
        body(),
        None,
        IfMatchDialect::Unquoted,
    )
    .await
    .unwrap()
    .expect("created");
    let second = super::cas::conditional_put_etag(
        &client,
        "b",
        "lease.json",
        body(),
        Some(&first),
        IfMatchDialect::Unquoted,
    )
    .await
    .unwrap();
    assert!(second.is_some(), "the replace by the kept ETag is written");
    let stale = super::cas::conditional_put_etag(
        &client,
        "b",
        "lease.json",
        aws_sdk_s3::primitives::ByteStream::from_static(b"{\"x\":1}"),
        Some("\"0000\""),
        IfMatchDialect::Unquoted,
    )
    .await
    .unwrap();
    assert_eq!(stale, None, "a wrong ETag still loses");
}

/// The lease on a probed RGW bucket: acquire, renew, release, and a peer
/// takes it after the release.
#[tokio::test]
async fn a_lease_round_trip() {
    let (_, client, _) = rgw().await;
    let profile = probe_cas(&client, "b", "_probe").await.unwrap().profile();
    let lease = S3Lease::new(client.clone(), "b".into(), "node".into()).with_profile(profile);
    let sub = LeaseSubsystem::Replication;
    let now = crate::event_outbox::current_unix_seconds();
    assert!(lease.try_acquire(sub, "r", "o1", now, 300).await.unwrap());
    assert!(!lease.try_acquire(sub, "r", "o2", now, 300).await.unwrap());
    lease
        .renew(sub, "r", "o1", now, 300)
        .await
        .expect("the renew writes by the lease's ETag");
    lease.release(sub, "r", "o1").await.unwrap();
    assert!(!lease.is_held(sub, "r", now).await.unwrap(), "released");
    assert!(lease.try_acquire(sub, "r", "o2", now, 300).await.unwrap());
}

/// The `expires_at` of the lock object `key` of bucket `b`.
async fn lock_expiry(client: &aws_sdk_s3::Client, key: &str) -> i64 {
    let out = client
        .get_object()
        .bucket("b")
        .key(key)
        .send()
        .await
        .unwrap();
    let body = out.body.collect().await.unwrap().into_bytes();
    let lock: serde_json::Value = serde_json::from_slice(&body).unwrap();
    lock["expires_at"].as_i64().unwrap()
}

/// The reference lock on a probed RGW bucket: acquire, a renew that
/// really moves the expiry (a renew whose `If-Match` the backend refused
/// read the unchanged lock as its own and reported success), a steal of a
/// lapsed lock, and a stale holder's release that keeps the peer's lock
/// (DELETE ignores `If-Match`, so the release reads first).
#[tokio::test]
async fn a_reference_lock_round_trip() {
    let (_, client, _) = rgw().await;
    let profile = probe_cas(&client, "b", "_probe").await.unwrap().profile();
    assert!(!profile.conditional_delete);
    let lock = S3ReferenceLock::new(client.clone(), "b".into(), "node".into())
        .with_tunables(60, 2)
        .with_profile(profile);
    let now = crate::event_outbox::current_unix_seconds();
    assert!(lock.try_acquire("ds", "o1", now).await.unwrap());
    assert!(!lock.try_acquire("ds", "o2", now).await.unwrap());
    assert_eq!(lock_expiry(&client, "ds").await, now + 60);
    lock.renew("ds", "o1", now + 30)
        .await
        .expect("the renew writes by the lock's ETag");
    assert_eq!(
        lock_expiry(&client, "ds").await,
        now + 90,
        "the renew moved the expiry"
    );
    lock.release("ds", "o1").await.unwrap();
    assert!(lock.try_acquire("ds", "o2", now).await.unwrap(), "released");
    // o1 holds nothing now: its release must leave o2's lock.
    lock.release("ds", "o1").await.unwrap();
    assert!(
        !lock.try_acquire("ds", "o3", now).await.unwrap(),
        "a stale release deleted the peer's lock"
    );

    // A lapsed lock is stolen with If-Match on its ETag.
    let lapsed = serde_json::json!({
        "owner": "gone", "node_id": "node", "epoch": 1, "expires_at": now - 1
    });
    client
        .put_object()
        .bucket("b")
        .key("lapsed")
        .body(aws_sdk_s3::primitives::ByteStream::from(
            serde_json::to_vec(&lapsed).unwrap(),
        ))
        .send()
        .await
        .unwrap();
    assert!(
        lock.try_acquire("lapsed", "o4", now).await.unwrap(),
        "a lapsed lock is stolen"
    );
}

/// The config DB sync on an RGW coordination bucket: the validation finds
/// the dialect, and the second upload (`If-Match` on the first one's ETag)
/// is written, not a conflict.
#[tokio::test]
async fn the_config_sync_uploads_twice() {
    let (endpoint, _, _) = rgw().await;
    let dir = tempfile::tempdir().unwrap();
    let key = "k".repeat(40);
    let sync = crate::config_db_sync::ConfigDbSync::new(
        &def(&endpoint),
        "b".into(),
        "k.db".into(),
        dir.path().join("deltaglider_config.db"),
        crate::config_db::ConfigDbKeys::primary_only(&key),
        false,
    )
    .await
    .unwrap();
    let v = sync.validate_coordination_bucket().await.unwrap();
    assert_eq!(v.profile().if_match, IfMatchDialect::Unquoted);
    sync.upload(b"one".to_vec()).await.unwrap();
    sync.upload(b"two".to_vec())
        .await
        .expect("the second upload replaces the first by its ETag");
}

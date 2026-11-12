// SPDX-License-Identifier: BUSL-1.1

use super::*;
use crate::deltaglider::ingest::{BodyHashes, IngestedBody};
use crate::deltaglider::spool::{mib_ceil, CONTENDED};
use bytes::Bytes;

const MIB: u64 = 1024 * 1024;

/// `data` as an ingested part body in `store`'s spool, the way the ingest
/// of an UploadPart makes one: a reservation that covers the file only,
/// taken without waiting beside the `held_mib` that the upload holds.
pub(super) fn spooled(
    store: &MultipartStore,
    data: &[u8],
    held_mib: usize,
) -> Result<IngestedBody, S3Error> {
    let size = data.len() as u64;
    let storage = store.spool().try_reserve(size, held_mib).map_err(|e| {
        assert_eq!(e.kind(), CONTENDED);
        S3Error::SlowDown(e.to_string())
    })?;
    if storage.reserved_mib() < mib_ceil(size) {
        return Err(S3Error::SlowDown("reservation below the part".into()));
    }
    let spool = storage.file().unwrap();
    std::fs::write(spool.path(), data).unwrap();
    Ok(IngestedBody::Spooled {
        spool,
        size,
        hashes: BodyHashes::of(data, None),
        storage,
    })
}

/// One UploadPart as the adapter runs it: admit the declared size, ingest
/// the body beside what the upload holds, keep it as a relay part.
pub(super) fn put(
    store: &MultipartStore,
    upload_id: &str,
    bucket: &str,
    key: &str,
    part_number: u32,
    data: impl AsRef<[u8]>,
) -> Result<String, S3Error> {
    let data = data.as_ref();
    let admission = store.admit_part(upload_id, bucket, key, part_number, data.len() as u64)?;
    let body = spooled(store, data, admission.held_spool_mib)?;
    store.upload_part(upload_id, bucket, key, part_number, body, admission)
}

/// The object a completion stores: its relay parts, joined.
fn joined(completed: &CompletedParts) -> Vec<u8> {
    completed
        .parts
        .iter()
        .flat_map(|p| std::fs::read(&p.path).unwrap())
        .collect()
}

fn parts_fixture() -> Vec<(u32, String)> {
    vec![(1, "\"etag1\"".to_string()), (2, "\"etag2\"".to_string())]
}

#[tokio::test]
async fn completion_registry_owner_join_tombstone_lifecycle() {
    let store = std::sync::Arc::new(MultipartStore::new(1024));
    let parts = parts_fixture();
    let up1 = store.create("b", "k", None, Default::default()).unwrap();

    // First request owns the completion.
    let owner = match store.begin_complete(&up1, "b", "k", &parts).unwrap() {
        BeginComplete::Owner(p) => p,
        _ => panic!("first begin_complete must be Owner"),
    };
    // An identical retry while in flight joins.
    let rx = match store.begin_complete(&up1, "b", "k", &parts).unwrap() {
        BeginComplete::Join(rx) => rx,
        _ => panic!("identical retry must Join"),
    };
    // A different part list while in flight is refused.
    let other = vec![(1, "\"other\"".to_string())];
    assert!(store.begin_complete(&up1, "b", "k", &other).is_err());

    // Owner publishes success: joiner sees the etag, later retries hit the tombstone.
    owner.publish(Ok("\"final-etag\"".to_string()));
    assert_eq!(rx.borrow().clone().unwrap().unwrap(), "\"final-etag\"");
    match store.begin_complete(&up1, "b", "k", &parts).unwrap() {
        BeginComplete::AlreadyDone { etag } => assert_eq!(etag, "\"final-etag\""),
        _ => panic!("retry after success must hit the tombstone"),
    }
    // Tombstone with a different part list is refused; different bucket/key
    // too. The completed upload is gone, so the answer is NoSuchUpload.
    for (bucket, parts) in [("b", &other), ("OTHER", &parts)] {
        assert!(matches!(
            store.begin_complete(&up1, bucket, "k", parts),
            Err(S3Error::NoSuchUpload(_))
        ));
    }
}

#[tokio::test]
async fn completion_registry_failure_clears_slot_for_fresh_retry() {
    let store = std::sync::Arc::new(MultipartStore::new(1024));
    let parts = parts_fixture();
    let up2 = store.create("b", "k", None, Default::default()).unwrap();
    let owner = match store.begin_complete(&up2, "b", "k", &parts).unwrap() {
        BeginComplete::Owner(p) => p,
        _ => panic!("must be Owner"),
    };
    owner.publish(Err(CompletionFailure::internal("backend exploded")));
    // Failure clears the slot: the next attempt is a fresh Owner, not a join.
    assert!(matches!(
        store.begin_complete(&up2, "b", "k", &parts).unwrap(),
        BeginComplete::Owner(_)
    ));
}

/// A joined retry answers the owner's S3 error, not a 500 (s3surface-17).
#[tokio::test]
async fn completion_joiner_sees_the_owners_error() {
    let store = std::sync::Arc::new(MultipartStore::new(1024));
    let parts = parts_fixture();
    let up4 = store.create("b", "k", None, Default::default()).unwrap();
    let owner = match store.begin_complete(&up4, "b", "k", &parts).unwrap() {
        BeginComplete::Owner(p) => p,
        _ => panic!("must be Owner"),
    };
    let rx = match store.begin_complete(&up4, "b", "k", &parts).unwrap() {
        BeginComplete::Join(rx) => rx,
        _ => panic!("must Join"),
    };
    let owner_error = s3s::S3Error::with_message(s3s::S3ErrorCode::EntityTooSmall, "tiny");
    owner.publish(Err(CompletionFailure::of(&owner_error)));
    let seen = rx.borrow().clone().unwrap().unwrap_err().to_s3s();
    assert_eq!(seen.code(), &s3s::S3ErrorCode::EntityTooSmall);
    assert_eq!(seen.message(), Some("tiny"));
}

#[tokio::test]
async fn completion_publisher_drop_without_publish_unwedges_registry() {
    let store = std::sync::Arc::new(MultipartStore::new(1024));
    let parts = parts_fixture();
    let up3 = store.create("b", "k", None, Default::default()).unwrap();
    let owner = match store.begin_complete(&up3, "b", "k", &parts).unwrap() {
        BeginComplete::Owner(p) => p,
        _ => panic!("must be Owner"),
    };
    let rx = match store.begin_complete(&up3, "b", "k", &parts).unwrap() {
        BeginComplete::Join(rx) => rx,
        _ => panic!("must Join"),
    };
    drop(owner); // task panicked / was aborted without publishing
    assert!(
        rx.borrow().clone().unwrap().is_err(),
        "joiner must be released"
    );
    assert!(matches!(
        store.begin_complete(&up3, "b", "k", &parts).unwrap(),
        BeginComplete::Owner(_)
    ));
}

/// F33 (epic #143): a Complete for another bucket or key joined an
/// in-flight completion on its upload id and its part list alone, and got
/// 200 with the owner's ETag while nothing was written under its name. It
/// is NoSuchUpload, like the same request after the completion (`Done`).
#[tokio::test]
async fn joining_an_inflight_completion_checks_bucket_and_key() {
    let store = std::sync::Arc::new(MultipartStore::new(1024));
    let parts = parts_fixture();
    let up5 = store.create("b", "k", None, Default::default()).unwrap();
    let _owner = match store.begin_complete(&up5, "b", "k", &parts).unwrap() {
        BeginComplete::Owner(p) => p,
        _ => panic!("must be Owner"),
    };
    for (bucket, key) in [("other", "k"), ("b", "other")] {
        assert!(
            matches!(
                store.begin_complete(&up5, bucket, key, &parts),
                Err(S3Error::NoSuchUpload(_))
            ),
            "a Complete of {bucket}/{key} joined the completion of b/k"
        );
    }
    assert!(matches!(
        store.begin_complete(&up5, "b", "k", &parts).unwrap(),
        BeginComplete::Join(_)
    ));
}

/// Review of #166: with no completion in flight, any Complete became the
/// owner, also for an unknown upload id or another bucket or key, and held
/// the owner window until `complete_parts` refused it. Only the upload's
/// own bucket and key may own its completion.
#[tokio::test]
async fn only_the_uploads_own_key_may_own_its_completion() {
    let store = std::sync::Arc::new(MultipartStore::new(1024));
    let parts = parts_fixture();
    let id = store.create("b", "k", None, Default::default()).unwrap();
    for (id, bucket, key) in [
        ("no-such-id", "b", "k"),
        (id.as_str(), "b", "other"),
        (id.as_str(), "other", "k"),
    ] {
        assert!(
            matches!(
                store.begin_complete(id, bucket, key, &parts),
                Err(S3Error::NoSuchUpload(_))
            ),
            "{id} {bucket}/{key} became the owner"
        );
    }
    assert!(matches!(
        store.begin_complete(&id, "b", "k", &parts).unwrap(),
        BeginComplete::Owner(_)
    ));
}

/// What the registry model says one `begin_complete` answers.
#[derive(Debug, Clone, PartialEq)]
enum ModelAnswer {
    Owner,
    Join,
    AlreadyDone(String),
    NoSuchUpload,
    InvalidRequest,
}

/// The model of one upload id's completion slot.
#[derive(Debug, Clone)]
enum ModelSlot {
    InFlight {
        parts: usize,
        bucket: usize,
        key: usize,
    },
    Done {
        parts: usize,
        bucket: usize,
        key: usize,
        etag: String,
    },
}

#[derive(Debug, Clone)]
enum RegistryOp {
    Begin {
        id: usize,
        parts: usize,
        bucket: usize,
        key: usize,
    },
    PublishOk {
        id: usize,
    },
    PublishErr {
        id: usize,
    },
    DropOwner {
        id: usize,
    },
}

fn registry_op() -> impl proptest::strategy::Strategy<Value = RegistryOp> {
    use proptest::prelude::*;
    prop_oneof![
        3 => (0..3usize, 0..2usize, 0..2usize, 0..2usize)
            .prop_map(|(id, parts, bucket, key)| RegistryOp::Begin { id, parts, bucket, key }),
        1 => (0..3usize).prop_map(|id| RegistryOp::PublishOk { id }),
        1 => (0..3usize).prop_map(|id| RegistryOp::PublishErr { id }),
        1 => (0..3usize).prop_map(|id| RegistryOp::DropOwner { id }),
    ]
}

proptest::proptest! {
    /// Model test of the completion registry (F33): over any sequence of
    /// Completes (upload 0 of b0/k0, upload 1 of b1/k1, an unknown id 2; two
    /// part lists, two buckets, two keys), publishes and dropped owners,
    /// `begin_complete` answers what the model says. A request owns a
    /// completion only for its upload's own bucket and key, and joins or
    /// hits the tombstone ONLY when bucket, key and part list all match.
    #[test]
    fn begin_complete_follows_the_registry_model(
        ops in proptest::collection::vec(registry_op(), 1..40)
    ) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _rt = rt.enter();
        let store = std::sync::Arc::new(MultipartStore::new(1024));
        let part_lists = [parts_fixture(), vec![(1, "\"other\"".to_string())]];
        let names = [["b0", "b1"], ["k0", "k1"]];
        // The upload records: (bucket, key) of ids 0 and 1; id 2 is unknown.
        let records = [Some((0usize, 0usize)), Some((1, 1)), None];
        let ids: Vec<String> = records
            .iter()
            .enumerate()
            .map(|(i, r)| match r {
                Some((b, k)) => store
                    .create(names[0][*b], names[1][*k], None, Default::default())
                    .unwrap(),
                None => format!("unknown-{i}"),
            })
            .collect();
        let id_of = |id: usize| ids[id].clone();
        let mut model: [Option<ModelSlot>; 3] = [None, None, None];
        let mut owners: [Option<CompletionPublisher>; 3] = [None, None, None];
        let mut published = 0;
        for op in ops {
            match op {
                RegistryOp::Begin { id, parts, bucket, key } => {
                    let want = match &model[id] {
                        None if records[id] == Some((bucket, key)) => ModelAnswer::Owner,
                        None => ModelAnswer::NoSuchUpload,
                        Some(ModelSlot::InFlight { parts: p, bucket: b, key: k }) => {
                            if (*b, *k) != (bucket, key) {
                                ModelAnswer::NoSuchUpload
                            } else if *p != parts {
                                ModelAnswer::InvalidRequest
                            } else {
                                ModelAnswer::Join
                            }
                        }
                        Some(ModelSlot::Done { parts: p, bucket: b, key: k, etag }) => {
                            if (*p, *b, *k) == (parts, bucket, key) {
                                ModelAnswer::AlreadyDone(etag.clone())
                            } else {
                                ModelAnswer::NoSuchUpload
                            }
                        }
                    };
                    let got = match store.begin_complete(
                        &id_of(id),
                        names[0][bucket],
                        names[1][key],
                        &part_lists[parts],
                    ) {
                        Ok(BeginComplete::Owner(p)) => {
                            owners[id] = Some(p);
                            model[id] = Some(ModelSlot::InFlight { parts, bucket, key });
                            ModelAnswer::Owner
                        }
                        Ok(BeginComplete::Join(_)) => ModelAnswer::Join,
                        Ok(BeginComplete::AlreadyDone { etag }) => ModelAnswer::AlreadyDone(etag),
                        Err(S3Error::NoSuchUpload(_)) => ModelAnswer::NoSuchUpload,
                        Err(S3Error::InvalidRequest(_)) => ModelAnswer::InvalidRequest,
                        Err(e) => panic!("unexpected {e:?}"),
                    };
                    proptest::prop_assert_eq!(got, want);
                }
                RegistryOp::PublishOk { id } => {
                    if let Some(owner) = owners[id].take() {
                        published += 1;
                        let etag = format!("\"e{published}\"");
                        owner.publish(Ok(etag.clone()));
                        let Some(ModelSlot::InFlight { parts, bucket, key }) = model[id].take()
                        else {
                            panic!("an owner without an in-flight slot");
                        };
                        model[id] = Some(ModelSlot::Done { parts, bucket, key, etag });
                    }
                }
                RegistryOp::PublishErr { id } => {
                    if let Some(owner) = owners[id].take() {
                        owner.publish(Err(CompletionFailure::internal("x")));
                        model[id] = None;
                    }
                }
                RegistryOp::DropOwner { id } => {
                    if owners[id].take().is_some() {
                        model[id] = None;
                    }
                }
            }
        }
    }
}

#[test]
fn completion_fingerprint_is_order_sensitive_and_content_sensitive() {
    let a = completion_fingerprint(&parts_fixture());
    assert_eq!(a, completion_fingerprint(&parts_fixture()));
    let reordered = vec![(2, "\"etag2\"".to_string()), (1, "\"etag1\"".to_string())];
    assert_ne!(a, completion_fingerprint(&reordered));
    let different = vec![(1, "\"etag1\"".to_string())];
    assert_ne!(a, completion_fingerprint(&different));
}

#[test]
fn test_create_and_upload_part() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();

    let data = Bytes::from(vec![0u8; 1024]);
    let etag = put(&store, &upload_id, "bucket", "key.bin", 1, data).unwrap();
    assert!(etag.starts_with('"'));
    assert!(etag.ends_with('"'));
}

#[test]
fn test_complete_roundtrip() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();

    let part1 = Bytes::from(vec![1u8; 100]);
    let part2 = Bytes::from(vec![2u8; 200]);
    let etag1 = put(&store, &upload_id, "bucket", "key.bin", 1, part1.clone()).unwrap();
    let etag2 = put(&store, &upload_id, "bucket", "key.bin", 2, part2.clone()).unwrap();

    let result = store
        .complete_parts(&upload_id, "bucket", "key.bin", &[(1, etag1), (2, etag2)])
        .unwrap();

    let data = joined(&result);
    assert_eq!(data.len(), 300);
    assert_eq!(result.total_size, 300);
    assert_eq!(&data[..100], &[1u8; 100]);
    assert_eq!(&data[100..], &[2u8; 200]);
    assert!(result.etag.ends_with("-2\""));
}

#[test]
fn test_abort() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();
    store.abort(&upload_id, "bucket", "key.bin").unwrap();

    let result = put(
        &store,
        &upload_id,
        "bucket",
        "key.bin",
        1,
        Bytes::from(vec![0u8; 10]),
    );
    assert!(result.is_err());
}

#[test]
fn test_bucket_key_mismatch() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket-a", "key.bin", None, HashMap::new())
        .unwrap();

    let result = put(
        &store,
        &upload_id,
        "bucket-b",
        "key.bin",
        1,
        Bytes::from(vec![0u8; 10]),
    );
    assert!(result.is_err());
}

#[test]
fn test_invalid_part_number() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();

    let result = put(
        &store,
        &upload_id,
        "bucket",
        "key.bin",
        0,
        Bytes::from(vec![0u8; 10]),
    );
    assert!(result.is_err());

    let result = put(
        &store,
        &upload_id,
        "bucket",
        "key.bin",
        10001,
        Bytes::from(vec![0u8; 10]),
    );
    assert!(result.is_err());
}

#[test]
fn test_list_parts() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();

    for i in 1..=3 {
        put(
            &store,
            &upload_id,
            "bucket",
            "key.bin",
            i,
            Bytes::from(vec![i as u8; 100]),
        )
        .unwrap();
    }

    let parts = store.list_parts(&upload_id, "bucket", "key.bin").unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].part_number, 1);
    assert_eq!(parts[1].part_number, 2);
    assert_eq!(parts[2].part_number, 3);
}

#[test]
fn test_overwrite_part() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();

    let etag1 = put(
        &store,
        &upload_id,
        "bucket",
        "key.bin",
        1,
        Bytes::from(vec![1u8; 100]),
    )
    .unwrap();
    let etag2 = put(
        &store,
        &upload_id,
        "bucket",
        "key.bin",
        1,
        Bytes::from(vec![2u8; 100]),
    )
    .unwrap();

    assert_ne!(etag1, etag2);

    let parts = store.list_parts(&upload_id, "bucket", "key.bin").unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].etag, etag2);
}

#[test]
fn test_complete_with_zero_parts() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();
    put(
        &store,
        &upload_id,
        "bucket",
        "key.bin",
        1,
        Bytes::from(vec![1u8; 100]),
    )
    .unwrap();

    // Complete with empty parts list should fail
    let result = store.complete_parts(&upload_id, "bucket", "key.bin", &[]);
    assert!(result.is_err(), "complete with zero parts should fail");
}

#[test]
fn test_complete_with_wrong_etag() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();
    put(
        &store,
        &upload_id,
        "bucket",
        "key.bin",
        1,
        Bytes::from(vec![1u8; 100]),
    )
    .unwrap();

    // Complete with wrong etag should fail
    let result = store.complete_parts(
        &upload_id,
        "bucket",
        "key.bin",
        &[(1, "\"wrong_etag\"".to_string())],
    );
    assert!(result.is_err(), "complete with wrong etag should fail");
}

#[test]
fn test_complete_with_non_contiguous_parts() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();

    let part1 = Bytes::from(vec![1u8; 100]);
    let part3 = Bytes::from(vec![3u8; 100]);
    let etag1 = put(&store, &upload_id, "bucket", "key.bin", 1, part1).unwrap();
    let etag3 = put(&store, &upload_id, "bucket", "key.bin", 3, part3).unwrap();

    // Parts 1 and 3 (skip 2) — should succeed per S3 spec
    let result = store
        .complete_parts(&upload_id, "bucket", "key.bin", &[(1, etag1), (3, etag3)])
        .unwrap();
    let data = joined(&result);
    assert_eq!(data.len(), 200);
    assert_eq!(&data[..100], &[1u8; 100]);
    assert_eq!(&data[100..], &[3u8; 100]);
}

#[test]
fn test_max_uploads_limit() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    // Override max_uploads for testing
    let store = MultipartStore {
        max_uploads: 3,
        ..store
    };

    // Create 3 uploads (at limit)
    for i in 0..3 {
        store
            .create("bucket", &format!("key{}.bin", i), None, HashMap::new())
            .unwrap();
    }

    // 4th upload should fail
    let result = store.create("bucket", "key3.bin", None, HashMap::new());
    assert!(result.is_err());
}

// === C4 security fix: state-machine tests ===

fn seed_upload(store: &MultipartStore) -> String {
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();
    let data = Bytes::from(vec![0u8; 100]);
    put(store, &upload_id, "bucket", "key.bin", 1, data).unwrap();
    upload_id
}

#[test]
fn test_complete_flips_state_to_completing() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = seed_upload(&store);
    let etag = {
        let u = store.uploads.read();
        let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
        format!("\"{}\"", p.md5_hex)
    };

    // Before complete → Open.
    assert_eq!(
        store.uploads.read().get(&upload_id).unwrap().state,
        MultipartState::Open
    );

    store
        .complete_parts(&upload_id, "bucket", "key.bin", &[(1, etag)])
        .unwrap();

    // After complete, upload stays in map but as Completing.
    assert_eq!(
        store.uploads.read().get(&upload_id).unwrap().state,
        MultipartState::Completing
    );
}

#[test]
fn test_abort_refused_when_completing() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = seed_upload(&store);
    let etag = {
        let u = store.uploads.read();
        let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
        format!("\"{}\"", p.md5_hex)
    };

    store
        .complete_parts(&upload_id, "bucket", "key.bin", &[(1, etag)])
        .unwrap();

    let err = store.abort(&upload_id, "bucket", "key.bin").unwrap_err();
    assert!(matches!(err, S3Error::InvalidRequest(_)));
    // Upload still in map, still Completing.
    assert_eq!(
        store.uploads.read().get(&upload_id).unwrap().state,
        MultipartState::Completing
    );
}

/// C-P0-1 regression: `purge_uploads_for_bucket` must NOT remove
/// uploads that are in `Completing` state. Doing so would tear down
/// state that the in-flight `engine.store_*` handler still has
/// borrowed paths/buffers for; the storage layer's `create_dir_all`
/// would then race to resurrect a bucket the operator just deleted.
///
/// Pre-fix: `purge_uploads_for_bucket` silently removed Completing
/// uploads and returned a usize. Post-fix: it returns
/// `Err(count_completing)` when any Completing upload targets the
/// bucket; `delete_bucket` translates that to `BucketNotEmpty`.
#[test]
fn test_purge_for_bucket_refuses_when_completing() {
    let store = MultipartStore::new(100 * 1024 * 1024);

    // One Open upload in `bucket-a` — would be safe to purge alone.
    let _ = seed_upload(&store); // bucket="bucket", key="key.bin"

    // Second upload, drive it into Completing.
    let upload_b = store
        .create("bucket", "other.bin", None, HashMap::new())
        .unwrap();
    put(
        &store,
        &upload_b,
        "bucket",
        "other.bin",
        1,
        Bytes::from_static(b"x"),
    )
    .unwrap();
    let etag = {
        let u = store.uploads.read();
        let p = u.get(&upload_b).unwrap().parts.get(&1).unwrap();
        format!("\"{}\"", p.md5_hex)
    };
    store
        .complete_parts(&upload_b, "bucket", "other.bin", &[(1, etag)])
        .unwrap();
    assert_eq!(
        store.uploads.read().get(&upload_b).unwrap().state,
        MultipartState::Completing,
        "second upload should be Completing"
    );

    // Purge must refuse, with the count of Completing uploads as
    // the error payload. Nothing must be removed (all-or-nothing).
    let result = store.purge_uploads_for_bucket("bucket");
    assert_eq!(result, Err(1), "must refuse with completing count");
    assert_eq!(
        store.uploads.read().len(),
        2,
        "must not have partially purged"
    );
}

/// Sister test: when ALL uploads for the bucket are `Open`, purge
/// proceeds and returns the count purged. Sanity check that the new
/// signature didn't break the happy path.
#[test]
fn test_purge_for_bucket_proceeds_when_all_open() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let _ = seed_upload(&store);
    let _ = store
        .create("bucket", "other.bin", None, HashMap::new())
        .unwrap();
    // Different bucket — must NOT be purged.
    let _ = store
        .create("other-bucket", "elsewhere.bin", None, HashMap::new())
        .unwrap();

    let result = store.purge_uploads_for_bucket("bucket");
    assert_eq!(result, Ok(2), "purges Open uploads in target bucket");
    assert_eq!(
        store.uploads.read().len(),
        1,
        "leaves the other-bucket upload alone"
    );
}

#[test]
fn test_upload_part_refused_when_completing() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = seed_upload(&store);
    let etag = {
        let u = store.uploads.read();
        let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
        format!("\"{}\"", p.md5_hex)
    };
    store
        .complete_parts(&upload_id, "bucket", "key.bin", &[(1, etag)])
        .unwrap();

    let err = put(
        &store,
        &upload_id,
        "bucket",
        "key.bin",
        2,
        Bytes::from(vec![0u8; 50]),
    )
    .unwrap_err();
    assert!(matches!(err, S3Error::InvalidRequest(_)));
}

#[test]
fn test_rollback_upload_returns_to_open() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = seed_upload(&store);
    let etag = {
        let u = store.uploads.read();
        let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
        format!("\"{}\"", p.md5_hex)
    };
    store
        .complete_parts(&upload_id, "bucket", "key.bin", &[(1, etag)])
        .unwrap();

    // Simulate engine.store* failure → rollback.
    store.rollback_upload(&upload_id);

    assert_eq!(
        store.uploads.read().get(&upload_id).unwrap().state,
        MultipartState::Open
    );

    // Client can now retry Complete or abort.
    store.abort(&upload_id, "bucket", "key.bin").unwrap();
}

#[test]
fn test_finish_upload_removes_entry() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = seed_upload(&store);
    let etag = {
        let u = store.uploads.read();
        let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
        format!("\"{}\"", p.md5_hex)
    };
    store
        .complete_parts(&upload_id, "bucket", "key.bin", &[(1, etag)])
        .unwrap();

    store.finish_upload(&upload_id);

    assert!(store.uploads.read().get(&upload_id).is_none());
}

#[test]
fn test_double_complete_returns_conflict() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = seed_upload(&store);
    let etag = {
        let u = store.uploads.read();
        let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
        format!("\"{}\"", p.md5_hex)
    };

    store
        .complete_parts(&upload_id, "bucket", "key.bin", &[(1, etag.clone())])
        .unwrap();
    let err = store
        .complete_parts(&upload_id, "bucket", "key.bin", &[(1, etag)])
        .unwrap_err();
    assert!(
        matches!(err, S3Error::InvalidRequest(_)),
        "double-complete should return InvalidRequest while in Completing, got {:?}",
        err
    );
}

#[test]
fn test_validation_failure_does_not_change_state() {
    // If complete() fails validation (wrong etag), state must stay Open
    // so the client can retry with correct metadata.
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = seed_upload(&store);

    let err = store
        .complete_parts(
            &upload_id,
            "bucket",
            "key.bin",
            &[(1, "\"wrong-etag\"".to_string())],
        )
        .unwrap_err();
    assert!(matches!(err, S3Error::InvalidPart(_)));
    assert_eq!(
        store.uploads.read().get(&upload_id).unwrap().state,
        MultipartState::Open,
        "validation failure must leave upload Open for retry"
    );
}

#[test]
fn test_abort_while_open_drops_upload() {
    // Baseline: abort on an Open upload still works normally.
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = seed_upload(&store);
    store.abort(&upload_id, "bucket", "key.bin").unwrap();
    assert!(store.uploads.read().get(&upload_id).is_none());
}

// === C3 DoS fix: size-cap + global-counter + TTL sweeper tests ===

#[test]
fn test_upload_part_rejects_when_cumulative_exceeds_max_object_size() {
    // max_object_size = 1 KiB. Upload 700 B + 200 B → OK, 300 B → rejected.
    let store = MultipartStore::new(1024);
    let upload_id = store.create("bucket", "key", None, HashMap::new()).unwrap();
    put(
        &store,
        &upload_id,
        "bucket",
        "key",
        1,
        Bytes::from(vec![0u8; 700]),
    )
    .unwrap();
    put(
        &store,
        &upload_id,
        "bucket",
        "key",
        2,
        Bytes::from(vec![0u8; 200]),
    )
    .unwrap();
    let err = put(
        &store,
        &upload_id,
        "bucket",
        "key",
        3,
        Bytes::from(vec![0u8; 300]),
    )
    .unwrap_err();
    assert!(
        matches!(err, S3Error::EntityTooLarge { size, max } if size == 1200 && max == 1024),
        "got {:?}",
        err
    );
}

#[test]
fn test_upload_part_overwrite_adjusts_cumulative_correctly() {
    // Overwrite a 1000 B part with 200 B — cumulative goes DOWN, not up.
    let store = MultipartStore::new(1500);
    let upload_id = store.create("bucket", "key", None, HashMap::new()).unwrap();
    put(
        &store,
        &upload_id,
        "bucket",
        "key",
        1,
        Bytes::from(vec![0u8; 1000]),
    )
    .unwrap();
    // Add 400 more via a second part — total 1400, under cap.
    put(
        &store,
        &upload_id,
        "bucket",
        "key",
        2,
        Bytes::from(vec![0u8; 400]),
    )
    .unwrap();
    // Now overwrite part 1 with 200 B. New cumulative = 200 + 400 = 600.
    put(
        &store,
        &upload_id,
        "bucket",
        "key",
        1,
        Bytes::from(vec![0u8; 200]),
    )
    .unwrap();
    // Counter should reflect the overwrite: two parts, 1 MiB of spool each.
    assert_eq!(store.in_flight_bytes(), 2 * MIB);
}

#[test]
fn test_upload_part_respects_global_byte_cap() {
    // Tight global cap: 2 MiB of spool budget across all uploads.
    let store = MultipartStore::new_for_test(10 * MIB, 2 * MIB, Duration::hours(24));
    let id_a = store.create("b", "a", None, HashMap::new()).unwrap();
    let id_b = store.create("b", "b", None, HashMap::new()).unwrap();

    // Upload A holds 1 MiB.
    put(&store, &id_a, "b", "a", 1, Bytes::from(vec![0u8; 1024])).unwrap();
    // Upload B holds 1 MiB (total now 2 MiB = cap).
    put(&store, &id_b, "b", "b", 1, Bytes::from(vec![0u8; 1024])).unwrap();
    // Next byte anywhere → SlowDown.
    let err = put(&store, &id_a, "b", "a", 2, Bytes::from(vec![0u8; 1])).unwrap_err();
    assert!(matches!(err, S3Error::SlowDown(_)), "got {:?}", err);
}

#[test]
fn test_abort_releases_in_flight_bytes() {
    let store = MultipartStore::new_for_test(10 * MIB, 2 * MIB, Duration::hours(24));
    let id = store.create("b", "a", None, HashMap::new()).unwrap();
    put(&store, &id, "b", "a", 1, Bytes::from(vec![0u8; 1024])).unwrap();
    assert_eq!(store.in_flight_bytes(), MIB);

    store.abort(&id, "b", "a").unwrap();
    assert_eq!(
        store.in_flight_bytes(),
        0,
        "abort must release bytes to the global counter"
    );
}

#[test]
fn test_finish_upload_releases_in_flight_bytes() {
    let store = MultipartStore::new_for_test(10 * 1024, 10 * MIB, Duration::hours(24));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let data = Bytes::from(vec![0u8; 500]);
    let etag = put(&store, &id, "b", "k", 1, data).unwrap();
    assert_eq!(store.in_flight_bytes(), MIB);

    store.complete_parts(&id, "b", "k", &[(1, etag)]).unwrap();
    // Still in map (Completing) — counter unchanged.
    assert_eq!(store.in_flight_bytes(), MIB);

    store.finish_upload(&id);
    assert_eq!(store.in_flight_bytes(), 0);
}

#[test]
fn test_cleanup_expired_idle_ttl_sweeps_and_releases_bytes() {
    // Tiny idle TTL so we can trip it synchronously.
    let store = MultipartStore::new_for_test(10 * 1024, 10 * MIB, Duration::milliseconds(1));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    put(&store, &id, "b", "k", 1, Bytes::from(vec![0u8; 700])).unwrap();
    assert_eq!(store.in_flight_bytes(), MIB);

    // Sleep past the idle TTL.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let report = store.cleanup_expired(
        std::time::Duration::from_secs(3600),
        std::time::Duration::from_secs(3600),
    );
    assert_eq!(report.swept_open_uploads, 1);

    assert!(
        store.uploads.read().get(&id).is_none(),
        "idle upload should have been swept"
    );
    assert_eq!(
        store.in_flight_bytes(),
        0,
        "sweep must release bytes to the global counter"
    );
}

#[test]
fn test_cleanup_expired_preserves_recent_completing_upload() {
    // Completing uploads should survive until completing_timeout elapses.
    let store = MultipartStore::new_for_test(10 * 1024, 10 * MIB, Duration::hours(24));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let etag = put(&store, &id, "b", "k", 1, Bytes::from(vec![0u8; 100])).unwrap();
    store.complete_parts(&id, "b", "k", &[(1, etag)]).unwrap();

    let report = store.cleanup_expired(
        std::time::Duration::from_secs(3600),
        std::time::Duration::from_secs(3600),
    );
    assert_eq!(report.swept_completing_uploads, 0);

    assert!(
        store.uploads.read().get(&id).is_some(),
        "recent Completing uploads must be preserved"
    );
}

#[test]
fn test_cleanup_expired_sweeps_stuck_completing_upload() {
    let store = MultipartStore::new_for_test(10 * 1024, 10 * MIB, Duration::hours(24));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let etag = put(&store, &id, "b", "k", 1, Bytes::from(vec![0u8; 100])).unwrap();
    store.complete_parts(&id, "b", "k", &[(1, etag)]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));

    let report = store.cleanup_expired(
        std::time::Duration::from_secs(3600),
        std::time::Duration::from_millis(1),
    );
    assert_eq!(report.swept_completing_uploads, 1);
    assert_eq!(store.in_flight_bytes(), 0);
    assert!(store.uploads.read().get(&id).is_none());
}

#[test]
fn test_cleanup_expired_spares_completing_upload_with_store_in_flight() {
    // H18: a Completing upload past completing_timeout must NOT be swept
    // while its engine store is in flight (store_in_progress), else the relay
    // parts the store is reading get deleted and are lost on retry.
    let store = std::sync::Arc::new(MultipartStore::new_for_test(
        10 * 1024,
        10 * MIB,
        Duration::hours(24),
    ));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let etag = put(&store, &id, "b", "k", 1, Bytes::from(vec![0u8; 100])).unwrap();
    store.complete_parts(&id, "b", "k", &[(1, etag)]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));

    // Store in flight → spared even with a 1ms completing_timeout.
    let guard = store.store_guard(&id);
    let report = store.cleanup_expired(
        std::time::Duration::from_secs(3600),
        std::time::Duration::from_millis(1),
    );
    assert_eq!(
        report.swept_completing_uploads, 0,
        "an in-flight store must protect its Completing upload from the sweeper"
    );
    assert!(store.uploads.read().get(&id).is_some());

    // Once the store finishes (guard dropped), the sweeper reclaims it.
    drop(guard);
    let report = store.cleanup_expired(
        std::time::Duration::from_secs(3600),
        std::time::Duration::from_millis(1),
    );
    assert_eq!(report.swept_completing_uploads, 1);
    assert!(store.uploads.read().get(&id).is_none());
}

#[test]
fn test_cleanup_orphan_relay_entries_removes_untracked_entries() {
    let dir = tempfile::tempdir().unwrap();
    let active_dir = dir.path().join("active");
    let orphan_dir = dir.path().join("orphan");
    let orphan_file = dir.path().join("stray.tmp");
    fs::create_dir_all(&active_dir).unwrap();
    fs::create_dir_all(&orphan_dir).unwrap();
    fs::write(orphan_dir.join("part-00001.bin"), b"orphan").unwrap();
    fs::write(&orphan_file, b"stray").unwrap();

    let mut active = HashSet::new();
    active.insert(active_dir.clone());
    let (dirs_removed, files_removed) =
        cleanup_orphan_relay_entries_at(dir.path(), &active, std::time::Duration::ZERO);

    assert_eq!(dirs_removed, 1);
    assert_eq!(files_removed, 1);
    assert!(active_dir.exists(), "active relay dir must be preserved");
    assert!(!orphan_dir.exists(), "orphan relay dir must be removed");
    assert!(!orphan_file.exists(), "orphan relay file must be removed");
}

#[test]
fn orphan_sweep_min_age_spares_recent_entries() {
    // H19: a freshly-created (untracked) relay dir — as a concurrent
    // promotion would produce after the active-set snapshot — must NOT be
    // deleted by the periodic sweep when a min_age guard is set.
    let dir = tempfile::tempdir().unwrap();
    let fresh_orphan = dir.path().join("fresh");
    fs::create_dir_all(&fresh_orphan).unwrap();
    fs::write(fresh_orphan.join("part-00001.bin"), b"in-flight").unwrap();

    let active = HashSet::new(); // nothing tracked yet (mid-promotion)
    let (dirs_removed, _files) =
        cleanup_orphan_relay_entries_at(dir.path(), &active, std::time::Duration::from_secs(3600));
    assert_eq!(dirs_removed, 0, "a recent dir must be spared by min_age");
    assert!(
        fresh_orphan.exists(),
        "the in-flight relay dir must survive"
    );
}

#[test]
fn test_complete_parts_returns_the_relay_parts_in_order() {
    let store = MultipartStore::new(10 * 1024);
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let e2 = put(&store, &id, "b", "k", 2, b"world").unwrap();
    let e1 = put(&store, &id, "b", "k", 1, b"hello").unwrap();

    let completed = store
        .complete_parts(&id, "b", "k", &[(1, e1), (2, e2)])
        .unwrap();
    assert_eq!(completed.total_size, 10);
    assert_eq!(completed.parts.len(), 2);
    assert_eq!(
        completed.parts[0].md5,
        <[u8; 16]>::from(Md5::digest(b"hello"))
    );
    assert_eq!(completed.parts[1].size, 5);
    assert_eq!(joined(&completed), b"helloworld");
}

fn small_spool(dir: &tempfile::TempDir, mib: u64) -> SpoolDir {
    SpoolDir::new(dir.path().join("spool"), mib << 20).unwrap()
}

/// The relay files of upload `id` in `store`'s relay dir.
fn relay_files(store: &MultipartStore, id: &str) -> Vec<PathBuf> {
    match fs::read_dir(store.relay_dir_for_upload(id)) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// An ingested part BECOMES the relay part: its spool file is renamed into
/// the upload's relay dir (in the spool dir), and it holds ONE reservation,
/// the one its ingest took. No copy, no second reservation.
#[test]
fn an_ingested_part_becomes_the_relay_part_with_one_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let spool = small_spool(&dir, 32);
    let store = MultipartStore::new(64 << 20).with_spool(spool.clone());
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let body = spooled(&store, &[7u8; 3 << 20], 0).unwrap();
    let IngestedBody::Spooled { spool: file, .. } = &body else {
        unreachable!()
    };
    let ingested_at = file.path().to_path_buf();
    assert_eq!(spool.free_mib(), 29);
    let admission = store.admit_part(&id, "b", "k", 1, 3 << 20).unwrap();
    store
        .upload_part(&id, "b", "k", 1, body, admission)
        .unwrap();
    assert_eq!(
        spool.free_mib(),
        29,
        "the part keeps the ingest's reservation"
    );
    assert!(
        !ingested_at.exists(),
        "the spool file was renamed, not copied"
    );
    let files = relay_files(&store, &id);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].starts_with(spool.dir()), "{files:?}");
    assert_eq!(fs::read(&files[0]).unwrap(), vec![7u8; 3 << 20]);
    store.abort(&id, "b", "k").unwrap();
    assert_eq!(spool.free_mib(), 32);
    assert!(!store.relay_dir_for_upload(&id).exists());
}

/// A part that replaces another: the old relay file is deleted and its
/// budget returns; each attempt writes a file of its own name.
#[test]
fn an_overwritten_part_deletes_the_old_file_and_returns_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    let spool = small_spool(&dir, 32);
    let store = MultipartStore::new(64 << 20)
        .with_spool(spool.clone())
        .with_relay_upload_max(Some(8 << 20));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let mib = |n: usize| vec![7u8; n << 20];
    put(&store, &id, "b", "k", 1, mib(2)).unwrap();
    put(&store, &id, "b", "k", 2, mib(3)).unwrap();
    assert_eq!(spool.free_mib(), 27);
    let before = relay_files(&store, &id);
    put(&store, &id, "b", "k", 2, mib(1)).unwrap();
    assert_eq!(spool.free_mib(), 29);
    let after = relay_files(&store, &id);
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(
        before.iter().filter(|p| !after.contains(p)).count(),
        1,
        "the replaced file is gone, the new one has its own name"
    );
    store.finish_upload(&id);
    assert_eq!(spool.free_mib(), 32);
    assert!(relay_files(&store, &id).is_empty());
}

/// A part refused when it is kept (the upload is completing, or a cap is
/// reached since its admission) leaves no file and no reservation behind.
#[test]
fn a_refused_part_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let spool = small_spool(&dir, 32);
    let store = MultipartStore::new(4 << 20).with_spool(spool.clone());
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let etag = put(&store, &id, "b", "k", 1, vec![1u8; 3 << 20]).unwrap();

    // Admitted at a declared size that fits, refused on the size that
    // arrived.
    let admission = store.admit_part(&id, "b", "k", 2, 1).unwrap();
    let body = spooled(&store, &[2u8; 2 << 20], 3).unwrap();
    let err = store
        .upload_part(&id, "b", "k", 2, body, admission)
        .unwrap_err();
    assert_eq!(err.code(), "EntityTooLarge", "{err:?}");
    assert_eq!(relay_files(&store, &id).len(), 1);
    assert_eq!(spool.free_mib(), 29);

    // Admitted, then refused because the upload is completing.
    let admission = store.admit_part(&id, "b", "k", 2, 4).unwrap();
    store.complete_parts(&id, "b", "k", &[(1, etag)]).unwrap();
    let body = spooled(&store, b"late", 3).unwrap();
    let err = store
        .upload_part(&id, "b", "k", 2, body, admission)
        .unwrap_err();
    assert!(matches!(err, S3Error::InvalidRequest(_)), "{err:?}");
    assert_eq!(relay_files(&store, &id).len(), 1);
    assert_eq!(spool.free_mib(), 29);
}

/// A part for an unknown, aborted or completing upload, or one past a cap,
/// is refused by `admit_part`: before its body is read.
#[test]
fn admit_part_refuses_before_the_body() {
    let store = MultipartStore::new_for_test(4 << 20, 6 << 20, Duration::hours(24));
    let code =
        |r: Result<PartAdmission, S3Error>| r.map(|a| a.held_spool_mib).map_err(|e| e.code());
    assert_eq!(
        code(store.admit_part("nope", "b", "k", 1, 1)),
        Err("NoSuchUpload")
    );
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    assert_eq!(
        code(store.admit_part(&id, "b", "other", 1, 1)),
        Err("NoSuchUpload")
    );
    assert_eq!(
        code(store.admit_part(&id, "b", "k", 0, 1)),
        Err("InvalidArgument")
    );
    assert_eq!(
        code(store.admit_part(&id, "b", "k", 1, (4 << 20) + 1)),
        Err("EntityTooLarge")
    );
    let etag = put(&store, &id, "b", "k", 1, vec![0u8; 3 << 20]).unwrap();
    assert_eq!(code(store.admit_part(&id, "b", "k", 2, 1 << 20)), Ok(3));
    // The global cap: another upload holds 3 MiB of 6.
    let other = store.create("b", "o", None, HashMap::new()).unwrap();
    put(&store, &other, "b", "o", 1, vec![0u8; 3 << 20]).unwrap();
    assert_eq!(code(store.admit_part(&id, "b", "k", 2, 1)), Err("SlowDown"));
    // Replacing a part counts only the difference.
    assert!(store.admit_part(&id, "b", "k", 1, 3 << 20).is_ok());
    store.complete_parts(&id, "b", "k", &[(1, etag)]).unwrap();
    assert_eq!(
        code(store.admit_part(&id, "b", "k", 1, 1)),
        Err("InvalidRequest")
    );
    store.abort(&other, "b", "o").unwrap();
    assert_eq!(
        code(store.admit_part(&other, "b", "o", 1, 1)),
        Err("NoSuchUpload")
    );
}

/// An abort while a part streams: the part, admitted before the abort,
/// is refused when it is kept, and neither its file nor the relay dir it
/// re-created stays. An abort after the part removes both too.
#[test]
fn an_abort_during_or_after_the_ingest_leaves_no_file_or_dir() {
    let dir = tempfile::tempdir().unwrap();
    let spool = small_spool(&dir, 32);
    let store = MultipartStore::new(64 << 20).with_spool(spool.clone());
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let admission = store.admit_part(&id, "b", "k", 1, 5).unwrap();
    let body = spooled(&store, b"hello", admission.held_spool_mib).unwrap();
    store.abort(&id, "b", "k").unwrap();
    let err = store
        .upload_part(&id, "b", "k", 1, body, admission)
        .unwrap_err();
    assert!(matches!(err, S3Error::NoSuchUpload(_)), "{err:?}");
    assert!(!store.relay_dir_for_upload(&id).exists());
    assert_eq!(spool.free_mib(), 32);

    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    put(&store, &id, "b", "k", 1, b"hello").unwrap();
    assert!(store.relay_dir_for_upload(&id).exists());
    store.abort(&id, "b", "k").unwrap();
    assert!(!store.relay_dir_for_upload(&id).exists());
    assert_eq!(spool.free_mib(), 32);
}

/// The idle-TTL sweep deletes a swept upload's relay files and returns
/// their budget.
#[test]
fn the_ttl_sweep_deletes_relay_files_and_returns_their_budget() {
    let dir = tempfile::tempdir().unwrap();
    let spool = small_spool(&dir, 32);
    let store = MultipartStore::new_for_test(64 << 20, 64 << 20, Duration::milliseconds(1))
        .with_spool(spool.clone());
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    put(&store, &id, "b", "k", 1, vec![0u8; 2 << 20]).unwrap();
    assert_eq!(spool.free_mib(), 30);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let report = store.cleanup_expired(
        std::time::Duration::from_secs(3600),
        std::time::Duration::from_secs(3600),
    );
    assert_eq!(report.swept_open_uploads, 1);
    assert_eq!(report.reclaimed_bytes, 2 << 20);
    assert!(!store.relay_dir_for_upload(&id).exists());
    assert_eq!(spool.free_mib(), 32);
}

/// An upload's parts that hold the budget make the next part's ingest a
/// SlowDown at once: an op that holds budget never waits for more.
#[test]
fn a_part_with_the_budget_taken_is_slowdown_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let spool = small_spool(&dir, 4);
    let store = MultipartStore::new(64 << 20)
        .with_spool(spool.clone())
        .with_relay_upload_max(Some(0));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    put(&store, &id, "b", "k", 1, vec![1u8; 1 << 20]).unwrap();
    let other = spool.try_acquire(3 << 20).unwrap();
    let err = put(&store, &id, "b", "k", 2, vec![1u8; 1 << 20]).unwrap_err();
    assert!(matches!(err, S3Error::SlowDown(_)), "got {err:?}");
    drop(other);
    put(&store, &id, "b", "k", 2, vec![1u8; 1 << 20]).unwrap();
}

#[test]
fn part_charge_is_whole_mib_at_least_one() {
    assert_eq!(part_charge(0), MIB);
    assert_eq!(part_charge(1), MIB);
    assert_eq!(part_charge(MIB), MIB);
    assert_eq!(part_charge(MIB + 1), 2 * MIB);
    assert_eq!(part_charge(5 << 30), 5 << 30);
}

/// Each part holds whole MiB of the spool, at least one: 20 parts of one
/// byte hold 20 MiB of the caps, and the next part past a cap is refused.
/// Before, the caps counted part bytes: 20 bytes held, and one writer
/// could pin most of the spool with tiny parts.
#[test]
fn tiny_parts_are_charged_their_spool_budget() {
    let dir = tempfile::tempdir().unwrap();
    let spool = small_spool(&dir, 64);
    // Per-upload cap 20 MiB, global cap 30 MiB.
    let store = MultipartStore::new_for_test(1 << 30, 30 * MIB, Duration::hours(24))
        .with_spool(spool.clone())
        .with_relay_upload_max(Some(20 * MIB));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    for n in 1..=20 {
        put(&store, &id, "b", "k", n, b"x").unwrap();
    }
    assert_eq!(store.in_flight_bytes(), 20 * MIB);
    assert_eq!(spool.free_mib(), 44);
    let err = put(&store, &id, "b", "k", 21, b"x").unwrap_err();
    assert_eq!(err.code(), "EntityTooLarge", "{err:?}");
    assert!(err.to_string().contains("whole MiB"), "{err}");
    // Overwriting a part needs no more budget.
    put(&store, &id, "b", "k", 20, b"y").unwrap();

    // The global cap counts charges too: 10 more MiB fit, the 11th not.
    let other = store.create("b", "o", None, HashMap::new()).unwrap();
    for n in 1..=10 {
        put(&store, &other, "b", "o", n, b"x").unwrap();
    }
    let err = put(&store, &other, "b", "o", 11, b"x").unwrap_err();
    assert!(matches!(err, S3Error::SlowDown(_)), "{err:?}");
    store.abort(&id, "b", "k").unwrap();
    store.abort(&other, "b", "o").unwrap();
    assert_eq!(store.in_flight_bytes(), 0);
    assert_eq!(spool.free_mib(), 64);
}

/// A part counts against the caps from its admission, while it streams:
/// parallel parts cannot pass a cap together that only some of them fit.
/// Before, the caps read only the kept parts, so N parallel parts each
/// passed `admit_part` and each reserved its declared size.
#[test]
fn parallel_parts_cannot_pass_the_caps_together() {
    // Per upload: 4 MiB.
    let store = MultipartStore::new_for_test(1 << 30, 1 << 30, Duration::hours(24))
        .with_relay_upload_max(Some(4 * MIB));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let a = store.admit_part(&id, "b", "k", 1, 2 * MIB).unwrap();
    let b = store.admit_part(&id, "b", "k", 2, 2 * MIB).unwrap();
    let err = store
        .admit_part(&id, "b", "k", 3, 1)
        .err()
        .expect("refused");
    assert_eq!(err.code(), "EntityTooLarge", "{err:?}");
    // A dropped admission (a failed ingest, a dropped request) gives its
    // share back.
    drop(b);
    let b = store.admit_part(&id, "b", "k", 2, 2 * MIB).unwrap();
    drop((a, b));

    // Global: 3 MiB across uploads.
    let store = MultipartStore::new_for_test(1 << 30, 3 * MIB, Duration::hours(24));
    let ids: Vec<_> = (0..3)
        .map(|i| {
            store
                .create("b", &format!("k{i}"), None, HashMap::new())
                .unwrap()
        })
        .collect();
    let held: Vec<_> = ids[..2]
        .iter()
        .enumerate()
        .map(|(i, id)| store.admit_part(id, "b", &format!("k{i}"), 1, 1).unwrap())
        .collect();
    let third = store.admit_part(&ids[2], "b", "k2", 1, MIB + 1);
    assert!(
        matches!(third, Err(S3Error::SlowDown(_))),
        "parallel parts passed the global cap"
    );
    let third = store.admit_part(&ids[2], "b", "k2", 1, MIB).unwrap();
    // Kept parts take over the charge of their admission: still 3 MiB.
    let mut kept = 0;
    for (i, admission) in held.into_iter().enumerate() {
        let body = spooled(&store, b"x", admission.held_spool_mib).unwrap();
        store
            .upload_part(&ids[i], "b", &format!("k{i}"), 1, body, admission)
            .unwrap();
        kept += 1;
    }
    assert_eq!(kept, 2);
    assert_eq!(store.in_flight_bytes(), 2 * MIB);
    assert!(store.admit_part(&ids[0], "b", "k0", 2, 1).is_err(), "full");
    drop(third);
    assert!(store.admit_part(&ids[0], "b", "k0", 2, 1).is_ok());
}

/// An admission ends with its part in every way: kept, refused when kept,
/// its upload aborted, or its request future dropped mid-ingest. After
/// each, the caps hold nothing for it.
#[tokio::test]
async fn an_admission_is_released_on_every_path() {
    let store = Arc::new(
        MultipartStore::new_for_test(1 << 30, 1 << 30, Duration::hours(24))
            .with_relay_upload_max(Some(2 * MIB)),
    );
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    // The whole per-upload cap is admitted, then the admission is dropped
    // with the future that held it.
    let admission = store.admit_part(&id, "b", "k", 1, 2 * MIB).unwrap();
    let task = tokio::spawn(async move {
        let _held = admission;
        futures::future::pending::<()>().await;
    });
    tokio::task::yield_now().await;
    assert!(
        store.admit_part(&id, "b", "k", 2, 1).is_err(),
        "held while streaming"
    );
    task.abort();
    let _ = task.await;
    // Refused when kept (aborted upload): the charge is released.
    let admission = store.admit_part(&id, "b", "k", 1, 2 * MIB).unwrap();
    let body = spooled(&store, &[1u8; 2 << 20], 0).unwrap();
    store.abort(&id, "b", "k").unwrap();
    let err = store
        .upload_part(&id, "b", "k", 1, body, admission)
        .unwrap_err();
    assert_eq!(err.code(), "NoSuchUpload");
    // An admission that outlives its upload's abort releases on drop.
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let admission = store.admit_part(&id, "b", "k", 1, 2 * MIB).unwrap();
    store.abort(&id, "b", "k").unwrap();
    drop(admission);
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    store
        .admit_part(&id, "b", "k", 1, 2 * MIB)
        .map(drop)
        .unwrap();
    assert_eq!(store.admitted.lock().total, 0);
    assert!(store.admitted.lock().by_upload.is_empty());
    assert_eq!(store.in_flight_bytes(), 0);
}

#[test]
fn max_total_multipart_bytes_truth_table() {
    const GIB: u64 = 1 << 30;
    assert_eq!(max_total_multipart_bytes(16 * GIB, None), 8 * GIB);
    assert_eq!(max_total_multipart_bytes(16 * GIB, Some(GIB)), GIB);
    assert_eq!(max_total_multipart_bytes(16 * GIB, Some(0)), 0);
    // A store follows its spool when nothing is configured.
    let dir = tempfile::tempdir().unwrap();
    let store = MultipartStore {
        max_total_configured: None,
        ..MultipartStore::new(1 << 30).with_spool(small_spool(&dir, 32))
    };
    assert_eq!(store.max_total_multipart_bytes(), 16 << 20);
}

/// The name of the function that `line` declares: the `fn <name>(` token
/// after any `pub`, `pub(..)`, `async`, `const` or `unsafe` prefix.
fn fn_name(line: &str) -> Option<String> {
    let mut rest = line.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("pub(") {
            rest = r.split_once(')')?.1.trim_start();
        } else if let Some(r) = ["pub ", "async ", "const ", "unsafe "]
            .iter()
            .find_map(|p| rest.strip_prefix(p))
        {
            rest = r.trim_start();
        } else {
            break;
        }
    }
    let name = rest.strip_prefix("fn ")?.split(['(', '<']).next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[test]
fn fn_name_reads_every_visibility_and_qualifier() {
    for (line, want) in [
        ("fn a(&self)", Some("a")),
        ("    pub fn b<T>(x: T)", Some("b")),
        ("pub(crate) fn c(&self) {", Some("c")),
        ("pub(super) async fn d(", Some("d")),
        ("async fn e(", Some("e")),
        ("pub(in crate::x) const unsafe fn f(", Some("f")),
        ("let fn_x = 1;", None),
        ("// fn g(", None),
    ] {
        assert_eq!(fn_name(line).as_deref(), want, "{line}");
    }
}

/// The `uploads` lock guards the map only: a function that takes it does
/// no file I/O (create, rename, delete, read, fsync), hashes no part data,
/// and calls none of the helpers that do. A slow disk then never blocks
/// every UploadPart, ListParts and Complete of the node (F10 of epic #143:
/// the part was written, fsynced and its MD5 computed under the lock).
/// Each such function names `self.uploads` and is checked here; the
/// runtime bound is `parallel_parts_never_hold_the_lock_or_the_reactor`.
#[test]
fn the_uploads_lock_is_never_held_for_file_io() {
    let text = crate::source_scan::prod_text(&crate::source_scan::read("src/multipart.rs"));
    let lines: Vec<&str> = text.lines().collect();
    let forbidden = [
        "fs::",
        "File::",
        "rename",
        "remove_",
        "persist",
        "keep_as",
        "create_dir",
        "discard_",
        "forget_relay_dir",
        "read_to_end",
        "sync_all",
        "sync_data",
        "Md5",
        "Sha256",
        "digest(",
        "Hasher",
    ];
    let mut locking = Vec::new();
    let mut offenders = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(name) = fn_name(lines[i]).filter(|_| lines[i..].iter().any(|l| l.contains('{')))
        else {
            i += 1;
            continue;
        };
        // The body: from the line with the first `{` to its matching `}`.
        let mut depth = 0i64;
        let mut opened = false;
        let mut body = Vec::new();
        let mut j = i;
        while j < lines.len() {
            depth += lines[j].matches('{').count() as i64;
            depth -= lines[j].matches('}').count() as i64;
            opened |= lines[j].contains('{');
            body.push(lines[j]);
            j += 1;
            if opened && depth <= 0 {
                break;
            }
        }
        let body = body.join("\n");
        if body.contains("self.uploads") {
            locking.push(name.clone());
            for f in forbidden {
                if body.contains(f) {
                    offenders.push(format!("{name}: {f}"));
                }
            }
        }
        i = j;
    }
    assert!(
        offenders.is_empty(),
        "file I/O under the uploads lock: {offenders:?}"
    );
    for expected in [
        "admit_part",
        "insert_part",
        "take_upload",
        "take_for_abort",
        "take_uploads_for_bucket",
        "take_expired",
        "active_relay_dirs",
        "complete_parts",
    ] {
        assert!(
            locking.iter().any(|n| n == expected),
            "the scan missed `{expected}` (found {locking:?})"
        );
    }
}

/// What the owner of a Complete publishes to its joiners when it ends without
/// a success: only a refusal that depends on the upload and its parts, as it
/// is; a refusal that depends on the owner's headers or principal (a
/// precondition, a quota, an access refusal), or no answer at all, is a
/// retryable 503, so the joiner becomes the owner and gets its own answer.
#[test]
fn joiner_failure_table() {
    use axum::http::StatusCode;
    use s3s::S3ErrorCode as C;
    let mut quota = s3s::S3Error::with_message(C::Custom("QuotaExceeded".into()), "full");
    quota.set_status_code(StatusCode::FORBIDDEN);
    let mut precondition = s3s::S3Error::with_message(C::PreconditionFailed, "etag");
    precondition.set_status_code(StatusCode::PRECONDITION_FAILED);
    let denied = s3s::S3Error::with_message(C::AccessDenied, "no");
    let table: [(Option<s3s::S3Error>, C, Option<StatusCode>); 8] = [
        (
            Some(quota),
            C::SlowDown,
            Some(StatusCode::SERVICE_UNAVAILABLE),
        ),
        (
            Some(precondition),
            C::SlowDown,
            Some(StatusCode::SERVICE_UNAVAILABLE),
        ),
        (
            Some(denied),
            C::SlowDown,
            Some(StatusCode::SERVICE_UNAVAILABLE),
        ),
        (None, C::SlowDown, Some(StatusCode::SERVICE_UNAVAILABLE)),
        (
            Some(s3s::S3Error::with_message(C::EntityTooSmall, "tiny")),
            C::EntityTooSmall,
            None,
        ),
        (
            Some(s3s::S3Error::with_message(C::InvalidPart, "etag")),
            C::InvalidPart,
            None,
        ),
        (
            Some(s3s::S3Error::with_message(C::InvalidPartOrder, "order")),
            C::InvalidPartOrder,
            None,
        ),
        // An internal error is retryable for a joiner: 503, never 500.
        (
            Some(s3s::S3Error::with_message(C::InternalError, "boom")),
            C::SlowDown,
            Some(StatusCode::SERVICE_UNAVAILABLE),
        ),
    ];
    for (answer, code, status) in table {
        let seen = joiner_failure(answer.as_ref()).to_s3s();
        assert_eq!(seen.code(), &code);
        // A shared answer keeps the owner's status; the others are 503.
        let expected = status.or_else(|| answer.as_ref().and_then(|a| a.status_code()));
        assert_eq!(seen.status_code(), expected, "{code:?}");
    }
}

fn guard_fixture() -> (
    std::sync::Arc<MultipartStore>,
    String,
    CompletionPublisher,
    tokio::sync::watch::Receiver<Option<CompletionResult>>,
) {
    let store = std::sync::Arc::new(MultipartStore::new(1024));
    let parts = parts_fixture();
    let id = store.create("b", "k", None, Default::default()).unwrap();
    let owner = match store.begin_complete(&id, "b", "k", &parts).unwrap() {
        BeginComplete::Owner(p) => p,
        _ => panic!("must be Owner"),
    };
    let rx = match store.begin_complete(&id, "b", "k", &parts).unwrap() {
        BeginComplete::Join(rx) => rx,
        _ => panic!("must Join"),
    };
    store.uploads.write().get_mut(&id).unwrap().state = MultipartState::Completing;
    (store, id, owner, rx)
}

/// A Complete whose store panics BEFORE the write committed: the upload is
/// open again, and joiners are told to retry.
#[tokio::test]
async fn a_panic_before_the_commit_rolls_the_upload_back() {
    let (store, id, owner, rx) = guard_fixture();
    let committed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = tokio::spawn({
        let (store, id) = (store.clone(), id.clone());
        async move {
            let _owner = owner;
            let _guard = store.completing_guard(&id, "\"etag\"", committed);
            panic!("the store panicked");
        }
    });
    assert!(task.await.unwrap_err().is_panic());
    assert_eq!(
        store.uploads.read().get(&id).map(|u| u.state),
        Some(MultipartState::Open)
    );
    let seen = rx.borrow().clone().unwrap().unwrap_err();
    assert_eq!(seen.code, s3s::S3ErrorCode::SlowDown, "retry");
}

/// A panic AFTER the write committed is not a retry: the upload is
/// completed, joiners and later retries get the success.
#[tokio::test]
async fn a_panic_after_the_commit_completes_the_upload() {
    let (store, id, owner, rx) = guard_fixture();
    let committed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = tokio::spawn({
        let (store, id, committed) = (store.clone(), id.clone(), committed.clone());
        async move {
            let _owner = owner;
            let _guard = store.completing_guard(&id, "\"etag\"", committed.clone());
            committed.store(true, std::sync::atomic::Ordering::SeqCst);
            panic!("a step after the commit panicked");
        }
    });
    assert!(task.await.unwrap_err().is_panic());
    assert!(
        store.uploads.read().get(&id).is_none(),
        "the upload is completed, not open"
    );
    assert_eq!(rx.borrow().clone().unwrap().unwrap(), "\"etag\"");
    match store
        .begin_complete(&id, "b", "k", &parts_fixture())
        .unwrap()
    {
        BeginComplete::AlreadyDone { etag } => assert_eq!(etag, "\"etag\""),
        _ => panic!("a retry gets the tombstone"),
    }
}

/// A guard that is finished leaves the upload alone.
#[tokio::test]
async fn a_finished_completing_guard_changes_nothing() {
    let (store, id, _owner, _rx) = guard_fixture();
    let committed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    store.completing_guard(&id, "\"etag\"", committed).finish();
    assert_eq!(
        store.uploads.read().get(&id).map(|u| u.state),
        Some(MultipartState::Completing)
    );
}

/// An `Err` published after a post-commit unwind wrote the success must
/// not erase it: the slot stays `Done`, joiners get the success, and a
/// retry gets the tombstone, not NoSuchUpload.
#[tokio::test]
async fn an_error_after_the_commit_never_erases_the_success() {
    let (store, id, owner, rx) = guard_fixture();
    let committed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    drop(store.completing_guard(&id, "\"etag\"", committed));
    owner.publish(Err(CompletionFailure::internal("a step after the commit failed")));
    assert_eq!(rx.borrow().clone().unwrap().unwrap(), "\"etag\"");
    match store.begin_complete(&id, "b", "k", &parts_fixture()).unwrap() {
        BeginComplete::AlreadyDone { etag } => assert_eq!(etag, "\"etag\""),
        _ => panic!("a retry gets the tombstone"),
    }
}

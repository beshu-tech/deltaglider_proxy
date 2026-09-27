// SPDX-License-Identifier: BUSL-1.1

use super::*;

fn parts_fixture() -> Vec<(u32, String)> {
    vec![(1, "\"etag1\"".to_string()), (2, "\"etag2\"".to_string())]
}

#[tokio::test]
async fn completion_registry_owner_join_tombstone_lifecycle() {
    let store = std::sync::Arc::new(MultipartStore::new(1024));
    let parts = parts_fixture();

    // First request owns the completion.
    let owner = match store.begin_complete("up1", "b", "k", &parts).unwrap() {
        BeginComplete::Owner(p) => p,
        _ => panic!("first begin_complete must be Owner"),
    };
    // An identical retry while in flight joins.
    let rx = match store.begin_complete("up1", "b", "k", &parts).unwrap() {
        BeginComplete::Join(rx) => rx,
        _ => panic!("identical retry must Join"),
    };
    // A different part list while in flight is refused.
    let other = vec![(1, "\"other\"".to_string())];
    assert!(store.begin_complete("up1", "b", "k", &other).is_err());

    // Owner publishes success: joiner sees the etag, later retries hit the tombstone.
    owner.publish(Ok("\"final-etag\"".to_string()));
    assert_eq!(rx.borrow().clone().unwrap().unwrap(), "\"final-etag\"");
    match store.begin_complete("up1", "b", "k", &parts).unwrap() {
        BeginComplete::AlreadyDone { etag } => assert_eq!(etag, "\"final-etag\""),
        _ => panic!("retry after success must hit the tombstone"),
    }
    // Tombstone with a different part list is refused; different bucket/key
    // too. The completed upload is gone, so the answer is NoSuchUpload.
    for (bucket, parts) in [("b", &other), ("OTHER", &parts)] {
        assert!(matches!(
            store.begin_complete("up1", bucket, "k", parts),
            Err(S3Error::NoSuchUpload(_))
        ));
    }
}

#[tokio::test]
async fn completion_registry_failure_clears_slot_for_fresh_retry() {
    let store = std::sync::Arc::new(MultipartStore::new(1024));
    let parts = parts_fixture();
    let owner = match store.begin_complete("up2", "b", "k", &parts).unwrap() {
        BeginComplete::Owner(p) => p,
        _ => panic!("must be Owner"),
    };
    owner.publish(Err(CompletionFailure::internal("backend exploded")));
    // Failure clears the slot: the next attempt is a fresh Owner, not a join.
    assert!(matches!(
        store.begin_complete("up2", "b", "k", &parts).unwrap(),
        BeginComplete::Owner(_)
    ));
}

/// A joined retry answers the owner's S3 error, not a 500 (s3surface-17).
#[tokio::test]
async fn completion_joiner_sees_the_owners_error() {
    let store = std::sync::Arc::new(MultipartStore::new(1024));
    let parts = parts_fixture();
    let owner = match store.begin_complete("up4", "b", "k", &parts).unwrap() {
        BeginComplete::Owner(p) => p,
        _ => panic!("must be Owner"),
    };
    let rx = match store.begin_complete("up4", "b", "k", &parts).unwrap() {
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
    let owner = match store.begin_complete("up3", "b", "k", &parts).unwrap() {
        BeginComplete::Owner(p) => p,
        _ => panic!("must be Owner"),
    };
    let rx = match store.begin_complete("up3", "b", "k", &parts).unwrap() {
        BeginComplete::Join(rx) => rx,
        _ => panic!("must Join"),
    };
    drop(owner); // task panicked / was aborted without publishing
    assert!(
        rx.borrow().clone().unwrap().is_err(),
        "joiner must be released"
    );
    assert!(matches!(
        store.begin_complete("up3", "b", "k", &parts).unwrap(),
        BeginComplete::Owner(_)
    ));
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
    let etag = store
        .upload_part(&upload_id, "bucket", "key.bin", 1, data)
        .unwrap();
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
    let etag1 = store
        .upload_part(&upload_id, "bucket", "key.bin", 1, part1.clone())
        .unwrap();
    let etag2 = store
        .upload_part(&upload_id, "bucket", "key.bin", 2, part2.clone())
        .unwrap();

    let result = store
        .complete(&upload_id, "bucket", "key.bin", &[(1, etag1), (2, etag2)])
        .unwrap();

    assert_eq!(result.data.len(), 300);
    assert_eq!(&result.data[..100], &[1u8; 100]);
    assert_eq!(&result.data[100..], &[2u8; 200]);
    assert!(result.etag.ends_with("-2\""));
}

#[test]
fn test_abort() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();
    store.abort(&upload_id, "bucket", "key.bin").unwrap();

    let result = store.upload_part(
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

    let result = store.upload_part(
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

    let result = store.upload_part(
        &upload_id,
        "bucket",
        "key.bin",
        0,
        Bytes::from(vec![0u8; 10]),
    );
    assert!(result.is_err());

    let result = store.upload_part(
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
        store
            .upload_part(
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

    let etag1 = store
        .upload_part(
            &upload_id,
            "bucket",
            "key.bin",
            1,
            Bytes::from(vec![1u8; 100]),
        )
        .unwrap();
    let etag2 = store
        .upload_part(
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
    store
        .upload_part(
            &upload_id,
            "bucket",
            "key.bin",
            1,
            Bytes::from(vec![1u8; 100]),
        )
        .unwrap();

    // Complete with empty parts list should fail
    let result = store.complete(&upload_id, "bucket", "key.bin", &[]);
    assert!(result.is_err(), "complete with zero parts should fail");
}

#[test]
fn test_complete_with_wrong_etag() {
    let store = MultipartStore::new(100 * 1024 * 1024);
    let upload_id = store
        .create("bucket", "key.bin", None, HashMap::new())
        .unwrap();
    store
        .upload_part(
            &upload_id,
            "bucket",
            "key.bin",
            1,
            Bytes::from(vec![1u8; 100]),
        )
        .unwrap();

    // Complete with wrong etag should fail
    let result = store.complete(
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
    let etag1 = store
        .upload_part(&upload_id, "bucket", "key.bin", 1, part1)
        .unwrap();
    let etag3 = store
        .upload_part(&upload_id, "bucket", "key.bin", 3, part3)
        .unwrap();

    // Parts 1 and 3 (skip 2) — should succeed per S3 spec
    let result = store
        .complete(&upload_id, "bucket", "key.bin", &[(1, etag1), (3, etag3)])
        .unwrap();
    assert_eq!(result.data.len(), 200);
    assert_eq!(&result.data[..100], &[1u8; 100]);
    assert_eq!(&result.data[100..], &[3u8; 100]);
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
    store
        .upload_part(&upload_id, "bucket", "key.bin", 1, data)
        .unwrap();
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
        .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
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
        .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
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
    store
        .upload_part(
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
        .complete(&upload_b, "bucket", "other.bin", &[(1, etag)])
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
        .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
        .unwrap();

    let err = store
        .upload_part(
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
        .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
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
        .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
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
        .complete(&upload_id, "bucket", "key.bin", &[(1, etag.clone())])
        .unwrap();
    let err = store
        .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
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
        .complete(
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
    store
        .upload_part(&upload_id, "bucket", "key", 1, Bytes::from(vec![0u8; 700]))
        .unwrap();
    store
        .upload_part(&upload_id, "bucket", "key", 2, Bytes::from(vec![0u8; 200]))
        .unwrap();
    let err = store
        .upload_part(&upload_id, "bucket", "key", 3, Bytes::from(vec![0u8; 300]))
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
    store
        .upload_part(&upload_id, "bucket", "key", 1, Bytes::from(vec![0u8; 1000]))
        .unwrap();
    // Add 400 more via a second part — total 1400, under cap.
    store
        .upload_part(&upload_id, "bucket", "key", 2, Bytes::from(vec![0u8; 400]))
        .unwrap();
    // Now overwrite part 1 with 200 B. New cumulative = 200 + 400 = 600.
    store
        .upload_part(&upload_id, "bucket", "key", 1, Bytes::from(vec![0u8; 200]))
        .unwrap();
    // Counter should reflect the overwrite.
    assert_eq!(store.in_flight_bytes(), 600);
}

#[test]
fn test_upload_part_respects_global_byte_cap() {
    // Tight global cap: 2 KiB total across all uploads.
    let store = MultipartStore::new_for_test(10 * 1024, 2 * 1024, Duration::hours(24));
    let id_a = store.create("b", "a", None, HashMap::new()).unwrap();
    let id_b = store.create("b", "b", None, HashMap::new()).unwrap();

    // Fill upload A to 1 KiB.
    store
        .upload_part(&id_a, "b", "a", 1, Bytes::from(vec![0u8; 1024]))
        .unwrap();
    // Fill upload B to 1 KiB (total now 2 KiB = cap).
    store
        .upload_part(&id_b, "b", "b", 1, Bytes::from(vec![0u8; 1024]))
        .unwrap();
    // Next byte anywhere → SlowDown.
    let err = store
        .upload_part(&id_a, "b", "a", 2, Bytes::from(vec![0u8; 1]))
        .unwrap_err();
    assert!(matches!(err, S3Error::SlowDown(_)), "got {:?}", err);
}

#[test]
fn test_abort_releases_in_flight_bytes() {
    let store = MultipartStore::new_for_test(10 * 1024, 2 * 1024, Duration::hours(24));
    let id = store.create("b", "a", None, HashMap::new()).unwrap();
    store
        .upload_part(&id, "b", "a", 1, Bytes::from(vec![0u8; 1024]))
        .unwrap();
    assert_eq!(store.in_flight_bytes(), 1024);

    store.abort(&id, "b", "a").unwrap();
    assert_eq!(
        store.in_flight_bytes(),
        0,
        "abort must release bytes to the global counter"
    );
}

#[test]
fn test_finish_upload_releases_in_flight_bytes() {
    let store = MultipartStore::new_for_test(10 * 1024, 10 * 1024, Duration::hours(24));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let data = Bytes::from(vec![0u8; 500]);
    let etag = store.upload_part(&id, "b", "k", 1, data).unwrap();
    assert_eq!(store.in_flight_bytes(), 500);

    store.complete(&id, "b", "k", &[(1, etag)]).unwrap();
    // Still in map (Completing) — counter unchanged.
    assert_eq!(store.in_flight_bytes(), 500);

    store.finish_upload(&id);
    assert_eq!(store.in_flight_bytes(), 0);
}

#[test]
fn test_cleanup_expired_idle_ttl_sweeps_and_releases_bytes() {
    // Tiny idle TTL so we can trip it synchronously.
    let store = MultipartStore::new_for_test(10 * 1024, 10 * 1024, Duration::milliseconds(1));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    store
        .upload_part(&id, "b", "k", 1, Bytes::from(vec![0u8; 700]))
        .unwrap();
    assert_eq!(store.in_flight_bytes(), 700);

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
    let store = MultipartStore::new_for_test(10 * 1024, 10 * 1024, Duration::hours(24));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let etag = store
        .upload_part(&id, "b", "k", 1, Bytes::from(vec![0u8; 100]))
        .unwrap();
    store.complete(&id, "b", "k", &[(1, etag)]).unwrap();

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
    let store = MultipartStore::new_for_test(10 * 1024, 10 * 1024, Duration::hours(24));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let etag = store
        .upload_part(&id, "b", "k", 1, Bytes::from(vec![0u8; 100]))
        .unwrap();
    store.complete(&id, "b", "k", &[(1, etag)]).unwrap();
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
        10 * 1024,
        Duration::hours(24),
    ));
    let id = store.create("b", "k", None, HashMap::new()).unwrap();
    let etag = store
        .upload_part(&id, "b", "k", 1, Bytes::from(vec![0u8; 100]))
        .unwrap();
    store.complete(&id, "b", "k", &[(1, etag)]).unwrap();
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
fn relayed_part_load_bytes_rejects_substituted_content() {
    // H20: a relay part file swapped on disk (shared-host local attacker)
    // must be REJECTED at read time — the recorded MD5 no longer matches.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("part-00001.bin");
    let good = Bytes::from_static(b"the-real-part-bytes");
    fs::write(&path, &good).unwrap();
    let expected: [u8; 16] = Md5::digest(&good).into();

    let spool =
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 1 << 20).unwrap();
    let payload = PartPayload::RelayedFile(path.clone(), spool.try_reserve(1, 0).unwrap());
    // Untouched → loads fine.
    assert_eq!(payload.load_bytes(&expected).unwrap(), good);

    // Attacker swaps the file content → integrity check fails.
    fs::write(&path, b"attacker-controlled-bytes").unwrap();
    let err = payload.load_bytes(&expected).unwrap_err();
    assert!(
        format!("{err:?}").contains("integrity check"),
        "substituted part must be rejected, got {err:?}"
    );
}

#[test]
fn test_relay_promotion_on_threshold_cross() {
    let store = MultipartStore::new(10 * 1024);
    let id = store
        .create_with_relay_policy("b", "k", None, HashMap::new(), Some(512), false)
        .unwrap();

    store
        .upload_part(&id, "b", "k", 1, Bytes::from(vec![0u8; 256]))
        .unwrap();
    {
        let uploads = store.uploads.read();
        let upload = uploads.get(&id).unwrap();
        assert!(matches!(
            upload.relay_strategy,
            RelayStrategy::InMemory { .. }
        ));
    }

    store
        .upload_part(&id, "b", "k", 2, Bytes::from(vec![1u8; 300]))
        .unwrap();
    let uploads = store.uploads.read();
    let upload = uploads.get(&id).unwrap();
    assert!(matches!(
        upload.relay_strategy,
        RelayStrategy::Relayed { .. }
    ));
    let part1 = upload.parts.get(&1).unwrap();
    let part2 = upload.parts.get(&2).unwrap();
    assert!(matches!(part1.payload, PartPayload::RelayedFile(..)));
    assert!(matches!(part2.payload, PartPayload::RelayedFile(..)));
}

#[test]
fn test_complete_passthrough_returns_relayed_file_payload() {
    let store = MultipartStore::new(10 * 1024);
    let id = store
        .create_with_relay_policy("b", "k", None, HashMap::new(), None, true)
        .unwrap();
    let e1 = store
        .upload_part(&id, "b", "k", 1, Bytes::from_static(b"hello"))
        .unwrap();
    let e2 = store
        .upload_part(&id, "b", "k", 2, Bytes::from_static(b"world"))
        .unwrap();

    let completed = store
        .complete_passthrough(&id, "b", "k", &[(1, e1), (2, e2)])
        .unwrap();
    assert_eq!(completed.total_size, 10);
    match completed.payload {
        PassthroughPayload::RelayedParts(paths) => {
            assert_eq!(paths.len(), 2);
            let mut data = Vec::new();
            for path in paths {
                data.extend_from_slice(&std::fs::read(path).unwrap());
            }
            assert_eq!(data, b"helloworld");
        }
        PassthroughPayload::Chunks(_) => {
            panic!("expected relayed part payload for always-relay upload")
        }
    }
}

/// Relay part files are scratch files: they must live in the spool dir
/// (and count against its budget). They were under the system temp dir.
#[test]
fn relay_parts_live_in_the_spool_dir() {
    let spool = crate::deltaglider::spool::SpoolDir::shared().unwrap();
    let store = MultipartStore::new(10 * 1024);
    let id = store
        .create_with_relay_policy("b", "k", None, HashMap::new(), None, true)
        .unwrap();
    store
        .upload_part(&id, "b", "k", 1, Bytes::from_static(b"hello"))
        .unwrap();
    let uploads = store.uploads.read();
    let part = uploads.get(&id).unwrap().parts.get(&1).unwrap();
    let PartPayload::RelayedFile(path, ..) = &part.payload else {
        panic!("expected a relayed part");
    };
    assert!(
        path.starts_with(spool.dir()),
        "relay part {path:?} is outside the spool dir {:?}",
        spool.dir()
    );
}

fn small_spool(dir: &tempfile::TempDir, mib: u64) -> SpoolDir {
    SpoolDir::new(dir.path().join("spool"), mib << 20).unwrap()
}

/// A relay part holds spool budget from UploadPart until the part is
/// dropped: overwrite, abort, or the end of the upload.
#[test]
fn relay_parts_hold_spool_budget_until_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let spool = small_spool(&dir, 32);
    let store = MultipartStore::new(64 << 20)
        .with_spool(spool.clone())
        .with_relay_upload_max(Some(8 << 20));
    let id = store
        .create_with_relay_policy("b", "k", None, HashMap::new(), None, true)
        .unwrap();
    let mib = |n: usize| Bytes::from(vec![7u8; n << 20]);
    store.upload_part(&id, "b", "k", 1, mib(2)).unwrap();
    store.upload_part(&id, "b", "k", 2, mib(3)).unwrap();
    assert_eq!(spool.free_mib(), 27);
    // Overwrite: the old part's budget goes back.
    store.upload_part(&id, "b", "k", 2, mib(1)).unwrap();
    assert_eq!(spool.free_mib(), 29);
    store.abort(&id, "b", "k").unwrap();
    assert_eq!(spool.free_mib(), 32);

    // Promotion reserves for the parts that were in memory.
    let id = store
        .create_with_relay_policy("b", "k", None, HashMap::new(), Some(3 << 20), false)
        .unwrap();
    store.upload_part(&id, "b", "k", 1, mib(2)).unwrap();
    assert_eq!(spool.free_mib(), 32, "in-memory parts hold no spool");
    store.upload_part(&id, "b", "k", 2, mib(2)).unwrap();
    assert_eq!(spool.free_mib(), 28);
    store.finish_upload(&id);
    assert_eq!(spool.free_mib(), 32);
}

/// UploadPart never waits for spool budget (it runs under the uploads
/// lock, and the upload may hold parts): a full budget is SlowDown now.
#[test]
fn relay_part_with_the_budget_taken_is_slowdown_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let spool = small_spool(&dir, 4);
    let store = MultipartStore::new(64 << 20).with_spool(spool.clone());
    let id = store
        .create_with_relay_policy("b", "k", None, HashMap::new(), None, true)
        .unwrap();
    let other = spool.try_acquire(4 << 20).unwrap();
    let err = store
        .upload_part(&id, "b", "k", 1, Bytes::from(vec![1u8; 1 << 20]))
        .unwrap_err();
    assert!(matches!(err, S3Error::SlowDown(_)), "got {err:?}");
    drop(other);
    store
        .upload_part(&id, "b", "k", 1, Bytes::from(vec![1u8; 1 << 20]))
        .unwrap();
}

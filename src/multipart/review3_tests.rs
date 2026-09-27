// SPDX-License-Identifier: BUSL-1.1

use super::*;

/// A relay part is clamped to what its upload does not hold yet: once an
/// upload holds the whole budget, every further part reserves 0 MiB and
/// is written anyway. The budget then pins every other request of the
/// node (up to the 24 h idle TTL for an abandoned upload) while the disk
/// use of this one upload grows without a bound.
#[test]
fn review3_relay_parts_past_the_budget_are_not_written_for_free() {
    let dir = tempfile::tempdir().unwrap();
    // 16 MiB budget: one upload may hold 4 MiB of it.
    let spool = SpoolDir::new(dir.path().join("spool"), 16 << 20).unwrap();
    let store = MultipartStore::new(64 << 20)
        .with_spool(spool.clone())
        .with_relay_upload_max(Some(4 << 20));
    let id = store
        .create_with_relay_policy("b", "k", None, HashMap::new(), None, true)
        .unwrap();
    let mib = |n: usize| Bytes::from(vec![7u8; n << 20]);
    store.upload_part(&id, "b", "k", 1, mib(4)).unwrap();
    assert_eq!(spool.free_mib(), 12);
    let mut accepted = 0;
    for n in 2..=4 {
        match store.upload_part(&id, "b", "k", n, mib(4)) {
            Ok(_) => accepted += 1,
            Err(e) => {
                let msg = e.to_string();
                assert_eq!(e.code(), "EntityTooLarge", "{e:?}");
                assert!(
                    msg.contains(RELAY_UPLOAD_MAX_ENV) && msg.contains("4194304"),
                    "the message names the variable and the limit: {msg}"
                );
            }
        }
    }
    assert_eq!(
        accepted, 0,
        "{accepted} more 4 MiB parts were written past the upload's share of the budget"
    );
    assert_eq!(spool.free_mib(), 12, "a refused part reserves nothing");
    // Re-uploading part 1 replaces it: within the share.
    store.upload_part(&id, "b", "k", 1, mib(4)).unwrap();
}

#[test]
fn relay_upload_cap_truth_table() {
    const MIB: u64 = 1 << 20;
    assert_eq!(relay_upload_cap(16 * MIB, None), Some(8 * MIB));
    assert_eq!(relay_upload_cap(MIB, None), Some(MIB), "never below 1 MiB");
    assert_eq!(relay_upload_cap(16 * MIB, Some(0)), None);
    assert_eq!(relay_upload_cap(16 * MIB, Some(3 * MIB)), Some(3 * MIB));
}

/// `0` = no per-upload cap: the upload may use the whole budget, and a
/// part past the budget is a SlowDown (never written for free).
#[test]
fn no_per_upload_cap_leaves_only_the_global_budget() {
    let dir = tempfile::tempdir().unwrap();
    let spool = SpoolDir::new(dir.path().join("spool"), 8 << 20).unwrap();
    let store = MultipartStore::new(64 << 20)
        .with_spool(spool.clone())
        .with_relay_upload_max(Some(0));
    let id = store
        .create_with_relay_policy("b", "k", None, HashMap::new(), None, true)
        .unwrap();
    let mib = |n: usize| Bytes::from(vec![7u8; n << 20]);
    store.upload_part(&id, "b", "k", 1, mib(8)).unwrap();
    let err = store.upload_part(&id, "b", "k", 2, mib(1)).unwrap_err();
    assert!(matches!(err, S3Error::SlowDown(_)), "{err:?}");
}

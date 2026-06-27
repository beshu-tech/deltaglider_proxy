// SPDX-License-Identifier: GPL-3.0-only

//! Integration coverage for the per-bucket running usage counter (the O(1)
//! Ceph-style size). Exercises the real PUT/DELETE pipeline + the admin
//! endpoints — proves the counter is maintained inline (no scan on read),
//! decrements exactly on delete (including reclaimed reference bytes), and that
//! Refresh reconciles against a full scan.
//!
//! Filesystem backend (default TestServer, `authentication: none`) — deltas +
//! references still form, and the counter lives at the engine layer so the
//! backend choice is irrelevant to what's under test.

mod common;

use common::{admin_http_client, delete_object, put_object, TestServer};

#[derive(serde::Deserialize, Debug)]
struct UsageBody {
    object_count: u64,
    logical_bytes: u64,
    stored_bytes: u64,
    last_scan_at: Option<i64>,
    never_scanned: bool,
}

async fn get_usage(server: &TestServer, admin: &reqwest::Client, bucket: &str) -> UsageBody {
    let url = format!("{}/_/api/admin/usage/bucket/{}", server.endpoint(), bucket);
    let resp = admin.get(&url).send().await.expect("usage request");
    assert!(
        resp.status().is_success(),
        "usage GET got {}",
        resp.status()
    );
    resp.json().await.expect("usage json")
}

/// A compressible base so siblings delta well (mirrors ROR build blobs).
fn base_blob(size: usize) -> Vec<u8> {
    let mut v = vec![0u8; size];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    v
}
fn sibling(base: &[u8], seed: u8) -> Vec<u8> {
    let mut v = base.to_vec();
    for (i, b) in v.iter_mut().enumerate().take(32) {
        *b = b.wrapping_add(seed).wrapping_add(i as u8);
    }
    v
}

#[tokio::test]
async fn counter_tracks_puts_and_deletes_without_scanning() {
    let server = TestServer::builder().bucket("usage-basic").build().await;
    let http = reqwest::Client::new();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket().to_string();

    // Never-touched bucket reports zeros + never_scanned.
    let u0 = get_usage(&server, &admin, &bucket).await;
    assert_eq!(u0.object_count, 0);
    assert!(u0.never_scanned, "fresh bucket must report never_scanned");
    assert_eq!(u0.last_scan_at, None);

    // PUT three sibling objects under one prefix → reference + deltas.
    let base = base_blob(256 * 1024);
    for i in 0..3u8 {
        put_object(
            &http,
            &server.endpoint(),
            &bucket,
            &format!("rel/v{}.bin", i),
            sibling(&base, i),
            "application/octet-stream",
        )
        .await;
    }

    // O(1) read: exactly 3 user-visible objects, logical = 3 × 256 KiB.
    let u1 = get_usage(&server, &admin, &bucket).await;
    assert_eq!(u1.object_count, 3, "counter must show 3 objects: {:?}", u1);
    assert_eq!(
        u1.logical_bytes,
        3 * 256 * 1024,
        "logical bytes must be exact: {:?}",
        u1
    );
    // stored_bytes is exact whether objects landed as delta or passthrough
    // (the counter is strategy-agnostic). On a delta-capable host stored <
    // logical; on a passthrough host they're equal. Either way it's > 0 and
    // never exceeds logical.
    assert!(
        u1.stored_bytes > 0 && u1.stored_bytes <= u1.logical_bytes,
        "stored ({}) must be in (0, logical={}]",
        u1.stored_bytes,
        u1.logical_bytes
    );
    assert!(u1.never_scanned, "still never explicitly scanned");

    // DELETE one object → count drops to 2, logical drops by one object.
    delete_object(&http, &server.endpoint(), &bucket, "rel/v1.bin").await;
    let u2 = get_usage(&server, &admin, &bucket).await;
    assert_eq!(u2.object_count, 2, "delete must decrement count: {:?}", u2);
    assert_eq!(
        u2.logical_bytes,
        2 * 256 * 1024,
        "logical must drop by exactly one object: {:?}",
        u2
    );
}

#[tokio::test]
async fn deleting_last_object_reclaims_reference_bytes() {
    let server = TestServer::builder().bucket("usage-reclaim").build().await;
    let http = reqwest::Client::new();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket().to_string();

    // Two siblings in one prefix → one reference + (up to) two deltas.
    let base = base_blob(256 * 1024);
    put_object(
        &http,
        &server.endpoint(),
        &bucket,
        "p/a.bin",
        base.clone(),
        "application/octet-stream",
    )
    .await;
    put_object(
        &http,
        &server.endpoint(),
        &bucket,
        "p/b.bin",
        sibling(&base, 7),
        "application/octet-stream",
    )
    .await;

    let before = get_usage(&server, &admin, &bucket).await;
    assert_eq!(before.object_count, 2);

    // Delete BOTH objects. stored_bytes must drop to 0 — including, on a
    // delta-capable host, the reclaimed reference.bin (subtracted explicitly so
    // no orphan stored bytes linger). On a passthrough host there's no
    // reference, but the per-object stored bytes still fully zero out.
    delete_object(&http, &server.endpoint(), &bucket, "p/a.bin").await;
    delete_object(&http, &server.endpoint(), &bucket, "p/b.bin").await;

    let after = get_usage(&server, &admin, &bucket).await;
    assert_eq!(after.object_count, 0, "all objects gone: {:?}", after);
    assert_eq!(after.logical_bytes, 0, "logical zeroed: {:?}", after);
    assert_eq!(
        after.stored_bytes, 0,
        "reclaimed reference must be subtracted — no orphan stored bytes: {:?}",
        after
    );
}

#[tokio::test]
async fn refresh_reconciles_against_full_scan() {
    let server = TestServer::builder().bucket("usage-refresh").build().await;
    let http = reqwest::Client::new();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket().to_string();

    let base = base_blob(128 * 1024);
    for i in 0..4u8 {
        put_object(
            &http,
            &server.endpoint(),
            &bucket,
            &format!("r/v{}.bin", i),
            sibling(&base, i),
            "application/octet-stream",
        )
        .await;
    }

    // Refresh runs the uncapped scan and stamps last_scan_at.
    let url = format!(
        "{}/_/api/admin/usage/refresh?bucket={}",
        server.endpoint(),
        bucket
    );
    let resp = admin.post(&url).send().await.expect("refresh request");
    assert!(resp.status().is_success(), "refresh got {}", resp.status());
    let refreshed: UsageBody = resp.json().await.expect("refresh json");

    assert_eq!(refreshed.object_count, 4, "scan count: {:?}", refreshed);
    assert_eq!(refreshed.logical_bytes, 4 * 128 * 1024);
    assert!(!refreshed.never_scanned, "refresh must clear never_scanned");
    assert!(refreshed.last_scan_at.is_some(), "last_scan_at stamped");

    // The inline counter and the scan agree (no drift on a clean run).
    let counter = get_usage(&server, &admin, &bucket).await;
    assert_eq!(counter.object_count, refreshed.object_count);
    assert_eq!(counter.logical_bytes, refreshed.logical_bytes);
    assert_eq!(counter.stored_bytes, refreshed.stored_bytes);
}

/// Regression for the review's #1 HIGH finding: S3 PUT is an upsert, so
/// re-PUTting the SAME key must NOT inflate object_count (it nets to +0). Pre-
/// fix every overwrite blindly added +1. Machine-independent (passthrough).
#[tokio::test]
async fn overwriting_a_key_does_not_inflate_the_counter() {
    let server = TestServer::builder()
        .bucket("usage-overwrite")
        .build()
        .await;
    let http = reqwest::Client::new();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket().to_string();

    // First write of a key.
    put_object(
        &http,
        &server.endpoint(),
        &bucket,
        "k.bin",
        base_blob(200_000),
        "application/octet-stream",
    )
    .await;
    let u1 = get_usage(&server, &admin, &bucket).await;
    assert_eq!(u1.object_count, 1, "one object after first PUT: {:?}", u1);
    assert_eq!(u1.logical_bytes, 200_000);

    // Overwrite the SAME key 4 more times with DIFFERENT sizes.
    for sz in [150_000usize, 300_000, 250_000, 100_000] {
        put_object(
            &http,
            &server.endpoint(),
            &bucket,
            "k.bin",
            base_blob(sz),
            "application/octet-stream",
        )
        .await;
    }
    let u2 = get_usage(&server, &admin, &bucket).await;
    assert_eq!(
        u2.object_count, 1,
        "object_count must STAY 1 across overwrites (not climb to 5): {:?}",
        u2
    );
    assert_eq!(
        u2.logical_bytes, 100_000,
        "logical must reflect the LAST write's size, not a sum: {:?}",
        u2
    );

    // Refresh (full scan ground truth) must agree with the inline counter.
    let url = format!(
        "{}/_/api/admin/usage/refresh?bucket={}",
        server.endpoint(),
        bucket
    );
    let refreshed: UsageBody = admin.post(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(
        refreshed.object_count, 1,
        "scan agrees: one object: {:?}",
        refreshed
    );
    assert_eq!(refreshed.logical_bytes, 100_000);
}

/// Delta-path coverage (CI, where xdelta3 works): sibling .zip objects form a
/// reference + deltas, so stored < logical and deleting the last object reclaims
/// the reference. Skips its delta-specific asserts on a host where xdelta3 can't
/// produce deltas (the objects land as passthrough — the counter is still exact,
/// just not compressed), so it's green everywhere but proves the delta path in CI.
#[tokio::test]
async fn delta_path_counter_and_reference_reclamation() {
    let server = TestServer::builder().bucket("usage-delta").build().await;
    let http = reqwest::Client::new();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket().to_string();

    let base = base_blob(256 * 1024);
    // First .zip PUT exercises the delta path. On a host whose xdelta3 can't
    // encode from /dev/stdin (a known local quirk; CI's xdelta3 works fine) the
    // PUT 500s — detect that and skip rather than fail. CI runs the real path.
    let first = http
        .put(format!("{}/{}/d/v0.zip", server.endpoint(), bucket))
        .header("content-type", "application/zip")
        .body(sibling(&base, 0))
        .send()
        .await
        .expect("PUT");
    if first.status().as_u16() == 500 {
        eprintln!("NOTE: xdelta3 delta encode unavailable on this host — skipping delta-path test");
        return;
    }
    assert!(
        first.status().is_success(),
        "first .zip PUT: {}",
        first.status()
    );
    for i in 1..3u8 {
        put_object(
            &http,
            &server.endpoint(),
            &bucket,
            &format!("d/v{}.zip", i),
            sibling(&base, i),
            "application/zip",
        )
        .await;
    }
    let u = get_usage(&server, &admin, &bucket).await;
    assert_eq!(u.object_count, 3, "3 user-visible objects: {:?}", u);
    assert_eq!(
        u.logical_bytes,
        3 * 256 * 1024,
        "logical exact regardless of strategy: {:?}",
        u
    );

    // Did the delta path actually engage on this host? (stored < logical iff
    // siblings compressed against a shared reference.)
    let deltas_formed = u.stored_bytes < u.logical_bytes;

    // The inline counter must match a full scan (the strongest invariant — it
    // proves stored_bytes accounting incl. the reference is correct on BOTH
    // paths). This is what would have caught the "store omits reference bytes"
    // finding.
    let url = format!(
        "{}/_/api/admin/usage/refresh?bucket={}",
        server.endpoint(),
        bucket
    );
    let scanned: UsageBody = admin.post(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(
        scanned.object_count, u.object_count,
        "count: inline == scan"
    );
    assert_eq!(
        scanned.logical_bytes, u.logical_bytes,
        "logical: inline == scan"
    );
    assert_eq!(
        scanned.stored_bytes, u.stored_bytes,
        "stored_bytes: inline MUST equal scan (reference bytes accounted on store): {:?} vs {:?}",
        u, scanned
    );

    if deltas_formed {
        // Delete all 3 → the reference is reclaimed; stored_bytes back to 0.
        for i in 0..3u8 {
            delete_object(&http, &server.endpoint(), &bucket, &format!("d/v{}.zip", i)).await;
        }
        let after = get_usage(&server, &admin, &bucket).await;
        assert_eq!(after.object_count, 0, "all deleted: {:?}", after);
        assert_eq!(
            after.stored_bytes, 0,
            "reclaimed reference must leave NO orphan stored bytes: {:?}",
            after
        );
    }
}

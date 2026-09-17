// SPDX-License-Identifier: BUSL-1.1

//! S3 backend parity tests
//!
//! TWO tests only (trimmed in a prior QA pass — see the note below): an
//! S3-plumbing smoke test and the delta+S3 interaction that no other suite
//! covers. NOT a re-run of s3_api_test's operations — those trait-level
//! behaviours are guaranteed by the AWS SDK + the filesystem suites, so we
//! don't re-pay a MinIO round-trip for them. Both gated with
//! skip_unless_minio!() — skip gracefully without MinIO.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{generate_binary, minio_client, mutate_binary, TestServer, MINIO_BUCKET};
use std::sync::atomic::{AtomicU64, Ordering};

/// Counter for unique test prefixes
static PREFIX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique prefix to isolate each test's data in the shared MinIO bucket
fn unique_prefix() -> String {
    let counter = PREFIX_COUNTER.fetch_add(1, Ordering::SeqCst);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("test-{}-{}", timestamp, counter)
}

#[tokio::test]
async fn test_s3_put_get_roundtrip() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    let data = b"Hello via S3 backend!";
    let key = format!("{}/hello.txt", prefix);

    client
        .put_object()
        .bucket(server.bucket())
        .key(&key)
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();
    let body = client
        .get_object()
        .bucket(server.bucket())
        .key(&key)
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), data);
}

// ── 9 filesystem-parity tests removed in QA hygiene pass ──────────────
//
// QA review finding #2: test_s3_put_get_delete_lifecycle,
// test_s3_put_overwrite, test_s3_list_objects_with_prefix,
// test_s3_list_objects_pagination, test_s3_copy_object,
// test_s3_delete_objects_batch, test_s3_head_object,
// test_s3_etag_consistent, and test_s3_unicode_key each verified
// `StorageBackend` trait behaviour that is already guaranteed by the
// filesystem-backed s3_api_test + s3_compat_test + s3_integration_test
// suites. Every such test spent a MinIO round-trip to re-verify trait
// semantics; S3-level differences are an AWS-SDK guarantee, not a
// proxy-level regression surface.
//
// What stayed, and why:
//   - test_s3_put_get_roundtrip  — smoke test for the S3-plumbing path
//     (SigV4 to MinIO, body bytestream, no delta pipeline). One
//     failure here tells you "the S3 backend is wired up at all."
//   - test_s3_delta_similar_files — real delta+S3 interaction: the
//     store.rs path that compresses v2 against v1's reference and
//     the retrieve.rs path that rehydrates. Neither filesystem
//     integration tests nor s3_integration_test exercises THIS
//     specific combination.
//
// If the S3 backend ever grows features that ARE S3-specific and
// don't round-trip through the trait (server-side encryption,
// object-lock, storage classes, requester-pays), add targeted tests
// HERE — keep them tight, one feature per test.
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_s3_delta_similar_files() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let base = generate_binary(100_000, 42);
    let variant = mutate_binary(&base, 0.01);

    // PUT base
    let url1 = format!(
        "{}/{}/{}/base.zip",
        server.endpoint(),
        server.bucket(),
        prefix
    );
    let resp1 = http
        .put(&url1)
        .header("content-type", "application/zip")
        .body(base.clone())
        .send()
        .await
        .unwrap();
    assert!(resp1.status().is_success());

    // PUT variant
    let url2 = format!(
        "{}/{}/{}/v1.zip",
        server.endpoint(),
        server.bucket(),
        prefix
    );
    let resp2 = http
        .put(&url2)
        .header("content-type", "application/zip")
        .body(variant.clone())
        .send()
        .await
        .unwrap();
    assert!(resp2.status().is_success());

    // Verify both retrievable
    let got_base = http.get(&url1).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(got_base.as_ref(), base.as_slice());

    let got_v1 = http.get(&url2).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(got_v1.as_ref(), variant.as_slice());
}

/// Issue #82: a delimiter-less listing must stop after about one upstream page
/// instead of reading the whole subtree — WITHOUT dropping a key whose raw form
/// sorts after the page.
///
/// Layout under a unique prefix `P` (= `test-<ts>-<n>`):
///   - `P/1.delta`                    → user key `P/1`   (the smallest user key)
///   - `P/1.0.<n>/app.zip.delta` ×1100 → forces upstream truncation
///   - `test-<ts>.delta` — a sibling OUTSIDE the listed prefix, at an escaping
///     candidate position for the settled anchor (`'.' > '-'`). An unscoped
///     confirmation probe would inject its user key `test-<ts>` as the FIRST
///     entry of the page, violating the S3 Prefix contract — the keys
///     assertion below fails on exactly that shape.
///
/// `P/1` sorts FIRST in user order, so a `MaxKeys=1` listing must return it.
/// Its raw key `P/1.delta` sorts AFTER every `P/1.0.*` key, so the fetch loop
/// stops at the anchor long before reaching it: the key is only served if the
/// late-candidate confirmation works. Reading forward to it instead is the
/// subtree scan this issue is about.
#[tokio::test]
async fn delimiterless_list_stops_early_without_dropping_late_delta_key() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    // Seed raw keys straight into the backend (no proxy round-trip per object).
    // Seed with a DIRECT MinIO client: these are internal raw keys (`.delta`),
    // which the proxy refuses on the client-facing API by design.
    let raw = minio_client().await;
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(32));
    let mut seeds: Vec<String> = (0..1100)
        .map(|n| format!("{prefix}/1.0.{n:04}/app.zip.delta"))
        .collect();
    seeds.push(format!("{prefix}/1.delta"));
    // The out-of-prefix sibling at an escaping candidate position (see the
    // doc comment). Present so the keys assertion doubles as the
    // prefix-escape regression test.
    seeds.push(format!("{}.delta", prefix.rsplit_once('-').unwrap().0));
    let mut handles = Vec::new();
    for key in seeds {
        let (c, b, s) = (raw.clone(), MINIO_BUCKET.to_string(), sem.clone());
        handles.push(tokio::spawn(async move {
            let _p = s.acquire().await.unwrap();
            c.put_object()
                .bucket(b)
                .key(&key)
                .body(ByteStream::from(b"x".to_vec()))
                .send()
                .await
                .expect("seed");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // Gate the perf property on request COUNTS, never wall-clock: at this
    // size a full drain (2 upstream pages) and an anchored exit (1 page) are
    // both sub-second, so only the counter can tell them apart.
    let pages_before = delegated_list_pages(&server.endpoint()).await;
    let probes_before = delegated_list_probes(&server.endpoint()).await;

    let started = std::time::Instant::now();
    let page = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix(format!("{prefix}/"))
        .max_keys(1)
        .send()
        .await
        .expect("list");
    let elapsed = started.elapsed();

    let pages = delegated_list_pages(&server.endpoint()).await - pages_before;
    let probes = delegated_list_probes(&server.endpoint()).await - probes_before;

    let keys: Vec<String> = page
        .contents()
        .iter()
        .filter_map(|o| o.key().map(str::to_string))
        .collect();
    eprintln!(
        "[info] delimiter-less MaxKeys=1 took {elapsed:?}, {pages} upstream \
         pages + {probes} probes, keys={keys:?}"
    );

    assert_eq!(
        keys,
        vec![format!("{prefix}/1")],
        "the smallest user key must be served even though its raw key sorts \
         after the page — dropping it is the correctness risk of stopping early"
    );
    assert!(
        page.is_truncated().unwrap_or(false),
        "1101 objects with MaxKeys=1 must report truncation"
    );
    assert_eq!(
        pages, 1,
        "the anchored early exit must settle on the FIRST upstream page — a \
         second page means the loop read on toward a horizon (the issue #82 \
         subtree scan, which this layout shrinks to exactly 2 pages)"
    );
    assert!(
        probes <= 3,
        "the candidate set for this anchor is 2 exact-key probes; {probes} \
         means candidate generation lost its bounds"
    );
}

/// Scrape one un-labelled counter from `GET /_/metrics`.
async fn scrape_counter(endpoint: &str, name: &str) -> u64 {
    let body = common::metrics_text(endpoint).await;
    body.lines()
        .filter(|l| !l.starts_with('#'))
        .find_map(|l| {
            let (n, v) = l.rsplit_once(' ')?;
            (n == name).then(|| v.trim().parse::<f64>().unwrap_or(0.0) as u64)
        })
        .unwrap_or(0)
}

async fn delegated_list_pages(endpoint: &str) -> u64 {
    scrape_counter(endpoint, "deltaglider_delegated_list_upstream_pages_total").await
}

async fn delegated_list_probes(endpoint: &str) -> u64 {
    scrape_counter(endpoint, "deltaglider_delegated_list_probe_requests_total").await
}

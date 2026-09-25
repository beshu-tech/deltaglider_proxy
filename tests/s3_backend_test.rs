// SPDX-License-Identifier: BUSL-1.1

//! S3 backend parity tests
//!
//! TWO tests only (trimmed in a prior QA pass — see the note below): an
//! S3-plumbing smoke test and the delta+S3 interaction that no other suite
//! covers. NOT a re-run of s3_api_test's operations — those trait-level
//! behaviours are guaranteed by the AWS SDK + the filesystem suites, so we
//! don't re-pay a MinIO round-trip for them. Both gated with
//! skip_unless_minio!() — skip gracefully without MinIO.

use crate::common;

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

async fn backend_heads(endpoint: &str) -> u64 {
    scrape_counter(endpoint, "deltaglider_backend_head_requests_total").await
}

/// Issue #92 and review C3: a client LIST reports the ORIGINAL size and ETag
/// of a delta, and sends ZERO backend HEADs doing it, on every node.
///
/// * The proxy that wrote the object (warm) lists the original size and the
///   same ETag a HEAD returns.
/// * A second proxy on the same bucket (cold caches: a restart, another
///   node) lists the original size and ETag too, from the durable listing
///   facts, still without a HEAD. Before C3 it listed the stored delta size.
/// * LastModified does not change between two LISTs.
#[tokio::test]
async fn list_reports_original_delta_sizes_without_backend_heads() {
    skip_unless_minio!();
    let writer = TestServer::s3().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let base = generate_binary(100_000, 7);
    let variant = mutate_binary(&base, 0.01);
    for (name, body) in [("base.zip", &base), ("v1.zip", &variant)] {
        let url = format!("{}/{}/{prefix}/{name}", writer.endpoint(), writer.bucket());
        let resp = http
            .put(&url)
            .header("content-type", "application/zip")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }
    let key = format!("{prefix}/v1.zip");

    let writer_client = writer.s3_client().await;
    let list_v1 = |client: aws_sdk_s3::Client, bucket: String| {
        let prefix = prefix.clone();
        let key = key.clone();
        async move {
            let out = client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(format!("{prefix}/"))
                .send()
                .await
                .unwrap();
            out.contents()
                .iter()
                .find(|o| o.key() == Some(key.as_str()))
                .cloned()
                .expect("v1.zip listed")
        }
    };

    // Warm (the writer), BEFORE any HEAD of the object: only the PUT can have
    // filled the cache, so this proves the PUT fill. Original size, no HEAD.
    let before = backend_heads(&writer.endpoint()).await;
    let warm = list_v1(writer_client.clone(), writer.bucket().to_string()).await;
    assert_eq!(
        backend_heads(&writer.endpoint()).await,
        before,
        "the writer sent no HEAD since the PUT, and the LIST sends none"
    );
    assert_eq!(warm.size(), Some(variant.len() as i64));

    // What a HEAD reports is the truth the LIST must match.
    let head = writer_client
        .head_object()
        .bucket(writer.bucket())
        .key(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(head.content_length(), Some(variant.len() as i64));
    assert_eq!(warm.e_tag(), head.e_tag());

    // Cold (a second proxy on the same bucket): original size, no HEAD.
    let reader = TestServer::s3().await;
    let reader_client = reader.s3_client().await;
    let before = backend_heads(&reader.endpoint()).await;
    let cold = list_v1(reader_client.clone(), reader.bucket().to_string()).await;
    assert_eq!(
        backend_heads(&reader.endpoint()).await,
        before,
        "a client LIST must not send HEADs"
    );
    assert_eq!(
        cold.size(),
        Some(variant.len() as i64),
        "a cold LIST reports the original size from the listing facts"
    );
    assert_eq!(cold.e_tag(), head.e_tag());
    let again = list_v1(reader_client, reader.bucket().to_string()).await;
    assert_eq!(
        again.last_modified(),
        cold.last_modified(),
        "LastModified must not flip between LISTs"
    );
}

/// Review C3: on a proxy-encrypted S3 backend, a cold LIST (another node,
/// or after a restart) reports the plaintext size and ETag, not those of the
/// ciphertext, without a HEAD. A DELETE drops the object's listing facts.
#[tokio::test]
async fn cold_list_of_an_encrypted_backend_reports_plaintext_facts() {
    skip_unless_minio!();
    const KEY_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let encrypted = || {
        TestServer::builder()
            .s3_endpoint(&common::minio_endpoint_url())
            .bucket(common::MINIO_BUCKET)
            .encryption_key(KEY_HEX)
    };
    let writer = encrypted().build().await;
    let prefix = unique_prefix();
    let key = format!("{prefix}/photo.jpg");
    let body = generate_binary(70_000, 11);
    let writer_client = writer.s3_client().await;
    writer_client
        .put_object()
        .bucket(writer.bucket())
        .key(&key)
        .body(ByteStream::from(body.clone()))
        .send()
        .await
        .unwrap();
    let head = writer_client
        .head_object()
        .bucket(writer.bucket())
        .key(&key)
        .send()
        .await
        .unwrap();

    let reader = encrypted().build().await;
    let reader_client = reader.s3_client().await;
    let before = backend_heads(&reader.endpoint()).await;
    let listed = reader_client
        .list_objects_v2()
        .bucket(reader.bucket())
        .prefix(format!("{prefix}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(backend_heads(&reader.endpoint()).await, before);
    let obj = listed.contents().first().expect("photo.jpg listed").clone();
    assert_eq!(obj.key(), Some(key.as_str()));
    assert_eq!(obj.size(), Some(body.len() as i64), "plaintext size");
    assert_eq!(obj.e_tag(), head.e_tag(), "plaintext ETag");

    // The backend holds one facts entry for the object; a DELETE drops it.
    let raw = common::minio_client().await;
    let prefix = &prefix;
    let facts = |raw: aws_sdk_s3::Client| async move {
        raw.list_objects_v2()
            .bucket(common::MINIO_BUCKET)
            .prefix(".dg/facts/")
            .send()
            .await
            .unwrap()
            .contents()
            .iter()
            .filter_map(|o| o.key().map(str::to_string))
            .filter(|k| k.contains(prefix.as_str()))
            .count()
    };
    assert_eq!(facts(raw.clone()).await, 1);
    reader_client
        .delete_object()
        .bucket(reader.bucket())
        .key(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(facts(raw).await, 0, "DELETE drops the listing facts");
}

/// Review C3, lazy backfill: an object stored without listing facts (before
/// they existed) lists with its stored size until one HEAD of it on a node
/// whose LIST found the facts missing; that node then writes them, and every
/// node lists the original size.
#[tokio::test]
async fn a_head_backfills_missing_listing_facts() {
    skip_unless_minio!();
    let writer = TestServer::s3().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();
    let base = generate_binary(100_000, 21);
    let variant = mutate_binary(&base, 0.01);
    for (name, body) in [("base.zip", &base), ("v1.zip", &variant)] {
        let url = format!("{}/{}/{prefix}/{name}", writer.endpoint(), writer.bucket());
        let resp = http.put(&url).body(body.clone()).send().await.unwrap();
        assert!(resp.status().is_success());
    }
    // Simulate an object from before the facts: drop its entries.
    let raw = minio_client().await;
    let facts_keys = |raw: aws_sdk_s3::Client, prefix: String| async move {
        raw.list_objects_v2()
            .bucket(MINIO_BUCKET)
            .prefix(".dg/facts/")
            .send()
            .await
            .unwrap()
            .contents()
            .iter()
            .filter_map(|o| o.key().map(str::to_string))
            .filter(|k| k.contains(&format!("{prefix}/v1.zip")))
            .collect::<Vec<_>>()
    };
    for k in facts_keys(raw.clone(), prefix.clone()).await {
        raw.delete_object()
            .bucket(MINIO_BUCKET)
            .key(k)
            .send()
            .await
            .unwrap();
    }
    let key = format!("{prefix}/v1.zip");
    let size_on = |server: &TestServer| {
        let url = format!(
            "{}/{}?list-type=2&prefix={prefix}/v1",
            server.endpoint(),
            server.bucket()
        );
        let http = http.clone();
        async move {
            let xml = http.get(url).send().await.unwrap().text().await.unwrap();
            let size = xml
                .split("<Size>")
                .nth(1)
                .unwrap()
                .split('<')
                .next()
                .unwrap();
            size.parse::<usize>().unwrap()
        }
    };
    let node = TestServer::s3().await;
    assert!(
        size_on(&node).await < variant.len(),
        "no facts: stored size"
    );
    node.s3_client()
        .await
        .head_object()
        .bucket(node.bucket())
        .key(&key)
        .send()
        .await
        .unwrap();
    // The backfill runs in the background.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while facts_keys(raw.clone(), prefix.clone()).await.is_empty() {
        assert!(std::time::Instant::now() < deadline, "facts not backfilled");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let other = TestServer::s3().await;
    assert_eq!(size_on(&other).await, variant.len());
}

/// Review C9: `metadata=true` must reach the engine. The adapter always
/// listed in lite mode, so on an S3 backend (whose LIST carries no user
/// metadata) the `<UserMetadata>` extension came back without the user's
/// `x-amz-meta-*` pairs. The filesystem backend reads them during LIST
/// anyway, so only an S3 backend shows the defect.
#[tokio::test]
async fn test_s3_list_metadata_true_carries_user_metadata() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();
    client
        .put_object()
        .bucket(server.bucket())
        .key(format!("{prefix}/meta.txt"))
        .metadata("color", "teal-4711")
        .body(ByteStream::from_static(b"hello"))
        .send()
        .await
        .expect("PUT");
    let body = reqwest::Client::new()
        .get(format!(
            "{}/{}?list-type=2&metadata=true&prefix={prefix}/",
            server.endpoint(),
            server.bucket()
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("teal-4711"),
        "metadata=true LIST must carry the user's metadata: {body}"
    );
}

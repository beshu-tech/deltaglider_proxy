// SPDX-License-Identifier: BUSL-1.1

//! E1 security/hygiene fix regression tests: recursive DELETE uses a
//! paginated loop instead of materialising the full listing with
//! `u32::MAX`. A bucket with millions of keys used to balloon proxy
//! memory by ~300 B × key-count before a single delete ran.
//!
//! Tests here run against a spawned proxy (filesystem backend) with
//! enough objects to force at least two pages through the loop.

mod common;

use common::TestServer;

/// Seed N objects under a prefix, then issue a recursive DELETE via
/// `DELETE /bucket/prefix/` (trailing slash). Verify every object is
/// gone and the response JSON reports the right count.
///
/// The property under test is the PAGINATED sweep (the continuation-token
/// loop, not materialising the whole listing). Rather than seed >1000 objects
/// to force a second page of the default 1000-key window — slow to seed and
/// prone to tripping the request timeout on loaded CI hosts — we shrink the
/// window with `DGP_RECURSIVE_DELETE_PAGE_SIZE` so a small N exercises MORE
/// pages: N=120 over a 50-key window is three pages (50 + 50 + 20).
#[tokio::test]
async fn test_recursive_delete_paginates_and_deletes_all() {
    const DELETE_PAGE_SIZE: usize = 50;
    let server = TestServer::builder()
        .env(
            "DGP_RECURSIVE_DELETE_PAGE_SIZE",
            &DELETE_PAGE_SIZE.to_string(),
        )
        .build()
        .await;
    let client = reqwest::Client::new();

    // 120 objects over a 50-key delete window → three pages through the loop
    // (50 + 50 + 20), while staying fast to seed and delete (no timeout
    // fragility). Compile-time check that N genuinely spans ≥3 pages.
    const N: usize = 120;
    const _: () = assert!(N > DELETE_PAGE_SIZE * 2);
    let bucket = server.bucket();

    // Seed concurrently to keep the test quick, but cap in-flight requests with
    // a semaphore. Unbounded N-way fan-out opens N simultaneous TCP connections
    // and exhausts the runner's file-descriptor limit ("Too many open files",
    // Os code 24) on loaded CI hosts — bounding concurrency keeps open fds low
    // while still pipelining the seed.
    let base = server.endpoint();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(64));
    let put_handles: Vec<_> = (0..N)
        .map(|i| {
            let url = format!("{}/{}/toDelete/obj-{:05}.txt", base, bucket, i);
            let body = format!("payload-{}", i);
            let c = client.clone();
            let sem = sem.clone();
            tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                c.put(&url).body(body).send().await.unwrap();
            })
        })
        .collect();
    for h in put_handles {
        h.await.unwrap();
    }

    // Sanity check: a LIST should see all objects (or at least one page,
    // confirming the seed worked).
    let list_url = format!(
        "{}/{}?list-type=2&prefix=toDelete/&max-keys=1000",
        base, bucket
    );
    let resp = client.get(&list_url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Issue recursive delete.
    let del_url = format!("{}/{}/toDelete/", base, bucket);
    let resp = client.delete(&del_url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    // Should report all N deleted (no denies with no IAM).
    assert_eq!(
        body["deleted"].as_u64().unwrap_or(0),
        N as u64,
        "recursive delete must sweep all seeded objects, got {:?}",
        body
    );
    assert_eq!(body["denied"].as_u64().unwrap_or(99), 0);

    // After the delete, the LIST should return zero.
    let resp = client.get(&list_url).send().await.unwrap();
    let xml = resp.text().await.unwrap();
    assert!(
        !xml.contains("<Key>toDelete/"),
        "objects remain after recursive delete: {}",
        &xml[..xml.len().min(400)]
    );
}

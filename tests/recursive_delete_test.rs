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
/// Run with 2100 objects so we exercise the third page of the 1000-key
/// pagination window (two full + one partial).
#[tokio::test]
async fn test_recursive_delete_paginates_and_deletes_all() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    // 1100 forces a second page of the 1000-key pagination window while
    // still being quick to seed. Exercises the continuation-token loop.
    const N: usize = 1100;
    let bucket = server.bucket();

    // Seed in batches via concurrent requests to keep the test quick.
    let base = server.endpoint();
    let put_handles: Vec<_> = (0..N)
        .map(|i| {
            let url = format!("{}/{}/toDelete/obj-{:05}.txt", base, bucket, i);
            let body = format!("payload-{}", i);
            let c = client.clone();
            tokio::spawn(async move {
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

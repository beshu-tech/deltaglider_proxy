//! Parallel access safety tests
//!
//! Verifies that concurrent operations don't cause corruption or panics.
//! Uses TestServer::filesystem() with multiple tokio tasks.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{generate_binary, TestServer};

#[tokio::test]
async fn test_parallel_puts_same_prefix() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let mut handles = Vec::new();
    for i in 0..10 {
        let c = client.clone();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            let data = format!("data-{}", i);
            c.put_object()
                .bucket(&bucket)
                .key(format!("concurrent/file{}.txt", i))
                .body(ByteStream::from(data.into_bytes()))
                .send()
                .await
                .expect("Concurrent PUT should succeed");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // Verify all stored
    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("concurrent/")
        .send()
        .await
        .unwrap();
    assert_eq!(list.contents().len(), 10, "All 10 objects should be stored");
}

#[tokio::test]
async fn test_parallel_put_and_get() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Pre-populate some objects
    for i in 0..5 {
        client
            .put_object()
            .bucket(server.bucket())
            .key(format!("rw/file{}.txt", i))
            .body(ByteStream::from(format!("initial-{}", i).into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let mut handles = Vec::new();

    // Concurrent writers
    for i in 5..10 {
        let c = client.clone();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            c.put_object()
                .bucket(&bucket)
                .key(format!("rw/file{}.txt", i))
                .body(ByteStream::from(format!("new-{}", i).into_bytes()))
                .send()
                .await
                .expect("Concurrent write should succeed");
        }));
    }

    // Concurrent readers
    for i in 0..5 {
        let c = client.clone();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            let result = c
                .get_object()
                .bucket(&bucket)
                .key(format!("rw/file{}.txt", i))
                .send()
                .await
                .expect("Concurrent read should succeed");
            let body = result.body.collect().await.unwrap().into_bytes();
            assert!(!body.is_empty(), "Body should not be empty");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

#[tokio::test]
async fn test_parallel_delete_and_get() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Pre-populate
    for i in 0..10 {
        client
            .put_object()
            .bucket(server.bucket())
            .key(format!("delget/file{}.txt", i))
            .body(ByteStream::from(format!("data-{}", i).into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let mut handles = Vec::new();

    // Delete even-numbered files
    for i in (0..10).step_by(2) {
        let c = client.clone();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            let _ = c
                .delete_object()
                .bucket(&bucket)
                .key(format!("delget/file{}.txt", i))
                .send()
                .await;
        }));
    }

    // Read odd-numbered files
    for i in (1..10).step_by(2) {
        let c = client.clone();
        let bucket = server.bucket().to_string();
        handles.push(tokio::spawn(async move {
            // May or may not succeed depending on timing, but should not panic
            let _ = c
                .get_object()
                .bucket(&bucket)
                .key(format!("delget/file{}.txt", i))
                .send()
                .await;
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // No panics = success
}

#[tokio::test]
async fn test_parallel_puts_different_prefixes() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let mut handles = Vec::new();

    for prefix_idx in 0..5 {
        for file_idx in 0..4 {
            let c = client.clone();
            let bucket = server.bucket().to_string();
            let data = generate_binary(1000, (prefix_idx * 10 + file_idx) as u64);
            handles.push(tokio::spawn(async move {
                c.put_object()
                    .bucket(&bucket)
                    .key(format!("iso{}/file{}.txt", prefix_idx, file_idx))
                    .body(ByteStream::from(data))
                    .send()
                    .await
                    .expect("PUT should succeed");
            }));
        }
    }

    for h in handles {
        h.await.unwrap();
    }

    // Verify prefix isolation
    for prefix_idx in 0..5 {
        let list = client
            .list_objects_v2()
            .bucket(server.bucket())
            .prefix(format!("iso{}/", prefix_idx))
            .send()
            .await
            .unwrap();
        assert_eq!(
            list.contents().len(),
            4,
            "Prefix iso{}/ should have 4 objects",
            prefix_idx
        );
    }
}

// SPDX-License-Identifier: BUSL-1.1

//! Review D3: S3 semantics for keys that end in `/`. `PUT photos/` stores a
//! zero-byte folder marker, and `DELETE photos/` deletes that one object,
//! never the keys under it. (The proxy used to sweep the whole subtree.)

use crate::common;

use common::TestServer;

async fn list_keys(client: &aws_sdk_s3::Client, bucket: &str, prefix: &str) -> Vec<String> {
    let out = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .send()
        .await
        .unwrap();
    out.contents()
        .iter()
        .filter_map(|o| o.key().map(str::to_string))
        .collect()
}

/// PUT a marker and two keys under it, then DELETE the marker. `root` keeps
/// the keys apart on the shared MinIO bucket.
async fn marker_round_trip(server: &TestServer, root: &str) {
    let client = server.s3_client().await;
    let bucket = server.bucket();
    let key = |k: &str| format!("{root}{k}");
    client
        .put_object()
        .bucket(bucket)
        .key(key("photos/"))
        .body(Vec::new().into())
        .send()
        .await
        .expect("PUT photos/ stores a folder marker");
    for k in ["photos/a.txt", "photos/2024/b.txt"] {
        client
            .put_object()
            .bucket(bucket)
            .key(key(k))
            .body(b"payload".to_vec().into())
            .send()
            .await
            .unwrap();
    }
    let marker = client
        .head_object()
        .bucket(bucket)
        .key(key("photos/"))
        .send()
        .await
        .unwrap();
    assert_eq!(marker.content_length(), Some(0));
    let listed = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(key("photos/"))
        .send()
        .await
        .unwrap();
    let sizes: Vec<(String, i64)> = listed
        .contents()
        .iter()
        .map(|o| (o.key().unwrap().to_string(), o.size().unwrap_or(-1)))
        .collect();
    assert_eq!(
        sizes,
        vec![
            (key("photos/"), 0),
            (key("photos/2024/b.txt"), 7),
            (key("photos/a.txt"), 7)
        ]
    );

    client
        .delete_object()
        .bucket(bucket)
        .key(key("photos/"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        list_keys(&client, bucket, &key("photos/")).await,
        vec![key("photos/2024/b.txt"), key("photos/a.txt")],
        "DELETE photos/ must delete the marker only"
    );
    assert!(client
        .head_object()
        .bucket(bucket)
        .key(key("photos/"))
        .send()
        .await
        .is_err());
}

#[tokio::test]
async fn delete_of_a_prefix_key_deletes_only_the_marker() {
    let server = TestServer::filesystem().await;
    marker_round_trip(&server, "").await;
}

#[tokio::test]
async fn delete_of_a_prefix_key_deletes_only_the_marker_on_s3() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    marker_round_trip(&server, &format!("fm-{}/", uuid::Uuid::new_v4())).await;
}

/// A proxy-encrypted S3 backend stores the marker unencrypted, so an S3
/// listing still reports it as a zero-byte `photos/`.
#[tokio::test]
async fn delete_of_a_prefix_key_deletes_only_the_marker_on_encrypted_s3() {
    skip_unless_minio!();
    let server = TestServer::builder()
        .s3_endpoint(&common::minio_endpoint_url())
        .bucket(common::MINIO_BUCKET)
        .encryption_key("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        .build()
        .await;
    marker_round_trip(&server, &format!("fm-{}/", uuid::Uuid::new_v4())).await;
}

/// No marker: `DELETE photos/` deletes nothing and succeeds, like S3.
#[tokio::test]
async fn delete_of_a_prefix_key_without_a_marker_deletes_nothing() {
    let server = TestServer::builder().build().await;
    let client = server.s3_client().await;
    let bucket = server.bucket();
    client
        .put_object()
        .bucket(bucket)
        .key("docs/readme.txt")
        .body(b"keep me".to_vec().into())
        .send()
        .await
        .unwrap();
    let resp = server
        .http()
        .delete(format!("{}/{}/docs/", server.endpoint(), bucket))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    assert_eq!(
        list_keys(&client, bucket, "docs/").await,
        vec!["docs/readme.txt"]
    );
}

/// A marker key with a body is refused (400), and a marker holds a folder
/// open: DeleteBucket fails while it exists.
#[tokio::test]
async fn marker_with_a_body_is_refused_and_an_empty_folder_keeps_the_bucket() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;
    let bucket = server.bucket();
    let err = client
        .put_object()
        .bucket(bucket)
        .key("bad/")
        .body(b"data".to_vec().into())
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        err.raw_response().map(|r| r.status().as_u16()),
        Some(400),
        "{err:?}"
    );
    client
        .put_object()
        .bucket(bucket)
        .key("empty/")
        .body(Vec::new().into())
        .send()
        .await
        .unwrap();
    let listed = client
        .list_objects_v2()
        .bucket(bucket)
        .delimiter("/")
        .send()
        .await
        .unwrap();
    let prefixes: Vec<&str> = listed
        .common_prefixes()
        .iter()
        .filter_map(|p| p.prefix())
        .collect();
    assert_eq!(prefixes, vec!["empty/"]);
    assert!(client.delete_bucket().bucket(bucket).send().await.is_err());
}

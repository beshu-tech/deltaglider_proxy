// SPDX-License-Identifier: BUSL-1.1

//! The coordination bucket (`config_sync_bucket`) is reserved for the
//! proxy's own sync client: an object written there by an S3 client could
//! replace the synced IAM database (a rollback that re-enables a disabled
//! key on every peer) or a lease. Every S3 request to it is refused, for
//! every identity, and ListBuckets hides it.
//!
//! Filesystem backend: config sync on a filesystem singleton degrades to a
//! warning, and the reservation does not depend on the sync running.

use crate::common::{self, TestServer};

const SYNC: &str = "dgp-sync";

async fn reserved_server() -> TestServer {
    TestServer::builder()
        .config_sync_bucket(SYNC)
        .env("DGP_CONFIG_DB_KEY", common::TEST_CONFIG_DB_KEY)
        .build()
        .await
}

fn status_of<T, E: aws_sdk_s3::error::ProvideErrorMetadata>(
    r: Result<T, aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>>,
) -> u16 {
    match r {
        Ok(_) => 200,
        Err(e) => e.raw_response().map(|r| r.status().as_u16()).unwrap_or(0),
    }
}

#[tokio::test]
async fn s3_clients_cannot_touch_the_coordination_bucket() {
    let server = reserved_server().await;
    // The bucket exists on the backend (as it does when the sync runs).
    let data = server.data_dir().expect("filesystem data dir");
    std::fs::create_dir_all(data.join(SYNC).join("_dgp")).unwrap();

    // The bootstrap credentials are an admin identity: refused anyway.
    let s3 = server.s3_client().await;
    let put = s3
        .put_object()
        .bucket(SYNC)
        .key("_dgp/probe.txt")
        .body(b"x".to_vec().into())
        .send()
        .await;
    assert_eq!(status_of(put), 403, "PUT into the coordination bucket");
    let get = s3
        .get_object()
        .bucket(SYNC)
        .key("_dgp/probe.txt")
        .send()
        .await;
    assert_eq!(status_of(get), 403, "GET from the coordination bucket");
    let list = s3.list_objects_v2().bucket(SYNC).send().await;
    assert_eq!(status_of(list), 403, "LIST of the coordination bucket");
    let del = s3.delete_bucket().bucket(SYNC).send().await;
    assert_eq!(
        status_of(del),
        403,
        "DeleteBucket of the coordination bucket"
    );
    let head = s3.head_bucket().bucket(SYNC).send().await;
    assert_eq!(
        status_of(head),
        403,
        "HeadBucket of the coordination bucket"
    );
    assert!(
        !data.join(SYNC).join("_dgp").join("probe.txt").exists(),
        "the refused PUT must not reach the backend"
    );

    let buckets = s3.list_buckets().send().await.expect("ListBuckets");
    let names: Vec<_> = buckets.buckets().iter().filter_map(|b| b.name()).collect();
    assert!(names.contains(&server.bucket()), "{names:?}");
    assert!(
        !names.contains(&SYNC),
        "ListBuckets shows the coordination bucket: {names:?}"
    );

    // Other buckets are unaffected.
    s3.put_object()
        .bucket(server.bucket())
        .key("ok.txt")
        .body(b"ok".to_vec().into())
        .send()
        .await
        .expect("PUT into a client bucket");
}

#[tokio::test]
async fn admin_bulk_endpoints_refuse_the_coordination_bucket() {
    let server = reserved_server().await;
    let data = server.data_dir().expect("filesystem data dir");
    std::fs::create_dir_all(data.join(SYNC)).unwrap();
    let admin = common::admin_http_client(&server.endpoint()).await;
    let r = admin
        .get(format!(
            "{}/_/api/admin/objects/list?bucket={SYNC}&prefix=_dgp/",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "admin list of the coordination bucket");
}

#[tokio::test]
async fn a_config_that_exposes_the_coordination_bucket_is_refused() {
    let server = reserved_server().await;
    let admin = common::admin_http_client(&server.endpoint()).await;
    // Public read on the coordination bucket.
    let r = admin
        .put(format!(
            "{}/_/api/admin/config/section/storage",
            server.endpoint()
        ))
        .json(&serde_json::json!({ "buckets": { SYNC: { "public": true } } }))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_client_error(),
        "public coordination bucket must be refused, got {}",
        r.status()
    );
    // Another bucket aliased onto the coordination bucket's storage.
    let r = admin
        .put(format!(
            "{}/_/api/admin/config/section/storage",
            server.endpoint()
        ))
        .json(&serde_json::json!({ "buckets": { "innocent": { "alias": SYNC } } }))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_client_error(),
        "alias onto the coordination bucket must be refused, got {}",
        r.status()
    );
}

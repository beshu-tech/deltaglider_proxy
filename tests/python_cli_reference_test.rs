// SPDX-License-Identifier: BUSL-1.1

//! A deltaspace that the Python `deltaglider` CLI wrote: its `reference.bin`
//! records no `dg-file-size` (issue #187). The test plants that metadata on
//! MinIO by hand, on purpose.

use crate::common;

use common::{
    generate_binary, get_bytes, minio_client, mutate_binary, put_and_get_storage_type, TestServer,
    MINIO_BUCKET,
};

fn unique_prefix() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("py-cli-ref-{ts}")
}

/// Open access: the test drives the proxy with unsigned reqwest.
async fn proxy() -> TestServer {
    TestServer::builder()
        .s3_endpoint(&common::minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .open_access()
        .build()
        .await
}

/// #187: the Python CLI wrote references with no `dg-file-size` (and some
/// tools wrote `"0"`). A delta GET read that as a 0-byte limit and answered
/// a permanent 503 SlowDown; it must serve the object, checked by SHA-256.
#[tokio::test]
async fn test_delta_get_with_a_reference_that_records_no_size() {
    skip_unless_minio!();
    let prefix = unique_prefix();
    let base = generate_binary(100_000, 187);
    let variant = mutate_binary(&base, 0.05);
    {
        let server = proxy().await;
        let http = reqwest::Client::new();
        for (name, body) in [("base.zip", base.clone()), ("v1.zip", variant.clone())] {
            put_and_get_storage_type(
                &http,
                &server.endpoint(),
                server.bucket(),
                &format!("{prefix}/{name}"),
                body,
                "application/zip",
            )
            .await;
        }
    }
    let direct = minio_client().await;
    let listed = direct
        .list_objects_v2()
        .bucket(MINIO_BUCKET)
        .prefix(&prefix)
        .send()
        .await
        .unwrap();
    let ref_key = listed
        .contents()
        .iter()
        .filter_map(|o| o.key())
        .find(|k| k.ends_with("reference.bin"))
        .expect("the PUTs wrote a reference")
        .to_string();
    let head = direct
        .head_object()
        .bucket(MINIO_BUCKET)
        .key(&ref_key)
        .send()
        .await
        .unwrap();
    let mut meta = head.metadata().cloned().unwrap_or_default();
    assert!(meta.remove("dg-file-size").is_some(), "{meta:?}");

    // Absent, then "0": each through a fresh proxy (empty reference cache).
    for size in [None, Some("0")] {
        let mut m = meta.clone();
        if let Some(v) = size {
            m.insert("dg-file-size".into(), v.into());
        }
        direct
            .copy_object()
            .bucket(MINIO_BUCKET)
            .key(&ref_key)
            .copy_source(format!("{MINIO_BUCKET}/{ref_key}"))
            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace)
            .set_metadata(Some(m))
            .send()
            .await
            .unwrap();
        let server = proxy().await;
        let got = get_bytes(
            &reqwest::Client::new(),
            &server.endpoint(),
            server.bucket(),
            &format!("{prefix}/v1.zip"),
        )
        .await;
        assert_eq!(got, variant, "dg-file-size {size:?}");
    }
}

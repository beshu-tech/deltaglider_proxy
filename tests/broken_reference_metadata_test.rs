// SPDX-License-Identifier: BUSL-1.1

//! A deltaspace reference with broken metadata (issue #187): no
//! `dg-file-size`, a size of `"0"`, or no DG metadata at all, as a copy
//! outside the proxy leaves it. The tests plant that metadata on MinIO by
//! hand, on purpose: the read serves such a reference, and the
//! `backfill-metadata` job heals it.

use crate::common;

use common::{
    admin_http_client, generate_binary, get_bytes, minio_client, mutate_binary,
    put_and_get_storage_type, TestServer, MINIO_BUCKET,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

fn unique_prefix() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("broken-ref-{ts}")
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

/// #187: a reference with no `dg-file-size` (or `"0"`). A delta GET read
/// that as a 0-byte limit and answered a permanent 503 SlowDown; it must
/// serve the object, checked by SHA-256.
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

/// Replace the metadata of `key` in place (a metadata-only self-copy).
async fn set_metadata(
    raw: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    meta: HashMap<String, String>,
) {
    raw.copy_object()
        .bucket(bucket)
        .key(key)
        .copy_source(format!("{bucket}/{key}"))
        .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace)
        .set_metadata(Some(meta))
        .send()
        .await
        .expect("metadata self-copy");
}

async fn metadata_of(raw: &aws_sdk_s3::Client, bucket: &str, key: &str) -> HashMap<String, String> {
    raw.head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("HEAD")
        .metadata()
        .cloned()
        .unwrap_or_default()
}

/// #187: the `backfill-metadata` job re-stamps every reference whose type,
/// SHA-256 or size is missing, from its own bytes, and leaves a reference
/// whose bytes do not match its recorded SHA-256 as it is.
#[tokio::test]
async fn the_heal_job_restamps_broken_references() {
    skip_unless_minio!();
    // Its own bucket: the job walks a whole bucket.
    let bucket = format!("heal-ref-{}", &unique_prefix()[11..]);
    let raw = minio_client().await;
    // name, how its reference is broken
    let cases = ["nosize", "zerosize", "stripped", "wrongsha"];
    let mut variants = HashMap::new();
    {
        let writer = TestServer::s3_with_endpoint(&common::minio_endpoint_url(), &bucket).await;
        let client = writer.s3_client().await;
        for (i, name) in cases.iter().enumerate() {
            let base = generate_binary(60_000, 300 + i as u64);
            let variant = mutate_binary(&base, 0.05);
            for (file, body) in [("base.zip", base), ("v1.zip", variant.clone())] {
                client
                    .put_object()
                    .bucket(&bucket)
                    .key(format!("{name}/{file}"))
                    .body(aws_sdk_s3::primitives::ByteStream::from(body))
                    .send()
                    .await
                    .expect("PUT");
            }
            variants.insert(*name, variant);
        }
    }
    let listed = raw.list_objects_v2().bucket(&bucket).send().await.unwrap();
    let ref_key = |name: &str| {
        listed
            .contents()
            .iter()
            .filter_map(|o| o.key())
            .find(|k| k.starts_with(&format!("{name}/")) && k.ends_with("reference.bin"))
            .unwrap_or_else(|| panic!("no reference for {name}"))
            .to_string()
    };
    let mut good = HashMap::new();
    for name in cases {
        let key = ref_key(name);
        let mut meta = metadata_of(&raw, &bucket, &key).await;
        good.insert(name, meta.clone());
        match name {
            "nosize" => {
                meta.remove("dg-file-size");
            }
            "zerosize" => {
                meta.insert("dg-file-size".into(), "0".into());
            }
            "stripped" => meta.retain(|k, _| !k.starts_with("dg-")),
            _ => {
                meta.remove("dg-file-size");
                meta.insert("dg-file-sha256".into(), "ab".repeat(32));
            }
        }
        set_metadata(&raw, &bucket, &key, meta).await;
    }
    let planted_wrong = metadata_of(&raw, &bucket, &ref_key("wrongsha")).await;

    // A fresh proxy: no cache of the references.
    let server = TestServer::s3_with_endpoint(&common::minio_endpoint_url(), &bucket).await;
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/backfill-metadata",
            server.endpoint()
        ))
        .json(&serde_json::json!({ "buckets": [bucket], "refresh_last_modified": false }))
        .send()
        .await
        .expect("job POST");
    assert!(resp.status().is_success(), "job POST: {}", resp.status());
    let mut finished = false;
    for _ in 0..600 {
        let v: serde_json::Value = admin
            .get(format!(
                "{}/_/api/admin/jobs/bucket/{bucket}",
                server.endpoint()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if v["active"].is_null() {
            finished = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(finished, "the heal job did not finish within 60 s");

    for name in ["nosize", "zerosize", "stripped"] {
        let key = ref_key(name);
        let meta = metadata_of(&raw, &bucket, &key).await;
        let body = raw
            .get_object()
            .bucket(&bucket)
            .key(&key)
            .send()
            .await
            .unwrap()
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes();
        assert_eq!(
            meta.get("dg-file-size"),
            Some(&body.len().to_string()),
            "{name}: {meta:?}"
        );
        assert_eq!(
            meta.get("dg-file-sha256"),
            Some(&hex::encode(Sha256::digest(&body))),
            "{name}"
        );
        assert_eq!(
            meta.get("dg-file-sha256"),
            good[name].get("dg-file-sha256"),
            "{name}"
        );
        let got = server
            .s3_client()
            .await
            .get_object()
            .bucket(&bucket)
            .key(format!("{name}/v1.zip"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{name}: GET after heal: {e:?}"))
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes();
        assert_eq!(&got[..], &variants[name][..], "{name}");
    }
    // Bytes that do not match the recorded SHA-256: left as they are.
    assert_eq!(
        metadata_of(&raw, &bucket, &ref_key("wrongsha")).await,
        planted_wrong
    );
}

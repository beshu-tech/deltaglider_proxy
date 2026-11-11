// SPDX-License-Identifier: BUSL-1.1

//! Epic #142 (F43): small list/metadata inconsistencies, on the filesystem
//! backend and on MinIO.

use crate::common;
use common::TestServer;

async fn servers() -> Vec<(&'static str, TestServer, String)> {
    let mut out = vec![(
        "filesystem",
        TestServer::builder().build().await,
        String::new(),
    )];
    if common::minio_available().await {
        let s3 = TestServer::builder()
            .s3_endpoint(&common::minio_endpoint_url())
            .bucket(common::MINIO_BUCKET)
            .build()
            .await;
        // Keep the shared MinIO bucket apart between runs.
        let root = format!("edges-{}/", uuid::Uuid::new_v4().simple());
        out.push(("minio", s3, root));
    }
    out
}

/// `prefix=/` names no key: no stored key starts with `/`. The flat listing
/// said 0 keys; the `/` delimiter listed the whole root.
#[tokio::test]
async fn a_prefix_of_a_slash_lists_nothing_with_or_without_a_delimiter() {
    for (name, server, root) in servers().await {
        let client = server.s3_client().await;
        let bucket = server.bucket();
        client
            .put_object()
            .bucket(bucket)
            .key(format!("{root}top.txt"))
            .body(b"x".to_vec().into())
            .send()
            .await
            .unwrap();
        for delimiter in [None, Some("/"), Some("_")] {
            let mut req = client.list_objects_v2().bucket(bucket).prefix("/");
            if let Some(d) = delimiter {
                req = req.delimiter(d);
            }
            let out = req.send().await.unwrap();
            assert!(
                out.contents().is_empty() && out.common_prefixes().is_empty(),
                "{name} delimiter {delimiter:?}: {:?} {:?}",
                out.contents(),
                out.common_prefixes()
            );
        }
    }
}

/// `max-uploads=0` answers no uploads and echoes 0 (it echoed 1).
#[tokio::test]
async fn max_uploads_of_zero_is_echoed_as_zero() {
    let server = TestServer::builder().build().await;
    let client = server.s3_client().await;
    client
        .create_multipart_upload()
        .bucket(server.bucket())
        .key("mp/a.bin")
        .send()
        .await
        .unwrap();
    let out = client
        .list_multipart_uploads()
        .bucket(server.bucket())
        .max_uploads(0)
        .send()
        .await
        .unwrap();
    assert_eq!(out.max_uploads(), Some(0));
    assert!(out.uploads().is_empty(), "{:?}", out.uploads());
    assert_eq!(out.is_truncated(), Some(false));
}

/// An empty user-metadata value is metadata: HEAD returns the header with
/// an empty value (it was dropped).
#[tokio::test]
async fn an_empty_metadata_value_is_kept() {
    for (name, server, root) in servers().await {
        let client = server.s3_client().await;
        let key = format!("{root}meta.txt");
        client
            .put_object()
            .bucket(server.bucket())
            .key(&key)
            .metadata("e", "")
            .metadata("f", "v")
            .body(b"x".to_vec().into())
            .send()
            .await
            .unwrap();
        let head = client
            .head_object()
            .bucket(server.bucket())
            .key(&key)
            .send()
            .await
            .unwrap();
        let meta = head.metadata().cloned().unwrap_or_default();
        assert_eq!(meta.get("f").map(String::as_str), Some("v"), "{name}");
        assert_eq!(
            meta.get("e").map(String::as_str),
            Some(""),
            "{name}: {meta:?}"
        );
    }
}

/// The same with a raw request (`x-amz-meta-e:` with nothing after it).
#[tokio::test]
async fn an_empty_raw_metadata_header_is_kept() {
    for (name, server, root) in servers().await {
        let http = server.http();
        let url = format!("{}/{}/{root}raw.txt", server.endpoint(), server.bucket());
        let put = http
            .put(&url)
            .header("x-amz-meta-e", "")
            .body("x")
            .send()
            .await
            .unwrap();
        assert!(put.status().is_success(), "{name} {}", put.status());
        let head = http.head(&url).send().await.unwrap();
        assert_eq!(
            head.headers()
                .get("x-amz-meta-e")
                .map(|v| v.to_str().unwrap()),
            Some(""),
            "{name}: {:?}",
            head.headers()
        );
    }
}

/// Non-ASCII metadata is returned in the one form that S3 uses
/// (`=?UTF-8?B?…?=`), whichever backend stored it (the filesystem backend
/// answered `B`, MinIO `q`).
#[tokio::test]
async fn non_ascii_metadata_comes_back_in_one_form() {
    let mut forms = Vec::new();
    for (name, server, root) in servers().await {
        let http = server.http();
        let url = format!("{}/{}/{root}enc.txt", server.endpoint(), server.bucket());
        let put = http
            .put(&url)
            .header("x-amz-meta-note", "=?UTF-8?B?w6k=?=")
            .body("x")
            .send()
            .await
            .unwrap();
        assert!(put.status().is_success(), "{name} {}", put.status());
        let head = http.head(&url).send().await.unwrap();
        let raw = head
            .headers()
            .get("x-amz-meta-note")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        forms.push((name, raw));
    }
    for (name, raw) in &forms {
        assert_eq!(raw, "=?UTF-8?B?w6k=?=", "{name}: {forms:?}");
    }
}

/// A file name of 250 to 254 bytes is stored whole on every backend: a
/// delta-eligible key whose `.delta` name is above 255 bytes falls back to
/// a whole object instead of failing the second upload.
#[tokio::test]
async fn a_long_name_stores_whether_or_not_it_would_be_a_delta() {
    for (name, server, root) in servers().await {
        let client = server.s3_client().await;
        let base: Vec<u8> = (0..60_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let mut next = base.clone();
        next[100] ^= 0xff;
        for len in [249usize, 250, 254, 255] {
            let file = format!("{}.zip", "n".repeat(len - 4));
            let key = format!("{root}long/{file}");
            for body in [&base, &next] {
                let put = client
                    .put_object()
                    .bucket(server.bucket())
                    .key(&key)
                    .body(body.clone().into())
                    .send()
                    .await;
                assert!(put.is_ok(), "{name} name of {len} bytes: {put:?}");
            }
            let got = client
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
            assert_eq!(got.as_ref(), next.as_slice(), "{name} {len}");
        }
    }
}

/// Pure: is `text` made only of characters that XML 1.0 allows?
fn is_xml10(text: &str) -> bool {
    text.chars().all(|c| {
        matches!(c, '\t' | '\n' | '\r')
            || ('\u{20}'..='\u{D7FF}').contains(&c)
            || ('\u{E000}'..='\u{FFFD}').contains(&c)
            || ('\u{10000}'..='\u{10FFFF}').contains(&c)
    })
}

/// F39 (epic #142). S3 stores a key with a control character, and so does
/// the filesystem backend, so the proxy does not refuse it. A default LIST
/// body then holds the raw character, as S3's does, and a strict XML parser
/// rejects it; the AWS SDKs ask for `encoding-type=url` by themselves, and
/// then every body is well-formed XML 1.0. This test holds the safe mode to
/// that, for keys of every class of XML-invalid character.
#[tokio::test]
async fn an_encoding_type_url_listing_is_well_formed_for_any_key() {
    let server = TestServer::builder().build().await;
    let http = server.http();
    let keys = [
        "ctl/a%01b",
        "ctl/c%08d",
        "ctl/e%0Bf",
        "ctl/g%1Fh",
        "ctl/i%7Fj",
        "ctl/k%EF%BF%BFl", // U+FFFF
        "ctl/m%EF%BF%BEn", // U+FFFE
        "ctl/plain",
        "ctl/%C3%A9",
        "ctl/%F0%9F%98%80",
    ];
    for key in keys {
        let put = http
            .put(format!("{}/{}/{key}", server.endpoint(), server.bucket()))
            .body("x")
            .send()
            .await
            .unwrap();
        assert!(put.status().is_success(), "{key}: {}", put.status());
    }
    for query in [
        "list-type=2&encoding-type=url&prefix=ctl%2F",
        "list-type=2&encoding-type=url&prefix=ctl%2F&delimiter=%2F",
        "encoding-type=url&prefix=ctl%2F",
        "list-type=2&encoding-type=url&prefix=ctl%2F&max-keys=1&start-after=ctl%2Fa",
    ] {
        let body = http
            .get(format!("{}/{}?{query}", server.endpoint(), server.bucket()))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(is_xml10(&body), "{query}: {body:?}");
    }
}

// SPDX-License-Identifier: BUSL-1.1

//! Wire seams of S3 edge semantics (review 4, s3surface): conditional
//! requests with the object's own `Last-Modified`, ranges on a HEAD and on an
//! empty object, `If-Range`, the 304 headers, and the filesystem backend's
//! file-versus-directory key space. The pure decisions behind them are unit
//! tests in `s3_adapter_s3s::tests`; these check what reaches the client.

use crate::common;

use common::{put_object, signed_setup};

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// A client that sends back the exact `Last-Modified` it received gets the
/// S3 answers: 304 for If-Modified-Since, 200 for If-Unmodified-Since, and a
/// conditional copy succeeds (s3surface-1).
#[tokio::test]
async fn own_last_modified_revalidates() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "lm.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/lm.txt");
    let head = http.head(&url).send().await.unwrap();
    let lm = header(&head, "last-modified").expect("Last-Modified");

    let resp = http
        .get(&url)
        .header("if-modified-since", &lm)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 304, "If-Modified-Since: {lm}");
    let resp = http
        .get(&url)
        .header("if-unmodified-since", &lm)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "If-Unmodified-Since: {lm}");
    let resp = http
        .head(&url)
        .header("if-unmodified-since", &lm)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "HEAD If-Unmodified-Since: {lm}"
    );

    let resp = http
        .put(format!("{endpoint}/{bucket}/lm-copy.txt"))
        .header("x-amz-copy-source", format!("{bucket}/lm.txt"))
        .header("x-amz-copy-source-if-unmodified-since", &lm)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "copy-source-if-unmodified-since: {lm}"
    );
}

/// A 304 carries `ETag` and `Last-Modified`, and no XML body headers
/// (s3surface-7).
#[tokio::test]
async fn not_modified_carries_validators() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "nm.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/nm.txt");
    let head = http.head(&url).send().await.unwrap();
    let etag = header(&head, "etag").unwrap();
    let lm = header(&head, "last-modified").unwrap();

    for (name, value) in [("if-none-match", &etag), ("if-modified-since", &lm)] {
        let resp = http.get(&url).header(name, value).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 304, "{name}");
        assert_eq!(
            header(&resp, "etag").as_deref(),
            Some(etag.as_str()),
            "{name}"
        );
        assert_eq!(
            header(&resp, "last-modified").as_deref(),
            Some(lm.as_str()),
            "{name}"
        );
        assert_eq!(header(&resp, "content-type"), None, "{name}");
        assert!(resp.bytes().await.unwrap().is_empty());
    }
}

/// HEAD with `Range` answers 206 like S3; GET keeps its 206 (s3surface-6).
#[tokio::test]
async fn head_with_range_is_partial() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "r.txt",
        b"abcd".to_vec(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/r.txt");

    let resp = http
        .head(&url)
        .header("range", "bytes=1-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);
    assert_eq!(
        header(&resp, "content-range").as_deref(),
        Some("bytes 1-3/4")
    );
    assert_eq!(header(&resp, "content-length").as_deref(), Some("3"));
    let resp = http
        .get(&url)
        .header("range", "bytes=1-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"bcd");
}

/// A suffix range on an empty object is 416, never a 206 whose
/// `Content-Length` promises a byte it does not send (s3surface-3).
#[tokio::test]
async fn range_on_empty_object_is_unsatisfiable() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "empty.txt",
        Vec::new(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/empty.txt");
    let resp = http
        .get(&url)
        .header("range", "bytes=-5")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 416);
    let resp = http
        .head(&url)
        .header("range", "bytes=-5")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 416);
}

/// A resumed download whose `If-Range` names another version gets the whole
/// object with 200; a matching one keeps the 206 (s3surface-4).
#[tokio::test]
async fn stale_if_range_serves_the_whole_object() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "ir.txt",
        b"0123456789".to_vec(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/ir.txt");
    let etag = header(&http.head(&url).send().await.unwrap(), "etag").unwrap();

    let resp = http
        .get(&url)
        .header("range", "bytes=5-")
        .header("if-range", "\"deadbeef\"")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"0123456789");

    let resp = http
        .get(&url)
        .header("range", "bytes=5-")
        .header("if-range", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"56789");
}

/// On the filesystem backend a key prefix is a directory. It never reads as
/// an object, and a key that needs a file where a directory is (or the
/// reverse) is a 400 that names the limitation, not a 500 (s3surface-2/9).
#[tokio::test]
async fn filesystem_prefix_directory_is_not_an_object() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "dir/x.txt",
        b"x".to_vec(),
        "text/plain",
    )
    .await;
    put_object(
        &http,
        &endpoint,
        &bucket,
        "file",
        b"f".to_vec(),
        "text/plain",
    )
    .await;

    for key in ["dir", "file/child"] {
        let url = format!("{endpoint}/{bucket}/{key}");
        let resp = http.get(&url).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 404, "GET {key}");
        assert!(
            resp.text().await.unwrap().contains("NoSuchKey"),
            "GET {key}"
        );
        let resp = http.head(&url).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 404, "HEAD {key}");
        let resp = http.delete(&url).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 204, "DELETE {key}");

        let resp = http.put(&url).body("new").send().await.unwrap();
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap();
        assert_eq!(status, 400, "PUT {key}: {body}");
        assert!(body.contains("InvalidRequest"), "PUT {key}: {body}");
        assert!(body.contains("filesystem backend"), "PUT {key}: {body}");
    }
    // The objects that own the paths are untouched.
    let resp = http
        .get(format!("{endpoint}/{bucket}/dir/x.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"x");
    let resp = http
        .get(format!("{endpoint}/{bucket}/file"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"f");
}

/// Every object and list request to a missing bucket is `404 NoSuchBucket`,
/// never an empty 200, a 204, or `NoSuchKey` (s3surface-5).
#[tokio::test]
async fn missing_bucket_is_no_such_bucket() {
    let (server, http) = signed_setup().await;
    let endpoint = server.endpoint();
    let base = format!("{endpoint}/no-such-bucket-here");
    let requests = [
        ("LIST v2", http.get(format!("{base}?list-type=2"))),
        ("LIST v1", http.get(base.clone())),
        ("GET", http.get(format!("{base}/k"))),
        ("DELETE", http.delete(format!("{base}/k"))),
        (
            "DELETE batch",
            http.post(format!("{base}?delete"))
                .header("content-md5", "5DKh5iefM5MSKvRILIuFwQ==")
                .body("<Delete><Object><Key>k</Key></Object></Delete>"),
        ),
    ];
    for (name, request) in requests {
        let resp = request.send().await.unwrap();
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap();
        assert_eq!(status, 404, "{name}: {body}");
        assert!(body.contains("NoSuchBucket"), "{name}: {body}");
    }
    let resp = http.head(format!("{base}/k")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404, "HEAD");
    // An empty bucket that exists still lists as an empty 200.
    let resp = http
        .get(format!(
            "{endpoint}/{}?list-type=2&prefix=nothing/",
            server.bucket()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

/// `max-keys=0` answers no keys (it answered one, s3surface-14).
#[tokio::test]
async fn max_keys_zero_lists_nothing() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "a.txt",
        b"a".to_vec(),
        "text/plain",
    )
    .await;
    for query in ["list-type=2&max-keys=0", "max-keys=0"] {
        let resp = http
            .get(format!("{endpoint}/{bucket}?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{query}");
        let body = resp.text().await.unwrap();
        assert!(!body.contains("<Contents>"), "{query}: {body}");
        assert!(body.contains("<MaxKeys>0</MaxKeys>"), "{query}: {body}");
    }
}

/// A copy onto itself that changes nothing is `400 InvalidRequest`, as on S3;
/// with `REPLACE` it goes on (s3surface-16).
#[tokio::test]
async fn self_copy_without_change_is_refused() {
    let (server, http) = signed_setup().await;
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    put_object(
        &http,
        &endpoint,
        &bucket,
        "self.txt",
        b"s".to_vec(),
        "text/plain",
    )
    .await;
    let url = format!("{endpoint}/{bucket}/self.txt");
    let resp = http
        .put(&url)
        .header("x-amz-copy-source", format!("{bucket}/self.txt"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("InvalidRequest"), "{body}");
    let resp = http
        .put(&url)
        .header("x-amz-copy-source", format!("{bucket}/self.txt"))
        .header("x-amz-metadata-directive", "REPLACE")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

fn upload_id(body: &str) -> String {
    body.split("<UploadId>")
        .nth(1)
        .unwrap()
        .split("</UploadId>")
        .next()
        .unwrap()
        .to_string()
}

/// A ranged UploadPartCopy of a passthrough source reads only the range: a
/// source larger than `max_object_size` can be copied
/// part by part. The whole-source read refused it (s3surface-10).
#[tokio::test]
async fn ranged_part_copy_reads_only_the_range() {
    let server = common::TestServer::builder()
        .max_object_size(4096)
        .build()
        .await;
    let http = server.http();
    let (endpoint, bucket) = (server.endpoint(), server.bucket().to_string());
    // A 9000-byte passthrough source over max_object_size: a file that is on
    // the backend already (a PUT could not store it).
    let dir = server.data_dir().unwrap().join(&bucket).join("deltaspaces");
    std::fs::create_dir_all(&dir).unwrap();
    let source: Vec<u8> = (1..=3u8).flat_map(|n| vec![b'a' + n; 3000]).collect();
    std::fs::write(dir.join("big.jpg"), &source).unwrap();

    let dst = format!("{endpoint}/{bucket}/copy.jpg");
    let resp = http.post(format!("{dst}?uploads")).send().await.unwrap();
    let id = upload_id(&resp.text().await.unwrap());
    let resp = http
        .put(format!("{dst}?partNumber=1&uploadId={id}"))
        .header("x-amz-copy-source", format!("{bucket}/big.jpg"))
        .header("x-amz-copy-source-range", "bytes=2990-3009")
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let etag = body
        .split("<ETag>")
        .nth(1)
        .unwrap()
        .split("</ETag>")
        .next()
        .unwrap();
    // A part over max_object_size is still refused.
    let resp = http
        .put(format!("{dst}?partNumber=2&uploadId={id}"))
        .header("x-amz-copy-source", format!("{bucket}/big.jpg"))
        .header("x-amz-copy-source-range", "bytes=0-4999")
        .send()
        .await
        .unwrap();
    let refused = resp.text().await.unwrap();
    assert!(refused.contains("EntityTooLarge"), "{refused}");
    let resp = http
        .post(format!("{dst}?uploadId={id}"))
        .body(format!(
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag}</ETag></Part></CompleteMultipartUpload>"
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let got = http.get(&dst).send().await.unwrap().bytes().await.unwrap();
    let mut want = vec![b'b'; 10];
    want.extend(vec![b'c'; 10]);
    assert_eq!(got.as_ref(), want.as_slice());
}

/// A GET with a condition header judges it on a fresh HEAD of storage; a GET
/// without one takes no HEAD (it serves the metadata cache). Two proxies
/// share one filesystem directory, so a PUT through node A leaves node B's
/// metadata cache stale.
#[tokio::test]
async fn every_get_judges_fresh_storage_metadata() {
    const KEY_ID: &str = "CONDKEY";
    const SECRET: &str = "CONDSECRET";
    let shared = tempfile::TempDir::new().unwrap();
    let node = || async {
        let doc = format!(
            "access:\n  access_key_id: {KEY_ID}\n  secret_access_key: {SECRET}\n\
             storage:\n  filesystem: {}\n\
             advanced:\n  listen_addr: {}\n  bootstrap_password_hash: \"{}\"\n",
            shared.path().display(),
            common::LISTEN_ADDR_PLACEHOLDER,
            common::TEST_BOOTSTRAP_PASSWORD_HASH
        );
        let data = tempfile::TempDir::new().unwrap();
        common::TestServer::from_config_document(&doc, data, (KEY_ID, SECRET), "releases", vec![])
            .await
    };
    let (a, b) = (node().await, node().await);
    let (ha, hb) = (a.http(), b.http());
    let url_b = format!("{}/releases/app.png", b.endpoint());
    let endpoint_a = a.endpoint();
    let put = |body: &[u8]| {
        put_object(
            &ha,
            &endpoint_a,
            "releases",
            "app.png",
            body.to_vec(),
            "image/png",
        )
    };

    put(b"v1").await;
    // B caches the first version.
    let first = hb.head(&url_b).send().await.unwrap();
    let etag1 = header(&first, "etag").unwrap();
    put(b"v2").await;

    // No condition: B's cache still holds v1, but the GET reads fresh
    // metadata, so the ETag and the body agree on v2 (a cached ETag with the
    // stored bytes would mix two versions in one answer).
    let plain = hb.get(&url_b).send().await.unwrap();
    assert_eq!(plain.status(), 200);
    assert_ne!(
        header(&plain, "etag").as_ref(),
        Some(&etag1),
        "a plain GET must not answer the cached ETag"
    );
    assert_eq!(&plain.bytes().await.unwrap()[..], b"v2");
    // A range on B is judged on the fresh size too.
    let ranged = hb
        .get(&url_b)
        .header("range", "bytes=0-0")
        .send()
        .await
        .unwrap();
    assert_eq!(ranged.status(), 206);
    assert_eq!(&ranged.bytes().await.unwrap()[..], b"v");
    // Re-cache v1 on B for the conditional checks below.
    put(b"v1").await;
    let first = hb.head(&url_b).send().await.unwrap();
    let etag1 = header(&first, "etag").unwrap();
    put(b"v2").await;

    // If-Match the stale ETag: storage holds another version → 412.
    let stale = hb
        .get(&url_b)
        .header("if-match", &etag1)
        .send()
        .await
        .unwrap();
    assert_eq!(
        stale.status(),
        412,
        "the condition is judged on fresh storage state"
    );
    // If-None-Match the stale ETag: the object changed → 200 with the new body.
    let changed = hb
        .get(&url_b)
        .header("if-none-match", &etag1)
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), 200);
    assert_ne!(header(&changed, "etag").as_ref(), Some(&etag1));
    assert_eq!(&changed.bytes().await.unwrap()[..], b"v2");
}

// SPDX-License-Identifier: BUSL-1.1

//! Tests for the server-side bulk-object admin endpoints
//! (`/_/api/admin/objects/{copy,move,delete,zip,list}`).
//!
//! These replace what the React s3-browser previously orchestrated via
//! @aws-sdk/client-s3 in the browser. Each test exercises the public
//! HTTP contract — the same shape the React client will call.

use crate::common;

use common::{admin_http_client, TestServer};
use serde_json::{json, Value};

/// Bulk copy with relative-path preservation. Two source keys, one
/// nested, copied into a destination prefix; both expected destination
/// keys land.
#[tokio::test]
async fn test_bulk_copy_preserves_relative_paths() {
    let server = TestServer::builder().build().await;
    let http = server.http();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket();

    // Seed 2 source keys.
    for key in ["a.txt", "nested/b.txt"] {
        http.put(format!("{}/{}/{}", server.endpoint(), bucket, key))
            .body(b"x".to_vec())
            .send()
            .await
            .unwrap();
    }

    let body = json!({
        "source_bucket": bucket,
        "dest_bucket": bucket,
        "dest_prefix": "copy-of/",
        "items": [
            { "source_key": "a.txt", "relative": "a.txt" },
            { "source_key": "nested/b.txt", "relative": "nested/b.txt" }
        ]
    });
    let resp = admin
        .post(format!("{}/_/api/admin/objects/copy", server.endpoint()))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let r: Value = resp.json().await.unwrap();
    assert_eq!(r["succeeded"].as_u64(), Some(2));
    assert_eq!(r["failed"].as_u64(), Some(0));

    // Verify destinations.
    for key in ["copy-of/a.txt", "copy-of/nested/b.txt"] {
        let head = http
            .head(format!("{}/{}/{}", server.endpoint(), bucket, key))
            .send()
            .await
            .unwrap();
        assert_eq!(head.status().as_u16(), 200, "dest key {} missing", key);
    }
}

/// Collisions in the destination plan must be rejected BEFORE any copy.
#[tokio::test]
async fn test_bulk_copy_rejects_collisions() {
    let server = TestServer::filesystem().await;
    let admin = admin_http_client(&server.endpoint()).await;

    let body = json!({
        "source_bucket": server.bucket(),
        "dest_bucket": server.bucket(),
        "dest_prefix": "out/",
        // Two relative keys that resolve to the same dest_key:
        "items": [
            { "source_key": "a/x.txt", "relative": "x.txt" },
            { "source_key": "b/x.txt", "relative": "x.txt" }
        ]
    });
    let resp = admin
        .post(format!("{}/_/api/admin/objects/copy", server.endpoint()))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);
}

/// Bulk move = copy + source-delete only when ALL copies succeeded.
/// Source bucket loses the items; destination gains them.
#[tokio::test]
async fn test_bulk_move_atomic_delete() {
    let server = TestServer::builder().build().await;
    let http = server.http();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket();

    for key in ["mv1.txt", "mv2.txt"] {
        http.put(format!("{}/{}/{}", server.endpoint(), bucket, key))
            .body(b"data".to_vec())
            .send()
            .await
            .unwrap();
    }

    let body = json!({
        "source_bucket": bucket,
        "dest_bucket": bucket,
        "dest_prefix": "moved/",
        "items": [
            { "source_key": "mv1.txt", "relative": "mv1.txt" },
            { "source_key": "mv2.txt", "relative": "mv2.txt" }
        ]
    });
    let resp = admin
        .post(format!("{}/_/api/admin/objects/move", server.endpoint()))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let r: Value = resp.json().await.unwrap();
    assert_eq!(r["succeeded"].as_u64(), Some(2));
    assert_eq!(r["deleted"].as_u64(), Some(2));

    // Sources gone, destinations present.
    for src in ["mv1.txt", "mv2.txt"] {
        let head = http
            .head(format!("{}/{}/{}", server.endpoint(), bucket, src))
            .send()
            .await
            .unwrap();
        assert_eq!(
            head.status().as_u16(),
            404,
            "source {} should be deleted",
            src
        );
    }
    for dst in ["moved/mv1.txt", "moved/mv2.txt"] {
        let head = http
            .head(format!("{}/{}/{}", server.endpoint(), bucket, dst))
            .send()
            .await
            .unwrap();
        assert_eq!(head.status().as_u16(), 200, "dest {} missing", dst);
    }
}

/// Bulk delete is idempotent — missing keys are reported as deleted.
#[tokio::test]
async fn test_bulk_delete_idempotent_on_missing() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket();

    http.put(format!("{}/{}/exists.txt", server.endpoint(), bucket))
        .body(b"x".to_vec())
        .send()
        .await
        .unwrap();

    let body = json!({
        "bucket": bucket,
        "keys": ["exists.txt", "ghost.txt"]
    });
    let resp = admin
        .post(format!("{}/_/api/admin/objects/delete", server.endpoint()))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let r: Value = resp.json().await.unwrap();
    assert_eq!(r["deleted"].as_u64(), Some(2)); // missing key counts as deleted
    assert_eq!(r["failed"].as_u64(), Some(0));
}

/// Zip download bundles requested objects, returns application/zip
/// with the right Content-Disposition.
#[tokio::test]
async fn test_zip_download_returns_archive() {
    let server = TestServer::builder().build().await;
    let http = server.http();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket();

    // Seed 2 small files.
    for (key, body) in [("z1.txt", &b"alpha"[..]), ("z2.txt", &b"bravo"[..])] {
        http.put(format!("{}/{}/{}", server.endpoint(), bucket, key))
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
    }

    let resp = admin
        .get(format!(
            "{}/_/api/admin/objects/zip?keys={}/z1.txt,{}/z2.txt",
            server.endpoint(),
            bucket,
            bucket
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.contains("application/zip"), "ct: {}", ct);
    let cd = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        cd.contains("attachment") && cd.contains(".zip"),
        "cd: {}",
        cd
    );
    let body = resp.bytes().await.unwrap();
    // Local-file header magic is "PK\x03\x04".
    assert_eq!(&body[..4], b"PK\x03\x04");
    // Both names appear somewhere in the archive (uncompressed STORED, so
    // the bytes are inline).
    let s = body.iter().copied().collect::<Vec<u8>>();
    let s_str = String::from_utf8_lossy(&s);
    assert!(s_str.contains("z1.txt"));
    assert!(s_str.contains("z2.txt"));
}

/// Read every entry of a ZIP body: (name, content), in archive order.
fn unzip(body: &[u8]) -> Vec<(String, Vec<u8>)> {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(body)).expect("a complete ZIP");
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let mut data = Vec::new();
            // read_to_end checks each entry's CRC-32.
            f.read_to_end(&mut data).unwrap();
            (f.name().to_string(), data)
        })
        .collect()
}

/// U4: the ZIP streams (no Content-Length: the proxy does not build the
/// archive first) and holds the exact bytes of a delta-reconstructed
/// object, decoded through the spooled streaming path, and of a 24 MiB
/// passthrough object. A missing key lands in the skip report.
#[tokio::test]
async fn test_zip_streams_delta_and_large_objects() {
    let server = TestServer::builder()
        // Every delta GET decodes to a spool file, the path of large deltas.
        .env("DGP_SPOOL_THRESHOLD_BYTES", "1")
        .build()
        .await;
    let http = server.http();
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();
    let bucket = server.bucket().to_string();

    let base = common::generate_binary(200_000, 7);
    let variant = common::mutate_binary(&base, 0.01);
    let big = common::big_passthrough_body(24 * 1024 * 1024);
    common::put_and_get_storage_type(
        &http,
        &ep,
        &bucket,
        "rel/v1.zip",
        base.clone(),
        "application/zip",
    )
    .await;
    let st = common::put_and_get_storage_type(
        &http,
        &ep,
        &bucket,
        "rel/v2.zip",
        variant.clone(),
        "application/zip",
    )
    .await;
    assert_eq!(st, "delta", "the fixture needs a delta-stored object");
    common::put_object(
        &http,
        &ep,
        &bucket,
        "rel/media/clip.mp4",
        big.clone(),
        "video/mp4",
    )
    .await;

    let keys = [
        "rel/v1.zip",
        "rel/v2.zip",
        "rel/ghost.bin",
        "rel/media/clip.mp4",
    ]
    .map(|k| format!("{bucket}/{k}"))
    .join(",");
    let resp = admin
        .get(format!("{ep}/_/api/admin/objects/zip"))
        .query(&[("keys", keys)])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers().get("content-length").is_none(),
        "a streamed ZIP has no Content-Length: {:?}",
        resp.headers()
    );
    let body = resp.bytes().await.unwrap();
    let entries = unzip(&body);
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "v1.zip",
            "v2.zip",
            "media/clip.mp4",
            "_deltaglider-skipped-files.txt"
        ]
    );
    assert!(entries[0].1 == base, "reference object differs");
    assert!(
        entries[1].1 == variant,
        "delta-reconstructed object differs"
    );
    assert!(entries[2].1 == big, "large passthrough object differs");
    let report = String::from_utf8(entries[3].1.clone()).unwrap();
    assert!(report.contains("rel/ghost.bin"), "{report}");
}

/// U4: an object that fails after its bytes started (here: its file on
/// disk is shorter than its metadata says) aborts the download. The client
/// sees a failed transfer, never a well-formed archive that lacks bytes.
#[tokio::test]
async fn test_zip_aborts_when_an_object_fails_mid_stream() {
    let server = TestServer::builder().build().await;
    let http = server.http();
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();
    let bucket = server.bucket().to_string();
    common::put_object(
        &http,
        &ep,
        &bucket,
        "m/a.mp4",
        b"first".to_vec(),
        "video/mp4",
    )
    .await;
    let clip = common::big_passthrough_body(1024 * 1024);
    common::put_object(&http, &ep, &bucket, "m/clip.mp4", clip, "video/mp4").await;
    // Truncate the stored file in place; its xattr metadata keeps the size.
    let stored = walkdir(server.data_dir().expect("filesystem data dir"))
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "clip.mp4"))
        .expect("stored clip.mp4");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&stored)
        .unwrap()
        .set_len(1000)
        .unwrap();

    let resp = admin
        .get(format!("{ep}/_/api/admin/objects/zip"))
        .query(&[("keys", format!("{bucket}/m/a.mp4,{bucket}/m/clip.mp4"))])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.bytes().await;
    assert!(body.is_err(), "the download must fail, got a complete body");
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// A ZIP download is audited like bulk copy/move/delete: action
/// `bulk_zip`, the bucket, the key count and the IAM denials in the target,
/// the actor's IP and User-Agent.
#[tokio::test]
async fn test_zip_download_is_audited() {
    let server = TestServer::builder().build().await;
    let http = server.http();
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();
    let bucket = server.bucket().to_string();
    common::put_object(&http, &ep, &bucket, "au/a.txt", b"a".to_vec(), "text/plain").await;
    let resp = admin
        .get(format!("{ep}/_/api/admin/objects/zip"))
        .header("user-agent", "zip-audit-test/1.0")
        .query(&[("keys", format!("{bucket}/au/a.txt,{bucket}/au/ghost.txt"))])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp.bytes().await.unwrap();
    let audit: Value = admin
        .get(format!("{ep}/_/api/admin/audit?limit=100"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = audit["entries"]
        .as_array()
        .or(audit.as_array())
        .expect("audit rows");
    let row = rows
        .iter()
        .find(|e| e["action"] == "bulk_zip")
        .unwrap_or_else(|| panic!("no bulk_zip entry: {audit}"));
    let target = row["target"].as_str().unwrap();
    assert!(
        target.contains(&bucket) && target.contains("keys=2") && target.contains("denied=0"),
        "{row}"
    );
    assert_eq!(row["ua"], "zip-audit-test/1.0", "{row}");
    assert!(
        row["ip"]
            .as_str()
            .is_some_and(|ip| ip != "unknown" && !ip.is_empty()),
        "{row}"
    );
}

/// list_all expands a folder selection to the absolute key list.
#[tokio::test]
async fn test_list_all_expands_folder() {
    let server = TestServer::builder().build().await;
    let http = server.http();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket();

    for key in ["folder/a.txt", "folder/sub/b.txt", "outside/c.txt"] {
        http.put(format!("{}/{}/{}", server.endpoint(), bucket, key))
            .body(b"x".to_vec())
            .send()
            .await
            .unwrap();
    }

    let resp = admin
        .get(format!(
            "{}/_/api/admin/objects/list?bucket={}&prefix=folder/",
            server.endpoint(),
            bucket
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let r: Value = resp.json().await.unwrap();
    let keys: Vec<String> = r["keys"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert!(keys.contains(&"folder/a.txt".to_string()));
    assert!(keys.contains(&"folder/sub/b.txt".to_string()));
    assert!(!keys.iter().any(|k| k.starts_with("outside/")));
    assert_eq!(r["truncated"].as_bool(), Some(false));
}

/// list_all refuses an empty prefix to avoid accidentally walking
/// the entire bucket via the bulk-resolve helper.
#[tokio::test]
async fn test_list_all_refuses_empty_prefix() {
    let server = TestServer::filesystem().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .get(format!(
            "{}/_/api/admin/objects/list?bucket={}&prefix=",
            server.endpoint(),
            server.bucket()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

/// S6: admin handlers call the engine directly, so s3s never validates their
/// bucket names. A bucket like `../x` or `/etc` must be refused before any
/// filesystem path is built from it; so must `..` key segments.
#[tokio::test]
async fn test_admin_inputs_refuse_path_escapes() {
    let server = TestServer::filesystem().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();
    let bucket = server.bucket();

    let copy = |src: &str, key: &str| {
        json!({
            "source_bucket": src,
            "dest_bucket": bucket,
            "items": [{ "source_key": key, "relative": "x" }]
        })
    };
    for body in [
        copy("../escape", "a"),
        copy("/etc", "passwd"),
        copy(bucket, "../../x"),
    ] {
        let r = admin
            .post(format!("{ep}/_/api/admin/objects/copy"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_client_error(), "copy {body}: {}", r.status());
    }
    let r = admin
        .post(format!("{ep}/_/api/admin/objects/delete"))
        .json(&json!({ "bucket": bucket, "keys": ["../../outside"] }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error(), "delete: {}", r.status());
    let r = admin
        .get(format!("{ep}/_/api/admin/objects/zip?keys=..%2Fx%2Fsecret"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400, "zip");
    let r = admin
        .post(format!("{ep}/_/api/admin/usage/scan"))
        .json(&json!({ "bucket": "/tmp", "prefix": "" }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error(), "usage scan: {}", r.status());
    let r = admin
        .get(format!(
            "{ep}/_/api/admin/deltaspace/savings?bucket={bucket}&prefix=..%2F..%2F"
        ))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error(), "savings: {}", r.status());
}

/// S5 (CSRF): the session cookie is SameSite=Strict, which does not stop a
/// page on a sibling subdomain (same site, other origin). A state-changing
/// admin request that the browser marks as not same-origin must be refused;
/// the same request from our own origin, or from a non-browser client with
/// no fetch metadata, must pass.
#[tokio::test]
async fn test_admin_writes_refuse_cross_origin_browser_requests() {
    let server = TestServer::filesystem().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let url = format!("{}/_/api/admin/objects/delete", server.endpoint());
    let body = json!({ "bucket": server.bucket(), "keys": ["nope.txt"] });

    for (site, origin) in [
        (Some("same-site"), None),
        (Some("cross-site"), None),
        (None, Some("https://evil.example")),
    ] {
        let mut req = admin.post(&url).json(&body);
        if let Some(s) = site {
            req = req.header("Sec-Fetch-Site", s);
        }
        if let Some(o) = origin {
            req = req.header("Origin", o);
        }
        let r = req.send().await.unwrap();
        assert_eq!(r.status().as_u16(), 403, "site={site:?} origin={origin:?}");
    }
    let r = admin
        .post(&url)
        .header("Sec-Fetch-Site", "same-origin")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "same-origin");
    let r = admin.post(&url).json(&body).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200, "non-browser client");
}

/// D12: moving folder `f/` into its own subfolder `f/sub/` maps `f/1` onto
/// `f/sub/1`, which is itself a selected source not yet copied. The copy
/// loop overwrote it, then the delete phase removed it: `f/sub/1`'s bytes
/// were lost. A plan whose destination is another selected source must be
/// refused before anything is written.
#[tokio::test]
async fn test_move_into_own_subfolder_is_refused() {
    let server = TestServer::builder().build().await;
    let http = server.http();
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();
    let bucket = server.bucket();
    for (key, body) in [("f/1", "top"), ("f/sub/1", "nested")] {
        http.put(format!("{ep}/{bucket}/{key}"))
            .body(body)
            .send()
            .await
            .unwrap();
    }
    let body = json!({
        "source_bucket": bucket,
        "dest_bucket": bucket,
        "dest_prefix": "f/sub/",
        "items": [
            { "source_key": "f/1", "relative": "1" },
            { "source_key": "f/sub/1", "relative": "sub/1" }
        ]
    });
    for op in ["move", "copy"] {
        let r = admin
            .post(format!("{ep}/_/api/admin/objects/{op}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 409, "{op}");
    }
    for (key, want) in [("f/1", "top"), ("f/sub/1", "nested")] {
        let got = http
            .get(format!("{ep}/{bucket}/{key}"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(got, want, "{key} must be intact");
    }
}

/// Tier 4: admin bulk copy/move/delete called the engine directly and
/// skipped the quota gate, the event outbox and the audit log that the S3
/// path applies to the same writes.
#[tokio::test]
async fn test_bulk_ops_honour_quota_and_record_events_and_audit() {
    let server = TestServer::builder()
        .bucket_policy("frozen-bkt", "quota_bytes: 0")
        .build()
        .await;
    let http = server.http();
    let admin = admin_http_client(&server.endpoint()).await;
    let ep = server.endpoint();
    let bucket = server.bucket();
    http.put(format!("{ep}/{bucket}/src.txt"))
        .body("payload")
        .send()
        .await
        .unwrap();
    let r = http.put(format!("{ep}/frozen-bkt")).send().await.unwrap();
    assert!(r.status().is_success(), "create frozen-bkt: {}", r.status());

    // Quota: a frozen destination refuses the copy.
    let r: Value = admin
        .post(format!("{ep}/_/api/admin/objects/copy"))
        .json(&json!({
            "source_bucket": bucket, "dest_bucket": "frozen-bkt",
            "items": [{ "source_key": "src.txt", "relative": "src.txt" }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["succeeded"].as_u64(), Some(0), "{r}");
    assert_eq!(r["failed"].as_u64(), Some(1), "{r}");

    // Outbox + audit: a copy and a delete are recorded.
    let r = admin
        .post(format!("{ep}/_/api/admin/objects/copy"))
        .json(&json!({
            "source_bucket": bucket, "dest_bucket": bucket, "dest_prefix": "cp/",
            "items": [{ "source_key": "src.txt", "relative": "src.txt" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let r = admin
        .post(format!("{ep}/_/api/admin/objects/delete"))
        .json(&json!({ "bucket": bucket, "keys": ["cp/src.txt"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let outbox: Value = admin
        .get(format!("{ep}/_/api/admin/event-outbox?limit=100"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = outbox["rows"].as_array().expect("outbox rows");
    let has = |kind: &str| {
        rows.iter()
            .any(|e| e["kind"] == kind && e["key"] == "cp/src.txt")
    };
    assert!(has("ObjectCopied"), "no ObjectCopied: {outbox}");
    assert!(has("ObjectDeleted"), "no ObjectDeleted: {outbox}");
    // Each row carries its per-endpoint delivery state (none: delivery is off).
    assert!(
        rows.iter()
            .all(|e| e["deliveries"].as_array().is_some_and(Vec::is_empty)),
        "rows must carry an empty deliveries list: {outbox}"
    );
    let audit: Value = admin
        .get(format!("{ep}/_/api/admin/audit?limit=100"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = audit.to_string();
    assert!(
        text.contains("bulk_copy") && text.contains("bulk_delete"),
        "{text}"
    );
}

/// Browser review #11: a non-admin browser session (S3BrowserLift) may use
/// the bulk endpoints, and every key is authorized with the user's own IAM
/// policy, like the S3 API. Allowed keys work; denied keys are reported one
/// by one.
#[tokio::test]
async fn browser_session_bulk_ops_are_authorized_per_key() {
    let server = TestServer::builder()
        .auth("BULKLIFT", "BULKLIFTSECRET")
        .build()
        .await;
    let ep = server.endpoint();
    let bucket = server.bucket().to_string();
    let admin = admin_http_client(&ep).await;
    let s3 = server.s3_client().await;
    for key in ["dana/a.txt", "dana/b.txt", "shared/c.txt"] {
        s3.put_object()
            .bucket(&bucket)
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"x"))
            .send()
            .await
            .unwrap();
    }
    // dana: everything under dana/, nothing else.
    let resp = admin
        .post(format!("{ep}/_/api/admin/users"))
        .json(&json!({
            "name": "dana",
            "permissions": [{
                "actions": ["read", "write", "delete", "list"],
                "resources": [format!("{bucket}/dana/*")]
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let user: Value = resp.json().await.unwrap();
    let dana = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let resp = dana
        .post(format!("{ep}/_/api/admin/session/browser-connect"))
        .json(&json!({
            "access_key_id": user["access_key_id"],
            "secret_access_key": user["secret_access_key"],
            "endpoint": ep,
            "bucket": "",
            "region": "us-east-1",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Copy: the allowed item lands, the denied source is reported.
    let resp = dana
        .post(format!("{ep}/_/api/admin/objects/copy"))
        .json(&json!({
            "source_bucket": bucket,
            "dest_bucket": bucket,
            "dest_prefix": "dana/copies/",
            "items": [
                { "source_key": "dana/a.txt", "relative": "a.txt" },
                { "source_key": "shared/c.txt", "relative": "c.txt" }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
    let r: Value = resp.json().await.unwrap();
    assert_eq!(
        (r["succeeded"].as_u64(), r["failed"].as_u64()),
        (Some(1), Some(1)),
        "{r}"
    );
    assert_eq!(r["failures"][0]["source_key"], "shared/c.txt", "{r}");
    assert!(
        r["failures"][0]["error"]
            .as_str()
            .unwrap()
            .contains("AccessDenied"),
        "{r}"
    );

    // Copy INTO a prefix dana may not write: denied per key.
    let resp = dana
        .post(format!("{ep}/_/api/admin/objects/copy"))
        .json(&json!({
            "source_bucket": bucket,
            "dest_bucket": bucket,
            "dest_prefix": "shared/",
            "items": [{ "source_key": "dana/a.txt", "relative": "a.txt" }]
        }))
        .send()
        .await
        .unwrap();
    let r: Value = resp.json().await.unwrap();
    assert_eq!(r["failed"].as_u64(), Some(1), "{r}");

    // Delete: the allowed key goes, the denied key stays and is reported.
    let resp = dana
        .post(format!("{ep}/_/api/admin/objects/delete"))
        .json(&json!({ "bucket": bucket, "keys": ["dana/b.txt", "shared/c.txt"] }))
        .send()
        .await
        .unwrap();
    let r: Value = resp.json().await.unwrap();
    assert_eq!(
        (r["deleted"].as_u64(), r["failed"].as_u64()),
        (Some(1), Some(1)),
        "{r}"
    );
    assert_eq!(r["failures"][0]["key"], "shared/c.txt", "{r}");
    s3.head_object()
        .bucket(&bucket)
        .key("shared/c.txt")
        .send()
        .await
        .expect("the denied key must survive");

    // ZIP: a denied key is not in the archive; only denied keys → 403.
    let resp = dana
        .get(format!("{ep}/_/api/admin/objects/zip"))
        .query(&[("keys", format!("{bucket}/shared/c.txt"))])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let resp = dana
        .get(format!("{ep}/_/api/admin/objects/zip"))
        .query(&[("keys", format!("{bucket}/dana/a.txt,{bucket}/shared/c.txt"))])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    // The denied key's bytes are not in the archive; the report names it.
    let entries = unzip(&resp.bytes().await.unwrap());
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["dana/a.txt", "_deltaglider-skipped-files.txt"]);
    let report = String::from_utf8(entries[1].1.clone()).unwrap();
    assert!(
        report.contains("shared/c.txt") && report.contains("AccessDenied"),
        "{report}"
    );

    // List: dana's folder expands; another folder lists as empty (the S3
    // LIST rule: admitted, and filtered to the keys the user can see).
    let resp = dana
        .get(format!("{ep}/_/api/admin/objects/list"))
        .query(&[("bucket", bucket.as_str()), ("prefix", "dana/")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let r: Value = resp.json().await.unwrap();
    assert!(
        r["keys"]
            .as_array()
            .unwrap()
            .iter()
            .all(|k| k.as_str().unwrap().starts_with("dana/")),
        "{r}"
    );
    let resp = dana
        .get(format!("{ep}/_/api/admin/objects/list"))
        .query(&[("bucket", bucket.as_str()), ("prefix", "shared/")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let r: Value = resp.json().await.unwrap();
    assert_eq!(r["keys"], json!([]), "{r}");
}

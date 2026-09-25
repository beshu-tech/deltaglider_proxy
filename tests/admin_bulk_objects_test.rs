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
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();
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
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();
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
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();
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

/// list_all expands a folder selection to the absolute key list.
#[tokio::test]
async fn test_list_all_expands_folder() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();
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
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();
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
    let http = reqwest::Client::new();
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

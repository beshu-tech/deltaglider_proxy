//! Tests for the server-side bulk-object admin endpoints
//! (`/_/api/admin/objects/{copy,move,delete,zip,list}`).
//!
//! These replace what the React s3-browser previously orchestrated via
//! @aws-sdk/client-s3 in the browser. Each test exercises the public
//! HTTP contract — the same shape the React client will call.

mod common;

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

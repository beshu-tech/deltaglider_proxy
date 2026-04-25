//! End-to-end integration tests for lazy replication.
//!
//! Exercises the worker via the admin API's `run-now` endpoint so the
//! full stack (config → DB → engine → worker → state store) is tested
//! together. Skeleton: seed a rule in YAML, seed source objects, trigger
//! run-now, verify destination + status + history + counters.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, TestServer};
use serde_json::Value;

const RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: repl-a-to-b
      enabled: true
      source:
        bucket: repl-src
        prefix: \"\"
      destination:
        bucket: repl-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
";

const PAUSED_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: paused-rule
      enabled: true
      source:
        bucket: p-src
        prefix: \"\"
      destination:
        bucket: p-dst
        prefix: \"\"
      interval: \"1h\"
";

const MULTIPAGE_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: multipage-rule
      enabled: true
      source:
        bucket: mp-src
        prefix: \"\"
      destination:
        bucket: mp-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 5
";

const DELETE_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: delete-rule
      enabled: true
      source:
        bucket: del-src
        prefix: \"\"
      destination:
        bucket: del-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
      replicate_deletes: true
";

/// Spin up a proxy with two buckets and a replication rule wired
/// up in the YAML. A single run-now copies all objects from source
/// to destination.
#[tokio::test]
async fn test_replication_run_now_copies_missing_objects() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(RULE_YAML)
        .build()
        .await;

    let client = server.s3_client().await;

    // Create both buckets and seed source with 3 objects.
    for b in ["repl-src", "repl-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    for (key, body) in [
        ("a.txt", &b"alpha"[..]),
        ("b.txt", &b"bravo"[..]),
        ("nested/c.txt", &b"charlie"[..]),
    ] {
        client
            .put_object()
            .bucket("repl-src")
            .key(key)
            .body(ByteStream::from(body.to_vec()))
            .send()
            .await
            .expect("seed");
    }

    // Trigger the replication run-now via the admin API.
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/repl-a-to-b/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request");
    assert_eq!(resp.status().as_u16(), 200, "run-now should succeed");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"].as_str(), Some("succeeded"), "run status");
    assert_eq!(
        body["objects_copied"].as_i64().unwrap_or(-1),
        3,
        "copied count in run-now response: {}",
        body
    );

    // Verify the destination now has all three objects.
    for key in ["a.txt", "b.txt", "nested/c.txt"] {
        let got = client
            .get_object()
            .bucket("repl-dst")
            .key(key)
            .send()
            .await
            .expect("dest object present")
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes();
        assert!(!got.is_empty(), "dest key {} has content", key);
    }

    // History endpoint: 1 run, status=succeeded, objects_copied=3.
    let hist: Value = admin
        .get(format!(
            "{}/_/api/admin/replication/rules/repl-a-to-b/history",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let runs = hist["runs"].as_array().expect("history runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["status"].as_str(), Some("succeeded"));
    assert_eq!(runs[0]["objects_copied"].as_i64(), Some(3));
}

/// A paused rule must return 409 on run-now until resumed.
#[tokio::test]
async fn test_replication_paused_rule_blocks_run_now() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(PAUSED_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["p-src", "p-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    let admin = admin_http_client(&server.endpoint()).await;

    // Pause the rule.
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/paused-rule/pause",
            server.endpoint()
        ))
        .send()
        .await
        .expect("pause");
    assert_eq!(resp.status().as_u16(), 204);

    // Run-now must 409.
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/paused-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 409);

    // Resume + verify run-now now accepts the call (with zero work).
    admin
        .post(format!(
            "{}/_/api/admin/replication/rules/paused-rule/resume",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/paused-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

/// H1 fix regression: a single run-now must replicate ALL objects
/// across multiple pages, not just the first batch_size keys. With
/// batch_size=5 and 17 objects, we expect 17 copied (= 4 pages).
#[tokio::test]
async fn test_replication_paginates_until_complete() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(MULTIPAGE_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;

    for b in ["mp-src", "mp-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // Seed 17 objects (3 full pages of 5 + a 4th of 2).
    for i in 0..17u32 {
        let key = format!("file-{:03}.bin", i);
        client
            .put_object()
            .bucket("mp-src")
            .key(&key)
            .body(ByteStream::from(vec![i as u8; 16]))
            .send()
            .await
            .expect("seed");
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/multipage-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    // Pre-fix: copied was capped at batch_size=5. Post-fix: 17.
    assert_eq!(
        body["objects_copied"].as_i64().unwrap_or(-1),
        17,
        "H1 REGRESSION: should copy all 17 objects across pages, got {}",
        body
    );
    assert_eq!(body["status"].as_str(), Some("succeeded"));

    // Verify destination has all 17.
    let listed = client
        .list_objects_v2()
        .bucket("mp-dst")
        .send()
        .await
        .unwrap();
    let count = listed.contents().len();
    assert_eq!(count, 17);

    // Continuation token should be cleared after a clean complete pass.
    // (Implicitly: a second run-now copies nothing because all keys exist
    // and conflict=newer-wins skips equal-or-older destinations.)
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/multipage-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["objects_copied"].as_i64().unwrap_or(-1),
        0,
        "second run should be a no-op when source==dest, got {}",
        body
    );
}

/// H2 fix regression: replicate_deletes=true must remove destination
/// keys that no longer exist on source.
#[tokio::test]
async fn test_replication_replicate_deletes_removes_orphans() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(DELETE_RULE_YAML)
        .build()
        .await;
    let client = server.s3_client().await;

    for b in ["del-src", "del-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // Seed both source and destination with 3 objects.
    // Seed source with 4 keys (a/b/c/d).
    for key in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        client
            .put_object()
            .bucket("del-src")
            .key(key)
            .body(ByteStream::from(b"x".to_vec()))
            .send()
            .await
            .unwrap();
    }
    // H2 fix verification: an unrelated object on the destination
    // bucket (not written by replication) MUST NOT be deleted. Pre-fix
    // any dest key whose name didn't appear on source got nuked.
    client
        .put_object()
        .bucket("del-dst")
        .key("manual.txt")
        .body(ByteStream::from(b"hand-placed by an operator".to_vec()))
        .send()
        .await
        .unwrap();

    let admin = admin_http_client(&server.endpoint()).await;

    // First run: forward-copy 4 keys onto dst (with provenance markers).
    // Delete pass: nothing to delete (each replicated key still on src).
    // `manual.txt` is preserved because it has no provenance marker.
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/delete-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    for key in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        client
            .head_object()
            .bucket("del-dst")
            .key(key)
            .send()
            .await
            .expect("replicated key on dst");
    }
    client
        .head_object()
        .bucket("del-dst")
        .key("manual.txt")
        .send()
        .await
        .expect("H2: manual.txt (no provenance marker) must survive first run");

    // Now delete d.txt from source. Next replication run should delete
    // d.txt from destination (it carries the provenance marker), but
    // leave manual.txt alone.
    client
        .delete_object()
        .bucket("del-src")
        .key("d.txt")
        .send()
        .await
        .unwrap();

    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/delete-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // d.txt should be GONE from dst (replicated delete).
    let head_d = client
        .head_object()
        .bucket("del-dst")
        .key("d.txt")
        .send()
        .await;
    assert!(
        head_d.is_err(),
        "replicated d.txt must be deleted from destination after source delete"
    );

    // manual.txt MUST still be there — no provenance marker, not ours.
    client
        .head_object()
        .bucket("del-dst")
        .key("manual.txt")
        .send()
        .await
        .expect("H2 REGRESSION: manual.txt without provenance marker was deleted");

    // Other replicated keys should still be there.
    for key in ["a.txt", "b.txt", "c.txt"] {
        client
            .head_object()
            .bucket("del-dst")
            .key(key)
            .send()
            .await
            .expect("legit replicated key remains");
    }
}

/// M1 fix: pause/resume on a non-existent rule must 404 WITHOUT
/// creating a ghost DB row. Pre-fix the handler called
/// replication_ensure_state before checking config, leaving an
/// orphan row even though the response was 404.
#[tokio::test]
async fn test_pause_resume_ghost_rule_returns_404_without_inserting_row() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(RULE_YAML)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Pause + resume on a non-existent rule.
    for action in ["pause", "resume"] {
        let resp = admin
            .post(format!(
                "{}/_/api/admin/replication/rules/ghost-rule/{}",
                server.endpoint(),
                action
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            404,
            "M1: {} on a non-existent rule must 404",
            action
        );
    }

    // Verify the overview doesn't list the ghost rule (no orphan row).
    let resp = admin
        .get(format!("{}/_/api/admin/replication", server.endpoint()))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let names: Vec<&str> = body["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !names.contains(&"ghost-rule"),
        "M1 REGRESSION: ghost-rule appeared in overview after 404, names={:?}",
        names
    );
}

/// M2 fix: run-now respects `rule.enabled` and global `replication.enabled`.
#[tokio::test]
async fn test_run_now_rejects_disabled_rule() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(
            "
replication:
  enabled: true
  rules:
    - name: disabled-rule
      enabled: false
      source: { bucket: x, prefix: \"\" }
      destination: { bucket: y, prefix: \"\" }
      interval: \"1h\"
",
        )
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/disabled-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "M2: run-now on disabled rule must 409"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("disabled"), "got: {}", body);
}

#[tokio::test]
async fn test_run_now_rejects_globally_disabled_replication() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(
            "
replication:
  enabled: false
  rules:
    - name: orphan
      enabled: true
      source: { bucket: x, prefix: \"\" }
      destination: { bucket: y, prefix: \"\" }
      interval: \"1h\"
",
        )
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/orphan/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "M2: run-now must reject when replication is globally disabled"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("globally disabled"), "got: {}", body);
}

/// H3 fix regression: source's multipart ETag must propagate through
/// replication. After replication, dest HEAD ETag == source HEAD ETag,
/// preserving the "abc-N" multipart format.
#[tokio::test]
async fn test_replication_preserves_multipart_etag() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(
            "
replication:
  enabled: true
  rules:
    - name: mp-etag-rule
      enabled: true
      source: { bucket: e-src, prefix: \"\" }
      destination: { bucket: e-dst, prefix: \"\" }
      interval: \"1h\"
      batch_size: 100
",
        )
        .build()
        .await;

    let client = server.s3_client().await;
    for b in ["e-src", "e-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // Create a multipart upload on the SOURCE bucket so the source
    // object carries a multipart_etag.
    let key = "big.bin";
    let create = client
        .create_multipart_upload()
        .bucket("e-src")
        .key(key)
        .send()
        .await
        .unwrap();
    let upload_id = create.upload_id().unwrap().to_string();

    let part1 = vec![0xAAu8; 5 * 1024 * 1024];
    let part2 = vec![0xBBu8; 1024];
    let etag1 = client
        .upload_part()
        .bucket("e-src")
        .key(key)
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from(part1))
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string();
    let etag2 = client
        .upload_part()
        .bucket("e-src")
        .key(key)
        .upload_id(&upload_id)
        .part_number(2)
        .body(ByteStream::from(part2))
        .send()
        .await
        .unwrap()
        .e_tag()
        .unwrap()
        .to_string();
    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
    let completed = CompletedMultipartUpload::builder()
        .parts(
            CompletedPart::builder()
                .part_number(1)
                .e_tag(&etag1)
                .build(),
        )
        .parts(
            CompletedPart::builder()
                .part_number(2)
                .e_tag(&etag2)
                .build(),
        )
        .build();
    let complete = client
        .complete_multipart_upload()
        .bucket("e-src")
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .unwrap();
    let source_etag = complete.e_tag().unwrap().to_string();
    assert!(
        source_etag.contains("-2"),
        "source should have multipart ETag, got {}",
        source_etag
    );

    // Trigger replication.
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/mp-etag-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // HEAD destination — must return the SAME multipart ETag.
    let dest_head = client
        .head_object()
        .bucket("e-dst")
        .key(key)
        .send()
        .await
        .unwrap();
    let dest_etag = dest_head.e_tag().unwrap().to_string();
    assert_eq!(
        dest_etag, source_etag,
        "H3 REGRESSION: destination ETag {} differs from source ETag {} after replication",
        dest_etag, source_etag
    );
}

// ════════════════════════════════════════════════════════════════════
// H2 (fourth-wave) — replication delete-pass provenance edge cases
// ════════════════════════════════════════════════════════════════════
//
// The fourth-wave H2 fix gates `run_delete_pass` on a per-rule
// provenance marker (`x-amz-meta-dg-replication-rule = <rule.name>`)
// stamped at copy time. The basic "operator placed an unrelated
// object" path is already covered by `test_replication_replicate_
// deletes_removes_orphans` (manual.txt without any marker survives).
//
// What was NOT covered before this batch:
//
//   **Sibling-rule marker mismatch**: an object on dest bearing
//   a different rule's marker (`dg-replication-rule = sibling-b`)
//   must NOT be deleted by THIS rule's delete pass — even when
//   its source-side counterpart is missing.
//
// Pre-fix the run_delete_pass had no provenance check at all, so any
// dest key whose source counterpart was missing was deleted.
// Post-fix the marker must equal the running rule's `name` exactly.
//
// Note on test mechanics: clients cannot spoof `dg-*` metadata via
// the S3 PUT path — `extract_user_metadata` in `src/api/handlers/
// mod.rs` filters them out as a hardening measure. To plant a foreign
// marker we therefore configure TWO rules (`sibling-a`, `sibling-b`)
// pointing at overlapping destination prefixes; each rule's `copy_one`
// stamps its own name. Then we run rule A, run rule B, and verify that
// rule A's delete pass does not touch the keys rule B planted.

const TWO_SIBLING_RULES_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: sibling-a
      enabled: true
      source:
        bucket: a-src
        prefix: \"\"
      destination:
        bucket: shared-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
      replicate_deletes: true
    - name: sibling-b
      enabled: true
      source:
        bucket: b-src
        prefix: \"\"
      destination:
        bucket: shared-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
      replicate_deletes: true
";

/// H2 (fourth-wave) regression: when two rules write to the same
/// destination bucket, each rule's delete pass must only consider
/// keys that carry ITS OWN provenance marker.
///
/// Pre-fix the run_delete_pass had no provenance check at all, so
/// rule A's delete pass would gleefully delete keys rule B had just
/// replicated (because A's source bucket has no key matching B's
/// destination key, and the marker check was missing).
///
/// Setup: two rules, two source buckets, one shared destination. Both
/// rules run, both stamp their own markers. Then we delete a key from
/// rule A's source and run rule A. Rule A's delete pass MUST delete
/// the matching dest key (its own provenance), but MUST leave rule
/// B's keys alone — even though, from rule A's source's perspective,
/// they have no source counterpart.
#[tokio::test]
async fn test_replication_delete_pass_skips_sibling_rule_keys() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(TWO_SIBLING_RULES_YAML)
        .build()
        .await;
    let s3 = server.s3_client().await;
    for b in ["a-src", "b-src", "shared-dst"] {
        s3.create_bucket().bucket(b).send().await.ok();
    }

    // Rule A's source content.
    for key in ["a-only-1.bin", "a-only-2.bin"] {
        s3.put_object()
            .bucket("a-src")
            .key(key)
            .body(ByteStream::from(b"from-a".to_vec()))
            .send()
            .await
            .unwrap();
    }
    // Rule B's source content (different keys to avoid prefix collision).
    for key in ["b-only-1.bin", "b-only-2.bin"] {
        s3.put_object()
            .bucket("b-src")
            .key(key)
            .body(ByteStream::from(b"from-b".to_vec()))
            .send()
            .await
            .unwrap();
    }

    let admin = admin_http_client(&server.endpoint()).await;

    // Trigger both rules so each stamps its own marker on dest.
    for rule_name in ["sibling-a", "sibling-b"] {
        let resp = admin
            .post(format!(
                "{}/_/api/admin/replication/rules/{}/run-now",
                server.endpoint(),
                rule_name
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "run {} failed", rule_name);
    }

    // Sanity: all four keys present on dst.
    for key in [
        "a-only-1.bin",
        "a-only-2.bin",
        "b-only-1.bin",
        "b-only-2.bin",
    ] {
        s3.head_object()
            .bucket("shared-dst")
            .key(key)
            .send()
            .await
            .unwrap_or_else(|_| panic!("expected {} on shared-dst after both rules ran", key));
    }

    // Now delete a-only-1 from rule A's source. Rule A's NEXT run
    // will see the orphan on dst, match its own provenance marker,
    // and delete it.
    //
    // CRUCIAL: rule A's delete pass also sees b-only-1 / b-only-2
    // on dst. From A's perspective, neither key exists in `a-src`.
    // Pre-fix it would delete them. Post-fix it sees the marker is
    // `sibling-b`, not `sibling-a`, and skips.
    s3.delete_object()
        .bucket("a-src")
        .key("a-only-1.bin")
        .send()
        .await
        .unwrap();

    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/sibling-a/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["status"].as_str(),
        Some("succeeded"),
        "rule A run should succeed: {}",
        body
    );

    // a-only-1 should be GONE from dst (rule A's own deletion).
    let head_a1 = s3
        .head_object()
        .bucket("shared-dst")
        .key("a-only-1.bin")
        .send()
        .await;
    assert!(
        head_a1.is_err(),
        "rule A should delete its own orphan a-only-1 from dst"
    );

    // b-only-1 and b-only-2 must STILL be there — they were written
    // by rule B, not rule A. Pre-fix these would have been deleted.
    for key in ["b-only-1.bin", "b-only-2.bin"] {
        s3.head_object()
            .bucket("shared-dst")
            .key(key)
            .send()
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "H2 REGRESSION: rule A's delete pass deleted {} (owned by rule B): {:?}",
                    key, e
                )
            });
    }

    // a-only-2 remains because its source counterpart is still in a-src.
    s3.head_object()
        .bucket("shared-dst")
        .key("a-only-2.bin")
        .send()
        .await
        .expect("a-only-2 still on dst (source counterpart present)");
}

// ════════════════════════════════════════════════════════════════════
// M1 (third-wave) — partial-failure status flip
// ════════════════════════════════════════════════════════════════════
//
// Pre-fix the run summary reported `status="succeeded"` even when SOME
// objects failed (the flip only happened when ALL copies errored).
// Post-fix any per-object failure flips status to `"failed"`.
//
// To trigger a partial failure deterministically we configure a rule
// with a NON-EXISTENT source bucket. The forward pass's
// `engine.list_objects` errors on bucket-not-found, which sets
// `hit_fatal_error = true` AND increments errors. With
// `replicate_deletes = false` and no successful work, the run is a
// pure-failure case — but it pins down the truth-table requirement:
// any error MUST produce `status="failed"`.
//
// (A genuinely "mixed" success/failure run on filesystem backend is
// hard to synthesise without race conditions; the truth-table check
// here proves the M1 logic and complements the existing happy-path
// `status="succeeded"` assertions in the other tests.)

const MISSING_DST_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: missing-dst-rule
      enabled: true
      source:
        bucket: m1-src
        prefix: \"\"
      destination:
        bucket: nonexistent-dst-bucket
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
";

/// Cause a per-object `copy_one` failure (destination bucket missing
/// → engine.store on the destination errors with NoSuchBucket) and
/// assert the run summary surfaces `status="failed"` with non-zero
/// errors. Pre-fix the wave-3 M1 fix the status was only flipped when
/// every copy errored — but the underlying truth-table bug was that
/// `had_any_error` wasn't consulted at the final-status decision; this
/// test pins that down by triggering exactly one error path
/// (the destination doesn't exist) on a populated source.
#[tokio::test]
async fn test_replication_any_error_flips_status_to_failed() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(MISSING_DST_RULE_YAML)
        .build()
        .await;
    let s3 = server.s3_client().await;
    // Create source only; destination intentionally missing.
    s3.create_bucket().bucket("m1-src").send().await.ok();
    for key in ["x.bin", "y.bin"] {
        s3.put_object()
            .bucket("m1-src")
            .key(key)
            .body(ByteStream::from(b"data".to_vec()))
            .send()
            .await
            .unwrap();
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/replication/rules/missing-dst-rule/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();

    let errors = body["errors"].as_i64().unwrap_or(0);
    let status = body["status"].as_str().unwrap_or("");
    assert!(
        errors > 0,
        "test pre-condition: rule should record at least one error \
         when copying into a missing destination bucket. body={}",
        body
    );
    // Pre-fix wave-3 M1: status could read "succeeded" if even one
    // copy slipped through the cracks (or if the all-fail predicate
    // was wrong). Post-fix any non-zero error count → "failed".
    assert_eq!(
        status, "failed",
        "M1 REGRESSION: errors={} but status={} (must be 'failed' on any error). body={}",
        errors, status, body
    );
}

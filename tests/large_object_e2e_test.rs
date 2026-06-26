// SPDX-License-Identifier: GPL-3.0-only

//! Extended e2e validation + CI regression gate for large-file replication.
//!
//! Proves the streaming-multipart copy CHARACTERISTICS (bounded memory, real
//! part/object concurrency, per-part range-resume, multipart used, delta
//! byte-savings) and FAILS on regression — gating ONLY on deterministic,
//! machine-independent invariants (byte-count ratios, exact part counts,
//! in-flight peaks, retry counts the algorithm controls). NEVER on wall-clock
//! or raw RSS (those are informational via `eprintln!` only).
//!
//! Tests 1–5 use the filesystem backend (the buffered-multipart path still
//! drives plan_parts → ranged-GET → upload_part → buffer_unordered, so every
//! structural invariant is exercised without MinIO). Test 6 needs MinIO for
//! delta reconstruction + native multipart.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, big_passthrough_body, metrics_snapshot, TestServer};
use serde_json::Value;

const MIB: usize = 1024 * 1024;

/// Replication rule (src → dst) with the given object/part concurrency,
/// substituted into the storage section so each test shapes its own geometry.
fn rule_yaml(transfers: u32, upload_concurrency: u32) -> String {
    format!(
        "
replication:
  enabled: true
  tick_interval: \"30s\"
  transfers: {transfers}
  upload_concurrency: {upload_concurrency}
  rules:
    - name: e2e
      enabled: true
      source:
        bucket: e2e-src
        prefix: \"\"
      destination:
        bucket: e2e-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
"
    )
}

/// Trigger the replication rule synchronously and assert it succeeded.
async fn run_now_ok(server: &TestServer) -> Value {
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:e2e/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request");
    assert_eq!(resp.status().as_u16(), 200, "run-now should succeed");
    let outcome: Value = resp.json().await.unwrap();
    assert_eq!(
        outcome["status"].as_str(),
        Some("succeeded"),
        "run status: {outcome}"
    );
    outcome
}

/// Seed one passthrough `.bin` into the source bucket.
async fn seed_passthrough(server: &TestServer, key: &str, len: usize) -> Vec<u8> {
    let body = big_passthrough_body(len);
    let client = server.s3_client().await;
    for b in ["e2e-src", "e2e-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    client
        .put_object()
        .bucket("e2e-src")
        .key(key)
        .body(ByteStream::from(body.clone()))
        .send()
        .await
        .expect("seed object");
    body
}

// ── Test 1: bounded memory ──────────────────────────────────────────────
//
// GATING: 0 < peak_part_bytes_resident < object_size / 4.
// 64 MiB object, concurrency 2, 5 MiB parts → ~10 MiB resident high-water.
// The lower bound (>0) catches a regression that drops the streaming path and
// re-buffers the whole object via retrieve (resident stays 0); the upper bound
// (< object_size/4) catches unbounded part buffering. Both ends machine-
// independent — they are byte counts the algorithm controls.
#[tokio::test]
async fn memory_bounded_resident_part_bytes() {
    let obj_size = 64 * MIB;
    let server = TestServer::builder()
        .auth("k", "s")
        .extra_yaml_storage_section(&rule_yaml(1, 2))
        .env("DGP_STREAM_COPY_THRESHOLD", "1048576") // 1 MiB
        .env("DGP_MULTIPART_PART_SIZE", "5242880") // 5 MiB (S3 min)
        .build()
        .await;

    seed_passthrough(&server, "big.bin", obj_size).await;
    run_now_ok(&server).await;

    let m = metrics_snapshot(&server.endpoint()).await;
    eprintln!(
        "[info] peak resident {:.1} MiB / object {} MiB",
        m.part_bytes_resident_peak as f64 / MIB as f64,
        obj_size / MIB
    );
    assert!(
        m.part_bytes_resident_peak > 0,
        "streaming path must have buffered at least one part — peak resident 0 means the \
         copy fell back to whole-object buffering"
    );
    assert!(
        m.part_bytes_resident_peak < (obj_size as u64) / 4,
        "peak resident part bytes {} must stay below object_size/4 ({}) — full-object \
         buffering would regress this to ~{}",
        m.part_bytes_resident_peak,
        obj_size / 4,
        obj_size
    );
}

// ── Test 2: part count matches the pure plan ────────────────────────────
//
// GATING: multipart_parts_total == plan_parts(size, part_size).len().
// Same pure fn both sides. Single-PUT regression → 1 part → FALSE.
#[tokio::test]
async fn part_count_matches_plan() {
    let obj_size = 64 * MIB;
    let part_size: u64 = 5 * MIB as u64;
    let server = TestServer::builder()
        .auth("k", "s")
        .extra_yaml_storage_section(&rule_yaml(1, 2))
        .env("DGP_STREAM_COPY_THRESHOLD", "1048576")
        .env("DGP_MULTIPART_PART_SIZE", &part_size.to_string())
        .build()
        .await;

    seed_passthrough(&server, "big.bin", obj_size).await;
    run_now_ok(&server).await;

    let expected = deltaglider_proxy::transfer_plan::plan_parts(obj_size as u64, part_size).len();
    let m = metrics_snapshot(&server.endpoint()).await;
    eprintln!(
        "[info] uploaded {} parts, plan expected {}",
        m.multipart_parts_total, expected
    );
    assert_eq!(
        m.multipart_parts_total, expected as u64,
        "uploaded part count must equal the pure plan_parts count"
    );
}

// ── Test 3: in-flight parts reach the configured concurrency ────────────
//
// GATING: parts_inflight_peak == upload_concurrency (barrier forces overlap).
// Serialized pipeline caps the peak at 1 → FALSE.
#[tokio::test]
async fn inflight_parts_reach_concurrency() {
    let concurrency: u32 = 3;
    let obj_size = 32 * MIB; // 5 MiB parts → ~7 parts, >= concurrency
    let server = TestServer::builder()
        .auth("k", "s")
        .extra_yaml_storage_section(&rule_yaml(1, concurrency))
        .env("DGP_STREAM_COPY_THRESHOLD", "1048576")
        .env("DGP_MULTIPART_PART_SIZE", "5242880")
        .env("DGP_TEST_PART_BARRIER", "1")
        .env("DGP_TEST_PART_DELAY_MS", "200")
        .build()
        .await;

    seed_passthrough(&server, "big.bin", obj_size).await;
    run_now_ok(&server).await;

    let m = metrics_snapshot(&server.endpoint()).await;
    eprintln!(
        "[info] parts_inflight_peak {} (configured concurrency {})",
        m.parts_inflight_peak, concurrency
    );
    assert_eq!(
        m.parts_inflight_peak, concurrency as u64,
        "parts in flight must peak at the configured upload_concurrency"
    );
}

// ── Test 4: concurrent objects overlap ──────────────────────────────────
//
// GATING: objects_inflight_peak >= 2 (transfers=2, 2+ objects, barrier).
// Lost object concurrency → 1 → FALSE.
#[tokio::test]
async fn inflight_objects_overlap() {
    let server = TestServer::builder()
        .auth("k", "s")
        .extra_yaml_storage_section(&rule_yaml(2, 2))
        .env("DGP_STREAM_COPY_THRESHOLD", "1048576")
        .env("DGP_MULTIPART_PART_SIZE", "5242880")
        .env("DGP_TEST_OBJECT_BARRIER", "1")
        .env("DGP_TEST_OBJECT_DELAY_MS", "250")
        .build()
        .await;

    // Three objects so a transfers=2 batch must overlap at least two.
    for i in 0..3 {
        seed_passthrough(&server, &format!("obj-{i}.bin"), 8 * MIB).await;
    }
    run_now_ok(&server).await;

    let m = metrics_snapshot(&server.endpoint()).await;
    eprintln!("[info] objects_inflight_peak {}", m.objects_inflight_peak);
    assert!(
        m.objects_inflight_peak >= 2,
        "objects in flight must peak at >= 2 with transfers=2 and 3 objects"
    );
}

// ── Test 5: per-part range-resume fires exactly once ────────────────────
//
// GATING: part_retries_total == 1 AND byte-identical (fail-once hook).
// Whole-object retry/abort → != 1 or corrupt → FALSE.
//
// The fail-once env (DGP_TEST_FAIL_PART_ONCE) and its `static FIRED` live in
// the SPAWNED server process (set via builder.env), not the test process, so
// each TestServer has its own one-shot fault and no `static LOCK`/env-mutation
// serialization is needed.
#[tokio::test]
async fn per_part_resume_single_retry() {
    let obj_size = 32 * MIB;
    let server = TestServer::builder()
        .auth("k", "s")
        .extra_yaml_storage_section(&rule_yaml(1, 2))
        .env("DGP_STREAM_COPY_THRESHOLD", "1048576")
        .env("DGP_MULTIPART_PART_SIZE", "5242880")
        .env("DGP_TEST_FAIL_PART_ONCE", "2") // fail part #2 exactly once
        .build()
        .await;

    let body = seed_passthrough(&server, "big.bin", obj_size).await;
    run_now_ok(&server).await;

    let m = metrics_snapshot(&server.endpoint()).await;
    eprintln!("[info] part_retries_total {}", m.part_retries_total);
    assert_eq!(
        m.part_retries_total, 1,
        "exactly one per-part range-resume retry must fire"
    );

    // Byte-identical destination (range-resume reconstructed the part).
    let client = server.s3_client().await;
    let got = client
        .get_object()
        .bucket("e2e-dst")
        .key("big.bin")
        .send()
        .await
        .expect("dest present")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(&got[..], &body[..], "dest byte-identical after resume");
}

// ── Test 6: delta byte-savings on replicate (MinIO) ─────────────────────
//
// GATING: delta_bytes_saved_total delta > 0 AND the v2 delta is < logical/10
// bytes-on-wire. saved = logical - stored, so saved > logical*9/10 ⇒
// stored < logical/10. Reconstruct-and-reship full bytes → FALSE.
#[tokio::test]
async fn delta_savings_below_fraction() {
    skip_unless_minio!();

    let v_size = 8 * MIB;
    // Two MinIO buckets on the same backend, isolated per run.
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let src = format!("e2e-d-src-{suffix}");
    let dst = format!("e2e-d-dst-{suffix}");

    let rule = format!(
        "
replication:
  enabled: true
  tick_interval: \"30s\"
  transfers: 2
  upload_concurrency: 2
  rules:
    - name: e2e
      enabled: true
      source:
        bucket: {src}
        prefix: \"\"
      destination:
        bucket: {dst}
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
"
    );

    let server = TestServer::builder()
        .auth("k", "s")
        .s3_endpoint(&common::minio_endpoint_url())
        // CI sets this job-wide; set it per-server so the test is also
        // self-contained locally (http:// MinIO endpoint).
        .env("DGP_BACKEND_ALLOW_LOCAL", "true")
        .extra_yaml_storage_section(&rule)
        .build()
        .await;

    let client = server.s3_client().await;
    for b in [&src, &dst] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // v1: reference (8 MiB incompressible, .tar → delta-eligible).
    let v1 = big_passthrough_body(v_size);
    // v2: v1 with a 64 KiB middle window overwritten → small delta vs v1.
    let mut v2 = v1.clone();
    let mid = v_size / 2;
    for (i, b) in v2[mid..mid + 64 * 1024].iter_mut().enumerate() {
        *b = (i as u8) ^ 0xA5;
    }

    // Same prefix so v2 deltas against v1 on BOTH src and dst.
    for (key, body) in [("rel/app-v1.tar", &v1), ("rel/app-v2.tar", &v2)] {
        client
            .put_object()
            .bucket(&src)
            .key(key)
            .body(ByteStream::from(body.clone()))
            .send()
            .await
            .expect("seed src");
    }

    let before = metrics_snapshot(&server.endpoint()).await;
    run_now_ok(&server).await;
    let after = metrics_snapshot(&server.endpoint()).await;

    // Replicating a delta object saves bytes via ONE of two paths:
    // - delta-passthrough: the .delta is shipped verbatim (egress saved), or
    // - reconstruct+recompress: re-encoded at the dest (compression saved).
    // Either is a valid "delta saved bytes" — assert the union, then that the
    // saving is most of the logical object (the v2 delta is tiny vs 8 MiB).
    let recompress = after
        .delta_bytes_saved_total
        .saturating_sub(before.delta_bytes_saved_total);
    let passthrough = after
        .delta_passthrough_bytes_saved_total
        .saturating_sub(before.delta_passthrough_bytes_saved_total);
    let saved = recompress.max(passthrough);
    eprintln!(
        "[info] saved {} for a {}-byte v2 (recompress {}, passthrough {})",
        saved, v_size, recompress, passthrough
    );
    assert!(saved > 0, "replicating a delta object must save bytes");
    assert!(
        saved > (v_size as u64) * 9 / 10,
        "the v2 delta must be < logical/10 on wire (saved {} of {})",
        saved,
        v_size
    );
}

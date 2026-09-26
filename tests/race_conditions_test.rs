// SPDX-License-Identifier: BUSL-1.1

//! Race tests: N requests in flight at once on one key, one deltaspace or
//! one budget, then an invariant on the end state. Every test fires its
//! requests with `join_all` (not one by one), so the proxy sees them
//! interleaved. `concurrency_test` covers "no panic, no corruption" for
//! plain parallel traffic; this file covers the check-then-act classes
//! (REPORT.md class 5): conditional writes, lifecycle re-check, bulk move,
//! first-baseline creation and the spool budget.

use crate::common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use common::{admin_http_client, generate_binary, mutate_binary, S3Requests, TestServer};
use futures::future::join_all;
use reqwest::Method;
use std::time::Duration;

const N: usize = 16;

fn obj_url(server: &TestServer, key: &str) -> String {
    format!("{}/{}/{}", server.endpoint(), server.bucket(), key)
}

async fn get(server: &TestServer, key: &str) -> (u16, Vec<u8>) {
    let resp = server
        .http()
        .s3_request(Method::GET, &obj_url(server, key))
        .send()
        .await
        .unwrap();
    let code = resp.status().as_u16();
    (code, resp.bytes().await.unwrap().to_vec())
}

/// `If-None-Match: *` from N writers at once: exactly one PUT wins, every
/// other one is refused (412, or 409 ConditionalRequestConflict), and the
/// stored bytes are the winner's. Passthrough and delta-eligible keys.
#[tokio::test]
async fn conditional_put_if_none_match_has_exactly_one_winner() {
    conditional_put_race(&TestServer::filesystem().await, "cond").await;
}

#[tokio::test]
async fn conditional_put_if_none_match_has_exactly_one_winner_on_s3() {
    skip_unless_minio!();
    let prefix = common::unique_bucket("cond");
    conditional_put_race(&TestServer::s3().await, &prefix).await;
}

async fn conditional_put_race(server: &TestServer, prefix: &str) {
    let http = server.http();
    let base = generate_binary(64 * 1024, 1);
    for key in [format!("{prefix}/one.bin"), format!("{prefix}-zip/one.zip")] {
        let key = key.as_str();
        let bodies: Vec<Vec<u8>> = (0..N)
            .map(|i| mutate_binary(&base, 0.01 + i as f64 * 0.001))
            .collect();
        let codes = join_all(bodies.iter().map(|b| {
            http.s3_request(Method::PUT, &obj_url(server, key))
                .header("If-None-Match", "*")
                .body(b.clone())
                .send()
        }))
        .await
        .into_iter()
        .map(|r| r.unwrap().status().as_u16())
        .collect::<Vec<_>>();
        let winners: Vec<usize> = (0..N).filter(|i| codes[*i] == 200).collect();
        assert_eq!(winners.len(), 1, "{key}: exactly one winner, got {codes:?}");
        assert!(
            codes.iter().all(|c| matches!(c, 200 | 412 | 409)),
            "{key}: losers are refused, got {codes:?}"
        );
        let (code, got) = get(server, key).await;
        assert_eq!(code, 200);
        assert!(
            got == bodies[winners[0]],
            "{key}: the stored bytes are the winner's"
        );
    }
}

/// CompleteMultipartUpload with `If-None-Match: *` from N uploads of one
/// key at once: exactly one completes.
#[tokio::test]
async fn complete_multipart_if_none_match_has_exactly_one_winner() {
    complete_multipart_race(&TestServer::filesystem().await, "cond-mp/one.bin").await;
}

#[tokio::test]
async fn complete_multipart_if_none_match_has_exactly_one_winner_on_s3() {
    skip_unless_minio!();
    let key = format!("{}/one.bin", common::unique_bucket("cond-mp"));
    complete_multipart_race(&TestServer::s3().await, &key).await;
}

async fn complete_multipart_race(server: &TestServer, key: &str) {
    let s3 = server.s3_client().await;
    let b = server.bucket();
    let mut uploads = Vec::new();
    for i in 0..N {
        let id = s3
            .create_multipart_upload()
            .bucket(b)
            .key(key)
            .send()
            .await
            .unwrap()
            .upload_id()
            .unwrap()
            .to_string();
        let body = format!("upload {i}").into_bytes();
        let etag = s3
            .upload_part()
            .bucket(b)
            .key(key)
            .upload_id(&id)
            .part_number(1)
            .body(ByteStream::from(body.clone()))
            .send()
            .await
            .unwrap()
            .e_tag()
            .unwrap()
            .to_string();
        uploads.push((id, etag, body));
    }
    let results = join_all(uploads.iter().map(|(id, etag, _)| {
        s3.complete_multipart_upload()
            .bucket(b)
            .key(key)
            .upload_id(id)
            .if_none_match("*")
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .parts(CompletedPart::builder().part_number(1).e_tag(etag).build())
                    .build(),
            )
            .send()
    }))
    .await;
    let winners: Vec<usize> = (0..N).filter(|i| results[*i].is_ok()).collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one Complete wins: {:?}",
        results.iter().map(|r| r.is_ok()).collect::<Vec<_>>()
    );
    for (i, r) in results.iter().enumerate() {
        if let Err(e) = r {
            let code = e.raw_response().map(|r| r.status().as_u16());
            assert!(
                matches!(code, Some(412 | 409)),
                "upload {i}: a loser is refused with 412/409, got {code:?}: {e:?}"
            );
        }
    }
    let (_, got) = get(server, key).await;
    assert_eq!(
        got, uploads[winners[0]].2,
        "the stored bytes are the winner's"
    );
}

fn lifecycle_race_yaml(bucket: &str) -> String {
    format!(
        r#"
lifecycle:
  enabled: true
  tick_interval: "1h"
  rules:
    - name: expire-race
      enabled: true
      bucket: {bucket}
      prefix: "old/"
      action: delete
      expire_after: "3s"
      batch_size: 1000
      include_globs: []
      exclude_globs: []
"#
    )
}

/// Lifecycle age-delete while clients overwrite the expired keys: an
/// overwrite is a new object the rule never judged (age 0), so after the
/// run and the writes every key holds the NEW bytes. A delete by key after
/// a stale check removes the overwrite (D6).
#[tokio::test]
async fn lifecycle_delete_never_removes_a_concurrent_overwrite() {
    let server = TestServer::builder()
        .extra_yaml_storage_section(&lifecycle_race_yaml("bucket"))
        .build()
        .await;
    lifecycle_race(&server, &server, 1).await;
}

/// Two instances on one S3 bucket: lifecycle runs on `a`, clients
/// overwrite through `b`, whose PUTs do not take `a`'s in-process lock.
/// Only a conditional delete (If-Match) keeps the overwrites.
#[tokio::test]
async fn lifecycle_delete_never_removes_an_overwrite_from_another_instance() {
    skip_unless_minio!();
    let (a, b) = two_instances_on_one_bucket(Some(lifecycle_race_yaml)).await;
    // The cross-instance window is small: without a conditional delete a
    // round loses an overwrite about every other time.
    lifecycle_race(&a, &b, 4).await;
}

/// Two proxies sharing one fresh MinIO bucket. `yaml` (given the bucket
/// name) goes into the first one's storage section.
async fn two_instances_on_one_bucket(yaml: Option<fn(&str) -> String>) -> (TestServer, TestServer) {
    let bucket = common::unique_bucket("race-2i");
    let spawn = |extra: Option<String>| {
        let mut b = TestServer::builder()
            .s3_endpoint(&common::minio_endpoint_url())
            .bucket(&bucket);
        if let Some(y) = extra {
            b = b.extra_yaml_storage_section(&y);
        }
        b.build()
    };
    let a = spawn(yaml.map(|f| f(&bucket))).await;
    let b = spawn(None).await;
    (a, b)
}

/// Overwrite every key once, all at once, while `done` runs. Returns the
/// new body of each key. (Overwriting round after round keeps every key
/// fresh, so the rule plans nothing and the race never happens.)
async fn overwrite_during<F: std::future::Future>(
    writer: &TestServer,
    keys: &[String],
    round: usize,
    done: F,
) -> (F::Output, Vec<Vec<u8>>) {
    let http = writer.http();
    let bodies: Vec<Vec<u8>> = keys
        .iter()
        .map(|k| format!("round {round}: new generation of {k}").into_bytes())
        .collect();
    let writes = join_all(keys.iter().zip(&bodies).map(|(k, b)| {
        http.s3_request(Method::PUT, &obj_url(writer, k))
            .body(b.clone())
            .send()
    }));
    let (out, ws) = tokio::join!(done, writes);
    for w in ws {
        assert_eq!(w.unwrap().status().as_u16(), 200, "overwrite PUT");
    }
    (out, bodies)
}

/// `rounds` times: let every key expire, then run the rule while a burst
/// overwrites every key. After each round every key holds its new bytes.
async fn lifecycle_race(runner: &TestServer, writer: &TestServer, rounds: usize) {
    let http = writer.http();
    let admin = admin_http_client(&runner.endpoint()).await;
    const KEYS: usize = 100;
    let keys: Vec<String> = (0..KEYS).map(|i| format!("old/k{i:04}.txt")).collect();
    join_all(keys.iter().map(|k| {
        http.s3_request(Method::PUT, &obj_url(writer, k))
            .body(b"old generation".to_vec())
            .send()
    }))
    .await
    .into_iter()
    .for_each(|r| assert_eq!(r.unwrap().status().as_u16(), 200));

    let ep = runner.endpoint();
    for round in 0..rounds {
        tokio::time::sleep(Duration::from_millis(3500)).await;
        let run = common::lifecycle_run_now_and_wait(&admin, &ep, "expire-race");
        let (run, last) = overwrite_during(writer, &keys, round, run).await;
        assert!(run["status"].as_str().is_some(), "run settled: {run}");

        let mut lost = Vec::new();
        for (k, want) in keys.iter().zip(&last) {
            let (code, body) = get(writer, k).await;
            if code != 200 || &body != want {
                lost.push(format!("{k}: {code} {:?}", String::from_utf8_lossy(&body)));
            }
        }
        assert!(
            lost.is_empty(),
            "round {round}: lifecycle removed {} of {KEYS} overwrites (run {run}):\n{}",
            lost.len(),
            lost.join("\n")
        );
    }
}

/// Admin bulk move and bulk copy (and a second move) of the same keys at
/// once. Every key keeps its bytes somewhere, and every copy the response
/// reports as done holds the source bytes.
#[tokio::test]
async fn admin_move_and_copy_of_the_same_keys_lose_nothing() {
    let server = TestServer::filesystem().await;
    let http = server.http();
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket().to_string();
    const KEYS: usize = 40;
    let body_of = |i: usize| format!("payload of key {i}").into_bytes();
    for i in 0..KEYS {
        let r = http
            .s3_request(Method::PUT, &obj_url(&server, &format!("src/k{i:03}.txt")))
            .body(body_of(i))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
    }
    let items: Vec<serde_json::Value> = (0..KEYS)
        .map(|i| {
            serde_json::json!({
                "source_key": format!("src/k{i:03}.txt"),
                "relative": format!("k{i:03}.txt"),
            })
        })
        .collect();
    let req = |op: &str, prefix: &str| {
        admin
            .post(format!("{}/_/api/admin/objects/{op}", server.endpoint()))
            .json(&serde_json::json!({
                "source_bucket": bucket,
                "dest_bucket": bucket,
                "dest_prefix": prefix,
                "items": items,
            }))
            .send()
    };
    let (mv, cp, mv2) = tokio::join!(
        req("move", "moved/"),
        req("copy", "copied/"),
        req("move", "moved2/")
    );
    let mut reports = Vec::new();
    for (name, r) in [("move", mv), ("copy", cp), ("move2", mv2)] {
        let r = r.unwrap();
        assert_eq!(r.status().as_u16(), 200, "{name}");
        let v: serde_json::Value = r.json().await.unwrap();
        reports.push((name, v));
    }

    let mut wrong = Vec::new();
    for i in 0..KEYS {
        let want = body_of(i);
        let mut holders = 0;
        for prefix in ["src/", "moved/", "copied/", "moved2/"] {
            let (code, got) = get(&server, &format!("{prefix}k{i:03}.txt")).await;
            if code == 200 {
                holders += 1;
                if got != want {
                    wrong.push(format!("{prefix}k{i:03}.txt holds other bytes"));
                }
            }
        }
        if holders == 0 {
            wrong.push(format!("k{i:03}: lost everywhere"));
        }
    }
    // A reported success names a destination that exists.
    for (name, v) in &reports {
        let failed: std::collections::HashSet<String> = v["failures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["source_key"].as_str().unwrap().to_string())
            .collect();
        let prefix = match *name {
            "move" => "moved/",
            "copy" => "copied/",
            _ => "moved2/",
        };
        if v["failures"].as_array().unwrap().len() < 100 {
            for i in 0..KEYS {
                if failed.contains(&format!("src/k{i:03}.txt")) {
                    continue;
                }
                let (code, _) = get(&server, &format!("{prefix}k{i:03}.txt")).await;
                if code != 200 {
                    wrong.push(format!("{name} reports k{i:03} done but {prefix} has none"));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{reports:?}\n{}", wrong.join("\n"));
}

/// Admin bulk move while clients overwrite the source keys: the move
/// deletes a source only if it is still the object it copied, so every
/// overwrite survives, in the source or (when it landed before the copy)
/// in the destination.
#[tokio::test]
async fn admin_move_never_removes_a_concurrent_overwrite() {
    let server = TestServer::filesystem().await;
    move_race(&server, &server).await;
}

/// Two instances on one S3 bucket: the move runs on `a`, clients
/// overwrite the sources through `b`.
#[tokio::test]
async fn admin_move_never_removes_an_overwrite_from_another_instance() {
    skip_unless_minio!();
    let (a, b) = two_instances_on_one_bucket(None).await;
    move_race(&a, &b).await;
}

async fn move_race(runner: &TestServer, writer: &TestServer) {
    let http = writer.http();
    let admin = admin_http_client(&runner.endpoint()).await;
    let bucket = runner.bucket().to_string();
    const KEYS: usize = 100;
    let src = |i: usize| format!("mvsrc/k{i:03}.txt");
    for i in 0..KEYS {
        let r = http
            .s3_request(Method::PUT, &obj_url(writer, &src(i)))
            .body(b"first version".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
    }
    let items: Vec<serde_json::Value> = (0..KEYS)
        .map(|i| serde_json::json!({ "source_key": src(i), "relative": format!("k{i:03}.txt") }))
        .collect();
    let mv = admin
        .post(format!("{}/_/api/admin/objects/move", runner.endpoint()))
        .json(&serde_json::json!({
            "source_bucket": bucket,
            "dest_bucket": bucket,
            "dest_prefix": "mvdst/",
            "items": items,
        }))
        .send();
    let keys: Vec<String> = (0..KEYS).map(src).collect();
    let (mv, last) = overwrite_during(writer, &keys, 0, mv).await;
    assert_eq!(mv.unwrap().status().as_u16(), 200, "move");
    let mut lost = Vec::new();
    for (i, want) in last.iter().enumerate() {
        let (sc, sb) = get(writer, &src(i)).await;
        let (dc, db) = get(writer, &format!("mvdst/k{i:03}.txt")).await;
        let kept = (sc == 200 && &sb == want) || (dc == 200 && &db == want);
        if !kept {
            lost.push(format!(
                "k{i:03}: src {sc}, dst {dc} {:?}",
                String::from_utf8_lossy(&db)
            ));
        }
    }
    assert!(
        lost.is_empty(),
        "the move lost {} overwrites:\n{}",
        lost.len(),
        lost.join("\n")
    );
}

/// Files named `reference.bin` under `dir` (recursive).
fn references_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(references_under(&p));
        } else if p.file_name().is_some_and(|n| n == "reference.bin") {
            out.push(p);
        }
    }
    out
}

/// N writers put the FIRST objects of one deltaspace at once: exactly one
/// baseline (reference.bin) is created, and every object reads back.
#[tokio::test]
async fn first_baseline_from_n_writers_makes_one_reference() {
    let server = TestServer::filesystem().await;
    let http = server.http();
    let base = generate_binary(256 * 1024, 5);
    let bodies: Vec<Vec<u8>> = (0..N)
        .map(|i| mutate_binary(&base, 0.005 + i as f64 * 0.001))
        .collect();
    let codes: Vec<u16> = join_all(bodies.iter().enumerate().map(|(i, b)| {
        http.s3_request(
            Method::PUT,
            &obj_url(&server, &format!("fresh/build-{i:02}.zip")),
        )
        .body(b.clone())
        .send()
    }))
    .await
    .into_iter()
    .map(|r| r.unwrap().status().as_u16())
    .collect();
    assert!(
        codes.iter().all(|c| *c == 200),
        "every PUT lands: {codes:?}"
    );
    let refs = references_under(&server.data_dir().unwrap().join(server.bucket()));
    assert_eq!(refs.len(), 1, "one baseline for the deltaspace: {refs:?}");
    for (i, b) in bodies.iter().enumerate() {
        let (code, got) = get(&server, &format!("fresh/build-{i:02}.zip")).await;
        assert_eq!(code, 200);
        assert!(&got == b, "build-{i:02}.zip reads back its bytes");
    }
}

/// The same race across TWO instances that share one MinIO bucket and a
/// coordination bucket (the cross-instance reference lock): one baseline.
#[tokio::test]
async fn first_baseline_across_two_instances_makes_one_reference() {
    skip_unless_minio!();
    let data_bucket = common::unique_bucket("race-ref");
    let minio = common::minio_client().await;
    minio
        .create_bucket()
        .bucket(&data_bucket)
        .send()
        .await
        .unwrap();
    let spawn = || {
        TestServer::builder()
            .s3_endpoint(&common::minio_endpoint_url())
            .bucket(&data_bucket)
            .config_sync_bucket(common::MINIO_BUCKET)
            .config_sync_object_key(&format!("race/{data_bucket}.db"))
            .build()
    };
    let (a, b) = tokio::join!(spawn(), spawn());
    let base = generate_binary(256 * 1024, 6);
    let bodies: Vec<Vec<u8>> = (0..N)
        .map(|i| mutate_binary(&base, 0.005 + i as f64 * 0.001))
        .collect();
    let (ha, hb) = (a.http(), b.http());
    let codes: Vec<u16> = join_all(bodies.iter().enumerate().map(|(i, body)| {
        let (server, http) = if i % 2 == 0 { (&a, &ha) } else { (&b, &hb) };
        http.s3_request(
            Method::PUT,
            &obj_url(server, &format!("fresh/build-{i:02}.zip")),
        )
        .body(body.clone())
        .send()
    }))
    .await
    .into_iter()
    .map(|r| r.unwrap().status().as_u16())
    .collect();
    // A PUT that met a peer's lock may answer 503 SlowDown (retryable).
    assert!(codes.iter().all(|c| matches!(c, 200 | 503)), "{codes:?}");
    let listed = minio
        .list_objects_v2()
        .bucket(&data_bucket)
        .send()
        .await
        .unwrap();
    let refs: Vec<&str> = listed
        .contents()
        .iter()
        .filter_map(|o| o.key())
        .filter(|k| k.ends_with("reference.bin"))
        .collect();
    assert_eq!(refs.len(), 1, "one baseline across instances: {refs:?}");
    for (i, body) in bodies.iter().enumerate() {
        if codes[i] != 200 {
            continue;
        }
        for server in [&a, &b] {
            let (code, got) = get(server, &format!("fresh/build-{i:02}.zip")).await;
            assert_eq!(code, 200, "build-{i:02}.zip on {}", server.endpoint());
            assert!(&got == body, "build-{i:02}.zip reads back its bytes");
        }
    }
}

/// Streaming PUTs and GETs at once against a small spool budget: none
/// deadlocks (all answer within the acquire timeout), an overload answers
/// 503 SlowDown, and after the storm the budget is whole again: the spool
/// dir is empty and every op succeeds alone.
#[tokio::test]
async fn spool_budget_under_concurrent_streaming_ops_returns_to_zero() {
    let spool_dir = tempfile::TempDir::new().unwrap();
    const MIB: usize = 1024 * 1024;
    let server = TestServer::builder()
        .env("DGP_SPOOL_DIR", &spool_dir.path().display().to_string())
        .env("DGP_SPOOL_MAX_BYTES", &(24 * MIB).to_string())
        .env("DGP_SPOOL_THRESHOLD_BYTES", &MIB.to_string())
        .env("DGP_SPOOL_ACQUIRE_TIMEOUT_SECS", "5")
        .build()
        .await;
    let http = server.http();
    let base = generate_binary(3 * MIB, 7);
    // Baseline + one delta first, so the storm has delta GETs to decode.
    let seed: Vec<Vec<u8>> = (0..2)
        .map(|i| mutate_binary(&base, 0.002 * (i + 1) as f64))
        .collect();
    for (i, b) in seed.iter().enumerate() {
        let r = http
            .s3_request(Method::PUT, &obj_url(&server, &format!("big/seed-{i}.zip")))
            .body(b.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200, "seed {i}");
    }
    let bodies: Vec<Vec<u8>> = (0..N)
        .map(|i| mutate_binary(&base, 0.003 + i as f64 * 0.001))
        .collect();
    let puts = bodies.iter().enumerate().map(|(i, b)| {
        let req = http
            .s3_request(
                Method::PUT,
                &obj_url(&server, &format!("big/storm-{i:02}.zip")),
            )
            .body(b.clone())
            .timeout(Duration::from_secs(60));
        async move { req.send().await.map(|r| r.status().as_u16()) }
    });
    let gets = (0..N).map(|i| {
        let req = http
            .s3_request(
                Method::GET,
                &obj_url(&server, &format!("big/seed-{}.zip", i % 2)),
            )
            .timeout(Duration::from_secs(60));
        async move {
            let r = req.send().await?;
            let code = r.status().as_u16();
            let body = r.bytes().await?;
            Ok::<_, reqwest::Error>((code, body.to_vec()))
        }
    });
    let started = std::time::Instant::now();
    let (put_codes, get_results) = tokio::join!(join_all(puts), join_all(gets));
    let elapsed = started.elapsed();
    let put_codes: Vec<u16> = put_codes
        .into_iter()
        .map(|r| r.expect("a PUT hung past its timeout (deadlock?)"))
        .collect();
    assert!(
        put_codes.iter().all(|c| matches!(c, 200 | 503)),
        "PUTs answer 200 or SlowDown: {put_codes:?}"
    );
    for (i, r) in get_results.into_iter().enumerate() {
        let (code, body) = r.expect("a GET hung past its timeout (deadlock?)");
        assert!(matches!(code, 200 | 503), "GET {i}: {code}");
        if code == 200 {
            assert!(body == seed[i % 2], "GET {i} returns the seed bytes");
        }
    }
    assert!(elapsed < Duration::from_secs(60), "storm took {elapsed:?}");

    // Every spool file is gone once the last request ends.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left: Vec<_> = std::fs::read_dir(spool_dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        if left.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "spool files leaked: {left:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The whole budget is free again: each op succeeds alone, including a
    // redo of every PUT the storm refused.
    for (i, b) in bodies.iter().enumerate() {
        let key = format!("big/storm-{i:02}.zip");
        if put_codes[i] != 200 {
            let r = http
                .s3_request(Method::PUT, &obj_url(&server, &key))
                .body(b.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 200, "{key} alone after the storm");
        }
        let (code, got) = get(&server, &key).await;
        assert_eq!(code, 200, "{key} alone after the storm");
        assert!(&got == b, "{key} reads back its bytes");
    }
}

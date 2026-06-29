// SPDX-License-Identifier: GPL-3.0-only

//! Integration coverage for the replication parity audit
//! (`POST /_/api/admin/jobs/replication:<rule>/verify`).
//!
//! TIGHT by design — the verdict truth table is exhaustively covered by
//! the pure tests in `src/replication/parity.rs`. This exercises only the
//! request-pipeline seam: replicate N → verify in_sync, then mutate the
//! destination out-of-band (relative to replication) into each unhappy
//! state and assert the count flips.
//!
//! Uses `.bin` passthrough objects so no delta seeding (xdelta3) is
//! involved — sha256 parity is the dominant verifier here.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, TestServer};
use serde_json::Value;

// Replication rule + a lifecycle rule (the latter only to assert that
// `verify` is rejected for non-replication job ids). One fragment because
// `extra_yaml_storage_section` is single-valued.
const PARITY_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: parity-a-to-b
      enabled: true
      source:
        bucket: parity-src
        prefix: \"\"
      destination:
        bucket: parity-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
lifecycle:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: lc-expire
      enabled: true
      bucket: parity-src
      prefix: \"\"
      expire_after: \"30d\"
";

/// Kick off the background parity audit (POST → 202) then poll the status
/// (GET) until it reaches a terminal state, returning the `outcome` object so
/// the existing assertions (out["in_sync"], out["matched"], …) keep working.
async fn parity_version(client: &reqwest::Client, endpoint: &str) -> u64 {
    client
        .get(format!("{endpoint}/_/api/admin/jobs/parity-version"))
        .send()
        .await
        .expect("parity-version GET")
        .json::<Value>()
        .await
        .unwrap()["version"]
        .as_u64()
        .expect("version is a number")
}

async fn verify(admin: &reqwest::Client, endpoint: &str) -> Value {
    let url = format!("{endpoint}/_/api/admin/jobs/replication:parity-a-to-b/verify");
    // The jobs/parity-version route is public (like iam/version), so the settle
    // barrier can be polled with a plain, unauthenticated client. `admin` is
    // still used for the verify POST/GET (those remain session-gated).
    let barrier = reqwest::Client::new();
    // Capture the settle-counter BEFORE kicking off — the audit bumps it on
    // terminal write, giving a deterministic barrier (no status-row polling).
    let before = parity_version(&barrier, endpoint).await;
    let resp = admin.post(&url).send().await.expect("verify POST");
    let code = resp.status().as_u16();
    assert!(
        code == 202 || code == 200,
        "verify POST should be 202 (background) or 200 (no-DB), got {code}"
    );
    // Wait for the counter to advance (≤ ~10s), then read the settled status.
    for _ in 0..100 {
        if parity_version(&barrier, endpoint).await > before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let s: Value = admin
        .get(&url)
        .send()
        .await
        .expect("verify GET")
        .json()
        .await
        .unwrap();
    match s.get("status").and_then(|v| v.as_str()) {
        Some("done") => s.get("outcome").cloned().expect("done has outcome"),
        Some("failed") => panic!("parity audit failed: {:?}", s.get("error")),
        other => panic!("parity audit did not settle to done; status={other:?}"),
    }
}

#[tokio::test]
async fn test_parity_audit_lifecycle() {
    const N: i64 = 4;

    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(PARITY_RULE_YAML)
        .build()
        .await;

    let client = server.s3_client().await;
    for b in ["parity-src", "parity-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // Seed N passthrough objects on source.
    for i in 0..N {
        client
            .put_object()
            .bucket("parity-src")
            .key(format!("obj-{i}.bin"))
            .body(ByteStream::from(format!("payload-{i}").into_bytes()))
            .send()
            .await
            .expect("seed source object");
    }

    let admin = admin_http_client(&server.endpoint()).await;

    // Replicate.
    let run = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:parity-a-to-b/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now");
    assert_eq!(run.status().as_u16(), 200);
    let run: Value = run.json().await.unwrap();
    assert_eq!(run["status"].as_str(), Some("succeeded"), "{run}");
    assert_eq!(run["objects_copied"].as_i64(), Some(N), "{run}");

    // 1. Fully in sync.
    let out = verify(&admin, &server.endpoint()).await;
    assert_eq!(
        out["in_sync"].as_bool(),
        Some(true),
        "expected in_sync: {out}"
    );
    assert_eq!(out["matched"].as_u64(), Some(N as u64), "{out}");
    assert_eq!(out["missing_on_dest"].as_u64(), Some(0), "{out}");
    assert_eq!(out["orphan_on_dest"].as_u64(), Some(0), "{out}");
    assert_eq!(out["checksum_mismatch"].as_u64(), Some(0), "{out}");
    assert_eq!(out["truncated"].as_bool(), Some(false), "{out}");

    // 2. Delete one dest object out-of-band → missing_on_dest == 1.
    client
        .delete_object()
        .bucket("parity-dst")
        .key("obj-0.bin")
        .send()
        .await
        .expect("delete dest object");
    let out = verify(&admin, &server.endpoint()).await;
    assert_eq!(out["missing_on_dest"].as_u64(), Some(1), "{out}");
    assert_eq!(out["in_sync"].as_bool(), Some(false), "{out}");
    assert_eq!(
        out["missing_samples"][0]["key"].as_str(),
        Some("obj-0.bin"),
        "{out}"
    );

    // 3. Add an extra object directly to the dest prefix → orphan_on_dest == 1
    //    (and the deleted obj-0 still counts as missing).
    client
        .put_object()
        .bucket("parity-dst")
        .key("extra.bin")
        .body(ByteStream::from(b"i am foreign".to_vec()))
        .send()
        .await
        .expect("put orphan dest object");
    let out = verify(&admin, &server.endpoint()).await;
    assert_eq!(out["orphan_on_dest"].as_u64(), Some(1), "{out}");
    assert_eq!(
        out["orphan_samples"][0]["key"].as_str(),
        Some("extra.bin"),
        "{out}"
    );
    assert_eq!(out["in_sync"].as_bool(), Some(false), "{out}");

    // 4. Overwrite a still-present dest object with different bytes →
    //    different sha256 → checksum_mismatch == 1.
    client
        .put_object()
        .bucket("parity-dst")
        .key("obj-1.bin")
        .body(ByteStream::from(b"TAMPERED DIFFERENT BYTES".to_vec()))
        .send()
        .await
        .expect("overwrite dest object");
    let out = verify(&admin, &server.endpoint()).await;
    assert_eq!(out["checksum_mismatch"].as_u64(), Some(1), "{out}");
    assert_eq!(
        out["mismatch_samples"][0]["key"].as_str(),
        Some("obj-1.bin"),
        "{out}"
    );
    assert_eq!(out["in_sync"].as_bool(), Some(false), "{out}");

    // 5. Verify on a lifecycle job id → 404 (parity verify is replication-only;
    //    the literal verify route rejects non-replication subsystems).
    let lc = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:lc-expire/verify",
            server.endpoint()
        ))
        .send()
        .await
        .expect("lifecycle verify request");
    assert_eq!(
        lc.status().as_u16(),
        404,
        "parity verify is replication-only — lifecycle jobs have no verify"
    );

    // 6. Cancel contract: cancelling a finished (done) audit is a 200 no-op that
    //    leaves the verdict intact; cancel on a lifecycle id is 404.
    let cancel = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:parity-a-to-b/verify/cancel",
            server.endpoint()
        ))
        .send()
        .await
        .expect("verify cancel request");
    assert_eq!(cancel.status().as_u16(), 200, "cancel returns 200");
    let cs: Value = cancel.json().await.unwrap();
    assert_eq!(
        cs["status"].as_str(),
        Some("done"),
        "cancelling a finished audit is a no-op — status stays done"
    );

    let lc_cancel = admin
        .post(format!(
            "{}/_/api/admin/jobs/lifecycle:lc-expire/verify/cancel",
            server.endpoint()
        ))
        .send()
        .await
        .expect("lifecycle cancel request");
    assert_eq!(
        lc_cancel.status().as_u16(),
        404,
        "verify cancel is replication-only"
    );
}

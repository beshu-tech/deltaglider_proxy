// SPDX-License-Identifier: BUSL-1.1

//! Issue #116 review D: a config apply builds a new engine; a run that
//! still holds the OLD engine (a replication or lifecycle run, a
//! multipart upload) must write with the per-bucket policy of the apply.

use crate::api::handlers::AppState;
use crate::config::Config;
use crate::deltaglider::DynEngine;
use crate::storage::{StorageBackend, StorageBackendExt};
use std::collections::HashMap;

const KB: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn config(dir: &std::path::Path, db_archive: &str) -> Config {
    Config::from_yaml_str(&format!(
        "storage:\n  filesystem: {}\n  buckets:\n    db-archive: {db_archive}\n",
        dir.display()
    ))
    .unwrap()
}

/// The key id stamped on a stored passthrough object (`None`: plaintext).
async fn stamp(engine: &DynEngine, bucket: &str, name: &str) -> Option<String> {
    let raw = engine
        .storage()
        .get_passthrough_metadata(bucket, "", name)
        .await
        .unwrap();
    raw.user_metadata
        .get(crate::storage::encrypting::ENCRYPTION_KEY_ID_KEY)
        .cloned()
}

#[tokio::test]
async fn a_run_on_the_old_engine_writes_with_the_applied_bucket_policy() {
    let dir = tempfile::tempdir().unwrap();
    let old_cfg = config(dir.path(), "{}");
    let new_cfg = config(
        dir.path(),
        &format!(
            "{{ encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-b }} }}"
        ),
    );
    let engine = DynEngine::new(&old_cfg, None).await.unwrap();
    engine.create_bucket("db-archive").await.unwrap();
    let app = AppState::for_tests(engine);
    // A long run took its engine snapshot before the apply.
    let running = app.engine.load_full();
    running
        .store("db-archive", "before.jpg", b"before", None, HashMap::new())
        .await
        .unwrap();
    assert_eq!(stamp(&running, "db-archive", "before.jpg").await, None);

    // The apply: build the new engine, install it.
    let new_engine = crate::config_apply::build_engine(&app, &new_cfg)
        .await
        .unwrap();
    crate::config_apply::install_engine(&app, new_engine, "test apply");

    // The rest of the run writes in the new mode.
    running
        .store("db-archive", "after.jpg", b"after", None, HashMap::new())
        .await
        .unwrap();
    assert_eq!(
        stamp(&running, "db-archive", "after.jpg").await.as_deref(),
        Some("db-archive-b"),
        "the old engine writes with the bucket policy that the apply published"
    );
    let current = app.engine.load_full();
    assert_eq!(
        current
            .storage()
            .get_passthrough("db-archive", "", "after.jpg")
            .await
            .unwrap(),
        b"after"
    );
}

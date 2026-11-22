// SPDX-License-Identifier: BUSL-1.1

//! Issue #116 review D: a config apply builds a new engine; a run that
//! still holds the OLD engine (a replication or lifecycle run, a
//! multipart upload) must write with the per-bucket policy of the apply.

use crate::api::handlers::AppState;
use crate::config::Config;
use crate::deltaglider::DynEngine;
use crate::storage::StorageBackendExt;
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

/// Issue #200: the kept reference files belong to the encryption settings
/// of the installed engine. An apply that changes them drops every kept
/// file, and the engine that the apply replaced keeps nothing more.
#[tokio::test]
async fn an_apply_that_changes_the_encryption_drops_the_kept_references() {
    const MIB: u64 = 1024 * 1024;
    let (endpoint, _fake) = crate::storage::fake_s3::start().await;
    let spool_dir = tempfile::tempdir().unwrap();
    let spool =
        crate::deltaglider::spool::SpoolDir::new(spool_dir.path().to_path_buf(), 64 * MIB).unwrap();
    let config = |db_archive: &str| {
        Config::from_yaml_str(&format!(
            "storage:\n  backends:\n  - name: hetzner-fsn1\n    type: s3\n    endpoint: {endpoint}\n    \
             region: us-east-1\n    force_path_style: true\n    access_key_id: a\n    \
             secret_access_key: b\n    allow_local: true\n  default_backend: hetzner-fsn1\n  \
             buckets:\n    releases:\n      backend: hetzner-fsn1\n    db-archive: {db_archive}\n"
        ))
        .unwrap()
    };
    let keeping = |engine: DynEngine| {
        engine
            .with_spool_dir(spool.clone())
            .with_kept_references(8 * MIB)
    };
    let kept = || -> usize {
        std::fs::read_dir(spool_dir.path().join("references"))
            .map(|d| {
                d.filter(|e| {
                    !e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with('.')
                })
                .count()
            })
            .unwrap_or(0)
    };
    // The boot: a plain engine (the routing backend over the wrapper that
    // encrypts over S3), installed.
    let engine = keeping(
        DynEngine::new(&config("{ backend: hetzner-fsn1 }"), None)
            .await
            .unwrap(),
    );
    engine.install_reference_policy();
    engine.create_bucket("releases").await.unwrap();
    let base: Vec<u8> = (0..64 * 1024u32).map(|i| (i * 31 % 251) as u8).collect();
    for i in 0..3u8 {
        let mut data = base.clone();
        data[1000..1016].fill(i);
        let key = format!("v/build-{i}.zip");
        engine
            .store("releases", &key, &data, None, HashMap::new())
            .await
            .unwrap();
    }
    let app = AppState::for_tests(engine);
    let running = app.engine.load_full();
    running.retrieve("releases", "v/build-1.zip").await.unwrap();
    assert_eq!(kept(), 1, "the plain bucket's reference is kept");

    // The apply gives another bucket a key: the settings changed.
    let new_cfg = config(&format!(
        "{{ backend: hetzner-fsn1, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", \
         key_id: db-archive-b }} }}"
    ));
    let new_engine = keeping(
        crate::config_apply::build_engine(&app, &new_cfg)
            .await
            .unwrap(),
    );
    crate::config_apply::install_engine(&app, new_engine, "test apply");
    assert_eq!(kept(), 0, "every kept file went with the old settings");

    // A run that still holds the old engine reads (it writes the reference
    // file again, from its memory), and keeps nothing.
    running.retrieve("releases", "v/build-1.zip").await.unwrap();
    assert_eq!(kept(), 0, "the replaced engine keeps nothing");

    // The installed engine keeps again.
    let current = app.engine.load_full();
    current.retrieve("releases", "v/build-2.zip").await.unwrap();
    assert_eq!(kept(), 1);
}

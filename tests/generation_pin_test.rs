// SPDX-License-Identifier: GPL-3.0-only

//! Finding #1 (2026-07-02 two-week review): a streaming multipart copy must
//! FAIL when the source object changes generation mid-copy, rather than
//! assembling a mixed-generation "frankenobject". This drives the engine
//! directly (no server, no timing) so it is fully deterministic: capture a
//! HEAD, overwrite the object, then a generation-PINNED ranged read against
//! the stale head must error.

use deltaglider_proxy::config::Config;
use deltaglider_proxy::deltaglider::DeltaGliderEngine;
use deltaglider_proxy::storage::FilesystemBackend;
use std::sync::Arc;

#[tokio::test]
async fn pinned_range_read_errors_when_source_changes_generation() {
    let dir = tempfile::tempdir().unwrap();
    let backend = FilesystemBackend::new(dir.path().to_path_buf())
        .await
        .expect("fs backend");
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
    engine.create_bucket("b").await.ok();

    // `.bin` is not delta-eligible → stored passthrough → range-able.
    let gen_a = vec![0xAAu8; 4096];
    engine
        .store("b", "obj.bin", &gen_a, None, Default::default())
        .await
        .expect("store gen A");
    let head_a = engine.head("b", "obj.bin").await.expect("head A");

    // A pinned read against the CURRENT generation succeeds.
    let ok = engine
        .retrieve_stream_range("b", "obj.bin", 0, 1023, Some(&head_a))
        .await
        .expect("pinned read of current gen");
    assert!(ok.is_some(), "range read available for passthrough");

    // Overwrite with a DIFFERENT same-size generation.
    let gen_b = vec![0xBBu8; 4096];
    engine
        .store("b", "obj.bin", &gen_b, None, Default::default())
        .await
        .expect("store gen B");

    // The stale pin now mismatches → the read MUST error (never mix bytes).
    match engine
        .retrieve_stream_range("b", "obj.bin", 0, 1023, Some(&head_a))
        .await
    {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("source changed during copy"),
                "wrong error for changed source: {msg}"
            );
        }
        Ok(_) => panic!("a changed source generation must fail the pinned read"),
    }

    // Unpinned reads are unaffected (the pin is opt-in, copy-path only).
    let unpinned = engine
        .retrieve_stream_range("b", "obj.bin", 0, 1023, None)
        .await
        .expect("unpinned read still works");
    assert!(unpinned.is_some());
}

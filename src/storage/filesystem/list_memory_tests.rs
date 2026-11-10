//! F3 (epic #142): a LIST request holds a bounded number of directory
//! entries in memory, whatever the directory size and the delimiter. The
//! tests count held entries (`note_held`), not wall time or RSS.

use super::*;
use crate::config::Config;
use crate::deltaglider::{DeltaGliderEngine, DynEngine as Engine};
use std::sync::Arc;

/// Entries in the test directory. Far above the scan chunk.
const FILES: usize = 3_000;
/// What a request may hold: one scan chunk of each open directory plus slack.
const HELD_MAX: usize = 1_200;

async fn engine_with_folder(files: usize) -> (tempfile::TempDir, Engine) {
    let tmp = tempfile::tempdir().unwrap();
    let fs = FilesystemBackend::new(tmp.path().to_path_buf())
        .await
        .unwrap();
    let engine: Engine = DeltaGliderEngine::new_with_backend(
        Arc::new(super::super::DynStorageBackend::new_box(fs)),
        &Config::default(),
        None,
    );
    engine.create_bucket("b").await.unwrap();
    let ds = tmp.path().join("b").join("deltaspaces").join("big");
    std::fs::create_dir_all(&ds).unwrap();
    for i in 0..files {
        // Foreign files (no metadata): listed from their stat.
        std::fs::write(ds.join(format!("k_{i:05}.jpg")), b"x").unwrap();
    }
    (tmp, engine)
}

#[tokio::test]
async fn a_list_request_holds_a_bounded_number_of_entries() {
    let (_tmp, engine) = engine_with_folder(FILES).await;
    for (prefix, delimiter) in [
        ("big/", None),
        ("big/", Some("/")),
        ("big/", Some("_")),
        ("", Some("_")),
        ("", Some("/")),
    ] {
        HELD_PEAK.with(|p| p.set(0));
        let page = engine
            .list_objects("b", prefix, delimiter, 1, None, false)
            .await
            .unwrap();
        assert_eq!(
            page.objects.len() + page.common_prefixes.len(),
            1,
            "{prefix:?} {delimiter:?}"
        );
        let held = HELD_PEAK.with(|p| p.get());
        assert!(
            held <= HELD_MAX,
            "{held} entries held for {FILES} files, prefix {prefix:?}, delimiter {delimiter:?}"
        );
    }
}

/// Paging a folder of `FILES` keys through each delimiter lists every key
/// once, in order, and the bound holds on every page and with the refill
/// reached (a small chunk).
#[tokio::test]
async fn paging_a_big_folder_lists_every_key_once_within_the_bound() {
    let (_tmp, engine) = engine_with_folder(FILES).await;
    for chunk in [None, Some(7)] {
        scan::set_chunk_for_test(chunk);
        for delimiter in [None, Some("/"), Some("_"), Some("k")] {
            let mut got: Vec<String> = Vec::new();
            let mut token: Option<String> = None;
            let mut pages = 0;
            loop {
                HELD_PEAK.with(|p| p.set(0));
                let page = engine
                    .list_objects("b", "big/", delimiter, 400, token.as_deref(), false)
                    .await
                    .unwrap();
                let held = HELD_PEAK.with(|p| p.get());
                assert!(
                    held <= HELD_MAX,
                    "{held} held, {delimiter:?}, chunk {chunk:?}"
                );
                got.extend(page.objects.into_iter().map(|(k, _)| k));
                got.extend(page.common_prefixes);
                pages += 1;
                if !page.is_truncated {
                    break;
                }
                token = page.next_continuation_token;
            }
            let want: Vec<String> = match delimiter {
                // `big/k_00000.jpg` has a `k` after the prefix: one prefix.
                Some("k") => vec!["big/k".to_string()],
                Some("_") => vec!["big/k_".to_string()],
                _ => (0..FILES).map(|i| format!("big/k_{i:05}.jpg")).collect(),
            };
            assert_eq!(got, want, "{delimiter:?} chunk {chunk:?}");
            if want.len() == FILES {
                assert_eq!(pages, FILES.div_ceil(400), "{delimiter:?}");
            }
        }
    }
    scan::set_chunk_for_test(None);
}

/// F19 + the class guard of epic #142: for random key sets (a `.dg`
/// directory, Unicode and control characters among them) the recursive
/// delimiter walk lists exactly the flat listing, with any chunk size and
/// page size; the walk with another delimiter finds the same keys; and
/// DeleteBucket succeeds once every listed key is deleted.
mod listing_model {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    /// Segments with the characters that broke listings: a `.dg` directory,
    /// a dot-dir, Unicode, a control character, the delimiter `_`, `-`/`.`
    /// that sort before `/`.
    const SEGS: [&str; 9] = [
        "a",
        "b",
        ".dg",
        ".well-known",
        "é",
        "x\u{1}y",
        "a_b",
        "a-b",
        "a.b",
    ];

    fn keys() -> impl Strategy<Value = Vec<(Vec<usize>, usize)>> {
        proptest::collection::vec(
            (
                proptest::collection::vec(0..SEGS.len(), 0..3),
                0..SEGS.len(),
            ),
            0..14,
        )
    }

    async fn walk(engine: &Engine, prefix: &str, max_keys: u32, out: &mut BTreeSet<String>) {
        let mut token: Option<String> = None;
        loop {
            let page = engine
                .list_objects("b", prefix, Some("/"), max_keys, token.as_deref(), false)
                .await
                .unwrap();
            for (k, _) in page.objects {
                assert!(out.insert(k), "a key listed twice");
            }
            for cp in page.common_prefixes {
                Box::pin(walk(engine, &cp, max_keys, out)).await;
            }
            if !page.is_truncated {
                return;
            }
            token = page.next_continuation_token;
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn the_delimiter_walk_equals_the_flat_listing(
            files in keys(),
            chunk in 1usize..5,
            max_keys in 1u32..5,
        ) {
            scan::set_chunk_for_test(Some(chunk));
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (_tmp, engine) = engine_with_folder(0).await;
                let mut stored = BTreeSet::new();
                for (dirs, file) in &files {
                    let mut parts: Vec<&str> = dirs.iter().map(|&d| SEGS[d]).collect();
                    parts.push(SEGS[*file]);
                    let key = parts.join("/");
                    // A refused key (a reserved name) is not stored: it is
                    // not in the model either.
                    if engine
                        .store("b", &key, b"x", None, Default::default())
                        .await
                        .is_ok()
                    {
                        stored.insert(key);
                    }
                }
                let flat = engine
                    .list_objects("b", "", None, 1000, None, false)
                    .await
                    .unwrap();
                let flat: BTreeSet<String> = flat.objects.into_iter().map(|(k, _)| k).collect();
                prop_assert_eq!(&flat, &stored, "flat listing");
                let mut walked = BTreeSet::new();
                walk(&engine, "", max_keys, &mut walked).await;
                prop_assert_eq!(&walked, &flat, "delimiter walk");
                // Another delimiter finds the same keys: every key is under
                // one common prefix or listed.
                let by_x = engine
                    .list_objects("b", "", Some("y"), 1000, None, false)
                    .await
                    .unwrap();
                for (k, _) in &by_x.objects {
                    prop_assert!(flat.contains(k));
                    prop_assert!(!k.contains('y'));
                }
                for cp in &by_x.common_prefixes {
                    prop_assert!(flat.iter().any(|k| k.starts_with(cp.as_str())), "{cp}");
                }
                // The bucket deletes once the walk sees nothing.
                for key in &walked {
                    engine.delete("b", key).await.unwrap();
                }
                let after = engine
                    .list_objects("b", "", Some("/"), 1000, None, false)
                    .await
                    .unwrap();
                prop_assert!(after.objects.is_empty() && after.common_prefixes.is_empty());
                engine.delete_bucket("b").await.unwrap();
                Ok(())
            })?;
            scan::set_chunk_for_test(None);
        }
    }
}

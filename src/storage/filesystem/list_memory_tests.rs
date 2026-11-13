// SPDX-License-Identifier: BUSL-1.1

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

/// A folder of `n` empty subfolders and one file after them: a LIST with the
/// delimiter `_` drops every subfolder (it holds no listed key) and lists the
/// file, so the page reads all `n` entries to return one.
async fn engine_with_empty_folders(n: usize) -> (tempfile::TempDir, Engine) {
    let (tmp, engine) = engine_with_folder(0).await;
    let ds = tmp.path().join("b").join("deltaspaces").join("big");
    for i in 0..n {
        std::fs::create_dir(ds.join(format!("f_{i:06}"))).unwrap();
    }
    std::fs::write(ds.join("zz.jpg"), b"x").unwrap();
    (tmp, engine)
}

async fn one_page_passes(engine: &Engine) -> usize {
    scan::READ_DIR_PASSES.with(|c| c.set(0));
    let page = engine
        .list_objects("b", "big/", Some("_"), 1, None, false)
        .await
        .unwrap();
    assert_eq!(
        page.objects
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        ["big/zz.jpg"],
        "the empty folders list nothing"
    );
    scan::READ_DIR_PASSES.with(|c| c.get())
}

/// Dropped entries must not make the page quadratic: each refill reads the
/// directory again, so the chunk doubles and the passes are O(log N), not
/// O(N / chunk). With the fixed chunk of 8 this folder took 4,001 / 8 = 501
/// passes.
#[tokio::test]
async fn a_page_that_drops_many_entries_reads_the_directory_a_few_times() {
    const N: usize = 4_000;
    let (_tmp, engine) = engine_with_empty_folders(N).await;
    scan::set_chunk_for_test(Some(8));
    let passes = one_page_passes(&engine).await;
    scan::set_chunk_for_test(None);
    // 8 + 16 + ... + 4096 covers 4,001 entries: 10 passes. Allow slack, far
    // below the 501 of a fixed chunk.
    assert!(
        passes <= 12,
        "{passes} read_dir passes for {N} dropped entries"
    );
}

/// The growth stops at the cap: a request holds at most the cap (+1 for the
/// heap's spare) of one directory, and the passes beyond it are linear in
/// N / cap.
#[tokio::test]
async fn the_chunk_growth_stops_at_the_cap() {
    const N: usize = 1_000;
    let (_tmp, engine) = engine_with_empty_folders(N).await;
    scan::set_chunk_for_test(Some(4));
    scan::set_cap_for_test(Some(64));
    HELD_PEAK.with(|p| p.set(0));
    let passes = one_page_passes(&engine).await;
    let held = HELD_PEAK.with(|p| p.get());
    scan::set_chunk_for_test(None);
    scan::set_cap_for_test(None);
    assert!(held <= 64 + 1, "{held} entries held, cap 64");
    // 4 + 8 + 16 + 32 = 60, then 64 per pass: 1 + 4 + ceil(941 / 64) = 20.
    assert!((15..=25).contains(&passes), "{passes} passes");
}

/// One budget for the whole request. Four nested folders hold 150 keys each
/// (every level is wider than the cap). The walk reads each level in chunks
/// of at most the budget minus what its ancestors hold: the request never
/// holds the cap once per level, and the keys come out in order, page after
/// page, with nothing lost or repeated.
#[tokio::test]
async fn a_nested_wide_tree_holds_one_budget_and_lists_in_order() {
    const LEVELS: usize = 4;
    const PER_LEVEL: usize = 150;
    const BUDGET: usize = 64;
    const FLOOR: usize = 4;
    let (tmp, engine) = engine_with_folder(0).await;
    let mut dir = tmp.path().join("b").join("deltaspaces").join("big");
    let mut want: Vec<String> = Vec::new();
    let mut key_dir = "big".to_string();
    for level in 0..LEVELS {
        for i in 0..PER_LEVEL {
            std::fs::write(dir.join(format!("f{i:03}.jpg")), b"x").unwrap();
            want.push(format!("{key_dir}/f{i:03}.jpg"));
        }
        if level + 1 < LEVELS {
            dir = dir.join("s");
            std::fs::create_dir_all(&dir).unwrap();
            key_dir = format!("{key_dir}/s");
        }
    }
    want.sort();

    scan::set_chunk_for_test(Some(FLOOR));
    scan::set_cap_for_test(Some(BUDGET));
    for max_keys in [40, 1000] {
        let mut got: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        let mut peak = 0;
        loop {
            HELD_PEAK.with(|p| p.set(0));
            let page = engine
                .list_objects("b", "big/", None, max_keys, token.as_deref(), false)
                .await
                .unwrap();
            peak = peak.max(HELD_PEAK.with(|p| p.get()));
            got.extend(page.objects.into_iter().map(|(k, _)| k));
            if !page.is_truncated {
                break;
            }
            token = page.next_continuation_token;
        }
        assert_eq!(got, want, "max_keys {max_keys}");
        // The budget, one floor and the heap's spare entry per level.
        let bound = BUDGET + LEVELS * (FLOOR + 1);
        assert!(peak <= bound, "peak {peak} > {bound}, max_keys {max_keys}");
    }
    scan::set_chunk_for_test(None);
    scan::set_cap_for_test(None);
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
                let (tmp, engine) = engine_with_folder(0).await;
                let mut stored = BTreeSet::new();
                for (dirs, file) in &files {
                    let mut parts: Vec<&str> = dirs.iter().map(|&d| SEGS[d]).collect();
                    parts.push(SEGS[*file]);
                    // A key below a `.dg` directory: the write path refuses
                    // it now; an older release stored it (L1, #171).
                    let legacy = parts[..parts.len() - 1].contains(&".dg");
                    let key = parts.join("/");
                    if engine
                        .store("b", &key, b"x", None, Default::default())
                        .await
                        .is_ok()
                    {
                        stored.insert(key);
                    } else if legacy {
                        // Below the write path's validation, as an older
                        // release left it. A path that clashes with another
                        // key (a file where a folder goes) is skipped.
                        let path = tmp.path().join("b").join("deltaspaces").join(&key);
                        let made = path
                            .parent()
                            .is_some_and(|p| std::fs::create_dir_all(p).is_ok())
                            && !path.exists()
                            && std::fs::write(&path, b"x").is_ok();
                        if made {
                            stored.insert(key);
                        }
                    }
                    // Any other refused key (a reserved name) is not stored:
                    // it is not in the model either.
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
                // L3 (#171): a delimiter other than `/` pages like the model
                // (the flat keys collapsed by hand): no key and no common
                // prefix twice, none missing, at every page size.
                for delimiter in ["_", "-", ".", "b"] {
                    let mut want_objects = BTreeSet::new();
                    let mut want_prefixes = BTreeSet::new();
                    for k in &flat {
                        match k.find(delimiter) {
                            Some(i) => {
                                want_prefixes.insert(format!("{}{delimiter}", &k[..i]));
                            }
                            None => {
                                want_objects.insert(k.clone());
                            }
                        }
                    }
                    let mut objects = Vec::new();
                    let mut prefixes = Vec::new();
                    let mut token: Option<String> = None;
                    for pages in 0.. {
                        // A page that repeats its token never ends.
                        prop_assert!(pages < 200, "paging does not end, delimiter {}", delimiter);
                        let page = engine
                            .list_objects("b", "", Some(delimiter), max_keys, token.as_deref(), false)
                            .await
                            .unwrap();
                        objects.extend(page.objects.into_iter().map(|(k, _)| k));
                        prefixes.extend(page.common_prefixes);
                        if !page.is_truncated {
                            break;
                        }
                        token = page.next_continuation_token;
                    }
                    let got_objects: BTreeSet<String> = objects.iter().cloned().collect();
                    let got_prefixes: BTreeSet<String> = prefixes.iter().cloned().collect();
                    prop_assert_eq!(objects.len(), got_objects.len(), "an object twice, delimiter {}", delimiter);
                    prop_assert_eq!(prefixes.len(), got_prefixes.len(), "a prefix twice, delimiter {}: {:?}", delimiter, prefixes);
                    prop_assert_eq!(got_objects, want_objects, "objects, delimiter {}", delimiter);
                    prop_assert_eq!(got_prefixes, want_prefixes, "prefixes, delimiter {}", delimiter);
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

/// L1 (#171): a key that an older release stored below a `.dg` directory is
/// an ordinary object. Flat, by level and with another delimiter, LIST shows
/// it; a client reads and deletes it; the bucket deletes after that, and not
/// before. The folder with only a baseline stays unlisted.
#[tokio::test]
async fn a_legacy_dg_key_is_listed_readable_and_deletable() {
    let (tmp, engine) = engine_with_folder(0).await;
    let ds = tmp.path().join("b").join("deltaspaces").join("x");
    std::fs::create_dir_all(ds.join(".dg")).unwrap();
    std::fs::write(ds.join(".dg").join("foo.jpg"), b"legacy").unwrap();
    // The baseline of folder `x` shares the directory; `y` has only one.
    std::fs::write(ds.join(".dg").join("reference.bin"), b"ref").unwrap();
    let only_baseline = tmp
        .path()
        .join("b")
        .join("deltaspaces")
        .join("y")
        .join(".dg");
    std::fs::create_dir_all(&only_baseline).unwrap();
    std::fs::write(only_baseline.join("reference.bin"), b"ref").unwrap();

    let names = |page: &crate::deltaglider::ListObjectsPage| {
        let mut v: Vec<String> = page.objects.iter().map(|(k, _)| k.clone()).collect();
        v.extend(page.common_prefixes.iter().cloned());
        v
    };
    let list = |prefix: &'static str, delimiter: Option<&'static str>| {
        let engine = &engine;
        async move {
            engine
                .list_objects("b", prefix, delimiter, 1000, None, false)
                .await
                .unwrap()
        }
    };
    assert_eq!(names(&list("", None).await), ["x/.dg/foo.jpg"]);
    assert_eq!(names(&list("", Some("/")).await), ["x/"]);
    assert_eq!(names(&list("x/", Some("/")).await), ["x/.dg/"]);
    assert_eq!(names(&list("x/.dg/", Some("/")).await), ["x/.dg/foo.jpg"]);
    assert_eq!(names(&list("x/", Some("g")).await), ["x/.dg"]);
    assert_eq!(names(&list("", Some(".")).await), ["x/."]);
    assert_eq!(names(&list("y/", Some("/")).await), Vec::<String>::new());

    assert!(engine.head("b", "x/.dg/foo.jpg").await.is_ok());
    // The bucket holds a key that LIST shows: it does not delete.
    assert!(matches!(
        engine.delete_bucket("b").await,
        Err(crate::deltaglider::EngineError::Storage(
            crate::storage::StorageError::BucketNotEmpty(_)
        ))
    ));
    engine.delete("b", "x/.dg/foo.jpg").await.unwrap();
    assert!(names(&list("", None).await).is_empty());
    // The test baselines have no metadata: the engine cannot reclaim them.
    std::fs::remove_dir_all(ds.join(".dg")).unwrap();
    std::fs::remove_dir_all(&only_baseline).unwrap();
    engine.delete_bucket("b").await.unwrap();
}

/// L3 (#171): the page after a common prefix starts after it. The key
/// `big/a_1.jpg` sorts after the token `big/a_`, and still collapses into the
/// prefix that the token ended.
#[tokio::test]
async fn a_collapsed_page_never_repeats_the_prefix_of_the_token() {
    let (tmp, engine) = engine_with_folder(0).await;
    let ds = tmp.path().join("b").join("deltaspaces").join("big");
    for k in ["a_1.jpg", "a_2.jpg", "a_3.jpg", "b.jpg"] {
        std::fs::write(ds.join(k), b"x").unwrap();
    }
    let mut got = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..10 {
        let page = engine
            .list_objects("b", "big/", Some("_"), 1, token.as_deref(), false)
            .await
            .unwrap();
        got.extend(page.objects.into_iter().map(|(k, _)| k));
        got.extend(page.common_prefixes);
        if !page.is_truncated {
            break;
        }
        token = page.next_continuation_token;
    }
    assert_eq!(got, ["big/a_", "big/b.jpg"]);
}

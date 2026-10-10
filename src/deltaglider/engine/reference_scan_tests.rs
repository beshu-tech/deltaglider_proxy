// SPDX-License-Identifier: BUSL-1.1

//! `list_deltaspace_references` (the reference half of the savings chip, the
//! dashboard scan and `stats`): it must find the references of a scope from
//! a listing of that scope, and read metadata only where a reference is.
//! On prod (Hetzner) a savings request for a folder of screenshots listed
//! the whole bucket and then sent one HEAD per directory: hundreds of 404s
//! that dragged every other request of the backend.

use super::*;
use crate::config::Config;
use crate::storage::{fake_s3, s3_test_support, FilesystemBackend, StorageBackend};
use crate::types::FileMetadata;
use md5::{Digest, Md5};
use sha2::Sha256;

const SCOPE: &str = "ror/e2e_reports/";
const REF_DS: &str = "ror/e2e_reports/builds";
const REF_BODY: &[u8] = b"the baseline of builds";
const PNG: &[u8] = b"\x89PNG screenshot";

fn passthrough(name: &str, body: &[u8]) -> FileMetadata {
    FileMetadata::new_passthrough(
        name.into(),
        hex::encode(Sha256::digest(body)),
        hex::encode(Md5::digest(body)),
        body.len() as u64,
        None,
    )
}

fn reference(body: &[u8]) -> FileMetadata {
    FileMetadata::new_reference(
        "__reference__".into(),
        "app.zip".into(),
        hex::encode(Sha256::digest(body)),
        hex::encode(Md5::digest(body)),
        body.len() as u64,
        None,
    )
}

async fn put_ref<B: StorageBackend>(b: &B, prefix: &str, body: &[u8]) {
    b.put_reference(
        "b",
        prefix,
        body,
        &reference(body),
        crate::deltaglider::RefWriteProof::for_tests(),
    )
    .await
    .unwrap();
}

/// `dirs` screenshot folders in the scope (passthrough PNGs, no reference),
/// one deltaspace with a reference in the scope, and a reference and a
/// file outside it.
async fn seed<B: StorageBackend>(b: &B, dirs: usize) {
    for i in 0..dirs {
        let prefix = format!("{SCOPE}run{i}/shots");
        b.put_passthrough("b", &prefix, "a.png", PNG, &passthrough("a.png", PNG))
            .await
            .unwrap();
    }
    put_ref(b, REF_DS, REF_BODY).await;
    put_ref(b, "other/lib", b"another baseline").await;
    b.put_passthrough("b", "other", "x.png", PNG, &passthrough("x.png", PNG))
        .await
        .unwrap();
}

fn found(scan: &ReferenceScan) -> Vec<(String, u64)> {
    scan.references
        .iter()
        .map(|(p, m)| {
            assert!(
                matches!(m.storage_info, crate::types::StorageInfo::Reference { .. }),
                "{p}: {m:?}"
            );
            (p.clone(), m.file_size)
        })
        .collect()
}

/// The prod shape on S3: many directories without a reference and one with
/// a reference under the scope. The scan lists the scope only and sends one
/// HEAD, for the reference it found.
#[tokio::test]
async fn a_scoped_scan_lists_the_scope_and_heads_only_the_references() {
    let (ep, fake) = fake_s3::start().await;
    let s3 = s3_test_support::for_test_endpoint(&ep);
    seed(&s3, 30).await;
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(s3), &Config::default(), None);
    fake.clear();

    let scan = engine
        .list_deltaspace_references("b", SCOPE, Some(REFERENCE_SCAN_LIMIT))
        .await
        .unwrap();

    assert_eq!(
        found(&scan),
        vec![(REF_DS.to_string(), REF_BODY.len() as u64)]
    );
    assert!(!scan.truncated);
    // The background listing-facts writer works under `.dg/`; only the
    // scan's own requests count.
    let requests: Vec<String> = fake
        .requests()
        .into_iter()
        .filter(|r| !r.contains("/b/.dg/") && !fake_s3::list_prefix(r).starts_with(".dg/"))
        .collect();
    let heads: Vec<&String> = requests.iter().filter(|r| r.starts_with("HEAD ")).collect();
    assert_eq!(
        heads,
        vec![&format!("HEAD /b/{REF_DS}/reference.bin")],
        "one HEAD, for the one reference; none for a directory without one"
    );
    let lists: Vec<&String> = requests
        .iter()
        .filter(|r| r.contains("list-type=2"))
        .collect();
    assert!(
        !lists.is_empty() && lists.iter().all(|r| fake_s3::list_prefix(r) == SCOPE),
        "the scan must list the scope `{SCOPE}` only, not the whole bucket: {lists:?}"
    );
}

/// The cap counts references found, in prefix order; `truncated` says that
/// more references remain, not that more directories do.
#[tokio::test]
async fn the_cap_counts_references_and_truncated_means_more_references() {
    let (ep, _fake) = fake_s3::start().await;
    let s3 = s3_test_support::for_test_endpoint(&ep);
    seed(&s3, 5).await;
    put_ref(&s3, &format!("{SCOPE}a"), b"a").await;
    put_ref(&s3, &format!("{SCOPE}c/d"), b"cd").await;
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(s3), &Config::default(), None);

    let two = engine
        .list_deltaspace_references("b", SCOPE, Some(2))
        .await
        .unwrap();
    assert_eq!(
        found(&two),
        vec![
            (format!("{SCOPE}a"), 1),
            (REF_DS.to_string(), REF_BODY.len() as u64)
        ]
    );
    assert!(two.truncated, "a third reference remains");

    let three = engine
        .list_deltaspace_references("b", SCOPE, Some(3))
        .await
        .unwrap();
    assert_eq!(three.references.len(), 3);
    assert!(
        !three.truncated,
        "every reference is in; the screenshot folders are not references"
    );
}

/// The old walk, as the oracle: every deltaspace of the bucket, the scope
/// rule, then each one's reference metadata, skipping the ones without.
async fn directory_walk<S: StorageBackend>(
    engine: &DeltaGliderEngine<S>,
    scope: &str,
) -> Vec<(String, u64)> {
    let scope = scope.trim_end_matches('/');
    let mut out = Vec::new();
    for p in engine.storage().list_deltaspaces("b").await.unwrap() {
        let in_scope = scope.is_empty() || p == scope || p.starts_with(&format!("{scope}/"));
        if !in_scope {
            continue;
        }
        if let Ok(m) = engine.storage().get_reference_metadata("b", &p).await {
            out.push((p, m.file_size));
        }
    }
    out.sort();
    out
}

/// The filesystem backend finds what the old directory walk found, for a
/// mixed tree and every shape of scope.
#[tokio::test]
async fn filesystem_scan_matches_the_directory_walk_for_every_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let fs = FilesystemBackend::new(tmp.path().to_path_buf())
        .await
        .unwrap();
    fs.create_bucket("b").await.unwrap();
    seed(&fs, 5).await;
    put_ref(&fs, "", b"root").await;
    put_ref(&fs, &format!("{REF_DS}/nightly"), b"nightly").await;
    put_ref(&fs, "ror/e2e_reports_old", b"old").await;
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(fs), &Config::default(), None);

    let all = engine
        .list_deltaspace_references("b", "", None)
        .await
        .unwrap();
    assert_eq!(
        found(&all).into_iter().map(|(p, _)| p).collect::<Vec<_>>(),
        vec![
            "",
            "other/lib",
            REF_DS,
            "ror/e2e_reports/builds/nightly",
            "ror/e2e_reports_old"
        ],
        "the oracle below must not be vacuous"
    );
    for scope in [
        "",
        "/",
        "ror",
        "ror/",
        "ror/e2e_reports",
        SCOPE,
        "ror/e2e",
        REF_DS,
        "ror/e2e_reports/builds/",
        "ror/e2e_reports/run0/shots",
        "missing/",
        "other/x.png",
        "ror/../other",
    ] {
        let scan = engine
            .list_deltaspace_references("b", scope, None)
            .await
            .unwrap();
        assert_eq!(
            found(&scan),
            directory_walk(&engine, scope).await,
            "scope {scope:?}"
        );
        assert!(!scan.truncated);
    }
}

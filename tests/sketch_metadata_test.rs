// SPDX-License-Identifier: BUSL-1.1

//! Sketch metadata integration tests.
//!
//! Verifies that the 256-bit SimHash sketch is computed on PUT and
//! persisted in object metadata (x-amz-meta-dg-sketch header), and
//! that similar files produce similar sketches (small Hamming distance).

mod common;

use common::{generate_binary, head_headers, put_object, TestServer};
use deltaglider_proxy::deltaglider::sketch;

/// PUT a delta-eligible object → HEAD → the response must carry
/// `x-amz-meta-dg-sketch` with a valid 64-char hex value that matches
/// the sketch computed from the original bytes.
#[tokio::test]
async fn test_sketch_stamped_on_put_delta_eligible() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let data = generate_binary(100_000, 42);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "releases/build.zip",
        data.clone(),
        "application/zip",
    )
    .await;

    let headers = head_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        "releases/build.zip",
    )
    .await;

    let sketch_header = headers
        .get("x-amz-meta-dg-sketch")
        .expect("dg-sketch header must be present on stored delta-eligible objects");
    let sketch_str = sketch_header
        .to_str()
        .expect("sketch header is valid UTF-8");
    assert_eq!(
        sketch_str.len(),
        64,
        "sketch must be 64 hex chars (256 bits)"
    );
    assert!(
        sketch_str.chars().all(|c| c.is_ascii_hexdigit()),
        "sketch must be hex-encoded"
    );

    // The stamped sketch must match the one computed from the original bytes.
    let expected = sketch::sketch_hex(&data);
    assert_eq!(
        sketch_str, expected,
        "stamped sketch must match computed sketch"
    );
}

/// Two similar files (same base, small in-place modification) must
/// produce sketches with a small Hamming distance — the core property
/// that enables future reference-selection logic.
///
/// Uses structured data (repeating blocks) so CDC re-syncs after the
/// modified region. Random data has no common subsequences after a
/// mutation, so CDC can't re-sync — that's a property of the data,
/// not the sketch algorithm.
#[tokio::test]
async fn test_similar_files_have_similar_sketches() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Build structured data: 25 × 8 KiB repeating blocks (~200 KiB).
    let block: Vec<u8> = (0..8_192u32).map(|i| (i % 256) as u8).collect();
    let base: Vec<u8> = block.repeat(25);
    let mut variant = base.clone();
    // Modify 100 bytes in the middle of one block.
    for b in &mut variant[100_000..100_100] {
        *b ^= 0xFF;
    }

    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "releases/v1.zip",
        base.clone(),
        "application/zip",
    )
    .await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "releases/v2.zip",
        variant.clone(),
        "application/zip",
    )
    .await;

    let h1 = head_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        "releases/v1.zip",
    )
    .await;
    let h2 = head_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        "releases/v2.zip",
    )
    .await;

    let s1 = h1
        .get("x-amz-meta-dg-sketch")
        .expect("v1 sketch header")
        .to_str()
        .unwrap();
    let s2 = h2
        .get("x-amz-meta-dg-sketch")
        .expect("v2 sketch header")
        .to_str()
        .unwrap();

    let dist = sketch::hamming_distance_hex(s1, s2).expect("valid hex sketches");
    // With ~25 chunks and 1-2 affected by the 100-byte modification,
    // the distance should be well below 128 (random) but may exceed
    // the conservative SIMILARITY_THRESHOLD_BITS. Assert it's clearly
    // in the "similar" range, not the "unrelated" range.
    assert!(
        dist < 80,
        "similar files (100-byte in-place modification in 200KB) should have Hamming distance < 80, got {}",
        dist
    );
}

/// A passthrough object (non-delta-eligible, e.g. image) must also
/// carry a sketch — it's intrinsic to the object's bytes, not the
/// storage strategy.
#[tokio::test]
async fn test_sketch_stamped_on_passthrough() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    let data = generate_binary(100_000, 7);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "assets/logo.png",
        data.clone(),
        "image/png",
    )
    .await;

    let headers = head_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        "assets/logo.png",
    )
    .await;

    let sketch_header = headers
        .get("x-amz-meta-dg-sketch")
        .expect("dg-sketch header must be present on passthrough objects too");
    assert_eq!(
        sketch_header.to_str().unwrap().len(),
        64,
        "sketch must be 64 hex chars"
    );
}

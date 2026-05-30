// SPDX-License-Identifier: GPL-3.0-only

//! Criterion benchmarks for the delta-codec hot paths.
//!
//! Establishes a regression guard around the two perf-sensitive operations the
//! engine performs per request: `DeltaCodec::encode` (PUT pipeline) and
//! `DeltaCodec::decode` (GET reconstruction). Both shell out to the `xdelta3`
//! CLI subprocess (see `src/deltaglider/codec.rs`), so these numbers capture
//! the real subprocess + pipe overhead — the cost the project has historically
//! asserted is "acceptable" rather than measured.
//!
//! Run locally / manually (NOT part of the CI merge gate):
//!
//! ```bash
//! cargo bench --bench codec
//! ```
//!
//! If `xdelta3` is not on PATH the benches print a notice and no-op instead of
//! panicking, so `cargo bench` stays green on machines without the binary.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use deltaglider_proxy::deltaglider::DeltaCodec;
use std::hint::black_box;

/// Representative reference/target sizes. Small enough to keep the bench fast,
/// large enough that the xdelta3 sliding-window work dominates fixed subprocess
/// overhead at the upper end.
const SIZES: &[(&str, usize)] = &[("64KiB", 64 * 1024), ("1MiB", 1024 * 1024)];

/// Build a deterministic pseudo-random source buffer plus a target that is a
/// small mutation of it. This mirrors the real delta workload: a new artifact
/// version that differs from the reference baseline in a handful of regions, so
/// the resulting delta is small and the encoder does meaningful matching work.
fn make_source_and_target(size: usize) -> (Vec<u8>, Vec<u8>) {
    // LCG — same constants used in the codec unit tests for incompressible data.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut source = Vec::with_capacity(size);
    for _ in 0..size {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        source.push((state >> 33) as u8);
    }

    // Target = source with ~0.1% of bytes mutated, spread across the buffer.
    let mut target = source.clone();
    let stride = (size / 1000).max(1);
    for i in (0..target.len()).step_by(stride) {
        target[i] = target[i].wrapping_add(17);
    }

    (source, target)
}

fn bench_encode(c: &mut Criterion) {
    let codec = DeltaCodec::default();
    if !codec.is_cli_available() {
        eprintln!(
            "[codec bench] skipping encode: xdelta3 CLI not available on PATH \
             — install xdelta3 to benchmark the delta hot paths"
        );
        return;
    }

    let mut group = c.benchmark_group("codec/encode");
    for &(label, size) in SIZES {
        // Pre-generate inputs OUTSIDE the timed closure — b.iter must time
        // only the encode call, not data synthesis.
        let (source, target) = make_source_and_target(size);
        group.throughput(Throughput::Bytes(target.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(source, target),
            |b, (source, target)| {
                b.iter(|| {
                    let delta = codec
                        .encode(black_box(source), black_box(target))
                        .expect("encode should succeed");
                    black_box(delta);
                });
            },
        );
    }
    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let codec = DeltaCodec::default();
    if !codec.is_cli_available() {
        eprintln!(
            "[codec bench] skipping decode: xdelta3 CLI not available on PATH \
             — install xdelta3 to benchmark the delta hot paths"
        );
        return;
    }

    let mut group = c.benchmark_group("codec/decode");
    for &(label, size) in SIZES {
        // Pre-compute source + delta outside the timed closure — decode timing
        // must exclude both data synthesis and the encode step.
        let (source, target) = make_source_and_target(size);
        let delta = codec
            .encode(&source, &target)
            .expect("encode for decode setup should succeed");
        group.throughput(Throughput::Bytes(target.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(source, delta),
            |b, (source, delta)| {
                b.iter(|| {
                    let reconstructed = codec
                        .decode(black_box(source), black_box(delta))
                        .expect("decode should succeed");
                    black_box(reconstructed);
                });
            },
        );
    }
    group.finish();
}

criterion_group! {
    name = benches;
    // Tighter defaults than criterion's 3s warm-up / 5s measure: subprocess
    // benches are slow, and this is a local signal, not a precision instrument.
    // CLI flags (--warm-up-time / --measurement-time) still override these.
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    targets = bench_encode, bench_decode
}
criterion_main!(benches);

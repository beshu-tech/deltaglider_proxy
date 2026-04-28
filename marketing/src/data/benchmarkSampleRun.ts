/**
 * Snapshot from repo harness output: docs/benchmark/results/hetzner-20260428-140514Z.tgz
 * (Alpine virt ISOs ×5, single-VM smoke, --no-proxy-restart). Update when publishing a new canonical run.
 */
export const BENCHMARK_SAMPLE_META = {
  runId: 'hetzner-20260428-140514Z',
  profile: 'Hetzner Cloud hel1 · ccx33 single-VM · Docker proxy · Alpine virt ISO 3.19.x × 5',
} as const;

/** Mode labels matching benchmark HTML report short names */
export const BENCHMARK_MODE_LABELS = [
  'Passthrough',
  'Compression',
  'Encryption',
  'Comp + encrypt',
] as const;

/** Wall-clock MB/s from summary.json mb_s_wall */
export const BENCHMARK_THROUGHPUT_MBPS = {
  put: [105.57, 8.88, 94.32, 8.73],
  cold_get: [97.05, 89.38, 299.9, 85.15],
  warm_get: [90.13, 91.72, 280.56, 93.11],
} as const;

/** Logical upload size (same artifacts every mode) + implied stored after Δ saved (Prometheus) */
export const BENCHMARK_STORAGE_GB = {
  logical: 0.316669952,
  implied_stored: [0.316669952, 0.123753516, 0.316669952, 0.123753516],
} as const;

/** Docker CPU % mean / max per mode window (resource_timeseries rollup) */
export const BENCHMARK_DOCKER_CPU_PCT = {
  mean: [120.07, 98.47, 80.1, 98.23],
  max: [142.22, 101.13, 80.34, 100.63],
} as const;

/** Derived headline stats for narrative copy (same pinned run). */
export const BENCHMARK_NARRATIVE = {
  /** PUT wall MB/s passthrough vs compression */
  putPassthroughMbS: 105.57,
  putCompressionMbS: 8.88,
  /** Approx. ratio passthrough PUT / compression PUT */
  putPassthroughVsCompressionRatio: 105.57 / 8.88,
  /** Cold GET MB/s encryption (proxy AES path still serves plaintext to client) */
  coldGetEncryptionMbS: 299.9,
  coldGetPassthroughMbS: 97.05,
  /** Implied stored GB vs logical for compression modes (~Δ saved vs logical upload) */
  impliedStoredCompressionGb: 0.123753516,
  logicalGb: 0.316669952,
  /** Approx. share of logical bytes NOT stored after compression (delta win) */
  storageReductionVsLogicalPct:
    (1 - 0.123753516 / 0.316669952) * 100,
} as const;

# DeltaGlider compression tax benchmark report

## Summary

Run ID:

Date:

Region/location:

Client VM:

Proxy VM:

Backend:

Dataset:

Total original bytes:

## Headline result

| Mode | PUT MB/s | PUT factor | Cold GET MB/s | Warm GET MB/s | Warm GET factor | Stored/original | Verdict |
|---|---:|---:|---:|---:|---:|---:|---|
| Passthrough | | 1.00x | | | 1.00x | 1.00x | Baseline |
| Compression | | | | | | | |
| Encryption | | | | | | | |
| Compression + encryption | | | | | | | |

## Operator interpretation

### Passthrough

What this proves:

### Compression only

What this proves:

### Encryption only

What this proves:

### Compression + encryption

What this proves:

## Dataset

List artifact family, source URLs, count, total size, and why it is realistic.

| Artifact | Bytes | SHA-256 | Source |
|---|---:|---|---|
| | | | |

## Raw benchmark numbers

### Sequential

| Mode | Phase | Ops | Total bytes | Total seconds | MB/s | p50 ms | p95 ms | p99 ms | Failed ops |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|

### Concurrency 4

| Mode | Phase | Ops | Total bytes | Total seconds | MB/s | p50 ms | p95 ms | p99 ms | Failed ops |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|

## Proxy metrics

Include before/after deltas for:

- `deltaglider_delta_bytes_saved_total`
- `deltaglider_delta_compression_ratio`
- `deltaglider_delta_encode_duration_seconds`
- `deltaglider_delta_decode_duration_seconds`
- `deltaglider_delta_decisions_total`
- `deltaglider_cache_hits_total`
- `deltaglider_cache_misses_total`
- `deltaglider_cache_miss_rate_ratio`

## Host metrics

Client:

Proxy:

CPU peak:

RSS peak:

Network notes:

## Cold vs warm GET

Explain whether reference-cache warmth materially changes GET throughput.

## Negative controls

If a dataset did not compress well, document it here. This is product honesty,
not a failure.

## Final verdict

Use the compact phrasing:

```text
Compression + encryption: PUT __x, warm GET __x, stored/original __x.
<Fit / not fit> for <workload>.
```

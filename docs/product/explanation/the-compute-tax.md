# The compute tax

Delta compression and encryption both cost CPU time, and the first question an engineer asks is how much of that cost shows up in request latency. This page gives the measured answer, explains where the cost lands and why, and shows how to measure it on your own data.

## The measured numbers

The repository carries a reproducible benchmark harness ([`docs/benchmark/`](https://github.com/beshu-tech/deltaglider_proxy/tree/main/docs/benchmark)) that pushes the same artifact set through four proxy modes — passthrough, delta compression, encryption, and both together — against the same S3 backend. The numbers below come from a run in April 2026 on a Hetzner Cloud VM (8 vCPU), with the client, the proxy, and the backend all in one region so the network does not dominate the measurement. The artifacts were consecutive Alpine Linux ISO releases, about 60 MB each — a realistic versioned-artifact workload. Concurrency was 1, so every number is a single-stream worst case.

| Mode | PUT | Cold GET | Warm GET |
|---|---|---|---|
| Passthrough (baseline) | 106 MB/s | 97 MB/s | 90 MB/s |
| Encryption only | 94 MB/s | no slowdown | no slowdown |
| Delta compression | 9 MB/s | 89 MB/s | no slowdown |
| Compression + encryption | 9 MB/s | 85 MB/s | no slowdown |

## Where the tax lands, and why

**The delta tax lands almost entirely on writes.** When a delta-eligible file arrives, the proxy runs xdelta3 at maximum compression against the prefix's reference file. That encode is the expensive step: single-stream upload throughput drops from about 106 MB/s to about 9 MB/s — roughly twelve times slower, so a 100 MB artifact that passed through in one second takes around eleven. The encode is single-threaded per object; concurrent uploads run their encodes on separate cores, so a CI pipeline pushing several artifacts in parallel scales across the machine.

This asymmetry is deliberate. A versioned-artifact workload writes each object once — usually from a CI job that nobody is sitting in front of — and the write is the only place the proxy spends real compute. In exchange, the stored footprint shrinks by whatever your data allows (the [delta compression page](delta-compression.md) explains what compresses well).

**Reads — the path a human actually waits on — are nearly untaxed.** A cold read of a delta object must rebuild the original from the reference plus the delta, and the proxy verifies the result against the SHA-256 recorded at upload *before* the first byte is sent. In the measured run that added about 30% to the time to first byte (a median of 0.6 seconds became 0.8 seconds) and cost about 8% in throughput. Once the prefix's reference is warm in the proxy's cache, there is no measurable difference from passthrough at all.

**Encryption is effectively free.** AES-256-GCM runs at multiple gigabytes per second on any CPU with hardware AES support, so the network is always the slower party. The measured cost was about 11% on writes and nothing on reads.

## What pays no tax at all

The file router sends already-compressed formats — images, video, archives of random data — straight to passthrough. Those requests never touch xdelta3 and pay nothing. The tax applies only to files the proxy actually tries to delta-encode.

One caveat worth knowing: the first request that touches a prefix on a freshly started proxy has to download that prefix's reference file from the backend before it can encode or decode anything. That is a one-time cost per prefix at backend speed; every later request finds the reference in the local cache.

## Measuring it on your own data

Two tools ship with the project:

- **The Delta Efficiency Panel** in the admin UI (`/_/admin/diagnostics/delta-efficiency`) scans a prefix of your real data and reports the compression ratio it would achieve — the savings side of the trade.
- **The benchmark harness** in [`docs/benchmark/`](https://github.com/beshu-tech/deltaglider_proxy/tree/main/docs/benchmark) measures the speed side: it runs your artifact set through all four modes and produces the same per-phase report quoted above.

Numbers move with artifact size, similarity, and hardware, so treat the table above as the shape of the trade-off rather than a guarantee: writes pay once, reads stay fast, encryption is free.

## Related

- [How delta compression works](delta-compression.md) — the mechanism the write tax pays for
- [About encryption at rest](encryption-at-rest.md) — what the encryption modes do
- [DeltaGlider compression vs. S3 Object Versioning](versioning-vs-s3-versioning.md) — a different trade-off entirely

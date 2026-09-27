# Capacity planning and hardware sizing

This page describes the resource profile of the proxy and how to size CPU, memory, and disk before a high-throughput deployment. DeltaGlider changes the payload: it runs an xdelta3 encode on a write and a reconstruction on a read. For this reason, its resource profile differs from that of a pass-through proxy. Choose the values of the settings below from your object sizes and your concurrency.

## The resource model in one paragraph

A request is cheap until it touches a delta. Passthrough reads (already-compressed media, anything on a non-delta prefix) stream through in constant memory and cost almost nothing beyond the network. Delta writes and reads use the CPU, the RAM and the spool disk, because encoding and reconstruction shell out to the `xdelta3` binary. A delta object of `DGP_SPOOL_THRESHOLD_BYTES` (16 MiB by default) or less is encoded and reconstructed in memory. A larger one goes through files in the spool directory (`DGP_SPOOL_DIR`), so its memory stays bounded and it uses disk instead. Sizing therefore depends on two things: how big your delta-eligible objects are, and how many of them you process at once. It does not depend on the raw request rate.

## CPU

Most of the CPU cost is the `xdelta3` subprocess. The proxy runs it once for each delta encode (on PUT) and once for each delta reconstruction (on a cold GET). xdelta3 typically processes a 100 MB object in under five seconds. This is still CPU work that a byte-copy proxy does not do.

- `DGP_CODEC_CONCURRENCY` limits the concurrency (default: `num_cpus × 4`, minimum 16). The default is high because xdelta3 decode is fast, so the bottleneck is I/O and not CPU. The setting caps how many encode and decode operations run at once, so the subprocesses cannot oversubscribe the host. A GET beyond the cap waits up to 60 seconds for a codec slot. A PUT beyond the cap fails at once with `503 SlowDown`, and S3 clients retry it.
- As a rule of thumb, size the cores for your peak concurrent delta operations, not for your total request rate. A workload of 90% passthrough reads and 10% delta writes needs far fewer cores than its request count suggests.
- The proxy kills a hung subprocess after `DGP_CODEC_TIMEOUT_SECS` (default 60s), so that it cannot hold a slot permanently.

If you are CPU-bound, mark the prefixes that hold incompressible data as passthrough, so that they skip xdelta3 entirely. This change usually helps more than more cores.

## Memory

Memory is easy to underestimate, because it scales with object size × concurrency and not with the size of the dataset.

- **Passthrough reads:** these reads stream, so they use constant memory.
- **Single-request uploads (`PutObject`):** the proxy reads the whole request body into memory before it stores it, because it checks the SigV4 payload hash and the `Content-MD5` of the complete body. For this reason, one upload holds up to its own size in RAM, for passthrough and delta objects alike, and the cap is `DGP_MAX_OBJECT_SIZE`. Plan for roughly `N × object` of transient RAM for N concurrent uploads. An `UploadPart` request is read into memory in the same way, up to the size of its part.
- **Delta reads (reconstruction) up to `DGP_SPOOL_THRESHOLD_BYTES`** (default 16 MiB, or `max_object_size` when that is smaller): xdelta3 needs the reference baseline and the output object in RAM at once. Peak working memory for one such GET is on the order of the object size plus the reference. With N concurrent delta GETs of such objects, plan for roughly `N × (object + reference)` of transient RAM on top of the baseline footprint.
- **Delta reads larger than `DGP_SPOOL_THRESHOLD_BYTES`:** the proxy copies the reference to a spool file, reconstructs the object into a second spool file, checks its SHA-256, and then streams the file. xdelta3 maps the reference file instead of reading it into the heap, so the memory stays bounded whatever the object size. Each such GET reserves `object + reference` of the spool budget (`DGP_SPOOL_MAX_BYTES`) while it runs, and range reads of the same object share one reconstruction for `DGP_RANGE_SPOOL_TTL_SECS` (default 60 s).
- **Multipart uploads:** an upload that can become a delta keeps its parts in memory until it holds more than 64 MiB (`DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES`), and then moves them to relay files in the spool. An upload that cannot become a delta writes every part to the spool from the first part. One upload can hold at most `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` of spool space (default half of `DGP_SPOOL_MAX_BYTES`).
- **Reference cache (`DGP_CACHE_MB`, default 100 MB):** an LRU that keeps hot baselines in memory so only the first cold read of a deltaspace pays a backend round-trip. Raising it trades RAM for fewer backend fetches on read-heavy workloads; it does not change the per-request buffering cost.
- **Metadata cache (`DGP_METADATA_CACHE_MB`, default 50 MB):** caches per-object `FileMetadata` for HEAD/GET/LIST. It is small and bounded.
- **Listing-size cache (`DGP_LIST_SIZE_CACHE_MB`, default 32 MB):** remembers the original size and ETag of each stored delta or ciphertext that this proxy wrote, read, or listed. A listing page that finds all its objects in this cache sends no extra request. For the other objects, the proxy reads the listing facts index on the backend with one listing request per page (see [the delta-compression explainer](../explanation/delta-compression.md)). An entry takes about 275 bytes for a 60-byte object key (the entry holds the endpoint, the bucket, the key, both ETags and the size), so the default holds about 120,000 objects.

As a worked example, suppose that your delta-eligible objects average 40 MB and you expect up to 16 concurrent delta reads at peak. These objects are larger than the default spool threshold of 16 MiB, so they reconstruct to spool files: the reads need roughly `16 × (40 MB + reference)` ≈ 1.3 GB of spool disk, and little extra RAM. If you raise `DGP_SPOOL_THRESHOLD_BYTES` above 40 MB, the same reads reconstruct in memory and need about 1.3 GB of transient RAM instead. Lowering codec concurrency caps either number. Lowering `DGP_MAX_OBJECT_SIZE` does not make a large object passthrough: the proxy refuses an upload larger than `DGP_MAX_OBJECT_SIZE` with `413`.

[The streaming-versus-buffering section of the delta-compression explainer](../explanation/delta-compression.md#the-trade-off-streaming-versus-buffering) describes the trade-off in detail. It also says when to disable compression on a bucket.

## Disk

The proxy holds metadata and routing; the bytes live on your backends. Disk needs are modest and depend on backend choice:

- **Filesystem-backed buckets** store baselines and `.delta` files on the disk of the proxy host. Size this disk for the compressed footprint of those buckets, which is a fraction of the logical size, plus headroom for in-flight uploads.
- **S3/S3-compatible backends** stream to the provider; the proxy keeps no permanent copy. Local disk then holds only the OS, the binary, the encrypted IAM DB (`deltaglider_config.db`), and the spool directory.
- **The spool directory** (`DGP_SPOOL_DIR`, default `<system temp>/dgp-spool`) holds every scratch file: large delta reconstructions and encodes, multipart relay parts and the temporary files of encrypted uploads. `DGP_SPOOL_MAX_BYTES` (default 16 GiB) caps its use, and the directory must be writable. Put it on a disk with at least that much free space. A request that needs spool space and finds none free waits up to `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS` (default 120 s) and then fails with `503 SlowDown`.
- **Migrations (Route 2)** read each source object once and write it through the proxy. Budget for transient bandwidth during the copy and, for filesystem backends, for transient disk. See [how migration works](../explanation/how-migration-works.md).

## Request concurrency and the front door

`DGP_MAX_CONCURRENT_REQUESTS` (a tower concurrency limit, default 1024) caps the whole HTTP surface, independent of the codec pool. It is the ceiling on simultaneous in-flight requests of any kind, and excess requests wait. Multipart uploads have their own caps (`DGP_MAX_MULTIPART_UPLOADS`, and a total-bytes ceiling). See [rate limits and concurrency](rate-limits.md) for the full reference of the protection layers.

## Sizing checklist

Before production, decide each of these from your workload, not the defaults:

| Lever | Env var | Default | Size it from |
|---|---|---|---|
| Codec concurrency | `DGP_CODEC_CONCURRENCY` | num_cpus × 4 (min 16) | Peak concurrent delta ops |
| Max object size | `DGP_MAX_OBJECT_SIZE` | 100 MB | Largest object that clients upload (bigger uploads are refused with `413`) |
| Spool threshold | `DGP_SPOOL_THRESHOLD_BYTES` | 16 MiB | Delta objects above it use spool disk instead of RAM |
| Spool budget | `DGP_SPOOL_MAX_BYTES` | 16 GiB | Concurrent large delta operations and multipart uploads |
| Reference cache | `DGP_CACHE_MB` | 100 MB | Number/size of hot baselines, read-heaviness |
| Metadata cache | `DGP_METADATA_CACHE_MB` | 50 MB | LIST/HEAD volume |
| Listing-size cache | `DGP_LIST_SIZE_CACHE_MB` | 32 MB | Number of delta objects that listings report |
| HTTP concurrency ceiling | `DGP_MAX_CONCURRENT_REQUESTS` | 1024 | Peak in-flight requests |

A reasonable starting point for a single instance with moderate throughput is 4 to 8 cores and 4 GB RAM, with `DGP_CODEC_CONCURRENCY` and the caps left at their defaults. Then watch the [Prometheus metrics](metrics.md) (codec timings, queue depth, cache hit rate) under real load, and adjust the settings. Scale out with multiple instances behind a load balancer (each stateless on the data path; share IAM via [config sync](../how-to/run-multiple-instances.md)) rather than scaling a single box indefinitely.

## Related

- [How delta compression works](../explanation/delta-compression.md): the streaming-vs-buffering trade-off these numbers come from.
- [Rate limits and concurrency](rate-limits.md): every protection-layer limit and its override.
- [Metrics](metrics.md): the Prometheus signals to watch under load.
- [Configuration](configuration.md): every field and env var in one place.
- [How to run multiple instances (HA)](../how-to/run-multiple-instances.md): scaling out instead of up.

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
- **Single-request uploads (`PutObject`, and `CopyObject` through the store):** the proxy checks the payload hash, the checksums and `Content-MD5` while the body arrives, so it never needs the whole body in memory. A body of at most `DGP_SPOOL_THRESHOLD_BYTES` goes into memory while the memory budget for upload bodies (`DGP_INGEST_MEMORY_MAX_BYTES`, 1 GiB by default) has room for it, and every other body goes into a spool file. The RAM that uploads use is therefore at most `DGP_INGEST_MEMORY_MAX_BYTES`, plus fixed buffers of about 1 MiB for each upload that writes to the spool. In a bucket that the proxy encrypts, the proxy encrypts a body in memory into a second buffer of about the same size, and the budget does not count that buffer. So the RAM bound of uploads to such buckets is about twice `DGP_INGEST_MEMORY_MAX_BYTES`. Concurrent large uploads use spool DISK instead: plan for roughly `N × object` of spool space for N concurrent uploads larger than the threshold, and twice that for buckets that the proxy encrypts, because the encrypted copy of the body is written to the spool too. An `UploadPart` request is never read into memory: its body goes straight into a spool file (see the multipart item below).
- **Delta reads (reconstruction) up to `DGP_SPOOL_THRESHOLD_BYTES`** (default 16 MiB, or `max_object_size` when that is smaller): xdelta3 needs the reference baseline and the output object in RAM at once. Peak working memory for one such GET is on the order of the object size plus the reference. With N concurrent delta GETs of such objects, plan for roughly `N × (object + reference)` of transient RAM on top of the baseline footprint.
- **Delta reads larger than `DGP_SPOOL_THRESHOLD_BYTES`:** the proxy copies the reference to a spool file, reconstructs the object into a second spool file, checks its SHA-256, and then streams the file. xdelta3 maps the reference file instead of reading it into the heap, so the memory stays bounded whatever the object size. Each such GET reserves `object + reference` of the spool budget (`DGP_SPOOL_MAX_BYTES`) while it runs, and range reads of the same object share one reconstruction for `DGP_RANGE_SPOOL_TTL_SECS` (default 60 s).
- **Multipart uploads:** every part goes from the request straight into a spool file, the relay part, and stays there until the upload completes or is aborted. So multipart uploads use spool disk, not memory: each open upload holds the size of the parts that it received so far. When an upload that can become a delta completes, the proxy joins its relay files into one spool file and delta-encodes that file as a stream, so the memory of the proxy does not grow with the size of the object. The spool use of that upload peaks at twice its size for a short time, and `DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES` sets the largest upload that gets this treatment. One upload can hold at most `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` of spool space (default half of `DGP_SPOOL_MAX_BYTES`), which is also the largest multipart object, and all open uploads together at most `DGP_MAX_TOTAL_MULTIPART_BYTES` (default half of `DGP_SPOOL_MAX_BYTES`). Both limits count the spool space that the parts reserve, not their bytes: the spool reserves whole MiB for each file, so every part counts at least 1 MiB, also a part of one byte. A part counts from the moment that the proxy admits it, before it arrives, so parts that arrive in parallel cannot pass a limit together. An upload that a client abandons holds its share until the idle sweep removes it (`DGP_MULTIPART_IDLE_TTL_HOURS`).
- **Reference cache (`DGP_CACHE_MB`, default 100 MB):** an LRU that keeps hot baselines in memory so only the first cold read of a deltaspace pays a backend round-trip. Raising it trades RAM for fewer backend fetches on read-heavy workloads; it does not change the per-request buffering cost.
- **Metadata cache (`DGP_METADATA_CACHE_MB`, default 50 MB):** caches per-object `FileMetadata` for HEAD/GET/LIST. It is small and bounded.
- **Listing-size cache (`DGP_LIST_SIZE_CACHE_MB`, default 32 MB):** remembers the original size and ETag of each stored delta or ciphertext that this proxy wrote, read, or listed. A listing page that finds all its objects in this cache sends no extra request. For the other objects, the proxy reads the listing facts index on the backend with one listing request per page (see [the delta-compression explainer](../explanation/delta-compression.md)). An entry takes about 275 bytes for a 60-byte object key (the entry holds the endpoint, the bucket, the key, both ETags and the size), so the default holds about 120,000 objects.

As a worked example, suppose that your delta-eligible objects average 40 MB and you expect up to 16 concurrent delta reads at peak. These objects are larger than the default spool threshold of 16 MiB, so they reconstruct to spool files: the reads need roughly `16 × (40 MB + reference)` ≈ 1.3 GB of spool disk, and little extra RAM. If you raise `DGP_SPOOL_THRESHOLD_BYTES` above 40 MB, the same reads reconstruct in memory and need about 1.3 GB of transient RAM instead. Lowering codec concurrency caps either number. Lowering `DGP_MAX_OBJECT_SIZE` does not make a large object passthrough: the proxy refuses an upload larger than `DGP_MAX_OBJECT_SIZE` with `400 EntityTooLarge`.

[The streaming-versus-buffering section of the delta-compression explainer](../explanation/delta-compression.md#the-trade-off-streaming-versus-buffering) describes the trade-off in detail. It also says when to disable compression on a bucket.

## Disk

The proxy holds metadata and routing; the bytes live on your backends. Disk needs are modest and depend on backend choice:

- **Filesystem-backed buckets** store baselines and `.delta` files on the disk of the proxy host. Size this disk for the compressed footprint of those buckets, which is a fraction of the logical size, plus headroom for in-flight uploads.
- **S3/S3-compatible backends** stream to the provider; the proxy keeps no permanent copy. Local disk then holds only the OS, the binary, the encrypted IAM DB (`deltaglider_config.db`), and the spool directory.
- **The spool directory** (`DGP_SPOOL_DIR`, default `<system temp>/dgp-spool`) holds every scratch file: upload bodies larger than `DGP_SPOOL_THRESHOLD_BYTES`, large delta reconstructions and encodes, multipart relay parts and the temporary files of encrypted uploads. `DGP_SPOOL_MAX_BYTES` caps its use, and the directory must be writable. By default (`auto`) the proxy sets the cap at start to 50% of the free space of the directory's filesystem, or to 25% when the directory shares its device with a filesystem backend or with the data directory (at least 1 GiB, whole MiB; see [Auto-sized limits](configuration.md#auto-sized-limits)). With 60 GiB free, the spool budget is 30 GiB, the largest multipart object is 15 GiB, and `max_object_size` is 15 GiB (5 GiB when any backend is S3, because an S3 backend stores a client object with one PUT request). The delta encode holds the body, the reference and the delta output in the spool at once, so an object above a third of the budget (10 GiB here) is stored whole, not as a delta, with `dg-passthrough-reason: delta-unavailable`. In a bucket that the proxy encrypts, and for the delta of a multipart upload, the encode also holds a second copy of the body (the ciphertext file, or the relay parts), so the ceiling there is a quarter of the budget (7.5 GiB here). Size the budget at three times your largest delta-eligible object, or at four times for an encrypted bucket or a multipart upload; `limits.delta_ceiling`, `limits.delta_ceiling_encrypted` and `limits.delta_ceiling_multipart` show the ceilings. Every start measures the disk again, so a fuller disk at a restart gives smaller limits; reads of stored objects do not depend on them. Put the directory on a real disk, and plan the free space of that disk for the uploads that arrive at the same time. On a tmpfs or on a Kubernetes `emptyDir`, the free space does not show the real limit (memory, or the `sizeLimit` of the volume), so set `DGP_SPOOL_MAX_BYTES` yourself there; without a value the proxy uses 1 GiB and logs a warning. The Helm chart and the operator set `DGP_SPOOL_MAX_BYTES` to three quarters of the size limit of the `/tmp` volume that holds the spool. The proxy creates the directory with mode `0700`. It sets an existing directory that only its group can write to to `0700`, and it does not start when another user owns the directory or when all users can write to it. An upload whose declared size is larger than `DGP_SPOOL_MAX_BYTES` gets `400 EntityTooLarge`. In a bucket that the proxy encrypts, the upload also needs spool space for its encrypted copy, so the limit there is about half of `DGP_SPOOL_MAX_BYTES`. A copy of a large delta object holds the spool file of its source while it writes the destination, so such a copy needs about twice its size in spool space, and a copy that cannot fit gets `400 EntityTooLarge` at once. A request that needs spool space and finds none free waits up to `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS` (default 120 s) and then fails with `503 SlowDown`.
- **Migrations (Route 2)** read each source object once and write it through the proxy. Budget for transient bandwidth during the copy and, for filesystem backends, for transient disk. See [how migration works](../explanation/how-migration-works.md).

## Disk watermarks

The proxy measures the free space of three kinds of directory every `DGP_DISK_MONITOR_INTERVAL_SECS` seconds (30 by default): the spool directory (role `spool`), the root of every filesystem backend (role `backend:<name>`, for example `backend:hetzner-fsn1`) and the data directory that holds the config database and the job state (role `data`). An S3 backend has no local disk and is not measured. Directories on one device share one level, and the proxy measures the device once.

Each disk has one of four levels. A level starts when the free space of the disk falls below the larger of a percent of the disk and a floor in bytes:

| Level | Gauge value | Percent of the disk | Floor | What the proxy does |
|---|---|---|---|---|
| ok | 0 | | | Nothing |
| low | 1 | 15% (`DGP_DISK_LOW_PERCENT`) | Twice the largest spool reservation that one upload can make | Logs a warning once |
| high | 2 | 10% (`DGP_DISK_HIGH_PERCENT`) | The largest spool reservation that one upload can make | Refuses new spool reservations (see below) |
| critical | 3 | 5% (`DGP_DISK_CRITICAL_PERCENT`) | 1 GiB | Also refuses writes to the buckets of a filesystem backend on that disk |

The largest spool reservation that one upload can make is the per-upload limit of the effective limits (`limits.per_upload` in `GET /_/api/admin/config`), or the spool budget when the per-upload limit is `0`. A level never starts at a smaller free space than the level above it, whatever the settings.

A level rises at once to the level that the free space shows, because a disk that fills fast must not wait for three measures. A level falls by one step at each measure, and only when the free space is `DGP_DISK_RECOVER_POINTS` percentage points of the disk (2 by default) above the entry threshold of the current level. Take a 100 GiB disk at level high, which starts below 10 GiB of free space. The level stays high while the free space is below 12 GiB, however the value moves between 10 and 12 GiB, and then it falls to low. This keeps a value that moves around a threshold from changing the level at every measure.

The proxy writes a log line when a level changes. While a disk stays at level high or critical, it repeats the line every 10 minutes. The line names the role, the directory, the free and the total space, and the setting to change.

**What a level high or critical on the spool disk does.** A request that needs spool space and holds none yet (a large `PutObject`, a form `POST`, the first part of a multipart upload, a delta encode, a copy) gets `503 SlowDown` at once. The message names the level and the directory. S3 clients retry that status, so the upload goes through when the disk recovers. A request that already holds spool space runs to its end: the next part of a multipart upload, or the write that follows a body that is already on the disk. Reads, deletes and uploads that the proxy holds in memory are not affected, and `GET /_/ready` does not change, because a node with a full disk still serves reads.

**What a level critical on the disk of a filesystem backend does.** A write to a bucket of that backend (`PutObject`, `CopyObject`, `CreateMultipartUpload`, `CompleteMultipartUpload`, a form `POST`) gets `503 SlowDown`, and the message names the backend. A delete still works. Replication and lifecycle writes to that backend wait until the disk recovers.

**Where the free space is not the real limit.** On a Kubernetes `emptyDir` volume with a `sizeLimit`, and in a container with a disk quota, `statvfs` reports the disk of the host, and on a tmpfs it reports memory. When the proxy runs under Kubernetes, finds its spool directory on a tmpfs, or cannot measure the directory, it judges the spool a second way: by the share of the spool budget (`DGP_SPOOL_MAX_BYTES`) that running requests have reserved. This is the role `spool-budget`. It uses the same percents as a disk, so level low starts when 85% of the budget is reserved, level high at 90% and level critical at 95%; it has no floor in bytes, because the budget already limits every reservation. The level of the spool is the higher of the two. A request that the budget would have made wait for space now gets `503 SlowDown` at level high, and the message says that the spool budget is nearly all reserved. Set `DGP_SPOOL_MAX_BYTES` to the real size of the volume: the proxy cannot learn it. The [monitoring guide](../how-to/monitor-with-prometheus.md#disk-space-rules) has the alert rules.

**The check at start.** At start the proxy compares the free space of the spool directory with the spool budget. When the free space (plus the spool files that the proxy can reuse) is below the budget, it logs a warning that names the directory, both sizes and the settings to change (`DGP_SPOOL_MAX_BYTES`, `DGP_SPOOL_DIR`). Where the free space is not the real limit (see above), it logs one warning that says so. The check logs nothing that the derivation of the limits has already said at start.

The gauges are in the [metrics reference](metrics.md#disk-space).

## Request concurrency and the front door

`DGP_MAX_CONCURRENT_REQUESTS` (a tower concurrency limit, default 1024) caps the whole HTTP surface, independent of the codec pool. It is the ceiling on simultaneous in-flight requests of any kind, and excess requests wait. Multipart uploads have their own caps (`DGP_MAX_MULTIPART_UPLOADS`, and a total-bytes ceiling). See [rate limits and concurrency](rate-limits.md) for the full reference of the protection layers.

## Sizing checklist

Before production, decide each of these from your workload, not the defaults:

| Lever | Env var | Default | Size it from |
|---|---|---|---|
| Codec concurrency | `DGP_CODEC_CONCURRENCY` | num_cpus × 4 (min 16) | Peak concurrent delta ops |
| Max object size | `DGP_MAX_OBJECT_SIZE` | `auto` (half of the spool budget) | Largest object that clients upload (bigger uploads are refused with `400 EntityTooLarge`) |
| Spool threshold | `DGP_SPOOL_THRESHOLD_BYTES` | 16 MiB | Upload bodies and delta objects above it use spool disk instead of RAM |
| Upload memory budget | `DGP_INGEST_MEMORY_MAX_BYTES` | 1 GiB | RAM for upload bodies of at most the spool threshold; a body that does not fit goes to the spool |
| Spool budget | `DGP_SPOOL_MAX_BYTES` | `auto` (50% of the free space of the spool directory at start; 25% on a device shared with backend data) | Concurrent uploads above the spool threshold, large delta operations and multipart uploads. A request that finds no free spool space gets `503 SlowDown` |
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

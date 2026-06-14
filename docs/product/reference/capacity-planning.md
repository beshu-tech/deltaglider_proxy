# Capacity planning and hardware sizing

Reference for the proxy's resource profile and how to size CPU, memory, and disk before a high-throughput deployment. DeltaGlider does active payload manipulation — xdelta3 encode on write, reconstruction on read — so it has a different profile than a pass-through proxy. The numbers below are the levers; pick them from your object sizes and concurrency.

## The resource model in one paragraph

A request is cheap until it touches a delta. **Passthrough** objects (already-compressed media, anything on a non-delta prefix, anything over the size cap) stream through in constant memory and cost almost nothing beyond network. **Delta** writes and reads are where CPU and RAM go: encoding and reconstruction shell out to the `xdelta3` binary, which is a batch algorithm, so the object is buffered in memory while it runs. Sizing is therefore driven by two things — *how big your delta-eligible objects are* and *how many you process at once* — not by raw request rate.

## CPU

The cost centre is the `xdelta3` subprocess, invoked once per delta encode (on PUT) and once per delta reconstruction (on a cold GET). It is fast — xdelta3 typically processes a 100 MB object in under five seconds — but it is real CPU work, unlike a byte-copy proxy.

- **Concurrency is bounded by `DGP_CODEC_CONCURRENCY`** (default: number of CPU cores). This caps how many encode/decode operations run at once, so the proxy can't oversubscribe the box with subprocesses. Requests beyond the cap queue for a codec slot.
- **Rule of thumb:** size cores for your *peak concurrent delta operations*, not your total request rate. A workload that's 90% passthrough reads and 10% delta writes needs far fewer cores than its request count suggests.
- A hung subprocess is killed after `DGP_CODEC_TIMEOUT_SECS` (default 60s) so it can't permanently hold a slot.

If you are CPU-bound, the highest-leverage move is usually to mark genuinely-incompressible prefixes as passthrough (they skip xdelta3 entirely) rather than to add cores.

## Memory

Memory is the variable most people underestimate, because it scales with *object size × concurrency*, not with the dataset.

- **Passthrough reads/writes:** constant memory. They stream.
- **Delta reads (reconstruction):** xdelta3 needs the reference baseline and the output object in RAM at once. Peak working memory for a single delta GET is on the order of the reconstructed object size plus the reference — bounded per object by **`DGP_MAX_OBJECT_SIZE`** (default 100 MB). With the default cap and N concurrent delta GETs of large objects, plan for roughly `N × (object + reference)` of transient RAM on top of the baseline footprint.
- **Reference cache (`DGP_CACHE_MB`, default 50 MB):** an LRU that keeps hot baselines in memory so only the first cold read of a deltaspace pays a backend round-trip. Raising it trades RAM for fewer backend fetches on read-heavy workloads; it does not change the per-request buffering cost.
- **Metadata cache (`DGP_METADATA_CACHE_MB`, default 50 MB):** caches per-object `FileMetadata` for HEAD/GET/LIST. Small and bounded.

**Worked example.** Suppose your delta-eligible objects average 40 MB and you expect up to 16 concurrent delta reads at peak. Transient reconstruction memory is roughly `16 × (40 MB + reference)` ≈ 1.3 GB on top of the caches and base process. Lowering `DGP_MAX_OBJECT_SIZE` (objects above the cap go passthrough and stream) or lowering codec concurrency both cap that number directly.

The detailed trade-off — and when to just disable compression on a bucket — is in [the streaming-versus-buffering section of the delta-compression explainer](../explanation/delta-compression.md#the-trade-off-streaming-versus-buffering).

## Disk

The proxy holds metadata and routing; the bytes live on your backends. Disk needs are modest and depend on backend choice:

- **Filesystem-backed buckets** store baselines and `.delta` files on the proxy host's disk — size this for the *compressed* footprint of those buckets (the whole point is that it's a fraction of the logical size), plus headroom for in-flight uploads.
- **S3/S3-compatible backends** stream to the provider; the proxy keeps no permanent copy. Local disk is then just the OS, the binary, the encrypted IAM DB (`deltaglider_config.db`), and temporary multipart staging.
- **Migrations (Route 2)** read each source object once and write it through the proxy — budget transient bandwidth and, for filesystem backends, transient disk during the copy. See [how migration works](../explanation/how-migration-works.md).

## Request concurrency and the front door

Independent of the codec pool, the whole HTTP surface is capped by **`DGP_MAX_CONCURRENT_REQUESTS`** (tower concurrency limit, default 1024). This is the ceiling on simultaneous in-flight requests of any kind; excess requests wait. Multipart uploads have their own caps (`DGP_MAX_MULTIPART_UPLOADS`, and a total-bytes ceiling) — see [rate limits and concurrency](rate-limits.md) for the full protection-layer reference.

## Sizing checklist

Before production, decide each of these from your workload, not the defaults:

| Lever | Env var | Default | Size it from |
|---|---|---|---|
| Codec concurrency | `DGP_CODEC_CONCURRENCY` | CPU cores | Peak concurrent delta ops |
| Max delta-eligible object size | `DGP_MAX_OBJECT_SIZE` | 100 MB | Largest object you want compressed (bigger → passthrough) |
| Reference cache | `DGP_CACHE_MB` | 50 MB | Number/size of hot baselines, read-heaviness |
| Metadata cache | `DGP_METADATA_CACHE_MB` | 50 MB | LIST/HEAD volume |
| HTTP concurrency ceiling | `DGP_MAX_CONCURRENT_REQUESTS` | 1024 | Peak in-flight requests |

**A reasonable starting point** for a moderate-throughput single instance: 4–8 cores, 4 GB RAM, `DGP_CODEC_CONCURRENCY` left at default, caps left at default — then watch the [Prometheus metrics](metrics.md) (codec timings, queue depth, cache hit rate) under real load and adjust. Scale out with multiple instances behind a load balancer (each stateless on the data path; share IAM via [config sync](../how-to/run-multiple-instances.md)) rather than scaling a single box indefinitely.

## Related

- [How delta compression works](../explanation/delta-compression.md) — the streaming-vs-buffering trade-off these numbers come from.
- [Rate limits and concurrency](rate-limits.md) — every protection-layer limit and its override.
- [Metrics](metrics.md) — the Prometheus signals to watch under load.
- [Configuration](configuration.md) — every field and env var in one place.
- [How to run multiple instances (HA)](../how-to/run-multiple-instances.md) — scaling out instead of up.

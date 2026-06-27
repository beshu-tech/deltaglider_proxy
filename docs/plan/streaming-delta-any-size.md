# Streaming Delta Compression for Unbounded Object Sizes

Status: **PLAN** (not started). Funded multi-week project. Supersedes the
passthrough-only shortcut (explicitly rejected — the team wants true delta
DEDUP at any size, not just streamed storage).

Origin: an adversarial x-ray of the naive "stream xdelta3 stdout to the client"
plan found **12 blockers** (every review domain returned at least one). This
plan solves each. The naive approach is dead — see "Why not pipe-to-client".

## The one architectural decision everything hangs off

**Reconstruct delta GETs to a quota'd temp SPOOL FILE, then stream the file to
the client.** NOT "pipe xdelta3 stdout straight to the axum body."

This single choice collapses 6 of 7 GET-side blockers, because it decouples
xdelta3's lifetime from the client's download speed:

- xdelta3 runs flat-out (source = temp file, delta = stdin, stdout → spool
  file, hashed as written). Finishes in seconds, then **releases its codec
  permit + temp source immediately** (solves blockers 1, 3, 4, 5, 7).
- SHA-256 is computed over the spool as decode writes it; compared to
  `metadata.file_sha256` **before** the response's first byte (solves 2 — the
  integrity contract is preserved byte-for-byte, zero protocol change).
- The spool is seekable → range requests `seek()` into it; decode-once,
  serve-many (solves 6).
- Peak RSS per GET is the pump buffer (a few MB), not the object.

### Why not pipe-to-client (the rejected alternative)
Piping xdelta3 stdout straight to the HTTP body generates blockers 1–5
simultaneously AND cannot keep pre-flight integrity: you can't verify the whole
SHA-256 before the first byte if you've already streamed the first byte. S3
clients don't parse HTTP trailers, so there's no post-headers failure signal.
**Pre-flight integrity is fundamentally incompatible with true byte-streaming.**
Decode-to-spool is the only design that keeps the team's non-negotiable
integrity guarantee. Cost accepted: latency-to-first-byte becomes
"decode-to-disk time" (not true streaming TTFB), and transient local DISK (not
RAM) up to object size, quota'd. For a GB-artifact dedup product that trade is
correct.

The ENCODE/PUT side IS genuinely streaming (body → stdin as it arrives;
reference = seekable temp file; delta output is small + capped).

## The 12 blockers and where each is solved

| # | Blocker | Evidence | Solved in |
|---|---|---|---|
| 1 | 60s wall-clock watchdog SIGKILLs slow streams mid-flight | codec.rs:19,37 | Phase 1a (stall-based watchdog) + spool (xdelta3 writes disk, never blocked on client) |
| 2 | Pre-flight SHA-256 incompatible with streaming | retrieve.rs:401 | Phase 3 (hash spool before first byte; integrity gate preserved) |
| 3 | Pipe deadlock under client backpressure | codec.rs pipe_stdin_stdout_stderr | Phase 1b (bounded pump; xdelta3 → spool, not client) |
| 4 | Blocking pipe read on tokio runtime | codec.rs | Phase 1b (dedicated OS thread, never a tokio worker) |
| 5 | Codec permit held for client's whole download → DoS | retrieve.rs:364,380 | Phase 3 (release permit at decode-done) |
| 6 | Range = full re-decode from offset 0, N× for parallel ranges | s3_adapter_s3s.rs:217; retrieve.rs:282 | Phase 3 (seek into spool; decode-once reuse cache) |
| 7 | Reference temp files exhaust /tmp under concurrency | codec.rs:388 | Phase 1d (quota'd SpoolDir byte budget) |
| 8 | collect_blob_limited buffers whole body before store() | s3_adapter_s3s.rs:1291 | Phase 4 (stream body to xdelta3 stdin) |
| 9 | encode_and_store clones reference + data.to_vec() | store.rs:272 | Phase 4 (streaming encode, no full Vec) |
| 10 | Reference fully heap-loaded to encode against | get_reference_cached, mod.rs:1577 | Phase 2 (get_reference_to_file; filesystem hardlink) |
| 11 | multipart-complete assembles all parts into one Bytes | s3_adapter_s3s.rs:1108 | Phase 4c (stream relayed part files) |
| 12 | u64→i64 size cast with i64::MAX fallback; Content-Length must be exact | s3_adapter_s3s.rs:311,1761 | Phase 5 (cast audit; exact Content-Length) |

## Phases

### Phase 0 — Spikes (must land before any commitment)
- **Spike A — xdelta3 >2GB source window. ✅ DONE — PASS.** Tested stock
  xdelta3 3.0.11 (prod's exact binary, in debian:bookworm) against a 2.5GB
  source with a diff at byte 2.2GB (past the 2GB mark): encode rc=0, delta=9.3KB,
  decode reconstructed 2.68GB **byte-exact** (SHA-256 match), zero window
  warnings. Memory: decode peaked at **73MB RSS vs a 2.5GB source** — xdelta3
  **mmaps the source**, so source-as-seekable-file = bounded memory (validates
  the keystone). **NO custom XD3_USE_LARGESIZET build needed** — this sub-task
  and the `probe_largesize` are DELETED from scope. Stay on pinned 3.0.11.
- **Spike B — decode-to-spool throughput + stall semantics. ✅ DONE — PASS.**
  Stock 3.0.11 decoding 1.6GB: 6,144 chunks / 1.29s, **largest normal
  inter-chunk gap = 12.8ms** → a stall-timeout of even 1s has ~78× margin. A
  blocked sink (full pipe) makes xdelta3 **block on write with ZERO stdout
  progress** for the full stall window — cleanly distinguishable from normal
  progress. The stall-watchdog (kill on no-stdout-progress for N seconds,
  N >> 13ms) separates legit slow decode from a real hang. NOTE for Phase 1a:
  keep an ABSOLUTE ceiling too (a crafted delta could make xdelta3 spin
  forever while trickling output) — stall-timeout AND a generous hard cap.
  In the spool design xdelta3 writes to local disk (never the slow client), so
  it only stalls on a true hang or a full disk — both of which SHOULD kill.
- **Spike C — streaming-encode ratio decision. ✅ DONE — PASS** (riskiest encode
  unknown). A Rust harness streamed a 1.5GB target chunk-by-chunk, tee'd to
  (a) xdelta3 stdin (delta→capped spool) and (b) a passthrough spool, against a
  1GB reference file. Results: SIMILAR target → 537MB delta under the 805MB cap →
  **DELTA WINS**; DISSIMILAR target → delta hit the cap → `over_cap` detected →
  **abort feed + fall to PASSTHROUGH from the spool**. **Peak RSS = 2MB in BOTH
  cases** (bounded by chunk buffer, NOT the 1.5GB object). The "tee to passthrough
  spool + cap-and-abort" design is validated — encode blockers 8/9/10/11 are
  solvable with bounded memory. REFINEMENT for Phase 4: check the delta cap
  BEFORE writing the next chunk to xdelta3 (the spike overshot by one chunk
  because the cap check trailed the reader thread).

### Phase 1 — Codec foundation (keystone; blocks 1,3,4,7) — ✅ DONE (commit e959c7c)
- 1a Stall-based watchdog: `DGP_CODEC_STALL_SECS` (~30s); pump bumps
  `AtomicU64 last_progress_nanos`; kill only on no-progress, never elapsed.
- 1b Bounded pump replacing read_to_end: fixed-chunk reads on a dedicated OS
  thread → a `Sink`/`Write` callback (never a tokio worker).
- 1c New additive entry points: `decode_to_writer(src_path, delta: Read, out: Write)`,
  `encode_from_reader(src_path, target: Read, out: Write)`. Old `&[u8]→Vec<u8>`
  `encode`/`decode` stay as thin wrappers (small objects, migrate path, tests).
- 1d `SpoolDir`: `DGP_SPOOL_DIR` + `DGP_SPOOL_MAX_BYTES` Semaphore byte budget;
  reference temp + decode spool both draw from it (no ENOSPC). Gauges
  `spool_bytes_resident`/`_peak`.

### Phase 2 — Storage: reference-to-file (blocks 10, enables 7) — ✅ DONE (commit b718284)
- New trait method `get_reference_to_file(bucket, prefix, dest) -> u64` on
  StorageBackend (traits.rs:123). Default = get_reference + write. Filesystem
  override = hardlink/reflink (near-zero, the reference is already local!). S3
  override = stream GET body to file. Reference cache becomes path-aware above
  `DGP_REFERENCE_INLINE_MAX`; small refs stay in RAM (common case).

### Phase 3 — GET: decode-to-spool + streaming response (blocks 2,5,6) — ✅ DONE (commit c8d18f2)
- New `retrieve_delta_spooled` + `RetrieveResponse::SpooledFile { spool, metadata, cache_hit }`.
- Flow: acquire permit+spool → get_reference_to_file → spawn_blocking
  `decode_to_writer(ref, delta, tee(spool, sha256))` → **compare hash to
  file_sha256 (pre-flight gate)** → on mismatch delete spool + ChecksumMismatch
  (clean S3 error, never truncated 200) → **drop permit + reference here** →
  return SpooledFile; adapter streams spool in chunks; Drop deletes on last
  reader.
- Range: reconstruct-to-spool once, seek(start), stream end-start. Short-TTL
  `(bucket,key,sha256)→SharedSpool` cache so parallel multipart ranges decode once.

### Phase 4 — PUT: streaming encode (blocks 8,9,10,11)
- 4a `store_streaming(bucket, key, body: Stream<Bytes>, declared_len, …)`:
  reference→temp (P2); body→xdelta3 stdin as it arrives; delta→bounded spool
  (hard cap `ratio_threshold × declared_len`); tee body through Sha256+Md5
  (upload hashes, incremental).
- Ratio decision: tee body ALSO to a passthrough spool; delta wins → keep delta,
  drop passthrough spool; passthrough wins (poor ratio / over-cap) →
  store_passthrough_file (store.rs:680) from the spool. Bounded RAM, one
  transient on-disk copy (near-free when spool == destination fs).
- 4b Adapter: delta-eligible keys above an inline threshold stream `input.body`
  to store_streaming instead of collect_blob_limited; small objects keep
  buffered path.
- 4c Multipart-complete: feed the already-on-disk `RelayedParts(paths)` (exists,
  s3_adapter_s3s.rs:1087) through store_streaming; raise/remove the 64MB
  `DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES` delta cap.

### Phase 5 — Numeric/protocol + unbounding (block 12)
- 5a Audit every `try_from(...).unwrap_or(i64::MAX)` (s3_adapter_s3s.rs
  148/197/226/311/1016/1761/1889): replace silent i64::MAX with explicit
  InternalError so Content-Length is never wrong. Assert spool length ==
  file_size before streaming.
- 5b Decouple delta size gate: codec RAM cap stays for buffered small-object
  path; streaming paths gate on DGP_SPOOL_MAX_BYTES (disk) + the Phase-0
  source-window ceiling, NOT max_object_size. max_object_size becomes the
  "buffer-in-RAM below here" threshold, not a product ceiling.

## Ordering / parallelism
Critical path: Phase 0 → Phase 1 → Phase 2 → {Phase 3 GET, Phase 4 PUT in
PARALLEL (retrieve.rs vs store.rs)}. Phase 5a anytime after P1; 5b last (after
3+4 prove memory stays bounded). Within P1, stall-watchdog (1a) and spool budget
(1d) are independent of the pump rewrite (1b/1c).

## Testing (the non-negotiable ones)
- Stall watchdog: byte-then-sleep-past-stall → killed; steady-3×-stall → not killed.
- Streaming roundtrips (encode_from_reader / decode_to_writer) byte-exact, via proptest.
- Slow-client (blocker 5): N slow-draining delta GETs; assert a further GET still
  gets a codec slot (`codec_semaphore_available` returns promptly).
- Integrity-on-stream (blocker 2): corrupt reference → client gets S3 error
  status, NOT 200+truncated body.
- Range-after-decode (blocker 6): last-1KB range correct; N parallel ranges
  decode spool ONCE (decode-count gauge increments once).
- Memory high-water (CI gate, reuse `bump_peak` + IntGauge per MEMORY.md): large
  GET/PUT keeps decode-resident gauge bounded; assert `spool_bytes_peak > 0` AND
  `decode_resident_peak < bound` (the `>0` guard so it can't pass vacuously at 0).

## Riskiest unknowns (gate on Phase 0) — ALL RESOLVED ✅
1. xdelta3 >2GB source window (Spike A) — **RESOLVED: PASS.** Stock 3.0.11
   byte-exact at 2.5GB, mmaps (73MB RSS). No custom build. Scope shrank.
2. Streaming-encode ratio decision (Spike C) — **RESOLVED: PASS.** tee-to-
   passthrough-spool + cap-and-abort validated; 2MB peak RSS on a 1.5GB target.
   (The "passthrough-spool then encode" fallback is no longer needed — the
   simultaneous tee works.)
3. Decode-to-spool TTFB (Spike B) — **CHARACTERISED.** Decode throughput is high
   (1.6GB/1.29s); stall-watchdog cleanly separable (13ms normal gap). We trade
   true streaming TTFB for decode-to-disk-then-stream — sub-second TTFB on a
   multi-GB delta GET is impossible while keeping the pre-flight integrity gate.
   Accepted by design.

**Phase 0 is COMPLETE — all three spikes green. Phase 1 can begin.**

## What genuinely CANNOT be fully solved (honest scope)
- **True zero-latency streaming decode** — incompatible with pre-flight integrity
  (can't hash the whole output before emitting the first byte). Decode-to-spool
  is the answer; TTFB = decode-to-disk time. Out of scope by design.
- **Random-access decode** — xdelta3/VCDIFF decode is sequential from offset 0.
  The decode-once spool makes the common multipart-parallel-range case pay once,
  but a single cold range near the end of a 3GB delta still decodes ~3GB.
  Intrinsic to VCDIFF; fixing it needs a re-chunked delta format (breaks
  compatibility with existing prod deltas — out of scope).

## Critical files
- src/deltaglider/codec.rs — stall watchdog, bounded pump, encode_from_reader/
  decode_to_writer, SpoolDir, largesize probe
- src/deltaglider/engine/retrieve.rs — decode-to-spool GET, SpooledFile,
  pre-flight integrity, range-from-spool, permit release at decode-done
- src/deltaglider/engine/store.rs — streaming encode, ratio via capped spool +
  passthrough-spool fallback, streaming multipart-complete
- src/s3_adapter_s3s.rs — stream put_object body, wire SpooledFile, multipart
  delta branch, size-cast audit
- src/storage/traits.rs (+ filesystem.rs, s3.rs) — get_reference_to_file
- src/metrics.rs — spool/decode high-water gauges for the CI memory gate

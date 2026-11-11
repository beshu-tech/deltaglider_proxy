# Streaming ingest: request bodies and stored bodies out of memory

Status: **SHIPPED** (branch `feat/streaming-ingest-and-passthrough-reason`).
Builds on [streaming-delta-any-size.md](streaming-delta-any-size.md), which made
the delta codec stream. This record covers the rest of the data path: how a
request body enters the proxy, and how stored bodies move through the engine
and the storage backends.

## Problem

The codec streamed, but the bodies around it did not:

- Every `PutObject` body was collected whole into a `Vec` (up to about 2x the
  body during the last reallocation) before any check. The stated reason, "SigV4
  needs the whole body", was false: s3s checks `x-amz-content-sha256` while the
  body streams and fails the last read with `Sha256Mismatch`.
- No RAM budget across requests: 1024 concurrent requests x `max_object_size`.
- The browser form POST collected the whole form (`to_bytes(max_object_size)`)
  BEFORE it checked the signature: a pre-auth memory DoS.
- Multipart parts below 16 MiB lived in memory for up to 24 h, and file I/O ran
  under the `uploads` write lock.

## Sites found (audit, verified against the code)

Large (size of the object, or more):

1. PutObject collect, and the `Bytes` kept alive for the whole encode.
2. Still-framed aws-chunked bodies decoded into a second buffer (anonymous,
   open-access, unsigned streaming only).
3. UploadPart read whole; MD5 on the runtime thread; sync write + fsync under
   the `uploads` lock.
4. Form POST `to_bytes` + `field.bytes()`.
5. A kept delta of a large PUT read back into memory (`into_bytes`) to commit.
6. Encryption: deltas, references and file bodies sealed in memory (2x).
7. UploadPartCopy: whole-source `retrieve`, ranges collected.
8. Background copies of legacy `StorageInfo::Reference` objects read the source
   twice.
9. S3 backend: a multipart object stored from relay parts assembled in a `Vec`.
10. A small PUT into a folder with a large `reference.bin` loaded the reference.
11. A large delta GET read the whole delta into memory before the decode.
12. Encrypted reads of references and deltas collected and decrypted whole.

Left as follow-ups: the transfer delta fast path and the re-encrypt rewrite
(see the end).

## Final design

**Ingest (`deltaglider/ingest.rs`, `api/body_ingest.rs`).** Every client body
(PutObject, CopyObject through the store, UploadPart, UploadPartCopy, the form
file) is read ONCE, before any lock, into an `IngestedBody`:

- `Memory` when the declared length is at most `DGP_SPOOL_THRESHOLD_BYTES` and
  the `MemoryBudget` has room;
- `Spooled` otherwise. The spool reservation covers the body AND the temp files
  of its storage write (`file_put_spool_bytes`), so the store never asks for
  budget under its locks.

Hashes (SHA-256, MD5, the announced `x-amz-checksum-*` through
`s3s::checksum::ChecksumHasher`) are computed on the way. `Content-MD5`,
checksums and the payload hash are checked after the fill; a failure drops the
body, so nothing is stored and both budgets are whole again.

**Decisions the adversarial judge forced:**

- **RAM budget is request-lifetime and never waits.** `MemoryBudget::try_reserve`
  either grants or refuses. A small body that finds no RAM takes FREE spool
  budget without queueing (it never waits behind a large reservation), else
  `503 SlowDown`. Only a body above the threshold waits for spool space, before
  its first byte is polled. One budget per process, in `AppState`, re-attached
  on every engine rebuild. `DGP_INGEST_MEMORY_MAX_BYTES` (1 GiB; 0 = always
  spool).
- **Declared-length rules (pure, truth-tabled).** No length -> `411
  MissingContentLength`. Declared above the limit -> `413 EntityTooLarge`
  before a byte is read; declared above `DGP_SPOOL_MAX_BYTES` -> `413` naming it.
  The fill is capped at the declared length: longer fails at the first extra
  byte, shorter is `IncompleteBody`. No mid-stream spill from RAM to disk. The
  early quota check uses the declared length (`check_quota_for_write`, netting
  an overwrite); the exact check after the fill stays.
- **Multipart is relay-only.** Every part goes straight to a spool file kept as
  a relay part (`Spool::keep_as`, unique `part-<n>-<uuid>.bin` paths), whatever the
  type and size. In-memory parts, promotion and the chunks completion path are
  gone. Existence, state and caps are checked before the body (read lock, by
  declared size) and again under the `uploads` lock; that lock does no file I/O
  (guard `the_uploads_lock_is_never_held_for_file_io`). Completion reads each
  relay file once and checks its MD5/size in the same read. A delta candidate
  is joined into one spool file; if the join reservation is clamped below the
  total, the object is stored whole with reason `delta-unavailable`.
  `DGP_MAX_TOTAL_MULTIPART_BYTES` defaults to half the spool;
  `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` is the largest multipart object. A part is
  at most `min(5 GiB, max_object_size)`. No SHA-256 for a part that no signed
  payload hash needs.
- **Form POST: signature first, file last.** The form is streamed. The fields
  before the file (64 KiB each, 1 MiB total) are read; signature, expiry,
  credential scope, policy, IAM on the resolved key and the client-write gates
  run BEFORE the first file byte. The file is ingested with the cap
  `min(limit, Content-Length, content-length-range max)`; a shorter file is
  valid; the range minimum is checked after. Fields after the file are read
  under a small limit and IGNORED, as AWS does.
- **Encrypted bodies stream frame by frame.** Reads of references and deltas
  decrypt through `open_body_stream`; file bodies are sealed into a spool file
  that holds budget.
- **Reference objects take the verified spool path.** A `StorageInfo::Reference`
  object above the threshold is filled into a spool file through
  `open_reference_source`, its SHA-256 checked BEFORE the first byte, then
  streamed or seeked (never streamed raw from `open_object`).
- **Large reference + small PUT** is decided inside `store_delta_eligible` from
  the reference size already in hand: the body is written to a spool file
  (no-wait) and encoded as a stream; on contention it is stored whole with
  `delta-unavailable`. No extra HEAD.
- **S3 relay parts are ONE retryable PUT body** (`storage/s3/file_body.rs`):
  chained part files, exact length, reopened from the start on every retry.
- **Every passthrough says why** (`PassthroughReason`,
  `x-amz-meta-dg-passthrough-reason`).

**Guards.** `stored_bodies_are_never_heap_loaded`,
`whole_object_reads_are_marked_small` (`// small: <why>`),
`client_writes_take_the_object_lock_after_the_body`,
`the_uploads_lock_is_never_held_for_file_io`,
`request_bodies_are_never_collected_whole`, `no_form_part_is_read_whole`,
`the_file_is_ingested_only_after_the_signature_and_the_policy`; at run time,
`PutBody::into_small_bytes` refuses a file body.

## What shipped (commits)

1. Passthrough reason; relayed multipart delta candidates stream.
2. File bodies stream on every backend; encrypted reads stream.
3. A kept delta commits from its spool file.
4. Large reads stay out of memory; reference objects verified from a spool file.
5. Ingest primitives: one spool fill, streaming aws-chunked decoder (unit,
   proptest, fuzz), checksum plan, `MemoryBudget`.
6. PutObject and CopyObject through the ingest.
7. Multipart relay-only; UploadPartCopy streams.
8. Form POST streamed, signature before the file.
9. Audit, docs, this record.

## Follow-ups

- **Transfer delta fast path** (`transfer.rs` via `engine/raw.rs`
  `get_delta_raw` / `get_reference_raw`, plus the heal): the stored delta and
  reference are copied verbatim through memory. Move them to spool files. The
  `PENDING` entry of `whole_object_reads_are_marked_small` tracks it.
- **Re-encrypt rewrite** (`engine/rewrite.rs`, `open_raw`): deltas and
  references are read whole and sealed again. Same spool conversion.
- **S3 objects above 5 GiB**: native multipart upload from a file, and an early
  refusal of a single PUT above the backend's maximum.
- **Allocator memory-bound test binary**: a counting global allocator that pins
  peak heap per scenario (large PUT, multipart, delta GET).
- **copy.rs small copy**: a small source range is still copied out of the
  retrieved buffer (bounded by the threshold).

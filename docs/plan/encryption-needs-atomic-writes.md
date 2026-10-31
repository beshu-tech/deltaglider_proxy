# Plan: encryption needs atomic writes; conditional rewrites; no plaintext_reads

Status: approved by the owner 2026-10-31 (decisions below are his).

## Decisions
1. The conversion's rewrites are conditional (P8 of encryption-conversion-marker.md): every
   object write of the re-encrypt job carries a precondition on the listed ETag. A lost
   precondition means a client wrote a newer object (already encrypted): skip it, count it.
   Where every write of the job is conditional, the per-node write gate is dropped for that
   phase, so client writes on every instance stay safe and are never overwritten.
2. Encryption requires a backend with atomic writes (conditional PUT/DELETE). A fresh
   atomicity probe runs right before every action that turns encryption on or keeps a
   conversion going: apply/validate of an encryption block (bucket or backend), Resume /
   `open_window`, bucket create with encryption. A backend that fails or cannot be verified
   is refused with a clear error naming the backend and the reason; the UI disables the
   encryption options on such a backend and says why. Probe evidence on 2026-10-31: Backblaze
   B2 (eu-central-003) closes the connection on `If-None-Match`/`If-Match` PUTs.
3. An encrypted bucket that already exists on a backend without atomic writes (prod:
   `ror-encrypted` on `b2`) becomes READ-ONLY: reads keep working; every write (S3, form POST,
   admin bulk ops, jobs that write into it) is refused with a clear error. Boot logs it; the UI
   shows it on the bucket and backend cards.
4. `plaintext_reads` is removed, not deprecated: a config that contains the field fails to
   load with an error that names the field. All `serve` code paths, the Convert-now notice and
   the deprecation warnings go. No compatibility shim.

## Milestones
- N1: conditional rewrites + gate removal where safe (backend only).
- N3: remove `plaintext_reads` everywhere (backend, UI, docs), in parallel with N1.
- N2: atomicity probe + refusal on every encryption entry point + read-only encrypted buckets
  on non-atomic backends (backend + UI + docs), after N1 and N3 merge.
- Final: four-way deep review, visual check, gates.

## As built: N1 (2026-10-31)

- **One rewrite primitive, not five conditional copy paths.** The job no longer
  uses `copy_object_known` (server-side copy, multipart stream, delta fast path,
  spooled copy, buffered store). `engine/rewrite.rs` reads ONE stored form (the
  body and its version in one GET) and stores the same form again through the
  storage stack: one write per form, no delete of the other form (no condition
  could cover it), no re-encode of a passthrough object. Large passthrough bodies
  go through a spool file. A key that holds both forms (crash leftover) has each
  form rewritten in place; newest-wins by `created_at` (kept) is unchanged.
- **Trait.** `StorageBackend::put_if_unchanged(bucket, prefix, RewriteTarget,
  PutBody, metadata, version) -> WriteIf {Written, Changed, Unsupported}`, no
  default, forwarded by the encrypting wrapper (seals), routing, `FaultyFs` and
  the test stubs. S3: `If-Match` on the version (`FencedPut` outcome of the
  existing fenced PUT helpers, `OnUnsupported::Report`); 412/409 and a 404 to
  `If-Match` are `Changed`; 501 is `Unsupported` with nothing written. Filesystem:
  version = `fs:<inode>:<mtime ns>:<size>`, checked then written; the engine's
  object write lock (and the deltaspace lock for deltas and references), which
  every write takes, makes it atomic on the one node.
- **Version source.** `FileMetadata::backend_version` (serde skip), set only by the
  backends' read paths (S3 ETag at both return points of `metadata_of_response`;
  filesystem stat), guarded like `stored_last_modified`.
- **Deltas and references.** Delta rewrites hold the deltaspace lock and the
  cross-instance reference lock and write through
  `ReferenceLockGuard::rewrite_delta_if` (same bytes, so no reference check after).
  References: one GET, then `rewrite_reference_if` (If-Match on the read's ETag;
  under a cross-instance hold, a read that differs from the hold's fence is
  `Changed` without a request). No phase had to keep the gate for lack of a
  condition.
- **Gate.** `ReencryptParams::conditional_writes`, set by
  `maintenance::conversion::rewrites_are_conditional` when the job is queued and
  again at each run start: filesystem yes; S3 when the capability verdict of the
  backend definition is CAS, else the capability probe runs then (cached);
  `NonCas`, `Unknown` or `DGP_TEST_FORCE_NONCAS_BACKEND` (any backend, so gate
  tests keep the gate on a filesystem backend) keep it. `maintenance_gate_arms`
  skips a reencrypt row with the flag; the whole job is ungated (all phases,
  purge included: it deletes only noncurrent versions). An ungated run skips the
  drain. A gated run may fall back to an unconditional write on 501
  (`Unconditional::Allowed`); an ungated one fails the object instead.
- **Counting.** `Visit::Changed` -> skipped + jobs-only column
  `maintenance_jobs.objects_changed`; exposed as `objects_skipped_changed` on the
  run entry and the bucket status view; run summary note. A key gone since the
  listing (HEAD 404) is `Changed`, not a failure.
- **Seams.** `DGP_TEST_REWRITE_STALL_MS` (after the read, before the conditional
  write) and `DGP_TEST_REWRITE_LOCK_STALL_MS` (after the HEAD, before the object
  lock).
- **UI.** No UI file changed. The UI already follows `writes_blocked`; the apply
  dialog copy "uploads and deletes get a temporary 503" must say that this holds
  only on a backend without conditional writes (routed separately).

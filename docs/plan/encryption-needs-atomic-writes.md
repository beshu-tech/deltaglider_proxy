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

## As built: N2 (2026-11-01)

- **One module.** `coordination/atomicity.rs`: `probe_atomic_writes` (FRESH, never a cached
  verdict: the forced seam → NonCas; filesystem → `CasVerified { via: LocalFilesystem }`, no
  request; S3 → the `cas_probe` on a bucket the backend lists, the action's own bucket first;
  stores the verdict), `require_atomic_writes` → `AtomicWrites` proof (only it makes one) or
  `AtomicityRefusal { backend, verdict, what }` (`message()`, `details()` = the `atomic_writes`
  object `{code, backend, probe, probe_result, doc_url}`), `require_for_bucket`,
  `gate_encryption_change(old, new)` (one probe per backend of `Config::atomic_write_targets`),
  `EncryptedPlan` (from `Config::proxy_encrypted_storage`) and `spawn_watch`.
- **Targets.** `Config::atomic_write_targets(old)`: a backend site that becomes proxy-AES with a
  key or changes it (or is new), a new default backend that encrypts, a configured bucket that
  starts or changes proxy encryption or moves backend with it. Transient migrate routes skipped.
- **SSE decision.** The rule covers `aes256-gcm-proxy` only. SSE-S3/KMS objects are encrypted by
  the backend; the proxy never converts or rotates them (`resolve_desired` refuses SSE), so no
  conditional write of the proxy is at stake. A keyless proxy mode refuses writes: no target.
  The read-only rule covers every proxy-mode bucket (with or without a key).
- **Dropped connection.** `cas_probe::StepOutcome::Dropped` (SDK `DispatchFailure`,
  `TimeoutError`, `ResponseError`). When a conditional step drops, the probe sends a plain PUT of
  the key: written → `resolve_dropped` maps the drop to `NotImplemented` (definitive non-CAS,
  the B2 eu-central-003 behaviour), else `Other` (unknown). Bounded by the SDK retries and the
  15 s `PROBE_TIMEOUT`. Unit tests use a TCP-level fake (`cas_probe::b2_like`) that closes the
  socket on `If-Match`/`If-None-Match` PUTs.
- **Entry points (409 everywhere).** `TransitionError` and `Rejection` carry `details`; every
  config surface renders it as `atomic_writes` (section PUT/validate, document apply/validate,
  field PATCH body; PATCH has no encryption fields, so it never triggers). The apply gate runs
  in `transition_gates` for forward admin writes and in the dry run (`dry_run_gates`). Bucket
  create: an explicit `require_atomic_writes` when the new bucket's write encryption is proxy
  (also when it inherits the backend's) → 409 `atomic_writes_required`. `POST /jobs/reencrypt`:
  a proof per Proxy-target bucket before `open_window`; `errors[].atomic_writes`, 409 when none
  started for that reason. `queue_reencrypt` checks every Proxy job (`NotQueued::NotAtomic`)
  unless the request carries a proof for the bucket's backend; the apply's enqueue probes again
  (the lock was released). Boot conversion: probe before the window, audit
  `encryption_conversion_refused`. Migrate: `gate_encryption_change(cfg, flipped)` → 409.
  Re-encrypt run start: a cached CAS verdict of the current definition, else a fresh probe;
  Unknown → `FailedTransient`, NonCas → `Failed`.
- **Read-only.** `BackendCapabilityCache` holds the `EncryptedPlan`; `read_only_reason(bucket)`
  = plan backend × verdict of its current definition (non-CAS / unknown → reason; no verdict
  yet → writable). Enforced by `check_client_write_allowed` (403 AccessDenied: the refusal is
  configuration, not transient, so a client must not retry it as it would a 503; S3 has no
  read-only code; the same code as `replication_target_only`), so every caller of it (s3s
  PUT/DELETE/DeleteObjects/multipart create+complete/copy ×2/DeleteBucket, form POST, admin
  bulk, delete jobs) refuses; by the maintenance gate's write-refusal probe (`main.rs`), so
  replication, lifecycle and running delete jobs wait (`write_blocked`); and by
  `BackendCheck::gated` for maintenance jobs, migrate exempt (`ignoring_read_only`: moving the
  bucket out is the way out). Backfill creation refuses with 403. The plan is set at boot and
  in `apply_config_transition` (commit phase, also background transitions).
- **Watch.** `spawn_watch` (boot, background): probes every plan backend without a verdict,
  re-probes a non-CAS / unknown one every 10 min, wakes on `plan_changed`; logs each change of
  the read-only set. `POST /_/api/admin/backends/:name/capability-probe` = "Check again".
- **Form POST.** The client-write gate moved after the policy-signature check: its 403 names the
  bucket's configuration and backend, which an unverified caller must not learn (S23).
- **Seams.** `DGP_TEST_KEEP_WRITE_GATE` (the gate tests: encryption now runs only on atomic
  backends, so the gated path is a decrypt job; the seam keeps the same mechanics with an
  encrypt job) and `DGP_TEST_FORCE_NONCAS_WHILE_FILE` (the forced verdict holds while the file
  exists, so a test turns it CAS without a restart).
- **Flake (D).** `conversion_apply_test::a_versioned_bucket_keeps_plaintext_versions_until_a_job_purges_them`
  (5 of 80 runs): MinIO lists a bucket that is being created with creation date
  `0001-01-01T00:00:00Z`; the harness's stale-bucket sweep (`tests/common/minio.rs`) read it as
  older than 2 h and force-deleted the running test's own bucket; the proxy made it again
  without versioning, so the preview said `versioned: false`. Fix: `is_stale` never treats a
  time at or before the Unix epoch as stale (buckets and objects). 0 of 30 after.
- **Deviations.** No source guard existed for the `replication_target_only` call sites (none
  added). PATCH /config has no encryption fields (n/a). A re-encrypt run start trusts a cached
  CAS verdict. An S3 CreateBucket on a default backend that encrypts with the proxy is not
  refused (the bucket is read-only at once). Buckets the config does not name, on such a default
  backend, are read-only but not listed in `read_only_buckets`.

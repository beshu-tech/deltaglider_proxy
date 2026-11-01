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
- **Seams.** `DGP_TEST_REWRITE_STALL=<point>:<ms>[:<signal file>]`: `before-write`
  (after the read, before the conditional write) or `before-lock` (after the HEAD,
  before the object lock). (Two variables until the four-way review merged them.)
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
  yet → pending, see review fixes below). Enforced by `check_client_write_allowed` (403 AccessDenied: the refusal is
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
  CAS verdict.

## As built: N2 review fixes (2026-11-01)

- **Fail closed before the first verdict.** `BackendCapabilityCache::write_block(bucket,
  held_by)` → `WriteBlock::{Pending, ReadOnly}`; one `AppState::atomic_write_block` passes the
  backend the router resolved. Pending (no verdict for the backend's current definition) →
  `503 SlowDown` ("the atomic-write check is still running"); background writers and jobs wait
  on the same rule. A filesystem backend gets its verdict with the plan
  (`atomicity::static_verdict`), so it never waits. The watch probes every due backend at once
  (concurrently) and signals its first round; the boot waits for it up to 3 s. Seam:
  `DGP_TEST_ATOMICITY_PROBE_DELAY_MS` (every probe sleeps first; filesystem verdicts come from
  the probe then).
- **CreateBucket.** The s3s `create_bucket` asks the same rule: a bucket that would encrypt
  with the proxy on a backend without atomic writes is not created, `403 AccessDenied` (not
  409: clients read 409 on CreateBucket as already-exists and go on) with
  `[atomic_writes: backend=…, probe=…]` in the message; pending → 503. The admin create path
  already refused with a fresh probe.
- **Every read-only bucket listed.** `ProxyEncryptedStorage` keeps every backend whose own site
  encrypts with the proxy (`sites`) and resolves a bucket without a policy on the backend that
  held it (`held_by`, else the default). `atomicity::read_only_buckets(app)` lists the
  configured ones and every bucket of the storage listing; GET /backends, the capability probe
  answer, the bucket view and the watch log use it.

## As built: the `If-Match` dialect (2026-11-01, release blocker)

Measured with raw SigV4 requests on Hetzner Object Storage (hel1, Ceph RGW), bucket `debug`:
PUT `If-None-Match: *` → 412 on an existing key, 200 on a new one; PUT `If-Match` with a wrong
quoted ETag → 412; PUT `If-Match` with the CORRECT ETag quoted (as HEAD/PUT return it) → 412;
the same ETag unquoted → 200; `If-Match: *` → 200; GET `If-Match` quoted → 200; DELETE
`If-Match` with a wrong ETag → 204 and the object is gone. On main the probe sent the SDK's
quoted ETag in its right-ETag step, so Hetzner was `NonCas`: `beshu-encrypted` would have become
read-only at the next release, and every `If-Match` write would have reported a lost race.

- **Dialect.** `coordination::if_match::IfMatchDialect {Quoted (default), Unquoted}` and
  `ConditionalProfile {if_match, conditional_delete}`. `CapabilityVerdict::CasVerified` gained
  `if_match` (`#[serde(default)]`: a verdict without it reads as `quoted`; the verdict now also
  derives `Deserialize`). Admin: `capability.if_match` on GET /backends, `verdict_words`, the
  backend card.
- **Probe.** `cas_probe::probe_cas`: quoted right-ETag step first; on 412 (after the lost-response
  GET), a wrong unquoted step and, after a HEAD re-read, a right unquoted step. Pure
  `classify_cas_dialect` (truth table): Unquoted only when `If-None-Match`, the quoted wrong ETag
  AND the unquoted wrong ETag are refused and the unquoted right ETag is written. The DELETE step
  uses the found dialect. `ProbeResult.if_match` + `profile()`. 7 requests quoted, 11 unquoted.
- **One helper.** `IfMatchRequest::if_match_etag(etag, dialect)` on PutObject, DeleteObject,
  CopyObject and UploadPartCopy (`copy_source_if_match`), and GetObject (always quoted: RGW
  unquotes reads, measured 200). Source guard `if_match_headers_go_through_one_helper` (src/lib.rs):
  no `.if_match(` / `.copy_source_if_match(` in production code outside `if_match.rs`.
- **Where the dialect comes from.** Data backends: `if_match::record_for(def, profile)` from
  `establish_backend_verdict_within` (every S3 capability probe: boot gate, atomicity watch,
  fresh action probes, hot-apply, migrate target) and from the boot's coordination validation
  (same definition); `S3Backend::conditional_profile` looks it up by its health fingerprint
  (`with_conditional_profile` pins one for tests). Never probed → quoted, DELETE unproven.
  Coordination: `CoordinationValidation::{CachedWitness, Probed}` carry `profile`;
  `S3Lease::with_profile`, `S3ReferenceLock::with_profile`, `EncryptionWitness::with_if_match`,
  `ConfigDbSync::use_profile` (also set by `validate_coordination_bucket`). The witness object
  records `if_match`; a witness without it is stale (re-probe once: an older release's probe may
  not have tested the right ETag at all, so it must not vouch for `quoted`).
- **Call sites (If-Match).** `cas::conditional_put_etag` (lease/lock create-steal-renew, encryption
  witness manifest), `cas::delete_if_match` (lease/lock release, only with conditional DELETE),
  lease and lock read-first release DELETE, `config_db_sync::upload`, S3 `claim.rs` marker/claim
  Replace and Remove, `objects.rs` `apply_put_fence` (fenced reference/delta PUTs and
  `put_if_unchanged` through `put_object_*_fenced_with`), the fenced metadata self-copy
  (`copy_source_if_match`), `delete_s3_object_fenced`, `delete_variant_if`, `copy.rs` CopyObject and
  UploadPartCopy, `body.rs` resumed GET, and the probe itself.
- **Conditional DELETE.** Read first unless the profile proves DELETE `If-Match`: lease and lock
  release (already), `claim.rs` Remove (new: HEAD, Lost when the ETag differs or the object is gone),
  `delete_variant_if` (new: `variant_version` HEAD; the engine's `delete_if` / `only_if` already read
  before under the object lock), `delete_s3_object_fenced` (already HEADs always). Remaining race:
  a peer instance's write between the HEAD and the DELETE is deleted (request-time window); the
  in-process object lock still serialises this node. Unprobed backends pay one HEAD per such delete.
- **Tests.** `FakeS3::set_rgw_conditions` (PUT `If-Match` against the bare ETag, DELETE ignores it,
  copy source unquoted). `coordination/rgw_dialect_tests.rs`: probe → Unquoted, verdict + registry
  → `put_if_unchanged` Written then Changed, pinned delete reads first (probed and unprobed),
  `conditional_put_etag`, lease and reference-lock round trips (stale release keeps the peer's
  lock), config sync double upload; `conversion_tests::on_rgw_the_marker_is_replaced_and_closed_by_identity`.
  All nine failed on main (17be3eaa) with the RGW fake. One more defect showed there: the
  reference lock's renew, refused (412 on the quoted ETag), re-read the unchanged lock as its own
  and reported success without moving the expiry; with the dialect the renew really writes.

## As built: four-way review fixes (2026-11-01)

- **The real backend.** `StorageBackend::bucket_home` (router: route, cached resolution, or the
  shared HeadBucket round; `Undetermined` on a failed HEAD) and `bucket_home_now` (memory only,
  resolves in the background). `AppState::atomic_write_block` (async, request path: S3 verbs,
  CreateBucket, form POST, admin bulk, backfill, boot advice) and `atomic_write_block_now` (the
  maintenance gate's write-refusal probe, `BackendCheck::gated`). An undetermined home of a bucket
  without a policy is pending unless every encrypting site is CAS. Source guard
  `no_caller_judges_an_unresolved_bucket`. `atomicity::bucket_backend` gives the re-encrypt job
  (`require_for_bucket`, `proven_for_bucket`, `rewrites_are_conditional`) the router's backend for
  a bucket without a policy.
- **Unknown is pending.** `WriteBlock::{Pending {backend, unverified}, ReadOnly {backend}}`:
  no verdict or `Unknown` → 503 SlowDown; `NonCas` → 403. Action refusals stay strict.
  `merged_verdict`: `Unknown` never replaces CAS/NonCas of the same definition.
- **Cache.** Verdicts per (name, fingerprint), up to 4 per name, the running definition (from
  `set_encrypted_plan`) never evicted; a change of a running verdict notifies `plan_changed`.
- **Watch.** `recheck_after`: none → now; Unknown → 5 s doubling to 60 s; CAS / NonCas never on a
  timer (C6: probe key versions piled up in versioned B2 buckets). `BackendHealthCache::recovered`
  wakes it. `spawn_boot_probes` runs in parallel with the boot health gate (C5); one 15 s deadline
  per probe (5 s for the listing). `/_/ready` reports `atomic_writes` (never unready).
- **Probe cleanup.** `cas_probe::remove_all_versions`, also on the timeout path.
- **Messages.** S3 clients get fixed phrases (`WriteBlock::message`, no backend name, no probe
  error: C3); `admin_message` names backend and reason. No `[atomic_writes: …]` tag.
  `verdict_words` / `proxy_unavailable_reason` say "does not support" vs "could not verify".
- **Admin errors.** `AdminError` carries `atomic_writes` (`From<AtomicityRefusal>`,
  `AdminError::write_block`). GET /backends: `capability_words`, `proxy_unavailable`,
  `atomic_writes_checking`; bucket views `capability_words`.
- **Own writes.** `storage::AtomicityGate` (in `EncryptionPolicies`, kept by the cache): the
  encrypting wrapper keeps no listing token and writes no key claim in a proxy-encrypted bucket on
  a backend without proven atomic writes.
- **Order.** `set_encrypted_plan` before `install_engine` in the transition.
- **Worker.** `settle_gate_switch`: a failed save of a switch to the gate stops the run
  (`FailedTransient`).
- **C1.** The migrate job reads the source in the job read scope and reports adoptions.
- **C4.** Replication into a destination that refuses writes (other than a maintenance job): the
  event consumer skips the events (cursor moves, outbox pruned), marks the rule due, and the rule
  shows `deferred` (`replication::deferral`, one WARN per change); the scheduled full run catches
  up. Lifecycle rows show `deferred`.
- **C8.** The removed `plaintext_reads` error names its backend or bucket and the migrate; the prod
  fixture has an S3 backend with proxy AES; the matrix covers read-only and pending.
- **UI.** Server words, labelled doc links, Check again refreshes the create dialog, Resume and
  Rotate key disabled with reasons, "Checking atomic writes…".

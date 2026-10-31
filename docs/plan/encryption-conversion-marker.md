# Plan: replace `plaintext_reads: serve` with a conversion marker

Status: implemented (2026-10-31). Design v2, after two adversarial reviews
(Fable, DeepSeek V4.1 Flash). Section "As built" lists the deviations from
v2; section "Review log" at the end lists every finding and what v2 did with
it.

## Goal

A bucket that starts to encrypt keeps serving its plaintext history to
clients while the re-encrypt job converts it, with no config flag. The window
has a deadline, closes itself when the job succeeds, and is visible to every
instance that encrypts the bucket, because it is a sealed object in the
bucket, not a config line.

Honest limits (stated in the threat model):

- The marker narrows reads only on instances that already encrypt the bucket.
  An instance still on the old config writes and reads plaintext. Rollout
  order: apply the encryption on every instance, then let the job finish.
- On a versioned backend (B2 is always versioned), the plaintext versions
  survive as noncurrent versions unless the operator asks the job to purge
  them.
- A backend writer can always close the window (delete or spoil the marker):
  availability only. It can never open one.

Non-goals: name encryption; size-preserving bodies; the decrypt direction (a
bucket that stops encrypting keeps reading sealed objects through the
keyring; every sealed object is authenticated, so that mix is safe).

## Facts this plan rests on (verified 2026-10-31)

- Read rule: `seal::read_verdict(raw, key_is_folder, plaintext_ok)`
  (`src/storage/encrypting/seal.rs:258`); its refusal text names
  `plaintext_reads: serve` (`seal.rs:280-300`). `plaintext_ok`
  (`encrypting/mod.rs:849`) and `present_raw` (`mod.rs:871`) are SYNC;
  `present_raw` runs on GET, HEAD, range, copy source, reference source,
  metadata rewrite, reference fence, and from sync `present_entry` (LIST).
  The rule runs on fresh backend metadata even on a metadata-cache hit
  (`retrieve.rs:190` -> `mod.rs:1274`).
- `FileMetadata` has no raw `Last-Modified`; S3 `created_at` comes from
  `dg-created-at` user metadata (`s3/metadata.rs:196`), and a missing
  `Last-Modified` degrades to `Utc::now()` (`metadata.rs:456`).
  `metadata_of_response` has two return points (`:481`, `:552`).
- Key claim `.dg/facts/!!key-claim` (`listing_facts.rs:57`): trait methods
  with no default (`traits.rs:1334`), S3 CAS impl (`s3/claim.rs`), filesystem
  file impl, cache in `encrypting/sentinel.rs`. Body is not sealed.
  `gc_doomed` keeps unparseable facts keys (`listing_facts.rs:360`), so an
  older peer never deletes a new marker.
- `storage_identity(bucket)` = `backend\0real_bucket` (`traits.rs:1345`).
- Re-encrypt job: `kind = "reencrypt"` (`worker.rs:582`), arms the write gate,
  reads each object via `engine.head_listed` -> wrapper `head_object`; a
  refusal becomes `Visit::Failed` (`worker.rs:742`). `begin_page` re-resolves
  the desired key per page (`worker.rs:654`). Enqueue returns `Ok(None)` when
  ANY non-delete job is active on the bucket (`idx_maint_active_bucket_v2`,
  `jobs_db.rs:415`; `api/admin/maintenance.rs:180`).
- Rewrite paths: a plaintext->sealed rewrite changes the body, so it is
  retrieve->store (`transfer.rs:283+`), NOT source-conditional;
  `copy_source_if_match` covers only the metadata self-copy (`copy.rs:180`,
  `objects.rs:228`). `put_passthrough` takes no precondition.
- The proxy is not version-aware (`s3_adapter_s3s/bucket.rs:78`), but runs on
  versioned backends (`storage/s3/versions.rs`).
- Job state is node-local (`deltaglider_jobs.db`, never synced).

## Design

### 1. The marker

Key `.dg/facts/!!conversion` (hidden, reserved, skipped by scans and GC).

Body: `fp` (key fingerprint, in clear, selects the ring entry) + a seal
(AES-GCM-SIV) with AD = `"dg-conversion" ‖ format version ‖
storage_identity(bucket)`. Sealed fields:

- `cutoff`: unix seconds from the S3 server clock (`server_clock::ServerDate`
  on the request just before the PUT); local clock on the filesystem backend.
- `deadline`: `cutoff + window_days` (default 7 days).
- `run`: a random 128-bit run id (NOT the node-local job id).

Opened with the WHOLE keyring of the bucket, like objects. A marker that does
not open, belongs to another storage identity, or is past its deadline counts
as CLOSED (refuse plaintext), with an audit entry `encryption_marker_ignored`
(rate-limited).

Writes: create with `If-None-Match: *`. A `Replace` (deadline extension on
resume) and the close are done BY IDENTITY: read, open, check `run`, then
`If-Match` the ETag just read; on 412 re-read and retry (bounded); a 412 never
means "gone". Non-CAS backend (501): unconditional, as the key claim.

### 2. Two readers, two rules

**Client reads** (GET, HEAD, range, copy source): an unsealed, non-folder
object is served when the bucket writes plaintext, or when the window is open
(valid marker, `now_server < deadline`) and the object's RAW
`stored_last_modified < cutoff + grace` (grace 300 s: backend clock skew,
in-flight uploads). Otherwise refused (500, new message naming the
conversion). Missing `stored_last_modified` refuses.

**The conversion job** reads every unsealed object of the bucket through a
`ConversionReadProof` that only `maintenance::worker` can make (same pattern
as `RefWriteProof`), regardless of cutoff. Objects with
`stored_last_modified >= cutoff + grace` are converted too, but counted and
listed as `adopted_after_cutoff` in the run summary and the audit. This is
what `serve` does today; the operator now sees the list.

Plumbing (sync/async split): an async `window_of(bucket) -> Window` (cached,
section 3) is awaited by the async GET/HEAD/range/copy/reference handlers and
passed into the sync `present_raw`. LIST keeps showing unsealed entries as
today (it never serves content).

`FileMetadata::stored_last_modified: Option<DateTime<Utc>>`: set only from
the backend's own timestamp (S3 `Last-Modified` at both return points of
`metadata_of_response`; filesystem mtime); `None` when absent (never
`now()`); never written to the bare metadata map (`types.rs:551`). Source
guard: no other assignment.

`read_verdict` becomes a pure function of `(raw, key_is_folder, Window,
Reader)`, truth-tabled and proptested, including version rollback and replay
cases.

### 3. Marker cache

Per wrapper and bucket: `Open{cutoff, deadline, run, etag} | Closed`, TTL 60 s.
A client plaintext refusal while `Closed` is cached re-reads the marker once
(single-flight per bucket, at most every 5 s) before it refuses: a window
opened on another node takes effect at once. A stale `Open` after the close
serves only pre-cutoff objects, which the job converted. Deadline is checked
on every use, not only on refresh.

### 4. Starting a conversion (apply)

For each bucket that an apply makes start to encrypt (`encryption_changes`
already computes the set) and that holds at least one object:

1. Pre-commit, under the config pipeline (`run_internal_held`):
   - refuse the apply (409, names the job) if a non-delete job is active on
     the bucket;
   - if a valid marker already exists for this bucket: reuse it (idempotent,
     no 412 dead end);
   - else write the marker; writes run in parallel (bound 8); if any fails,
     delete the markers this apply wrote and fail the apply.
2. Commit the config.
3. Post-commit: enqueue one `reencrypt` job per bucket. If the enqueue loses a
   race (`Ok(None)`), the apply still succeeds with a warning; the admin view
   shows "conversion open, no job running" with a **Resume** action.
4. A versioned backend bucket (`GetBucketVersioning` = Enabled/Suspended):
   the apply response and the UI dialog warn that plaintext versions stay as
   noncurrent versions; the job takes `purge_plaintext_versions: bool`
   (default false; UI asks).

Key rotation during an open window: the marker opens with the keyring, so a
rotation keeps the window; the job re-resolves the key per page as today; a
rotation apply never re-creates the marker.

### 5. The job

- Phase order: marker check -> objects -> references -> (optional) purge of
  noncurrent plaintext versions of converted keys -> close.
- Per page: bounded parallel visits (8); cursor persisted after the page;
  rewrites idempotent (`needs_rewrite` skips stamped objects).
- Failures are classified: `rewrite_failed` (retryable) vs
  `adopted_after_cutoff` (converted, reported). Only `rewrite_failed > 0`
  keeps the window open; the run settles `completed_with_errors` and shows
  Resume.
- Resume: extends the marker's deadline by identity (Replace), then continues
  from the cursor.
- Close: by identity (section 1). Audit `encryption_conversion_closed` with the
  adopted-after-cutoff count.
- Deadline passed while the job is not running: the window is closed for
  clients (refuse), audit + UI alarm "conversion stopped before it finished";
  Resume reopens it with a new deadline and the ORIGINAL cutoff.
- While a marker exists, `backfill-metadata` is refused (it would move
  `Last-Modified` past the cutoff).
- Write gate: KEPT in this plan for the object walk and references. Dropping it
  needs a conditional `put_passthrough` (`If-Match` on the listed ETag, 412 =
  skip as changed) plus a CAS probe per backend; non-CAS backends keep the
  gate. That is a separate plan (P7 below, optional).

### 6. Config, compatibility, admin

- `plaintext_reads: serve` is honoured UNCHANGED for one release, with a
  deprecation warning at load, in `config lint`, and a UI banner with
  **Convert now** (starts the section-4 flow, then removes the line). No
  automatic job at boot. The next release rejects the field; `refuse` is
  accepted and ignored until then.
- Removed now: the 422 gate, `X-DGP-Confirm-Plaintext-Refusal`,
  `--confirm-plaintext-refusal`, `buckets_refusing_plaintext` (the boot check
  instead skips buckets with an open marker).
- Admin views: `serves_plaintext` -> `conversion: { open, cutoff, deadline,
  run, adopted_after_cutoff, job_on_this_node }`. Job ids are node-local, so
  the view shows the run id as text and links to a job only when this node
  has it.
- `token_heal::decide_miss`: "window open" replaces `plaintext_ok`.
- Source guard `no_plaintext_reads_remains` after the compat release.

Call sites to change (from both reviews): `seal.rs:258-300` (rule + refusal
text), `encrypting/mod.rs:833-898`, `construction.rs:466,566`,
`config/mod.rs:381-508`, `config/check.rs:348`, `config/registry.rs:143`,
`config/encryption_state.rs` (whole plaintext part),
`api/admin/config/transition.rs:410-456`, `field_level.rs:133-198`,
`bucket_encryption_view.rs:42-156`, `api/admin/backends.rs:44,264,420`,
`engine/buckets.rs:80-98`, `startup.rs:1897`, `main.rs:178-294,894`,
`cli/config.rs:477-543`, `token_heal.rs:96-121`, `maintenance/mod.rs:441-473`
(fixtures), `deltaglider_proxy.example.yaml:113`. UI:
`backendEncryptionPayload.ts`, `bucketEncryption.ts`,
`ServesPlaintextBanner.tsx`, `BucketEncryptionGroup.tsx`,
`BackendEncryptionEditor.tsx`, `BackendsPanel.tsx`, `adminApi/core.ts`,
`adminApi/backends.ts` + 4 tests. Docs: 12 pages under `docs/product` +
`CLAUDE.md`. Tests: listed in the code map (seal, token_heal, encrypting,
bucket_tests, transition, encryption_state, engine, bucket_keyring_test,
encryption_test, maintenance_reencrypt_test).

### 7. Observability

`deltaglider_plaintext_reads_total{outcome=served_in_window|refused}`,
`deltaglider_conversion_adopted_after_cutoff_total`; audit
`encryption_conversion_opened|extended|closed|close_blocked`,
`encryption_marker_ignored`.

## Phases

| Phase | Content | Done when |
|---|---|---|
| P1 | `stored_last_modified` (S3 both return points, filesystem, never `now()`, not in bare map) + source guard | unit tests |
| P2 | Marker: trait methods (no default) on every wrapper, S3/filesystem impls, seal format with storage identity + deadline, open with keyring, identity-based replace/close, cache + refusal refresh | truth table + proptest (forge, replay, cross-backend, deadline, rollback); MinIO CAS test |
| P3 | Two-reader rule (`Window`, `ConversionReadProof`), async `window_of`, refusal text | `read_verdict` truth table; integration: client reads stay up, planted-after-cutoff refused for clients, adopted by the job and listed |
| P4 | Job: marker lifecycle, classification, resume extends deadline, close by identity, backfill refused, parallel visits (8) | integration: crash/resume, cancel, close blocked by a re-PUT, rotation mid-window, stale-config peer |
| P5 | Apply flow: active-job refusal, idempotent marker, compensation, post-commit enqueue + orphan Resume, versioning warning, `serve` compat + deprecation, removal of the 422 gate/header/flag | transition tests; prod-shape fixture unchanged |
| P6 | UI: dialog with object count + versioning warning, progress banner, Resume, deadline alarm, Convert now for `serve` buckets | vitest + e2e |
| P6b | Optional purge of noncurrent plaintext versions | MinIO versioned-bucket test |
| P7 | Docs (threat model incl. honest limits, encryption reference, how-tos), CHANGELOG | docs checks |
| P8 (separate plan) | Drop the write gate: conditional `put_passthrough` + CAS probe; non-CAS keeps the gate | race test: a client overwrite during a rewrite is never lost |

## As built

Phases P1-P7 and P6b are on `main` (P7 = the product docs and the
CHANGELOG). P8 (drop the write gate) is not done: the re-encrypt job still
arms the write gate. Deviations from v2, from the commits:

- **Filesystem marker.** The file `.dg-conversion` in the bucket directory,
  beside `.dg-key-claim`; no client key reaches it (object keys map under
  `deltaspaces/`). The key claim and the marker share one small-object CAS
  helper (`s3/claim.rs`), which reads at most 4 KiB.
- **Seal key.** Derived from the site key (`v2::SiteKey::seal_conversion`),
  AES-256-GCM-SIV; body `dgc1.` hex(fp) `.` b64url(nonce | seal).
- **Trait shape.** `read_conversion_marker` / `write_conversion_marker`
  (raw I/O) plus one `conversion(bucket, ConversionOp)` (status, open,
  extend, close, as-built open/close, cached status), no defaults.
  Identity writes retry 3 times; outcomes Done / Absent / NotOurs / Blocked.
- **Window length.** Fixed at 7 days (`DEFAULT_WINDOW_SECS`); no config
  field. The internal open takes 1 to 365 days. A resume sets
  `max(deadline, server_now + 7 days)`, also on an expired marker.
- **As-built open/close skip the witness gate.** The apply is how a
  disagreeing node converges, and the marker is sealed with the new key.
  The apply opens with the config the new engine is BUILT with
  (`OpenAsBuilt`); a later `commit_iam` or held-commit/persist rollback
  removes the markers that the write created (`CloseAsBuilt`, by identity).
- **Cache.** 60 s per bucket; it keeps the marker's fields also past the
  deadline, for the UI's "stopped" state. This node's apply, job and
  Resume write through it. The admin bucket list reads through it
  (`ConversionOp::CachedStatus`), not a backend GET per bucket per list.
  The refresh after a refusal runs at most once per 5 s per bucket.
- **Apply rule.** `starts_conversion` (pure): after = proxy key without
  serve; before = anything a client could read plaintext in (plaintext,
  native SSE, proxy mode without a key) or a key WITH serve. So an apply
  that SETS serve starts no conversion (old behaviour kept), and removing
  serve (UI **Convert now**) starts one. Backup restore starts
  conversions too.
- **Validate count.** Capped at 10,000 objects (`more: true` beyond). The
  section validate names the list `conversion_preview`; the document
  validate (`POST /config/validate`) names the same list `conversions`.
- **Versioning.** `StorageBackend::bucket_versioning` (S3
  `GetBucketVersioning`, Enabled or Suspended).
- **Admin endpoint.** `POST /jobs/reencrypt` takes `open_window: true`
  (helper `maintenance::conversion::open_window`); the boot check names it
  in its warning and skips buckets with an open window.
- **Job.** 8 objects of a page at once (other maintenance kinds stay at 4).
  `close_blocked` is a failure-ring row under the marker key and settles
  the run with an error. `encryption_marker_ignored` is rate-limited to
  once per 10 min per bucket.
- **Purge (P6b).** The rule is "noncurrent versions without
  `dg-encrypted`, of keys whose current object is encrypted in every
  stored form (object, delta, reference); never a current or encrypted
  version, never `.dg/facts/`", NOT an in-memory list of the keys that the
  run converted, so it survives a crash-resume (cursor + count saved per
  page). An object key needs a current form (a client-deleted object keeps
  its history); a `reference.bin` is internal, so its plaintext history
  goes also when the reference was removed (no current plaintext one). It runs only when `objects_failed == 0`. Enabled per job: the
  job's `params` JSON (`ReencryptParams`), set by
  `purge_plaintext_versions` on `POST /jobs/reencrypt`, by the request
  header `X-DGP-Purge-Plaintext-Versions: true` on a config write (only
  for versioned buckets), and by the UI dialog's box. Audit
  `encryption_conversion_versions_purged` per page with deletions.
- **UI.** No path writes `plaintext_reads` any more; a key change on a
  backend no longer copies `serve` forward. The conversion state sits on
  the bucket card's "Now:" line and on the backend card's status block,
  and the bucket list polls every 3 s while a conversion runs.

### After the four-way review (2026-10-31)

- **Run ownership.** The apply, the boot and `open_window` store the
  marker's run id in the job's params (`ReencryptParams::run`). Only a job
  with that run extends and closes the marker; a job without a run
  (rotation, plain re-encrypt) never writes a marker.
- **Adoption.** In a bucket that encrypts, every unsealed object and
  reference that a client could not read is adopted and reported, also
  with no marker of the job's run (deleted, foreign, another run's,
  expired). Only a bucket whose rule is Serves adopts silently. The job
  row counts adoptions (`maintenance_jobs.objects_adopted`, a jobs-only
  column added at open without a schema bump); the failure ring keeps
  at most 100 adoption samples, outside the 200-row failure bound.
- **Open modes.** `OpenMode::Refresh` (apply, boot): a marker that opens
  keeps its run id and gets cutoff = server now and a new deadline, by
  identity. `OpenMode::Resume` (`open_window`): the marker is kept as it
  is, also past its deadline. A create whose read-back or answer failed is
  still `Created` (the run id that the call generated identifies it). An
  apply that stops encrypting a bucket closes its marker (any run) with
  the old engine's keyring. `ConversionOp::Open` always uses the built
  config and no witness check; `Expired` and `OpenedAsBuilt` are gone.
- **Boot (restart-based deployments).** For a bucket that encrypts, has
  plaintext in the 100-object sample, no encrypted object in it, no key
  claim, no marker and no active job, the boot opens the window (Refresh)
  and queues the job of its run (`maintenance::conversion::start_at_boot`).
- **Multi-instance.** Each node's apply queues its own job of the shared
  run (idempotent); the write gate is per node, so the docs require
  stopping client writes on every node, or routing them to the job's
  node, until the job ends. P8 stays a separate plan.
- **Backfill.** The check reads the raw marker and holds the config read
  lock until the job exists.
## Review log (v1 -> v2)

| Finding | Source | v2 decision |
|---|---|---|
| Job refused by its own window, wedges forever | both | Two readers: job reads all via `ConversionReadProof`, lists post-cutoff adoptions |
| Clock skew / MPU straddling cutoff | both | 300 s grace; MPU test across the cutoff |
| Apply commits with no job (`Ok(None)`) | both | Pre-commit active-job refusal; orphan marker shows Resume |
| Second encrypt-start hits 412 forever | DeepSeek | Existing valid marker is reused |
| Partial marker writes on abort | DeepSeek | Compensating delete |
| Rotation closes the window | both | Open with the keyring; `fp` in clear |
| Close blocked by a re-PUT, 412 read as gone | DeepSeek | Close and replace by identity, retry, audit `close_blocked` |
| Open-ended window on failure | DeepSeek | Sealed deadline; Resume extends it |
| Versioned buckets: rollback, plaintext history | both | Stated limit, UI warning, optional purge (P6b) |
| Window not shared with stale-config instances | DeepSeek | Goal rewritten; rollout order documented |
| AD binds bucket name only | both | AD binds `storage_identity` |
| Sync `present_raw` cannot fetch the marker | DeepSeek | Async `window_of`, `Window` passed in |
| `Last-Modified` -> `now()` fallback, two return points | DeepSeek | `Option`, both points, never `now()` |
| P6 gate removal loses writes | both | Gate kept; removal moved to a separate plan with conditional PUT |
| Missed call sites, refusal text | Fable | Listed in section 6 |
| `serve` auto-conversion at boot too aggressive | DeepSeek (Fable kept it) | Honour for one release + Convert now; no boot job |
| Backfill moves `Last-Modified` | Fable | Backfill refused while a marker exists |
| Node-local job ids | DeepSeek | Run id in the marker; node-local link only |
| Drop the cutoff, boolean marker + convert-on-read | DeepSeek | Rejected: the cutoff is what refuses plants during the window; convert-on-read adds writes to GETs. Kept the deadline idea. |
| Filesystem mtime settable, `cp -a` preserves it | both | Accepted for a single-node backend; documented |

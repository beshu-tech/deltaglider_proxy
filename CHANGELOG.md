# Changelog

## Unreleased

### Fourth-wave correctness fixes (SigV4 integrity + replication provenance + stubs)

Eight findings from the latest review. Two highs are silent
correctness/security failures; the rest tighten honesty in stub
handlers and replication-control endpoints.

**H1 — SigV4 didn't verify the signed payload hash against the body**
A credentialed client could sign hash A and ship body B; the
signature was computed over the canonical-request which only sees
the header value. SigV4's integrity contract requires the receiver
to verify the body downstream — we documented that as a future
guarantee but never implemented it. Fix: the auth middleware
stashes the signed `x-amz-content-sha256` value in a
`SignedPayloadHash` request extension; `put_object_inner` and
`upload_part` recompute the body's SHA-256 and constant-time-compare.
Mismatch → 400 BadDigest. UNSIGNED-PAYLOAD and STREAMING-* sentinels
are recognised and skip verification (they have their own contracts;
see aws_chunked.rs for the streaming variant we don't yet validate
end-to-end). Three integration tests cover signed-mismatch reject,
unsigned-payload pass-through, and the signed-match happy path.

**H2 — replication delete pass deleted unrelated destination objects**
Pre-fix, `replicate_deletes=true` listed every key under
`destination.prefix`, mapped each back to source via prefix-rewrite,
and deleted on source-NoSuchKey. With no provenance marker, an
operator-placed object or a sibling rule's data could be wiped.
Fix: every replicated object carries a `dg-replication-rule = <name>`
user-metadata key. The delete pass only considers candidates whose
metadata carries THIS rule's marker; HEAD-fallback is used when the
listing didn't surface user-metadata; HEAD failure preserves
(false-delete is much worse than a leftover). Integration test seeds
both replicated AND manual objects on destination, runs replication,
then deletes a source key; verifies the replicated key gets deleted
on the next run while the manual one survives.

**M1 — pause/resume created ghost rule rows**
Both handlers called `replication_ensure_state` BEFORE looking up
the rule in config, so a request for a non-existent rule inserted
a state row even though the response was 404. Fix: new
`rule_in_config()` helper runs first; ensure_state only fires
when the rule actually exists. Test verifies that 404 on a ghost
rule leaves the overview clean (no orphan row).

**M2 — run-now bypassed enabled flags**
`replication.enabled=false` and `rule.enabled=false` were honoured
by the (future) scheduler but admin-triggered run-now ignored
both. Fix: explicit gates that 409 with a descriptive message.
Two tests cover the global and per-rule cases.

**M4 — ACL/versioning PUT stubs returned fake 200**
`PUT /bucket?acl`, `PUT /bucket?versioning`, and `PUT /bucket/key?acl`
returned 200 OK while silently discarding the request body. Clients
believed grants/versioning had been applied. Fix: all three return
501 NotImplemented (preceded by a bucket/object existence check so
404 wins when the target is missing). Five integration tests cover
both 501-on-existing and 404-on-missing.

**L1 — tagging PUT/DELETE returned 501 even on missing target**
The previous wave fixed GET ?tagging precedence; PUT/DELETE were
still 501-first. Fix: each calls head/head_bucket before returning
501 so NoSuchKey/NoSuchBucket wins. Three tests cover PUT-on-missing,
DELETE-on-missing, and DELETE-on-existing-object (still 501).

**M3 verified intact** — bucket-subresource existence checks (?location,
?versioning, ?uploads) added in the previous wave still in place.

**L2 verified intact** — `size_is_known = file_size > 0 || !md5.is_empty()`
discriminator in `build_object_headers` correctly emits
`Content-Length: 0` for known-zero managed objects.

Tests: 7 new integration in `replication_test.rs` and `s3_correctness_test.rs`,
3 new auth integration tests in `auth_integration_test.rs`. Full
suite: 659 lib + 28 s3_correctness + 8 replication + 28 auth +
existing suites green. Clippy clean.

### Third-wave correctness fixes (replication + S3 conditionals + headers)

Nine findings from a follow-up review of the replication v1 commits
plus a couple of latent bugs in PUT / bucket subresource paths.

**H1 — replication only copied the first page**
`run_rule` listed one page with `batch_size`, copied it, finished
`succeeded`, and never resumed. A bucket with 50k keys and
`batch_size=100` was 99.8% unreplicated while the dashboard reported
success. Fix: full pagination loop. Per-page `continuation_token`
persisted via `replication_set_continuation_token` so a crash mid-
run resumes from the last batch. Cleared on a clean complete pass.
Bound `MAX_PAGES_PER_RUN = 10_000` defends against pagination
loops in pathological backends. Integration test seeds 17 objects
with `batch_size=5` (≥4 pages).

**H2 — replicate_deletes was config-only, no implementation**
The flag was validated, documented, and ignored. Fix: new
`run_delete_pass` runs after the forward copy when
`rule.replicate_deletes`. Paginates the destination prefix, HEADs
each key on source, deletes destination on `NoSuchKey` source.
Other source-HEAD errors preserve the destination key (false-
delete is worse than a leftover). Skips the delete pass when the
forward pass hit a fatal error (otherwise a transient list
failure could trigger a destination-wide wipe). Integration test
seeds an orphan and verifies it's deleted while legitimate keys
survive.

**H3 — replication lost multipart-ETag identity**
`copy_one` always called plain `engine.store`, so a destination
HEAD returned a fresh full-body MD5 instead of the source's
`"abc-N"` multipart format. Fix: when source's
`FileMetadata.multipart_etag` is `Some`, route through
`engine.store_with_multipart_etag` (the H1 helper from the prior
wave). Integration test creates a real multipart upload on
source, replicates, and asserts source/destination ETags match.

**M1 — replication status="succeeded" with partial failures**
Pre-fix the status only flipped to `failed` when EVERY copy
errored, so dashboards reading `last_status` got a silent partial
failure. Fix: status is `failed` whenever ANY object copy or
delete errored (or a fatal list/planner error happened); else
`succeeded`. The per-object failure ring still captures the full
error list; the status string is now just an honest summary.

**M2 — PUT Object ignored conditional headers**
`If-Match` / `If-None-Match` / `If-Modified-Since` /
`If-Unmodified-Since` were silently dropped, breaking
compare-and-swap and the canonical `If-None-Match: *` idempotent-
create primitive. Fix: new `evaluate_put_conditionals` runs
BEFORE `engine.store` and 412s on any precondition failure. AWS
PUT semantics: all four return 412 (no 304 — that's GET-only).
Two integration tests cover idempotent-create + CAS.

**M3 — bucket subresources answered for ghost buckets**
`GET ?location` / `GET ?versioning` / `GET ?uploads` returned 200
for buckets that didn't exist. Fix: new `require_bucket_exists`
helper at the top of each subresource branch. Three integration
tests pin the 404 contract.

**M4 — tagging stubs returned 501 even for missing resources**
The 501-NotImplemented stubs at GET `?tagging` (object + bucket)
swallowed the head-fail with `let _ = ...; // drain error` and
returned 501 unconditionally. AWS returns 404 first in this case.
Fix: propagate the head error so 404 NoSuchKey/NoSuchBucket wins.
Two integration tests pin the precedence.

**L1 — zero-byte managed objects omitted Content-Length: 0**
`build_object_headers` only emitted Content-Length when
`file_size > 0`, conflating "unknown streaming size" with "known
zero". Clients waiting for the response body got hung up on the
missing terminator for empty managed objects. Fix: new
discriminator `size_is_known = file_size > 0 || !md5.is_empty()`.
Managed objects always carry the canonical empty-MD5
(`d41d8cd98f00b204e9800998ecf8427e`) for empty content, so the
known-zero branch now emits `Content-Length: 0` while the
unknown-streaming branch (empty md5 + file_size = 0) still
omits it for chunked-transfer compatibility.

**L2 — copy-source conditional combos drifted from AWS spec**
AWS S3 CopyObject pairs `if-match` ↔ `if-unmodified-since` and
`if-none-match` ↔ `if-modified-since` with **positive-header
precedence**: when if-match passes, the date check is ignored
entirely. Pre-fix, our linear evaluator rejected requests AWS
would have accepted. Fix: explicit pair-aware evaluator —
if-match present → if-unmodified-since suppressed; if-none-match
present → if-modified-since suppressed. Solo-header behaviour
unchanged. Integration test verifies the if-match-passes-with-
stale-if-unmodified-since combination.

Tests (3 unit + 11 integration new): all under
`src/replication/worker.rs`, `tests/replication_test.rs`,
`tests/s3_correctness_test.rs`. Full suite:
659 lib tests + all integration green. Clippy clean.

### Lazy bucket replication (v1: run-now via admin API)

First cut of scheduled source→destination replication. Built around
the invariant that all copies route through `engine.retrieve` →
`engine.store`, so replication is transparent to per-backend
encryption and delta compression. Operators author rules in YAML
(`storage.replication.rules[]`); runtime state (progress, history,
failures) lives in the config DB.

Scope of this commit stream:

- **Config shape**: new `ReplicationConfig` / `ReplicationRule` /
  `ReplicationEndpoint` / `ConflictPolicy` types with full YAML
  round-trip. Static validation (rule-name regex, humantime
  interval parsing, minimum intervals, batch-size bounds, glob
  compilation) + multi-hop cycle detection in `Config::check`.
- **Pure planner**: `rewrite_key` + `should_replicate` + `plan_batch`
  in `src/replication/planner.rs` — I/O-free, heavily unit-tested
  (15 tests covering the conflict matrix, glob filtering, DG-internal
  skip, directory-marker skip).
- **Persistent state** (`ConfigDb` schema v6): new
  `replication_state` / `replication_run_history` /
  `replication_failures` tables. Boot-time `reconcile_on_boot`
  sweeps zombie `status='running'` rows from a prior crashed
  process. Rules removed from YAML CASCADE-delete their history.
- **Worker**: `src/replication/worker.rs::run_rule` — executes one
  pass of a single rule. Takes `Arc<Mutex<ConfigDb>>` and
  reacquires the lock at each sync boundary so the handler future
  stays `Send` (rusqlite's `Connection` is `!Sync`).
- **Admin API** (session-gated, not IAM-gated):
    - `GET  /_/api/admin/replication` — overview of all rules + state
    - `POST /_/api/admin/replication/rules/:name/run-now` — trigger
      a synchronous run; returns 409 Conflict on a paused rule
    - `POST /_/api/admin/replication/rules/:name/pause` + `/resume`
    - `GET  /_/api/admin/replication/rules/:name/history?limit=N`
    - `GET  /_/api/admin/replication/rules/:name/failures?limit=N`

YAML shape:

```yaml
storage:
  replication:
    enabled: true
    tick_interval: "30s"
    max_failures_retained: 100
    rules:
      - name: prod-to-backup
        enabled: true
        source: { bucket: prod-artifacts, prefix: "" }
        destination: { bucket: backup-artifacts, prefix: "" }
        interval: "15m"
        batch_size: 100
        replicate_deletes: false        # default
        conflict: newer-wins            # newer-wins | source-wins | skip-if-dest-exists
        include_globs: []
        exclude_globs: [".dg/*"]
```

**Scope deferred to a later commit stream:**
- Background scheduler loop (today, only `run-now` triggers the
  worker). Periodic ticks, continuation-token resumption of long
  runs, graceful shutdown are planned but not yet wired.
- Delete replication (`replicate_deletes: true` is validated but
  not implemented by the worker).
- Multi-hop copies or the admin UI panel.

Dependency added: `humantime = "2"`.

Tests: 15 unit (planner) + 8 unit (state_store) + 2 unit (worker)
+ 2 integration (end-to-end via spawned proxy). Clippy clean.

### Second-wave correctness fixes (H1/H2 + M1–M4 + L1)

Seven additional findings from a follow-up review of the multipart,
copy, tagging, and pagination surfaces.

**H1 — Multipart ETag was inconsistent across Complete vs HEAD/LIST**
CompleteMultipartUpload returned `"md5(concat)-N"` but the persisted
FileMetadata carried a fresh full-body MD5. Clients caching the
Complete ETag as an If-Match precondition hit 412 on the next write.
Fix: new `FileMetadata.multipart_etag: Option<String>` threaded
through `engine.store_with_multipart_etag` / `store_passthrough_
chunked_with_multipart_etag`; `etag()` returns the override when set.
Persisted on both backends (xattr round-trip, S3 user-metadata
`dg-multipart-etag`).

**H2 — DeleteBucket ignored active multipart uploads**
DeleteBucket returned 204 with MPU state still alive for that bucket.
After the delete, ListMultipartUploads reported ghost uploads and
UploadPart (M1 below) silently accepted bytes into the orphan state.
Fix: `MultipartStore.count_uploads_for_bucket` gate in the handler;
409 BucketNotEmpty with MPU wording in the error message.

**M1 — UploadPart / UploadPartCopy didn't check bucket existence**
The C2 wave added `ensure_bucket_exists` to Initiate and Complete but
missed the in-between part upload paths. Attacker could Initiate, have
the bucket deleted, and keep feeding parts. Fix: both UploadPart and
UploadPartCopy now call `ensure_bucket_exists` (UploadPartCopy also
checks the source bucket).

**M2 — CopyObject ignored x-amz-copy-source-if-\* preconditions**
Pre-fix, the four copy-source headers were read from the request but
never evaluated — clients saying "copy only if source is still vX"
got unconditional copies. New pure
`check_copy_source_conditionals(headers, source_metadata)` evaluates
`if-match` / `if-none-match` / `if-modified-since` /
`if-unmodified-since` per AWS spec (all violations return 412, even
the none-match/modified-since variants that would normally be 304
on GET). Wired into both `copy_object_inner` and `upload_part_copy`.

**M3 — invalid x-amz-metadata-directive silently became COPY**
Any value other than case-insensitive `REPLACE` fell through to
`COPY`. A typo like `REPLAC` succeeded with source metadata
preserved — the opposite of the client's intent. Now rejected with
`InvalidArgument` citing the bad value.

**M4 — tagging stubs returned fake 200 while discarding tag state**
Object and bucket PUT/GET/DELETE ?tagging used to return 200/empty
`<TagSet/>`, silently dropping tags the client attached. Clients
using tags for lifecycle, compliance labels, or ABAC would read them
back empty and assume something wiped them. All five handlers now
return 501 NotImplemented — honest "not supported."

**L1 — ListParts / ListMultipartUploads pagination lied**
`max_parts` and `max_uploads` were hardcoded to 1000 with
`is_truncated=false` and empty markers, regardless of input. An
upload with >1000 parts silently dropped the tail. Fix: full
pagination support — `ObjectQuery` + `BucketGetQuery` accept
`max-parts`/`part-number-marker` and
`max-uploads`/`key-marker`/`upload-id-marker`. New paginated helpers
`MultipartStore.list_parts_paginated` + `list_uploads_paginated`
implement tuple-cursor semantics matching AWS.

**Tests**: 5 unit (types) + 2 integration (multipart_etag_test) for
H1, plus 10 integration tests in `tests/s3_correctness_test.rs`
covering H2/M1/M2/M3/M4/L1. 618 lib tests + all integration green.
Clippy clean.

### Security hardening — adversarial review findings (C1–C4 + E1/E2/E4)

A static adversarial review surfaced four critical vulnerabilities and
four edge-case hazards. Every confirmed finding is fixed with
regression tests. No breaking changes for well-behaved clients;
behavioural tightening on attack paths.

**Critical — C1: IAM LIST bypass**
A user with policy `{ resources: ["bucket/alice/*"] }` calling
`GET /bucket?list-type=2&prefix=` previously received every key in
the bucket, because the middleware's `can_see_bucket` fallback
admitted the request and the handler returned unfiltered engine
output. Fix: new `ListScope` request extension marks LIST
authorisations as `Unrestricted` or `Filtered { user }`. When
Filtered, the handler post-filters each key and CommonPrefix
through `user.can(Read|List, bucket, key)`. Unscoped admins pay no
filter cost. A `x-amz-meta-dg-list-filtered: true` response header
signals filtered pages. 10 unit + 5 integration tests pin the matrix.

**Critical — C2: implicit bucket creation via PUT**
`PUT /new-bucket/key` on the filesystem backend previously
succeeded, silently creating the bucket directory as a side effect
of `ensure_dir` → `create_dir_all`. Bypassed `s3:CreateBucket`
equivalence and diverged from the S3 backend contract. Fix: handler
precheck `ensure_bucket_exists` on PUT / COPY (both ends) / Create
+ Complete multipart; backend-level `require_bucket_exists` refuses
writes when the bucket root is missing. 9 tests (5 unit + 4
integration) including filesystem-level "no directory was created"
assertions.

**Critical — C3: multipart in-memory DoS**
`upload_part` previously accepted arbitrarily many parts of
arbitrary size, capped only at Complete-time. Attacker opens
`DGP_MAX_MULTIPART_UPLOADS` (default 1000) uploads and fills each
— process OOMs. Three-layer mitigation: per-upload size cap
(cumulative rejects over `max_object_size`), global in-flight byte
counter (configurable via `DGP_MAX_TOTAL_MULTIPART_BYTES`,
defaulting to `max_object_size * max_uploads / 4`), and idle-TTL
sweeper (configurable via `DGP_MULTIPART_IDLE_TTL_HOURS`, default
24h). Sweeper preserves `Completing` uploads regardless of age.

**Critical — C4: multipart complete/abort race**
Pre-fix, `complete()` set a `completed: bool` under a write lock
then released; the handler then awaited `engine.store*`. During
that await, `abort()` happily removed the upload and returned 204
even though the object was about to land. Fix: replace with a
2-state `MultipartState::{Open, Completing}` enum; abort returns
`InvalidRequest` when state is Completing; handler now calls
`finish_upload` on store success or `rollback_upload` on failure.
8 unit tests cover every legal and illegal transition.

**Hygiene — E1: recursive DELETE no longer materialises full listing**
`DELETE /bucket/prefix/` (trailing slash) previously called
`list_objects(..., u32::MAX, ...)`, materialising the full prefix
in memory before deleting anything. Fix: paginate in 1000-key
windows. Memory stays O(page_size) regardless of prefix depth.
Integration test seeds 1100 objects to exercise the second page.

**Hygiene — E2: conformance marker for `get_passthrough_stream_range`**
The trait's default impl buffers the full object — correct as a
fallback, disastrous as the hot path for a ranged GET. Added a
source-walk conformance test that fails if any in-tree
`impl StorageBackend for X` block omits the override. Prevents a
new backend (future B2, GCS, etc.) from silently inheriting the
default.

**Hygiene — E4: sanitise internal error text to S3 clients**
Backend errors like `StorageError::Other` (with filesystem paths or
MinIO debug strings) and `EngineError::ChecksumMismatch` (with
computed + expected SHA hashes) used to get stringified into error
XML. Fix: new `sanitise_for_client` helper returns a generic
"Internal server error" to the client while logging full detail to
`tracing::error!` under a `dgp::sanitised_error` target.

Env-var knobs added:
- `DGP_MAX_TOTAL_MULTIPART_BYTES` (bytes, defaults to
  `max_object_size * max_uploads / 4`)
- `DGP_MULTIPART_IDLE_TTL_HOURS` (hours, default 24)

**Tests**: 613 lib tests + 19 new integration tests green. Clippy
clean.

**Deferred to a follow-up PR**: E3 (SigV4 chunk-signature
verification) ships under a feature flag.

### Declarative IAM — quality pass + convenience endpoints

Follow-up to the initial 3c.3 shipment (see below). Two reviews
(clean-code hygiene + correctness x-ray) surfaced 10 findings; the
real ones are fixed with regression tests, and two long-promised
operator-convenience endpoints land here:

**Correctness fixes**

- **Critical — mapping_rules wipe on idempotent re-apply**
  (`src/config_db/declarative.rs`). The old `Vec` + ambiguous helper
  couldn't distinguish "YAML matches non-empty DB, keep" from "YAML
  empty, wipe DB". Replaced with an explicit `MappingRulesAction::
  {Keep, ClearAll, ReplaceWith(Vec)}` enum set by `diff_iam`. Before
  the fix, every GitOps reconcile loop on a non-empty rule set
  silently wiped the table — next OAuth login had no mappings.
  Regression tests pin the tri-state matrix.

- **High — `permissions_equal` now normalises case before compare**.
  YAML authored with `effect: "allow"` used to mark every user as
  changed on every apply (DB stores canonical `"Allow"`). The diff
  now normalises both sides first.

- **High — misleading docs on `${env:NAME}` syntax**. The docs
  claimed env-var substitution worked; no implementation exists.
  Docs amended to reflect reality ("plaintext or materialise from
  secret manager at deploy time"); env-substitution stays on the
  roadmap for a later phase.

- **Medium — access-key swap validation**. A YAML that swaps
  access_keys between two surviving DB users now surfaces as a
  clean validation error ("user 'alice' collides on access_key_id
  with existing DB user 'bob'") instead of an ugly mid-transaction
  SQLite UNIQUE failure.

- **Medium — short-circuit step 4c on unrelated PATCHes**.
  `apply_config_transition` used to run the full reconcile +
  `rebuild_iam_index` + `trigger_config_sync` on every PATCH in
  declarative mode, even ones that only touched `log_level` or
  `cache_size_mb`. Now short-circuits when the IAM fields are
  unchanged from the previous config.

- **Low — no-op reconcile no longer triggers S3 sync upload** or
  surfaces a spurious "reconciled:" warning. Idempotent GitOps
  loops now leave the sync bucket alone.

**Hygiene**

- `ReconcileStats::audit_entries() + summary_line()` collapse a
  10-block audit-log loop + two independent format strings into
  single helpers.
- `replace_group_permissions` + `replace_user_permissions` are
  now 3-line delegates to a shared `replace_permissions(tx, table,
  fk, owner_id, perms)`.
- Vestigial `mapping_rules_need_clearing` helper (the bug's
  surface) is gone.

**Operator convenience**

- **Reconciler preview on `/validate`**. `POST /_/api/admin/config/
  section/access/validate` with a declarative-mode body now
  returns a preview warning line: `"declarative IAM preview:
  users(+1/~2/-0) groups(+0/~1/-0) providers(+0/~0/-0)
  mapping_rules=keep"`. The admin-UI's ApplyDialog surfaces it
  under Warnings. Runs the same `diff_iam` the live apply does
  — preview can't drift from reality. Also previews the
  empty-YAML gate refusal so operators catch the problem at
  dry-run time.

- **Export-as-declarative endpoint**. `GET /_/api/admin/config/
  declarative-iam-export` returns a self-contained `access:`
  YAML fragment with `iam_mode: declarative` + populated
  `iam_users` / `iam_groups` / `auth_providers` /
  `group_mapping_rules` projected from the current DB. Secrets
  redacted (operator wires via env). Roundtrip contract:
  exported YAML (with secrets re-injected) → PUT is an
  idempotent no-op. Makes Workflow A ("import existing DB into
  GitOps") a one-button operation instead of hand-assembly.

580 unit tests + 10 declarative integration tests + all encryption
integration tests green. Clippy `-D warnings` clean. Rustfmt clean.

### Declarative IAM reconciler (Phase 3c.3)

`iam_mode: declarative` now actually reconciles. Previously it was a
pure lockout (admin-API IAM mutations returned 403, but nothing synced
YAML → DB). The reconciler runs on every `/config/apply` or
section-PUT on `access` when the target mode is Declarative:

- **Pure diff first, side effects second.** `diff_iam` validates the
  YAML (unique names, valid group refs, valid permissions, no
  access-key collisions, no reserved `$`-prefixed names) and computes
  creates / updates / deletes. Any validation failure returns an
  error with ZERO DB writes.
- **Atomic apply.** `apply_iam_reconcile` runs all mutations in a
  single SQLite transaction. Partial failures roll the whole
  reconcile back; state never observed mid-apply.
- **ID preservation.** Users and groups are matched by NAME, not by
  DB id. Rotating an access key in YAML is an UPDATE that preserves
  the row id — so `external_identities` linked to that user stay
  valid. `external_identities` themselves are NOT reconciled (they
  are runtime OAuth byproducts) but ARE cascade-deleted when a
  YAML-authoritative delete removes the user or provider they
  reference.
- **Empty-YAML gate.** A `gui → declarative` flip with empty
  `iam_users` / `iam_groups` is refused — it would wipe the DB.
  Declarative → declarative with empty YAML is allowed
  (operator deliberately clearing all IAM).
- **Audit trail.** Every mutation emits an `iam_reconcile_*` audit
  entry (`_user_create` / `_update` / `_delete`, same for groups
  and providers, plus `_mapping_rules_replaced`).

Wire shape: `access.iam_users` / `iam_groups` / `auth_providers` /
`group_mapping_rules` on `AccessSection`. Users reference groups by
NAME; mapping rules reference providers and groups by NAME. IDs are
ephemeral and must not appear in YAML.

See `docs/product/reference/declarative-iam.md` for the full
walkthrough (including both workflows — importing an existing
populated DB, or authoring fresh IAM from YAML — and the complete
adversarial-edge table).

### Backends panel: legacy singleton now surfaces correctly

`GET /api/admin/backends` used to return an empty list when the
config used the legacy `storage.backend` singleton (vs the named
`storage.backends` list), so the admin UI showed "No named backends.
Using legacy single-backend mode." while the proxy was actively
serving. `GET /api/admin/config` already synthesised a `"default"`
entry to keep the UI functional elsewhere; `/backends` now does the
same.

Synthesised entries carry `is_synthesized: true` in the response so
the UI can disable destructive actions (they don't correspond to a
real `cfg.backends[]` row; `DELETE` would 404). The Backends panel
renders them with a "LEGACY SINGLETON" badge and no Delete button.

## v0.8.10

A QA-focused release. No user-facing behaviour changes in the S3 API
or delta codec. Two new admin endpoints, targeted new coverage for
hot paths, and a sharper test suite overall.

### New admin affordances

- **`POST /api/admin/config/sync-now`** — operator-triggered pull
  from the config-sync S3 bucket. Previously only reachable by
  waiting up to 5 minutes for the periodic poll. Useful for
  multi-replica deployments wanting immediate IAM propagation after
  an out-of-band mutation. Returns 404 when `config_sync_bucket` is
  unset, 200 with `{downloaded, status}` otherwise.
- **`GET /api/admin/iam/version`** — monotonic counter bumped on
  every IAM index rebuild (user/group CRUD, OAuth provider change,
  mapping-rule edit). Public on purpose — exposes only an opaque
  number. Powers the `wait_for_iam_rebuild` test barrier and is
  generally useful for scripting "wait for propagation."

### Shared helper moved into the library

`reopen_and_rebuild_iam` relocated from `src/startup.rs` (binary)
to `src/config_db_sync.rs` (library) so the admin `sync-now`
handler can reach it without duplicating DB-reopen + IAM-rebuild
logic. Behaviour unchanged; callers (startup, periodic poller,
sync-now) funnel through one path.

### Test posture overhaul (internal)

Not user-visible but worth recording because it affects CI + dev
loop durability:

- **Deterministic IAM rebuild barrier** (`wait_for_iam_rebuild`)
  replaces two `tokio::time::sleep(1s)` calls in
  `auth_integration_test.rs`. Auth suite went from ~3.5s to ~1.5s
  and is no longer flake-prone on slow CI runners.
- **6 new unit tests** for `classify_s3_error` / `classify_get_error`
  in `storage/s3.rs` — zero unit tests before, now covers the
  Hetzner/Ceph 403-for-missing-bucket quirk, object-level 403
  preservation, and NoSuchKey → NotFound mapping.
- **7 `test_create_bucket_*` integration tests replaced by 3 unit
  tests** in `handlers/bucket.rs` + property-based coverage via
  `proptest` (new dev-dep). ~1500 random cases per run, 0.08s.
- **9 S3-backend duplicate tests deleted** from
  `tests/s3_backend_test.rs` (verified trait behaviour already
  covered by the filesystem-backed suites). Kept 2: SDK-plumbing
  smoke + real delta+S3 interaction.
- **6 of 10 public-prefix LIST integration tests removed** — unit
  tests in `api/auth.rs` and `admission/evaluator.rs` already cover
  the policy-match shape. Kept: no-slash AWS-CLI bug regression,
  false-parent denial, partial-public root denial, admin sees all.
- **2 new integration tests for metadata-cache invalidation** in
  `tests/optimization_test.rs` — covers PUT-overwrite and batch-
  delete paths that the docstring pinned but no test exercised.
- **4 new HA config-sync tests** (`tests/config_sync_ha_test.rs`)
  exercising the real sync code path: startup pull, sync-now
  propagation, ETag no-op, wrong-passphrase rejection.
- **3 new same-key concurrency tests** (`tests/concurrency_test.rs`)
  for double-`CompleteMultipartUpload`, cross-upload-ID isolation,
  and PUT-vs-DELETE state consistency.
- **3 new SigV4 security tests** (`tests/auth_integration_test.rs`)
  covering signed-header tampering, presigned-URL rejection after
  user disable, and unsigned-extra-header tolerance (spec
  compliance).
- **5 new unit tests for `src/startup.rs`** (was 0/650 LOC).
- **Coverage reporting in CI.** New `coverage` job runs
  `cargo-llvm-cov --lib` and publishes a summary table +
  lcov.info artifact. `continue-on-error: true` — signal, not a
  gate.
- **TestServer hardening.** `wait_ready` now checks
  `process.try_wait()` BEFORE the health probe, so a stray proxy
  holding the test port fails loudly (`lsof -i :<port>`) instead
  of silently intercepting requests.

### Internal hygiene

- **Shared `useSectionEditor` hook** in the admin UI collapses
  ~450 LOC of triplicated apply-flow boilerplate across
  AdmissionPanel, CredentialsModePanel, and the Advanced sub-
  panels into one ~220 LOC hook. Future section panels plug in
  rather than re-carrying the §F5 snapshot-at-validate fix etc.
- **`with_config_db()` wrapper** in `src/api/admin/mod.rs` shrinks
  the "get DB from state → lock → run op → log-and-500" admin
  handler boilerplate from 5–8 lines to 3. Migrated 8 handlers
  in `external_auth.rs`.
- **6 duplicate `default_*` fns** in `config_sections.rs`
  eliminated by making the `config.rs` versions `pub(crate)`.
  Round-trip test continues to guard against drift.

### Documentation

- **`CLAUDE.md` refreshed** against recent architecture drift.
  Two new sections: "Testability principles" (7 bullets with
  prior-art citations) and "When proposing architecture" (4
  anti-patterns to avoid).

### Metrics

- **Test count**: 467 → 487 unit/property/binary tests. Integration
  tests net −12 (22 deleted as redundant, 10 added for genuine
  gaps).
- **CI time**: neutral — slower startup-unit-test overhead (~0.5s)
  offset by ~2s saved in auth + bucket-name tests.

## v0.8.0

A major release. Two overlapping threads land together: the
**progressive-disclosure YAML config** (phases 0–3 of
[docs/plan/progressive-config-refactor.md](docs/plan/progressive-config-refactor.md))
and the first waves of the **admin UI revamp**
([docs/plan/admin-ui-revamp.md](docs/plan/admin-ui-revamp.md)).

### Configuration (progressive-disclosure YAML — phases 0–3)

- **YAML is now canonical.** `deltaglider_proxy.yaml` (four-section
  `admission / access / storage / advanced` layout) is preferred
  over `deltaglider_proxy.toml`. Both still load; TOML is
  deprecated with a `tracing::warn!` on every load (silence with
  `DGP_SILENCE_TOML_DEPRECATION=1`). See
  [docs/HOWTO_MIGRATE_TO_YAML.md](docs/HOWTO_MIGRATE_TO_YAML.md).
- **Dual-shape loader.** Sectioned YAML (`admission:`/`access:`/
  `storage:`/`advanced:`) is transparent to the in-memory
  `Config` struct — the flat shape still loads unchanged.
- **Storage shorthand** — a single-backend deployment can write:
  ```yaml
  storage:
    s3: https://example.com
  ```
  which expands to a full `backend: { type: S3, ... }` at load
  time. Filesystem shorthand (`storage: { filesystem: /path }`)
  likewise. Long-form YAML stays valid; shorthand is operator
  convenience only.
- **Per-bucket `public: true`** shorthand expands to
  `public_prefixes: [""]`, and the canonical exporter collapses
  back when unambiguous. GUI "Public read" toggle maps 1:1 to the
  YAML.
- **Admission chain (operator-authored).** New
  `admission.blocks[]` wire format with `match` predicates
  (method / source_ip / source_ip_list / bucket / path_glob /
  authenticated / config_flag) and `action` variants
  (`allow-anonymous`, `deny`, `reject { status, message }`,
  `continue`). Evaluator dispatches live before the synthesized
  public-prefix blocks; reserved `public-prefix:*` name prefix;
  glob compile-check at parse time; 4096-entry cap on
  `source_ip_list`.
- **`access.iam_mode: gui | declarative`.** New lifecycle knob:
  `gui` (default) keeps the encrypted IAM DB as source of truth;
  `declarative` gates admin-API IAM mutation routes (the
  `require_not_declarative` middleware) behind 403 responses. A
  warn-level audit log line fires on every mode transition. The
  reconciler that sync-diffs DB to YAML in declarative mode is
  still Phase 3c.3; today declarative mode is a read-only
  lockout.
- **New admin endpoints** for GitOps + GUI round-trips:
  - `GET /api/admin/config/export[?section=<name>]` —
    canonical-YAML export with every secret redacted; scope-filter
    to one section when `section=` is present.
  - `POST /api/admin/config/validate` — dry-run a full YAML doc.
  - `POST /api/admin/config/apply` — atomic full-document apply,
    with runtime-secret preservation for redacted round-trips.
  - `POST /api/admin/config/trace` — evaluate a synthetic request
    against the live admission chain.
  - `GET /api/admin/config/defaults[?section=<name>]` — JSON
    Schema (via `schemars`) for the Config type, optionally scoped
    to one section for Monaco's per-editor schema.
- **Section-level admin API (Wave 1 of the UI revamp):**
  - `GET /api/admin/config/section/:name[?format=yaml]`
  - `PUT /api/admin/config/section/:name` — partial update,
    routes through the same `apply_config_transition` helper as
    field-level PATCH and document-level APPLY.
  - `POST /api/admin/config/section/:name/validate` — dry-run
    with a diff body (`{section: {field.path: {before, after}}}`)
    for the plan → diff → apply dialog.
  - `GET /api/admin/config/trace` — query-param variant for
    bookmarkable trace URLs.
  - `GET /api/admin/audit?limit=N` — recent entries from the
    in-memory audit ring (newest first; limit clamped to
    `[1, 500]`). Backs the Diagnostics → Audit log viewer
    (Wave 11).
- **CLI subcommands:**
  - `deltaglider_proxy config migrate <toml>` — TOML → YAML
    converter (emits canonical sectioned form).
  - `deltaglider_proxy config lint <yaml>` — offline schema +
    reference-resolution + dangerous-default warnings.
  - `deltaglider_proxy config defaults` — dump every default
    with its doc comment.

### Admin UI revamp (waves 1–11)

All ten waves from
[docs/plan/admin-ui-revamp.md](docs/plan/admin-ui-revamp.md) plus
a follow-on Wave 11 (audit log viewer) have landed as of this
release. Waves 1–3 shipped at tag time; waves 4–8 landed during
live-browser verification; waves 9 (Trace diagnostics), 10
(command palette + shortcuts help), 10.1 (finish §10 polish
items), and 11 (audit log viewer) shipped post-tag and are
rolled into the v0.8.0 entry so the `main` history is
self-describing. §9.1's Dashboard redesign was deferred — the
existing MetricsPage remained functional so the rewrite can
ship independently without blocking operators today.

- **Section-level API client helpers in `adminApi.ts`**:
  `getSection`, `putSection`, `validateSection`, `getSectionYaml`,
  `exportSectionYaml`, `getSectionSchema`, `getFullConfigSchema`.
- **New foundation components** ready for use in waves 4–7:
  - `FormField` — standardised label / YAML-path / help /
    default-placeholder / override-indicator / owner-badge wrapper.
  - `ApplyDialog` — the plan → diff → apply modal (§5.3).
  - `MonacoYamlEditor` — lazy-loaded Monaco + monaco-yaml with
    scoped JSON Schema and mobile fallback (§4.3, §10.4).
- **New hook `useDirtySection`** — per-panel dirty state backed
  by a module-level Set, with `useDirtyGlobalIndicators` for the
  `● ` tab-title prefix and `beforeunload` guard.
- **New sidebar** (`AdminSidebar`) — four-group IA
  (Diagnostics + Configuration). Nested sub-entries for Access /
  Storage / Advanced; amber dot on sections with unsaved edits.
- **URL scheme** — every admin page is a bookmarkable
  hierarchical URL:
  `/_/admin/diagnostics/dashboard`,
  `/_/admin/configuration/access/users`, etc. Legacy flat URLs
  (`/_/admin/users`, `/_/admin/backends`) keep working via
  `LEGACY_TO_NEW` in AdminPage.tsx.
- **Right-rail actions** (`RightRailActions`) visible on every
  Configuration page: Apply / Discard (gated on `dirty`) and
  Copy YAML (section-scoped). In practice the rail was
  simplified during Wave 3's adversarial review to copy-only —
  section-scoped paste and full-document Import/Export run from
  the `YamlImportExportModal` reached through the header actions.
- **YAML Import/Export modal** (`YamlImportExportModal`) reached
  from every admin page via the header actions. **IAM Backup**
  was renamed in the sidebar (was "Backup") to distinguish it
  from the YAML Import/Export surface: IAM Backup ships the full
  encrypted SQLCipher DB (users, groups, OAuth, mappings); YAML
  Import/Export ships the operator config document.
- **Admission block editor** (Wave 4, `AdmissionPanel`) — drag-
  to-reorder list of operator-authored blocks with Form ⇄ YAML
  toggle per row, inline validation matching the server rules
  (name charset, reserved `public-prefix:*` prefix, mutually
  exclusive `source_ip` / `source_ip_list`, 4xx/5xx `reject`
  status), and synthesized `public-prefix:*` blocks surfaced
  read-only below.
- **Access → Credentials & mode panel** (Wave 5) — the first
  screen on the Access section: iam_mode radio (gui /
  declarative with the Phase 3c.3 gap disclosed inline),
  authentication-mode dropdown, bootstrap SigV4 key pair with
  rotate-in-place, and a Change password link.
- **Storage → Buckets panel** (Wave 6) — per-bucket editor with
  tri-state Anonymous read access (None / Specific prefixes /
  Entire bucket). Selecting "Entire bucket" writes the compact
  `public: true` shorthand; "Specific prefixes" writes an
  explicit `public_prefixes` list. The GUI toggle maps 1:1 to
  the YAML.
- **Advanced panel sub-sections** (Wave 7) — the Advanced
  section is split into five dedicated sub-panels (Listener &
  TLS, Caches, Limits, Logging, Config DB sync), each with
  grouped forms, `🔁` restart-required badging, and monospace
  `from DGP_X_Y` chips on env-var-owned fields.
- **First-run setup wizard** (Wave 8) — five-screen guided
  onboarding at `/_/admin/setup`: backend pick → backend config
  (with live Test Connection for S3) → admin credentials →
  optional public bucket → review + apply. Routes to the
  Dashboard on success. Zero-to-working in under three minutes
  on a fresh install.
- **IAM source-of-truth banner** (`IamSourceBanner`) — surfaced
  on every Access sub-panel: explains in plain English whether
  the listed users/groups/providers are DB-managed (iam_mode:
  gui) or read-only because YAML owns the state (iam_mode:
  declarative).
- **AntD 6 shrink fix** — `theme.css` overrides the AntD 6
  radio/checkbox "shrink on click" default that was breaking
  Wave 4's match-action radio groups.
- **Admission trace panel** (Wave 9, `TracePanel`) — the
  `/_/admin/diagnostics/trace` placeholder is replaced by a real
  admission-chain debugger. Synthetic-request form (method /
  path / query / source IP / authenticated toggle), one-click
  example chips, and a result pane built around the Istio /
  Kiali "reason path" pattern: decision tag, matched-block name,
  resolved-request breakdown (method / bucket / key /
  list_prefix / authenticated), and Copy-as-JSON + Clear
  actions. Calls `POST /api/admin/config/trace` under the hood;
  no dirty state (pure read). A "How to read the output" info
  alert demystifies the panes for first-time users.
- **Command palette** (Wave 10, `CommandPalette`) — `⌘K` /
  `Ctrl+K` opens a fuzzy-filter palette over every entry in the
  four-group IA plus shell-scope actions (Export YAML, Import
  YAML, Setup wizard, Keyboard shortcuts, Back to Browser).
  Hand-rolled scorer (substring + subsequence, <50-line
  function); combobox ARIA with `aria-controls` /
  `aria-activedescendant`; group headings ("Recent" / "Navigate"
  / "Actions") rendered non-interactively so Arrow keys skip
  over them. Recent-items MRU (last 5) persists to localStorage
  and surfaces under the "Recent" heading on empty query.
- **Shortcuts reference modal** (Wave 10, `ShortcutsHelp`) —
  `?` opens a platform-aware shortcut list. Mac users see only
  `⌘` rows; Windows / Linux users see only `Ctrl` rows — no
  duplicate "same as ⌘K on non-Apple" noise rows. Detection via
  `navigator.userAgentData.platform` with `navigator.platform`
  + UA-string fallbacks (`platform.ts`). The listener itself
  still accepts BOTH modifiers so a Mac user on a PC keyboard
  still works.
- **`⌘S` / `Ctrl+S` → Apply dirty section** (Wave 10.1, §10.3).
  `useDirtySection.ts` gains `registerApplyHandler(section, fn)`
  + `useApplyHandler(section, fn, enabled)`; handlers are
  stacked per section so the most-recently-mounted panel wins
  when siblings share a section (Wave 5 master-detail).
  `AdminPage` dispatches `⌘S` via `requestApplyCurrent(
  sectionForPath(adminPath))`; falls through to the browser
  default when no handler is registered (Diagnostics pages, no
  dirty state). Wired into Admission, Credentials & mode, and
  all five Advanced sub-panels.
- **Mobile drawer** (Wave 10.1, §10.4) — below 900px the
  persistent sidebar collapses to an AntD Drawer that slides in
  from the left. Hamburger trigger lives in the header. Drawer
  auto-closes on navigation. `useIsNarrow` hook tracks the
  breakpoint live (rotate / devtools-toggle works without
  reload).
- **i18n scaffold** (Wave 10.1, §10.2, `i18n.ts`) — pass-through
  `t(key, fallback)` helper and matching `useT()` hook. Today
  returns `fallback ?? key` — no translation tables yet.
  Purpose: when we add a locale, we swap one file's internals
  and every existing call site continues to work.
- **Audit log viewer** (Wave 11, `AuditLogPanel`) — the admin UI
  now surfaces the server's audit trail at
  `/_/admin/diagnostics/audit`. Backed by a new in-memory ring
  in `src/audit.rs` (bounded `VecDeque<AuditEntry>`; default 500
  entries; override via `DGP_AUDIT_RING_SIZE`). Every
  `audit_log()` call pushes a sanitised copy onto the ring, so
  stdout / JSON log shippers see nothing change — the ring is
  supplementary. New `GET /api/admin/audit?limit=N` endpoint
  (session-gated; not IAM-gated — all admins see the same log).
  Panel renders a monospace table with colour-coded Action tags
  (red = `login_failed` / `delete`, green = `login`, etc.),
  free-text filter across every column, optional 3-second
  auto-refresh, and an "In-memory ring — not a compliance
  substitute" banner so operators don't mistake it for durable
  storage.
- **IAM Backup import preserves `external_identities`** (post-
  manual-review hardening). `POST /api/admin/backup` used to
  silently drop every OIDC identity binding; restoring a backup
  would orphan `user_id ↔ external_sub ↔ provider_id` links and
  returning OIDC users got re-provisioned as new accounts. Import
  now remaps `user_id` + `provider_id` through the existing ID
  maps and falls back to `groups.member_ids` / autoincrement
  heuristics for legacy backups that lack a `bu.id` field. New
  optional `BackupUser.id` field in exports. Idempotent: repeat
  imports skip existing `(provider_id, external_sub)` pairs.
  `ImportResult` gains `external_identities_created` +
  `external_identities_skipped` counters.
- **GUI polish** (post-manual-review round):
  * Groups create form closes and navigates to the Edit view after
    a successful create. Previously the form stayed open with
    stale fields; "+ New" appeared broken until a page reload.
  * ExtAuth mapping-rule "+ Add Rule" flushes pending edits on
    other rules BEFORE create + reload. Previously the reload
    overwrote the in-memory rules array, silently clobbering
    unsaved typing in sibling rows.
  * New mapping rules start with an empty `match_value` and a
    placeholder hint. Previously pre-filled with `*@example.com`
    as a literal default (forcing select-all + delete before typing).
  * Users list label reflects group inheritance: `N groups
    (inherited)` or `N rules · M groups` instead of a misleading
    "No access" for users who have group-inherited permissions.

### Bug fixes / correctness

- **`DGP_BOOTSTRAP_PASSWORD` env override.** `--config <path>`
  now applies env var overrides (including
  `DGP_BOOTSTRAP_PASSWORD_HASH`) on top of the file, matching the
  behaviour of implicit search-path loading. Previously the flag
  made env vars silently ignored.
- **Admission validator** rejects reserved `public-prefix:*`
  names up front.
- **Shape classifier** explicit check-by-key-presence (flat vs.
  sectioned), so typos inside a sectioned doc report section-
  scoped errors rather than "unknown variant" from a fallback
  parse.
- **Section PUT is RFC 7396 merge-patch** (post-tag regression
  fix from live-browser verification). The first cut of
  `PUT /api/admin/config/section/:name` replaced the whole
  section — a PUT with `{max_delta_ratio: 0.42}` silently reset
  `cache_size_mb`, `listen_addr`, and `log_level` to their
  compile-time defaults. Now: present keys apply in place;
  absent keys preserve the current value; explicit `null`
  reverts to the default; array fields (e.g.
  `admission.blocks`) stay atomic. Three regression tests lock
  this in.
- **`advanced.log_level` from YAML applied at startup** (post-
  tag fix). `init_tracing` previously read only from RUST_LOG
  and DGP_LOG_LEVEL env vars, silently ignoring the YAML value;
  runtime always ran at `debug` default unless env was set.
  Now: RUST_LOG > DGP_LOG_LEVEL > config.log_level > --verbose
  > default. Env-driven deployments (Docker + K8s) keep their
  semantics exactly.
- **Legacy admin URLs canonicalise in the browser bar** (post-
  tag fix). Bookmarked v0.7.x URLs like `/_/admin/users`
  resolved correctly but left the address bar on the legacy
  form, spreading the old shape through copy/paste. A
  `useEffect` in `AdminPage` now swaps in the canonical
  hierarchical URL via `navigate(..., replace=true)` without
  adding a history entry.

### Dependencies

Frontend: `monaco-editor`, `monaco-yaml`, `react-hook-form`, `zod`,
`@hookform/resolvers`, `@dnd-kit/core` + `sortable` + `utilities`
(§4.2–§4.4). Backend: `serde_yaml`, `schemars`, `ipnet`, `globset`.

### Breaking changes

None. TOML config is deprecated but still loads; field-level
`PATCH /api/admin/config` remains the stable GUI surface; every
existing integration test passes.

### Migration

Run `deltaglider_proxy config migrate deltaglider_proxy.toml
--out deltaglider_proxy.yaml`, point the server at the YAML, and
delete the TOML when ready. No config rewrites needed on the
YAML side for existing deployments — the sectioned shape is
semantically a superset of the flat shape.

## v0.7.2

### UI Polish & Usability
- **Presigned URL duration selector**: Share button is now a split button — main button generates a 7-day link, chevron dropdown offers 1 hour, 24 hours, or 7 days. Share modal displays "Expires in" label.
- **Version display**: Sidebar branding shows the proxy version (fetched from whoami, not the unauthenticated health endpoint).
- **OAuth user identity**: Sidebar shows the user's email address (from external identity) instead of the truncated access key ID. Username displayed on its own row for readability.
- **Ant Design tooltips removed globally**: CSS nuke (`display: none !important`) on `.ant-tooltip, .ant-popover` prevents all layout-shaking tooltip bugs. All 6 `<Tooltip>` usages replaced with native `title` attributes.
- **Analytics tab padding fix**: Removed double padding (AnalyticsSection had its own wrapper padding on top of the parent MetricsPage padding). Aligned card styles (borderRadius, fontSize, fontFamily, spacing) between Monitoring and Analytics tabs.

### Correctness
- **Analytics stats**: Stats endpoint now calls `list_objects` with `metadata=true`, so delta compression savings are correctly reported (was always showing 0% savings).
- **InspectorPanel crash on zip files**: Fixed React error #310 — `useState` for share duration was declared after the early return, causing hook count mismatch.
- **Whoami returns user info**: `GET /api/whoami` now resolves the logged-in user from the session cookie (name, access_key_id, is_admin). OAuth users show their email, IAM users show their name, bootstrap shows "admin".

### Security
- **Version removed from health endpoint**: `/_/health` no longer exposes `version` field. Version is only available via the authenticated `/_/api/whoami` endpoint.

### Infrastructure
- **Docker debugging tools**: `ntpstat` and `chrony` pre-installed in the runtime image for clock skew diagnosis.

## v0.7.1

### Features
- **Bulk copy/move**: Select multiple objects and copy or move them to a different bucket/prefix via a destination picker modal. Move = copy + delete source (only after all copies succeed).
- **Bulk download as ZIP**: Select multiple files and download them as a client-side ZIP (fflate). Size warning for selections >500MB.
- **Storage analytics dashboard**: New Analytics tab in Metrics page with summary cards (total storage, space saved, savings %, estimated monthly cost savings), per-bucket stacked bar chart, session savings area chart, and compression opportunity identifier.
- **Cost configuration**: Gear icon on the monthly savings card opens a preset selector (AWS S3, S3 IA, Hetzner, Backblaze, Cloudflare R2) with localStorage persistence.
- **Themed error pages**: OAuth error pages (provider not ready, auth failed, account disabled) respect the user's dark/light theme via CSS custom properties and inline localStorage/prefers-color-scheme detection.

### UI Components
- **BulkActionBar**: Toolbar with Copy, Move, ZIP, Delete buttons — replaces the inline delete button when objects are selected.
- **DestinationPickerModal**: Bucket dropdown + prefix autocomplete, preview of destination paths, move-mode deletion warning.
- **AnalyticsSection**: Summary cards, Recharts bar/area charts, compression opportunity cards.

## v0.7.0

### Features
- **OAuth/OIDC external authentication**: Login with Google (or any OIDC provider) via the admin GUI. PKCE, state parameter, nonce, JWT validation. Per-provider configuration with display names and custom branding.
- **Group mapping rules**: Map external identity claims (email domain, email glob, email regex, claim value) to IAM groups. Rules evaluated on every OAuth login; group memberships merged (not replaced).
- **Mandatory authentication**: Proxy refuses to start without authentication credentials unless `authentication = "none"` is explicitly set. Prevents accidental open-access deployments.
- **Per-bucket compression policies**: Enable/disable compression per bucket in the Backends panel. When per-bucket compression is ON but global ratio is 0, uses 0.75 default.
- **Multi-instance config sync**: ExternalAuthManager rebuilds after S3 config sync. Stale pending OAuth flows cleared on provider rebuild.

### UI
- **OAuth login buttons**: Connect page shows branded OAuth provider buttons with "credentials instead" collapsible.
- **Authentication panel**: Full OAuth provider CRUD, mapping rules with local state + Save button (not per-keystroke API calls).
- **Backends panel**: Merged Storage + Compression tabs. Per-bucket policy toggles inline with backend configuration.
- **Custom dropdowns**: `SimpleSelect` and `SimpleAutoComplete` replace broken Ant Design Select/AutoComplete popups.
- **Tab headers**: Centered, typographically consistent headers for all admin settings tabs.
- **Object table fixes**: `showSorterTooltip={false}` prevents sort header shaking. Native `title` attributes replace broken Ant tooltips.

### Security
- **Session cookie fixes**: SameSite=Lax (not Strict) for OAuth redirect compatibility. Secure flag auto-detects TLS.
- **XSS protection**: `escape_html()` on OAuth error page parameters.
- **Rate limiting on OAuth callback**: Prevents brute-force state token guessing.
- **Group membership merge**: OAuth re-login no longer wipes existing group memberships.

### Bug Fixes
- **Bucket not loading on first click**: `s3client.setBucket()` called before React state update.
- **Empty browser after login**: `s3.reconnect()` moved to useEffect (was in Promise callback before state committed).
- **Upload SignatureDoesNotMatch**: Strip leading/trailing slashes from upload destination path.
- **InternalError hiding real errors**: Error message now shows actual error, not generic text.
- **Reference.bin fallback**: GET on aliased bucket paths falls back to passthrough when reference fetch fails.

## v0.6.0

### Features
- **Multi-backend routing**: Route different buckets to different storage backends (filesystem, S3, mixed). `RoutingBackend` implements `StorageBackend` transparently — zero engine changes, shared caches/codec/locks. Configure via `[[backends]]` TOML array or Admin GUI.
- **Backends admin panel**: New "Backends" tab in Admin Settings. Add/remove named backends, test S3 connections, set default backend. Safety checks prevent deleting in-use or default backends.
- **Bucket routing UI**: Per-bucket policy cards now show Backend + Alias fields when multi-backend is active. Route virtual bucket names to specific backends with optional real-name aliasing.
- **TOML/env config hints**: Read-only settings (Limits, Security, Advanced Compression) now show copyable `TOML:` and `ENV:` examples below each field.
- **Bucket policies promoted**: Per-Bucket Compression section moved to top of Compression tab with improved UX — contextual labels, amber tint on disabled buckets, empty state explains use cases.

### Bug Fixes
- **Dropdown positioning**: Replaced Ant Design `<Select>` with `<Radio.Group>` for backend type chooser. Fixes broken dropdown positioning at non-100% browser zoom (known `@rc-component/trigger` bug).
- **list_buckets_with_dates**: Routed virtual buckets no longer mask real creation dates with `now()`. Backends are queried first; route names added only if not already found.
- **Config validation**: `default_backend` is now validated against the `backends` list at load time. Invalid references are cleared with a warning. Bucket policy backend references are also validated.
- **S3 credential validation**: `POST /api/admin/backends` now validates credentials upfront (400) instead of failing at engine rebuild (500).
- **Taint detection**: `compute_tainted_fields` now compares `backends` and `default_backend` between runtime and disk config.

### Code Quality
- **DRY**: `BackendInfoResponse::from()` impl replaces copy-pasted conversion in two files. `rebuild_engine` made `pub(super)` and shared across config.rs and backends.rs. `DEFAULT_CONFIG_FILENAME` constant replaces 6 magic strings.
- **Dead code**: Removed inline save block from backendTab (uses shared `saveSection`). `embeddedTab` prop made required.
- **Replay window**: Hoisted duplicate `DGP_REPLAY_WINDOW_SECS` env parse to single variable.

## v0.5.11

### Features
- **Per-bucket compression**: Configure compression enable/disable and max delta ratio per bucket via TOML (`[buckets.<name>]`), admin API, or GUI. Unconfigured buckets inherit global defaults.
- **Bucket Policies GUI**: New "Bucket Policies" section in Compression tab — add, remove, edit per-bucket overrides with live save.

### Security
- **COPY source path traversal**: Reject `..` in `x-amz-copy-source` bucket and key (filesystem backend).

### Bug Fixes
- **Bucket policy hot-reload**: Fixed case normalization mismatch (config stored original case, engine expected lowercase). Added rollback on engine rebuild failure.

## v0.5.10

### UI & Documentation
- **Path-based routing**: Clean URLs (`/_/browse`, `/_/admin/users`, `/_/docs/configuration`) replace hash routing. Deep-linking to doc pages with heading anchors. Old hash bookmarks auto-redirect.
- **Embedded docs**: Full-text search (minisearch + Cmd+K), Mermaid diagram rendering with getBBox viewBox fix, Lightbox for images, landing page with screenshots and feature cards.
- **Admin GUI**: All 39 settings surfaced across 5 tabs (Backend, Compression, Limits, Security, Logging). Theme toggle in Admin/Docs header.
- **Design system**: 16 new CSS color variables for dark/light themes. Responsive breakpoints (grids collapse on mobile, panels hide).

### Correctness
- **SettingsPage**: handleSave wrapped in try-catch (prevents stuck save button on network error).
- **Multipart**: complete() no longer removes upload before store() succeeds (prevents data loss on transient failure).
- **SigV4**: Credential scope format validation in both presigned and header auth paths.
- **ListObjectsV2**: max-keys=0 returns InvalidArgument per S3 spec.
- **DocsPage**: Fixed stale useCallback dependency in handleLinkClick.

### Infrastructure
- **build.rs**: `rerun-if-changed` for dist/ directory (no more stale embedded UI assets).
- **Dead code**: Removed ApiDocsPage, BrowserConnectionCard, dead hash routes (~400 lines).
- **DRY**: FullScreenHeader shared component, FULLSCREEN_VIEWS set, dedup-by-key unified.

## v0.5.9

### Bug Fixes
- **SigV4 replay**: Fixed false positives — presigned URLs and idempotent methods (GET/HEAD) skip replay detection. Replay window reduced from 300s to 2s.
- **Logging**: SigV4 mismatch logs method, path, scope, signature prefixes. Clock skew logs direction.
- **Config DB**: Added PRAGMA busy_timeout=5000ms on open and reopen.

## v0.5.4

### Security & Hardening

- **Per-request timeout**: Added `tower_http::timeout::TimeoutLayer` returning HTTP 504 Gateway Timeout after 300s (configurable via `DGP_REQUEST_TIMEOUT_SECS`). Prevents slow clients from holding concurrency slots forever.
- **Replay cache cap**: Replay cache capped at 500K entries. If exceeded (flood attack), cache is cleared with a `SECURITY |` warning.
- **Recursive delete IAM enforcement**: Server-side recursive prefix delete (`DELETE /bucket/prefix/`) now checks per-object IAM permissions. Previously bypassed individual Deny rules.
- **Bootstrap hash format validation**: Rejects malformed bcrypt hashes at startup instead of failing silently on first auth attempt.

### Features

- **Server-side recursive delete**: `DELETE /bucket/prefix/` (trailing slash) deletes all objects under the prefix. Per-object IAM checks enforced. Filesystem backend uses native `remove_dir_all`.
- **S3 batch delete**: Batch delete (`POST /?delete`) uses `DeleteObjects` API instead of per-file DELETE for ~10x fewer API calls.
- **Base64 bootstrap password hash**: `DGP_BOOTSTRAP_PASSWORD_HASH` accepts base64-encoded bcrypt hashes to avoid `$` escaping issues in Docker/env vars. Auto-detected format.
- **Config DB resilience**: If the encrypted config DB can't be opened (wrong password, corruption), creates a fresh database instead of crashing.
- **`config_sync_bucket` in TOML**: Config DB S3 sync bucket now configurable via TOML (was env-only).

### UI

- **Favicon**: Teal chain-link icon on dark background.
- **Reduced polling**: Browser file listing polls every 30s instead of every second.

### Defaults

- **`max_delta_ratio`**: Default raised from 0.5 to 0.75 (more files stored as deltas, better space savings for typical workloads).

### Code Quality

- Removed dead `http_put` test helper from storage resilience tests.

## v0.5.3

### Performance

- **Parallel delta reconstruction**: Reference and delta fetched concurrently via `tokio::join!` instead of sequentially. Saves ~100ms per delta GET on S3 backends.
- **Parallel metadata HEAD calls**: `resolve_object_metadata` fetches delta and passthrough metadata concurrently.
- **Legacy migration off GET path**: `migrate_legacy_reference_object_if_needed` no longer runs during GET (was adding 60+ seconds of xdelta3 encoding). Available via batch migration endpoint instead.

### Correctness

- **PATHOLOGICAL warnings**: Delta and reference files missing DG metadata now log prominent warnings instead of silently falling back.
- **`dg-ref-key` → `dg-ref-path` rename**: Reference paths stored as relative paths for portable deltaspaces. Legacy `dg-ref-key` read with automatic fallback.

### Testing

- **Metadata validation tests**: 7 new tests for missing, corrupt, partial, and wrong metadata scenarios.

### UI

- **Connect page simplification**: Shows only relevant fields per auth mode (bootstrap password OR S3 credentials, not both). Removed endpoint URL field (derived from window location).

## v0.5.2

### Correctness (Critical)

- **Fix GET on rclone-copied delta files**: `retrieve_stream` used `obj_key.filename` instead of `metadata.original_name` for storage lookups, causing 404 on files whose S3 key had a `.delta` suffix. This bug was not caught by 222 tests because they all follow the clean PUT→GET path.

### Testing

- **Storage resilience tests**: 6 new adversarial tests that would have caught the above bug:
  - Triangle invariant (LIST→HEAD→GET must all succeed for every key)
  - HEAD/GET content-length consistency
  - SHA256 roundtrip verification across all storage strategies
  - Unmanaged file triangle invariant (directly placed files)
  - External delete → 404 (not stale cached data)
  - LIST never exposes `.delta` suffix or `reference.bin`

### Security

- **Session cookie hardening**: Secure, HttpOnly, SameSite=Strict attributes.
- **Remove secrets from whoami**: `/whoami` endpoint no longer exposes secret access keys.
- **Cleanup `.bak` files**: Removed leftover backup files from refactoring.

## v0.5.1

### Security Fixes (Post-Release Audit)

- **Deny condition bypass**: Fixed condition evaluation that could skip Deny rules under certain group membership combinations.
- **Group `member_ids` persistence**: Group member lists were not persisted correctly to SQLCipher DB.
- **`evaluate_iam` alternate path**: Fixed edge case where IAM evaluation took a non-standard code path.
- **Session storage**: Server-side session storage for S3 credentials (no longer stored client-side).
- **`env_clear()` on subprocess**: xdelta3 subprocess environment cleared to prevent credential leaks.
- **`DGP_TRUST_PROXY_HEADERS`**: Default changed to `true` for reverse proxy deployments.

### Testing

- **Auth integration tests**: SigV4 signature verification, presigned URLs, clock skew rejection, IAM lifecycle.
- **IAM persona tests**: 23 tests for groups, ListBuckets filtering, prefix scoping, conditions.

### Infrastructure

- **Docker retry**: `apt-get` retries on network blips (`Acquire::Retries=3`).
- **Startup refactor**: Extracted `startup.rs` from `main.rs`, split `engine.rs` and `config_db.rs` into sub-modules.

## v0.5.0

### IAM Policy Conditions (iam-rs)

- **Full AWS IAM condition support**: Integrated `iam-rs` crate for standards-compliant policy evaluation with all AWS condition operators (StringEquals, StringLike, IpAddress, NumericLessThan, etc.).
- **`s3:prefix` condition**: Deny LIST requests based on the prefix query parameter. Example: `{"StringLike": {"s3:prefix": ".*"}}` blocks listing dotfiles.
- **`aws:SourceIp` condition**: Restrict operations to specific IP ranges. Example: `{"IpAddress": {"aws:SourceIp": "10.0.0.0/8"}}`.
- **DB schema v4**: New `conditions_json` column on permissions and group_permissions tables. Backward compatible — existing permissions work unchanged.
- **Permission validation**: Effect normalization (case-insensitive), max 100 rules per user/group, resource pattern validation (trailing wildcard only), `$`-prefix names blocked.
- **Frontend conditions UI**: Collapsible conditions section per permission rule with prefix and IP restriction inputs.

### IAM Module Refactor

- **Split `iam.rs` into module**: `iam/types.rs`, `iam/permissions.rs`, `iam/middleware.rs`, `iam/keygen.rs`, `iam/mod.rs`. Pure permission evaluation logic separated from framework-specific middleware.
- **Centralized permission authority**: All permission checks go through `AuthenticatedUser.can()`, `.can_with_context()`, `.can_see_bucket()`, `.is_admin()`.
- **Legacy admin as AuthenticatedUser**: Bootstrap credentials now create a `$bootstrap` AuthenticatedUser with wildcard permissions instead of bypassing authorization entirely.
- **Groups loaded on startup**: Previously groups were only loaded on first IAM mutation.
- **ListBuckets per-user filtering**: Users see only buckets they have permissions on.
- **IAM backup/restore**: Export/import all users, groups, permissions, and credentials as JSON via `/_/api/admin/backup`.

### Security Fixes

- **Batch delete per-key authorization**: Previously the middleware only checked delete permission at bucket level; now each key in a batch delete is individually authorized.
- **Progressive auth delay**: Failed auth attempts trigger exponential backoff (100ms→5s), making brute force expensive before lockout.
- **Attack detection logging**: `SECURITY |` log events for brute force detection, lockout, and repeated failures with IP and attempt count.
- **Condition parse fail-closed**: Malformed conditions produce an empty policy (deny-all) instead of stripping conditions (fail-open).
- **Error XML escaping**: Error `<Message>` now XML-escaped to prevent malformed responses.
- **Bucket names with dots**: S3-compliant validation now accepts dots in bucket names.
- **is_admin strict check**: Uses `== "Allow"` instead of `!= "Deny"`.

### Performance

- **Range passthrough**: Range requests on passthrough objects pass the Range header through to upstream S3 (or seek on filesystem) instead of buffering the entire file. A 1KB range on a 100MB file reads only 1KB from storage.
- **Request concurrency limit**: `tower::limit::ConcurrencyLimitLayer` with configurable max (default 1024, `DGP_MAX_CONCURRENT_REQUESTS`).
- **Write-before-delete**: Storage strategy transitions (delta↔passthrough) now write the new variant before deleting the old one, preventing transient 404s on concurrent GETs.
- **Prefix lock cleanup**: `cleanup_prefix_locks()` runs on every lock acquisition instead of only on delete.
- **Interleave-and-paginate dedup**: Shared function for the interleave/sort/paginate pattern used by engine, S3 backend, and filesystem backend.

### Unified Audit Logging

- **`src/audit.rs`**: Single audit module with `sanitize()`, `extract_client_info()`, and `audit_log()`. Eliminates duplicated sanitization and IP extraction across handlers and admin API.
- **Session TTL decoupled**: `SessionStore::ttl()` method replaces re-parsing `DGP_SESSION_TTL_HOURS` in the cookie formatter.

### Frontend

- **Conditions UI**: Permission editor supports prefix restriction and IP restriction inputs with collapsible conditions panel per rule.
- **IAM backup buttons**: Export/Import buttons in admin sidebar for JSON backup/restore.
- **Log level radio buttons**: Replaced broken Select dropdown with Radio.Group.
- **Polling fix**: Users/Groups panels load once on mount instead of re-polling on every render.
- **Deduplicated formatters**: Unified byte formatters, extracted CredentialsBanner and InspectorSection components.

### Tests

- **222 tests**: 180 unit + 42 integration (23 new persona tests covering groups, ListBuckets filtering, prefix scoping, cross-user isolation, multipart, content verification, deny-from-groups, IAM conditions).
- **iam-rs condition tests**: Unit tests for prefix deny with StringLike, IP deny with IpAddress.

### Metadata Cache

- **In-memory metadata cache**: New moka-based cache (`MetadataCache` in `metadata_cache.rs`) eliminates redundant HEAD calls for object metadata. 50 MB default budget (~125K–150K entries), 10-minute TTL. Populated on PUT, HEAD, and LIST+metadata=true. Consulted on HEAD, GET, and LIST (including for file_size correction on delta-compressed objects). Invalidated on DELETE and prefix delete. Configurable via `DGP_METADATA_CACHE_MB` env var or `metadata_cache_mb` TOML setting.

### Security Hardening

#### Tier 1 — Authentication & Session Security
- **Rate limiting**: Per-IP token bucket rate limiter on auth endpoints — 5 attempts per 15-minute window, 30-minute lockout after exhaustion. Prevents brute-force attacks on admin login.
- **Session IP binding**: Admin sessions are bound to the originating IP address. Requests from a different IP are rejected even with a valid session token.
- **Session concurrency cap**: Maximum 10 concurrent admin sessions. Oldest session evicted when the limit is reached.
- **Configurable session TTL**: Default reduced from 24h to 4h. Override with `DGP_SESSION_TTL_HOURS`.
- **Password quality enforcement**: Min 12 chars, max 128 chars, common password blocklist. Validated on both admin API and CLI password set flows.
- **SigV4 replay detection**: Duplicate signatures within a 5-second window are rejected to prevent request replay attacks.
- **Presigned URL max expiry**: Capped at 7 days (604,800 seconds), matching AWS S3.
- **Configurable clock skew**: `DGP_CLOCK_SKEW_SECONDS` (default 300s) controls SigV4 timestamp tolerance.

#### Tier 2 — Response Hardening & Anti-Fingerprinting
- **Security response headers**: All responses include `X-Content-Type-Options: nosniff` and `X-Frame-Options: DENY`. HSTS header added when TLS is enabled.
- **Anti-fingerprinting**: Debug/fingerprinting headers (`Server`, `x-amz-storage-type`, `x-deltaglider-cache`) suppressed by default. Enable with `DGP_DEBUG_HEADERS=true`.
- **Bootstrap password TTY safety**: Auto-generated bootstrap password displayed in plaintext only when stderr is a TTY. Hidden in container/CI/piped output to prevent credential leaks in log aggregators.
- **Multipart upload limits**: Concurrent multipart uploads capped at 100 (configurable via `DGP_MAX_MULTIPART_UPLOADS`) to prevent resource exhaustion.

### Usage Scanner

- **Background prefix size scanner**: `/_/api/admin/usage` endpoint computes prefix sizes asynchronously with 5-minute cached results, 1,000-entry LRU cache, and 100K-object scan cap per prefix.

## v0.4.0

### Single-Port Architecture

- **UI served at `/_/`**: The embedded admin UI and all admin APIs are now served under `/_/` on the same port as the S3 API. No more separate port (was port+1). The `/_/` prefix is safe because `_` is not a valid S3 bucket name character. Health, stats, and metrics endpoints are available at both root (`/health`) and under `/_/` (`/_/health`).

### IAM & Authentication

- **Bootstrap password**: Renamed from "admin password". A single infrastructure secret that encrypts the SQLCipher config DB, signs admin session cookies, and gates admin GUI access in bootstrap mode. Auto-generated on first run. Backward-compatible aliases (`DGP_ADMIN_PASSWORD_HASH`, `--set-admin-password`) still work.
- **Multi-user IAM (ABAC)**: Per-user credentials stored in encrypted SQLCipher database (`deltaglider_config.db`). Each user has access key, secret key, and permission rules with actions (`read`, `write`, `delete`, `list`, `admin`, `*`) and resource patterns (`bucket/*`). Admin = wildcard actions AND wildcard resources.
- **IAM mode auto-activation**: When the first IAM user is created, the proxy switches from bootstrap mode to IAM mode. Bootstrap credentials are migrated as "legacy-admin" user. Admin GUI access becomes permission-based (no password needed for IAM admins).
- **Admin API**: `/_/api/admin/users` CRUD, `/_/api/admin/users/:id/rotate-keys`, `/_/whoami`, `/_/api/admin/login-as` for IAM user impersonation.

### S3 API Compatibility

- **Range requests**: `Range` header support with 206 Partial Content responses, `Accept-Ranges: bytes` header on all object responses.
- **Conditional headers**: `If-Match` / `If-Unmodified-Since` (412 Precondition Failed), `If-None-Match` / `If-Modified-Since` (304 Not Modified).
- **Content-MD5 validation**: Validates `Content-MD5` header on PUT and UploadPart, rejects with 400 on mismatch.
- **Copy metadata directive**: `x-amz-metadata-directive: COPY` (default) or `REPLACE` on CopyObject.
- **ACL stubs**: GET/PUT `?acl` accepted and ignored for SDK compatibility.
- **Response header overrides**: `response-content-type`, `response-content-disposition`, `response-content-encoding`, `response-content-language`, `response-expires` query parameters on GET.
- **Per-request UUIDs**: `x-amz-request-id` header with unique UUID on every response.
- **Bucket naming validation**: Extracted `ValidatedBucket` and `ValidatedPath` extractors for automatic S3 path validation.
- **ListObjectsV2 improvements**: `start-after` parameter, `encoding-type` passthrough, `fetch-owner` support, base64 continuation tokens, max-keys capped at 1000.
- **Real creation dates**: `ListBuckets` returns actual bucket creation timestamps.

### Performance

- **Lite LIST optimization**: LIST operations no longer issue per-object HEAD calls. Sizes shown are stored (compressed) sizes. ~8x faster for large listings.
- **FS delimiter optimization**: `list_objects_delegated()` for filesystem backend uses a single `read_dir` at the prefix directory instead of a recursive walk when a delimiter is specified. Dramatically faster for buckets with many prefixes.

### Security

- **OsRng for tokens**: Session tokens and IAM access keys use `OsRng` (cryptographically secure) instead of `thread_rng`.
- **DB rekey verification**: Bootstrap password changes verify the new key can open the database before committing.
- **Proper transactions**: IAM user creation uses database transactions for atomicity.

### Code Quality

- **`S3Op` enum** (`storage/s3.rs`): Operation context for S3 error classification, replacing string-based operation names.
- **Session cookie helpers** (`session.rs`): Extracted session store into its own module with `OsRng` token generation.
- **`env_parse()` DRY** (`config.rs`): Extracted environment variable parsing boilerplate into reusable helpers.
- **Dead code cleanup**: Removed `AdminGate`, unused `#/settings` route, dead `UsersTab` and `UserModal` components.

### UI Features

- **File preview**: Double-click on any previewable file (text, images) to view inline via the inspector panel. Tooltip indicates previewable files.
- **Show/hide system files**: Toggle to show or hide DeltaGlider internal files (`.dg/` directory contents) in the object browser.
- **Folder size computation**: Folder sizes computed and displayed in the object table.
- **Delete user confirmation**: User deletion requires `window.confirm` dialog.
- **Full-screen admin overlay**: Admin settings now use a full-screen overlay with master-detail layout for user management.
- **Interactive API reference**: New `#/docs` page with interactive API documentation.
- **Key rotation safety**: Prevents self-lockout on key rotation; changing only the secret key no longer regenerates the access key.
- **Credentials display**: After creating a user, shows only the credentials with a dismissible banner before returning to the user list.

## v0.3.0

### S3-Compatible Endpoint Support

- **Disabled automatic request/response checksums**: AWS SDK for Rust (like boto3 1.36+) adds CRC32/CRC64 checksum headers by default. S3-compatible stores (Hetzner Object Storage, Backblaze B2, some MinIO configs) reject these with BadRequest. Now sets both `request_checksum_calculation` and `response_checksum_validation` to `WhenRequired`. (Port of Python DeltaGlider [6.1.1] fix.)
- **Retry with exponential backoff on PUT**: Hetzner returns transient 400 BadRequest errors (~1-2% of requests) with `connection: close` and no request-id. PUT operations now retry on 400 and 503 with 100/200/400ms backoff (3 retries). Also retries on network/timeout errors.
- **Verbose S3 error logging**: Every S3 error now logs operation, bucket, HTTP status code, `x-amz-request-id`, and full error details for production debugging.

### Unmanaged Object Support (Fixes #3, #4)

Objects that exist on the backend storage but were never stored through the proxy (no DeltaGlider metadata) are now fully accessible:

- **S3 backend**: HEAD, GET, and LIST now return fallback metadata from the S3 HEAD response (size, ETag, Last-Modified) instead of 404
- **Filesystem backend**: HEAD, GET, and LIST now return fallback metadata from filesystem stats (size, mtime) instead of 404
- **HEAD/GET consistency**: Both operations return metadata from the same source, ensuring consistent Content-Length and ETag
- **Delta and reference files**: Fallback metadata also works for delta/reference files without metadata (xattr or S3 headers)
- **Corrupt metadata recovery**: S3 objects with partial/corrupt DG headers now fall back to passthrough instead of hard-failing

### Error Handling Hardening

- **Error discrimination**: Replaced blanket `.ok()` and `map_err(|_| NotFound)` patterns with explicit error matching throughout the engine. `NotFound` → expected (object doesn't exist), `Io` → warn + retry path (concurrent access), other errors → propagate as 500
- **ENOENT classification**: `io_to_storage_error()` now maps file-not-found I/O errors to `StorageError::NotFound` instead of `StorageError::Io`, preventing false 500s for missing files
- **Filesystem xattr errors**: Only fall back to filesystem stats on `NotFound`; permission denied and other I/O errors are now propagated instead of silently swallowed
- **Reference cache errors**: `get_reference_cached()` now discriminates `NotFound` (→ MissingReference) from other storage errors (→ Storage)

### Security

- **SigV4 clock skew validation**: Regular (non-presigned) SigV4 requests now enforce a 15-minute clock skew window, matching AWS S3 behavior. Prevents replay attacks with arbitrarily old timestamps. Returns new `RequestTimeTooSkewed` error (403).
- **Reserved filename validation**: PUT requests for `reference.bin` and `*.delta` keys are rejected with 400 to prevent collision with internal storage files

### Reliability

- **Codec subprocess timeout**: xdelta3 subprocess now has a 5-minute timeout via `try_wait` polling loop. Kills hung processes and returns an error instead of blocking indefinitely.
- **Copy object size check**: `copy_object` now verifies actual data size after retrieval, catching cases where fallback metadata reports `file_size=0` that would bypass the pre-copy size check
- **S3 metadata size validation**: Rejects PUT if DeltaGlider metadata headers exceed S3's 2KB limit, instead of letting the upstream S3 return an opaque 400
- **Config validation**: Warns on `max_delta_ratio` outside [0.0, 1.0] and `max_object_size=0` at startup
- **Cache invalidation ordering**: Reference is now deleted from storage BEFORE cache invalidation, preventing concurrent GET from re-caching a stale reference between invalidation and deletion. Fixed in 3 code paths (passthrough fallback, deltaspace cleanup, legacy migration).

### DRY Cleanup

- **`FileMetadata::fallback()`**: New constructor consolidates 4 duplicate fallback metadata construction sites across S3 and filesystem backends
- **`Engine::validated_key()`**: Extracts the 5x repeated `ObjectKey::parse + validate_object + deltaspace_id` pattern
- **`try_unmanaged_passthrough()`**: Extracted 60-line nested match block from `retrieve_stream()` into a focused helper with flat control flow

### Testing

- 26 new tests (244 total, up from 218): unmanaged object operations, HEAD/GET metadata consistency, reserved filename rejection, copy with unmanaged sources, error discrimination (`io_to_storage_error` unit tests), delta byte-level round-trip integrity, multipart ETag format, user metadata round-trip, external file deletion → 404

### Infrastructure

- Docker build: native ARM64 on Blacksmith runners (no QEMU), cargo-chef for dep caching — ~5x faster builds
- RustSec: updated deps to fix 6 advisories in aws-lc-sys and rustls-webpki

## v0.2.0

### Cache Health Observability

Four layers of defense against silent cache degradation:

- **Startup warnings**: `[cache]` log prefix warns when cache is disabled (0 MB) or undersized (<1024 MB)
- **Periodic monitor**: Every 60s, warns on >90% utilization or >50% miss rate
- **Prometheus metrics**: `cache_max_bytes`, `cache_utilization_ratio`, `cache_miss_rate_ratio` — computed on scrape from existing atomic counters
- **Response header**: `x-deltaglider-cache: hit|miss` on delta-reconstructed GETs
- **Health endpoint**: `/health` now includes `cache_size_bytes`, `cache_max_bytes`, `cache_entries`, `cache_utilization_pct`

### Proxy Dashboard

Full Prometheus metrics dashboard in the built-in React UI (`#/metrics`):

- Top KPIs: uptime, peak memory, total requests, storage savings %
- Cache section: utilization gauge, hit rate with color coding, live hits vs misses chart
- Delta compression: encode/decode latency, compression ratio distribution, storage decisions
- HTTP traffic: operation breakdown (bar + donut chart), latency distribution, status codes, live request rate
- Authentication: success/failure counts with failure reason breakdown
- Auto-refresh every 5s, storage stats every 60s

### Correctness Fixes (11 bugs)

- **Codec stderr deadlock**: xdelta3 stderr was piped but only drained after stdout. If xdelta3 writes >64KB to stderr, stdout reader blocks forever. Now drains all 3 pipes concurrently.
- **Filesystem metadata-data split-brain**: Crash between `atomic_write` and `xattr_meta::write_metadata` left files without metadata. Now writes xattr to temp file before rename — atomic visibility.
- **Auth exemption path normalization**: `/health/` (trailing slash) was not matched by the exact-string exemption. Now strips trailing slashes.
- **SigV4 presigned URL case-sensitivity**: `x-amz-signature` (lowercase) was not excluded from canonical query string. Now uses case-insensitive comparison.
- **Stats cache thundering herd**: Lock released before `compute_stats`, so N concurrent requests all scanned storage. Now holds `tokio::sync::Mutex` across the async compute.
- **Multipart double-completion race**: Two concurrent `CompleteMultipartUpload` calls both read parts under read lock, both stored data. Now takes ownership under write lock atomically.
- **Multipart write lock starvation**: Assembly of large uploads held write lock during memcpy of all parts. Now removes upload from map (fast), releases lock, then assembles.
- **AWS chunked truncated payload**: Decoded length mismatch with `x-amz-decoded-content-length` was logged but data stored anyway. Now rejects with 400.
- **Admin config rollback**: Backend config was committed before engine swap. If `DynEngine::new()` failed, config showed new backend but engine was old. Now rolls back on failure.
- **S3 403 misclassification**: All 403 errors mapped to `BucketNotFound`. Now only maps for bucket-level operations; object-level 403 is reported as S3 error.
- **list_objects max_keys=0**: Produced `is_truncated=true` with no continuation token. Now clamps to >= 1.

### DRY & Code Quality

- **`StoreContext` parameter object**: Eliminated 3 `#[allow(clippy::too_many_arguments)]` suppressions from `encode_and_store`, `store_passthrough`, `set_reference_baseline`
- **`with_metrics()` helper**: Collapsed 8 inline `if let Some(m) = &self.metrics` blocks to one-liners
- **`try_acquire_codec()`**: Extracted codec semaphore acquisition with consistent error message
- **`cache_key()`**: Extracted `format!("{}/{}", bucket, deltaspace_id)` used 5 times
- **`delete_delta_idempotent()` / `delete_passthrough_idempotent()`**: Extracted 5 verbose `delete_ignoring_not_found` call sites
- **`paginate_sorted()`**: Extracted duplicated pagination logic from `list_objects`
- **`pipe_stdin_stdout_stderr()`**: Extracted duplicated codec pipe coordination from encode/decode
- **`object_url()`**: Extracted test helper URL construction
- **`parse_env()` / `parse_env_opt()`**: Extracted config env var parsing boilerplate
- **`header_value()`**: Renamed from cryptic `hval()`
- **`_guard`**: Renamed from misleading `_lock` (the Drop impl is load-bearing)
- Trimmed verbose `PERF:` archaeology comments — kept constraints, removed history
- Removed `Option<Arc<Metrics>>` from `AppState` — metrics are always present, eliminated branch per request
- Replaced `format!("%{:02X}")` with `write!()` in SigV4 URI encoding — no heap allocation per encoded byte

### Infrastructure

- `/stats` endpoint: 10s server-side cache, capped at 1,000 objects with `truncated` indicator
- `/health`, `/stats`, `/metrics` exempted from SigV4 authentication
- Demo server exposes `/health`, `/stats`, `/metrics` alongside admin API
- `prefixed_key()` helper in S3 backend eliminates 3x prefix if/else duplication

## v0.1.9

Initial public release with S3-compatible proxy, delta compression, filesystem and S3 backends, SigV4 authentication, multipart uploads, embedded React UI, and Prometheus metrics.

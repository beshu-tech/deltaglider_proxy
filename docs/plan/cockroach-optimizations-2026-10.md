# Cockroach pass: request and work cuts beyond DELETE (2026-10-12)

Source: seven parallel audits (GET, PUT, LIST, copy, background jobs, per-request
hot path, UI). Each audit measured with FakeS3 request logs, strace or criterion,
in scratch worktrees `~/Documents/deltaglider/.worktrees/cockroach-{get,put,list,copy,jobs,hotpath,ui}`
at `09cbb7fa` (v2.0.8). The prototypes in those worktrees are reference material,
not patches to apply: they mix env toggles, debug prints and test-only exports.
Port each item fresh on `main`, one commit per item, with its asserting cost test.

The method is the one from the DELETE work (6f12009d, 2b4f3525, 883ffb52, 933b9528,
c15e0cda, ad3eaaa8, 830d0e67, 91e3bf21): count every upstream request, keep what an
earlier read already returned, skip work that a read proves unnecessary, batch,
read one page instead of a scan, cache with a TTL and single-flight, put the cheap
and decisive check first, and lock the win with a cost test plus a source guard.

---

## 0. Owner decisions (2026-10-12)

| # | Question | Decision | Consequence in this plan |
|---|---|---|---|
| D1 | `metadata=true` LIST: sizes only, or full HeadObject metadata? | "Check how the UI consumes it; pick the best outcome." | The UI does not use it today. It sends one HEAD per visible row (`useS3Browser.ts:315`) to get two facts that the listing already knows: storage type and stored size. **Outcome:** `metadata=true` lists the DG storage facts of every entry from the listing, with zero HEADs, and the UI switches its table to it. User metadata leaves the listing (it was inconsistent: non-eligible keys never had it). See P8.1. |
| D2 | May an acked write's outbox event be lost in a crash before the batch commits? | Yes ("added robustness"). | `append_events` already queues and loses queued events on a crash (`event_outbox.rs:1158-1167`); the inline commit is only the uncontended branch. All appends go through the batched writer (P4.1). We also skip the append when nothing reads the outbox (P4.2), which loses nothing. |
| D3 | PUT responses carry storage facts as headers? | Yes. | P8.4. The proxy already gives these facts to every authenticated reader as `x-amz-meta-dg-*` on HEAD, so this adds no disclosure. |
| D4 | HEAD only one stored form, from a hint? | No. | Not planned. A crash between a write and its `drop_other_form` leaves both forms; newest-wins needs both reads. |

---

## 1. Cross-cutting rules for every item

1. **Asserting cost test first.** Each item adds a test that fails on `main` with the
   old count, then passes. Counts go in the cost ledger (P0.2). A test that never
   failed proves nothing.
2. **Fix the class.** When an item removes a request at one call site, it adds or
   extends a source guard so that every sibling call site keeps the rule. Guards
   listed per item.
3. **Backends.** Every item states its effect on: S3, filesystem, encrypting
   wrapper, routing, and B2 (non-CAS, 501 on conditional writes). An item that
   changes a `StorageBackend` method adds it with **no trait default**, so the
   compiler makes every wrapper forward it (precedent: `reference_fence`).
4. **Multi-instance.** Every cache added states its staleness window across nodes
   and why that window is safe. No new cross-node state.
5. **Transparency.** Clients never see a different S3 answer (body, ETag, size,
   status, ordering) because of an optimization. Additive headers are allowed (D3).
6. **Gates per commit:** `cargo fmt --all`, `cargo clippy --locked --all-targets
   --all-features -- -D warnings`, `cargo test --lib --locked`, the integration
   files the item touches (`cargo test --test all -- <file>::`), MinIO variants
   when S3 behaviour changes, and for UI items `npm run lint:strict`, `tsc`,
   `knip`, `npm run test:all`. Changelog entry per user-visible change; docs
   reference pages updated in the same commit (Diátaxis rules, plain prose).
7. **Bench on a quiet machine only** (load ≫ 14 cores → measure counts, not time).

---

## P0. Foundations (do first; everything else depends on it)

### P0.1 Shared cost-test kit
- Move the helpers that each audit re-invented into `src/test_support/cost.rs`
  (`#[cfg(test)]`): `s3_engine()`, `s3_engine_at(config)`, `fs_engine()`,
  `count(fake, method)`, `folder_lists(fake)` (excludes the background
  `.dg/facts` traffic), and `FakeS3::bytes()` (bytes down/up per request, from the
  copy audit's `fake_s3.rs` change).
- `FakeS3::requests()` entries get a structured form (`method, path, query,
  headers-of-interest`) besides the string, so tests can assert "a conditional
  PUT with `If-None-Match`" without string parsing.
- Edge case: background tasks (facts writer, usage flush) race the assertions.
  Give the kit `settle(fake)`: wait until the background queues are empty (poll
  the existing counters, never `sleep`).

### P0.2 The cost ledger
- `src/cost_ledger_tests.rs`: one table test. Each row = operation × backend ×
  cache state → exact request counts by method and bytes moved. Rows cover every
  operation in this plan (GET passthrough/delta/range/conditional, HEAD, PUT
  shapes from the PUT audit table, LIST shapes, copy shapes, delete shapes,
  maintenance per-object costs, idle-hour costs).
- Phase commits update the expected numbers, so `git log -p` on this one file
  shows every win and every regression.
- Pin today's numbers in the first commit (baseline). Today's numbers from the
  audits: passthrough GET 3, cold small-delta GET 5, passthrough overwrite 5,
  delta PUT multi-instance 11, delta overwrite multi-instance 13, CopyObject delta
  same folder 12 + 2 MiB down, migrate copy 10/object, reencrypt rewrite 11/object.

### P0.3 Load-free latency harness (for P1.1 and P7)
- `benches/hotpath.rs` (from the hotpath audit) cleaned up: criterion benches for
  IAM evaluation, SigV4 verify, metadata clone. Local only, not in CI.

---

## P1. Quick wins: one small diff each, low risk

### P1.1 `TCP_NODELAY` on the plain-HTTP listener — biggest single win
- **Now:** `src/main.rs:1109` `axum::serve(...)` without `.tcp_nodelay(true)`.
  hyper writes headers and body in separate `writev` calls; Nagle holds the body
  until the client's delayed ACK. Every GET after the first on a keep-alive
  connection waits about 40 ms (measured p50 41.7 ms → 1 ms).
- **Change:** `.tcp_nodelay(true)` (axum 0.7.9 `Serve::tcp_nodelay`).
- **Verify, don't trust:** the audit says the TLS path (`axum-server` 0.7) sets
  nodelay already. Read `axum-server`'s acceptor and confirm; if not, set it in
  the TLS acceptor too.
- **Also check:** the operator's HAProxy router config (`operator/src/resources.rs`)
  — HAProxy sets nodelay on its own sockets by default; confirm and note it.
- **Guard:** extract `serve_plain(listener, app)` in the library; a lib test
  binds loopback, serves a 4 KiB streamed body, sends 10 GETs on one connection,
  and asserts total < 150 ms (without the fix: > 400 ms; generous margin for a
  loaded CI box). Plus a source guard: no `axum::serve(` outside `serve_plain`.
- **Edge cases:** HTTP/2 (h2 over TLS) is unaffected; Unix sockets have no
  nodelay (not used).

### P1.2 UI: unbounded `GET /usage` loop in the file browser — a bug
- **Now:** `useS3Browser.ts:153-157` builds a new `folders` array per render;
  `ObjectTable.tsx:280-283` effect depends on it; `useComputeSize.ts:152`
  `setSizes` re-renders App → loop at 261-1,512 req/s while a folder with a cached
  scan is open (also in hidden tabs).
- **Change:** `useMemo` for `filteredFolders`; `autoPopulate` runs once per
  `(bucket, prefix)` visit (ref guard), so a future unstable prop cannot restart
  the loop.
- **Test:** `cockroachUi.folderUsageLoop.test.tsx` (fails on main) → rename
  `useS3Browser.usageOnce.test.tsx`. Add an ESLint-free source guard is not
  practical here; the test is the lock.
- **Release note:** ship in the next patch release on its own (it is a bug).

### P1.3 `/_/ready` answers from the health cache
- **Now:** `status.rs:311-323` sends a live `ListBuckets` per probe. With the
  operator (kubelet 10 s + 2 routers × 2 HAProxy checks 10 s) ≈ 1,800/h per pod
  per backend.
- **Change:** when the health loop runs and every backend verdict is younger than
  2 × `health_interval`, answer from `BackendHealthCache` (no I/O). Otherwise run
  the live probe through a single-flight with a 5 s TTL.
- **Operator:** the `admin` HAProxy section uses `track s3/<server>` instead of a
  second `httpchk` (halves router probes).
- **Edge cases:**
  - Health loop disabled → live probe path (unchanged semantics, now single-flight).
  - Boot: no verdict yet → live probe (readiness must not report ready before
    the first real probe).
  - Config-DB probe part of `/_/ready` stays live (local, cheap).
  - A backend that dies: readiness flips within ≤ 2 × interval (60 s) instead of
    at the next probe. Document in `reference/` health page.
- **Test:** ready cost test (0 upstream per probe when verdict fresh; 1 when stale;
  N concurrent stale probes → 1).

### P1.4 IAM-filtered LIST serves short pages
- **Now:** `iam/listing.rs:~395` keeps scanning until `max_keys+1` visible
  entries, so the next client page re-reads the same upstream page (2,500 keys:
  6 LISTs, unfiltered 4).
- **Change:** stop when the page has ≥ 1 visible entry and the scan is truncated
  (same contract as c053b0be).
- **Edge cases:** a page with zero visible entries must keep scanning (else an
  empty truncated page loops clients that treat it as end); the engine-page
  budget (`filtered_list_max_engine_pages`) still applies; the continuation token
  stays the last *visible* key (no hidden-key leak — keep the existing tests,
  add one with interleaved hidden keys at the page boundary).
- **Docs:** the ListObjectsV2 row already says short pages are possible.

### P1.5 Filesystem delimiter LIST: filter first, check visibility per page
- **Now:** `storage/filesystem/mod.rs:~1941` runs `dir_has_visible_data_recursive`
  on every child directory before the prefix filter and pagination
  (3,000 folders: 18,000 dir reads per 3 pages).
- **Change:** name filter + token filter first; merge objects and folders in key
  order; check visibility only until the page holds `max_keys+1` entries.
- **Edge cases:** a folder whose only content is `.dg/reference.bin` stays hidden
  (visibility check still runs for every *emitted* folder); `start-after` inside a
  folder name; `encoding-type=url`; the proptest of listing order if present
  (else add one: fs listing = sorted reference model).

### P1.6 Negative cache for missing listing facts
- **Now:** a delta (or encrypted object) without durable facts makes every LIST of
  its page send up to 4 facts LISTs, forever, until a HEAD/GET reads it.
- **Change:** 60 s "no facts" mark per exact stored identity (key + ETag + size) in
  `list_size_cache.rs`.
- **Follow-up in the same phase (P1.6b):** when a LIST marks a miss, queue a
  bounded background HEAD (max N in flight per process, dedup by key) that writes
  the facts. Old objects then heal for good and list their logical size; the
  docs caveat "lists its stored size until a HEAD" can go after a release.
- **Edge cases:** another node writes facts → seen ≤ 60 s later (acceptable,
  identical to cache TTL elsewhere); overwrite changes ETag → identity miss →
  reread; backfill queue must respect the backend health gate and stop on
  shutdown; encrypted backends: the HEAD decrypts metadata only.

### P1.7 DeleteBucket emptiness check uses `list_lite` with one key
- 5 → 4 requests measured; then replace the level-by-level walk with one flat
  `list_lite(max_keys=1)` now that the fs walk is sorted and early-stopping
  (7afccbdc) → 1-2 requests.
- **Edge case:** a bucket holding only `.dg/` internals (orphan reference) must
  still count as empty for DeleteBucket the way it does today — read the current
  `first_visible_key` rules and keep them in a unit test.

### P1.8 Every S3 verb checks its bucket through the 5 s cache
- **Now:** `copy.rs:52` (CopyObject source), `copy.rs:266` (UploadPartCopy source,
  once per part), `multipart.rs:17/127/266`, bucket sub-resource verbs send their
  own HeadBucket.
- **Change:** source bucket: drop the check, the source HEAD answers `NoSuchKey`
  and only then check the bucket through the cache to tell `NoSuchBucket` (same
  as the read-miss path in `object.rs`). Others: `ensure_bucket_exists`.
- **Guard:** extend `object_verbs_check_buckets_through_the_cache` from
  `object.rs` to all `src/s3_adapter_s3s/*.rs`.
- **Edge case:** a deleted bucket passes for ≤ 5 s — writes already accept this;
  DeleteBucket already forgets its own entry (12b96706).
- **Bonus:** single-flight the cold `ensure_bucket_exists` (N concurrent first
  requests → 1 HeadBucket).

### P1.9 Event delivery: claim and mark in batches
- Claim loop in one transaction (`event_outbox.rs:447`; 100 rows: 1.73 s → 20 ms).
- `mark_delivered` + endpoint records batched per claimed batch (402 pages / 1.18 s
  per 100 rows today).
- **Edge case:** at-least-once stays: a crash between delivery and the batched mark
  re-delivers the batch (receivers already dedup by event id — confirm the
  webhook payload carries a stable id; document).

### P1.10 Idle event consumer does not write
- `consumer_has_work` (read-only: `MAX(outbox.id)` vs the slowest replication
  cursor) before taking the lease (`replication/event_consumer.rs:320-347`):
  240 commits/h → 0. A failed read runs the tick as before.
- **Edge case (HA):** with `S3Lease`, skipping also skips the lease GET/PUT — good;
  a rule whose cursor row is missing counts as "has work".

### P1.11 Folder-marker PUT reads one form
- A key ending in `/` never has a `.delta` form; skip that HEAD (`store.rs:316`).
  3 → 2.

---

## P2. Write path

### P2.1 The prior read remembers which stored forms exist
- **Now:** `CounterPrior::Present` drops the form info; `drop_other_form`
  (`store.rs:1726`) skips the DELETE only on `Absent`. Same-form overwrite sends a
  DELETE for a form that the prior HEAD saw absent (passthrough overwrite 5 → 3;
  delta overwrite 7 → 5; the DELETE also triggers a facts LIST).
- **Change:** `CounterPrior::Present { forms: StoredForms, .. }`;
  `StoredForms::may_hold(form)`; `lock_and_check` uses `head_with_forms` so the
  conditional path keeps it.
- **Also fix (copy audit #4):** `engine/raw.rs::put_delta_raw` calls
  `delete_passthrough_idempotent` unconditionally, bypassing `drop_other_form`.
  It takes a `&CounterPrior` and goes through the helper; heal path passes
  `Unknown`. `reference_meta` uses `reference_metadata_if_present` (one HEAD).
- **Guard:** `writes_drop_the_other_form_through_one_helper` scans all of
  `src/deltaglider/engine/*.rs`, not only `store.rs`;
  `engine_code_reads_a_reference_once` allow-list goes to empty (add
  `FaultPoint::GetReferenceMetadata` for the fault test that needed `raw.rs`).
- **Edge cases:**
  - Unknown prior (no read / failed read) → DELETE as today.
  - A peer adds the other form between our read and our write → the race already
    documented on `drop_other_form`; newest-wins on read keeps GET correct; the
    next write or the reclaim sweep drops it. Add a model test.
  - The test `a_put_dropped_after_its_commit_is_still_counted` relied on the
    wasted DELETE; rewrite it to overwrite in the *other* form.

### P2.2 CAS objects: write first, read only on conflict, release by kept ETag
- **Applies to:** `coordination/reference_lock.rs` (`:376` GET→PUT, `:396`
  GET→DELETE) and `coordination/s3_lease.rs:281-401` (same shape).
- **Change:** acquire = `PUT If-None-Match:*`; on 412 → GET → if expired by server
  clock → steal with `PUT If-Match:<etag>`. Keep the ETag of our own last PUT/renew
  per (key, owner); release = `DELETE If-Match:<kept etag>`; on 412 the lock was
  stolen → nothing to release (log at debug). New `cas::conditional_put_etag`.
- **Measured:** 4 → 2 requests per multi-instance delta PUT (×2 for routed buckets
  while both lock keys are taken: P2.2 saves 4 there).
- **Edge cases:**
  - Contended acquire costs 1 more request than today — fine, contention is rare.
  - Renew failure leaves the kept ETag stale → release 412 → treat as lost
    (correct: `ensure_held` already stops writes).
  - Server-clock expiry judgement (`server_clock.rs`) still needs the GET on the
    conflict path — unchanged.
  - B2/non-CAS: the coordination bucket is CAS-probed at boot, so no new case.
  - Tests that fake a server refusing `If-None-Match` and the hung-bucket test
    need their fixtures adapted (the PUT is now the first request).
  - Previous-release peers read the lock body the same way — body format
    unchanged, so mixed-version clusters stay compatible.

### P2.3 One HEAD gives the reference fence and its metadata
- **Now:** under the cross-instance lock, `locking.rs:582` `reference_fence` HEADs
  `reference.bin`, then `store.rs:612` HEADs it again for metadata.
- **Change:** new trait method `reference_fence_with_metadata` (no default;
  forwarded by Box<dyn>, encrypting, routing, fault wrapper, fake). The fence and
  the metadata come from one response (stricter than today).
- **Edge case:** filesystem = unfenced → returns metadata with `Unfenced`.

### P2.4 Signed PUT: SHA-256 two passes, then one; hashing off the runtime
- **Now:** s3s `UploadStream` verifies `x-amz-content-sha256` while streaming;
  `verify_signed_payload_hash_s3s` (`object.rs:684`) hashes again;
  `store_inner` (`store.rs:365`) hashes a third time. 16 MiB: 9.4 ms per pass, on
  a tokio worker.
- **Step 1:** pass the verified digest down as the engine's `sha256` (3 → 2).
  `StorePut` gets `known_sha256: Option<[u8;32]>`; only set when the hex header
  was verifiable and verified.
- **Step 2:** prove that s3s verifies every shape we treat as `is_verifiable_hex`
  (plain signed, presigned with a hex payload hash, chunked/aws-chunked =
  not hex → not verifiable). Read s3s 0.14 source; add a contract test row in
  `s3s_contract_tests.rs` per shape. Only then drop our own verify (2 → 1).
  `test_sigv4_payload_hash_mismatch_rejected` must keep failing the request.
- **Step 3:** bodies above 1 MiB hash in `spawn_blocking` (or hash incrementally
  as the body arrives, if s3s exposes the stream before collection).
- **Edge cases:** `UNSIGNED-PAYLOAD`, `STREAMING-AWS4-HMAC-SHA256-PAYLOAD`,
  `STREAMING-UNSIGNED-PAYLOAD-TRAILER` with a checksum trailer → engine hashes as
  today; form POST has no payload hash → engine hashes.

### P2.5 Body copies on the buffered PUT; correct the stated RAM bound
- `encode_buffered` `data.to_vec()` (`store.rs:756`), S3 PUT
  `ByteStream::from(data.to_vec())` per attempt (`storage/s3/objects.rs:300`),
  first delta copies into the reference cache (`store.rs:938`): up to 3 copies of a
  body ≤ `max_object_size`.
- **Change:** carry `Bytes` from the adapter down; `ByteStream::from(Bytes)` is a
  refcount clone per retry; the reference cache stores the same `Bytes`.
- **Docs:** CLAUDE.md states the PUT RAM bound as `max_object_size`; until this
  lands it is about 2-3×. Fix the statement in the same commit.
- **Edge case:** the encrypting wrapper produces new ciphertext anyway (no gain
  there); the codec subprocess still needs the source as a file/stdin.

### P2.6 Large in-RAM PUT: hash from RAM, not from the spool
- `conditional.rs:213` writes the body to the spool; `store.rs:540` reads it back
  to hash. Hash from RAM (or reuse P2.4's digest) and pass it in. Size check stays.

### P2.7 Streaming delta PUT fills the reference spool cache
- **Now:** only GETs fill `range_spools` (`retrieve.rs:390`); every large PUT GETs
  the whole `reference.bin` (`store.rs:830`), even back-to-back into one folder.
- **Change:** `range_spools.get_or_fill(download_verified_reference(..., known
  size))`; a PUT that *creates* the reference seeds the cache from its own body
  spool (sha256 known). On `NoRoom` fall back to today's private download (the
  "holder never waits" rule).
- **Edge cases:** spool budget held up to the cache TTL (60 s; evictor exists);
  the cache entry must be invalidated when the reference changes (it is matched on
  the reference sha — confirm); adds a sha check of the reference that today's
  PUT does not do (good: detects a corrupt baseline before encoding against it;
  decide: corrupt baseline → fail the PUT with 500 and log, same as GET).
- **Risk:** medium. Streaming checklist items 2, 3, 6 in CLAUDE.md apply.

---

## P3. Read path

### P3.1 One round trip for a plain GET
- **Now:** `object.rs:69` → `engine.head()` (2 parallel HEADs: `k` and
  `k.delta`, one always 404) → `retrieve_stream` (GET). Cold passthrough 3
  requests / 2 round trips; cold small delta 5.
- **Change:** `engine.retrieve_stream_fresh`: `open_object` on the *likely* form
  (`FileRouter::is_delta_eligible` + bucket compression setting + metadata cache
  hint if present) — the GET response headers carry the metadata — in parallel
  with a HEAD of the other form. Newest-wins decides as today. A delta body that
  was already read is passed down (`prefetched`).
- **When it applies:** no `Range`, no conditional headers (`If-*`), no
  `partNumber`. Those keep HEAD-first, so a 304/412/416 never opens a body.
- **Edge cases (each needs a test):**
  - Wrong guess → the other form's HEAD says it exists → drop the opened stream
    (abort, do not drain) and GET the right form. Cost = today's.
  - Both forms exist (crash leftover) → newest-wins on the two metadata sets;
    if the opened one loses, drop it and open the winner.
  - Opened form's metadata names another layout (e.g. a passthrough object
    whose `dg-note` says delta) → fall back to the old path.
  - Metadata cache warm → today's path costs 1 GET; the new path must not add
    the parallel HEAD then. Rule: cache hit → GET the cached form only; the GET
    response metadata refreshes the cache. Ledger row: warm passthrough GET = 1.
  - Encrypting wrapper: `open_object` decrypts the stream; the response headers
    are the stored (ciphertext) metadata → must map through the same metadata
    decode as HEAD. Test on encrypted S3 and encrypted filesystem.
  - Routing: route resolution once for both requests.
  - Large delta (above the spool threshold): the delta body is small; reference
    path unchanged (P3.2/P3.4).
  - A spool-cache hit for a large delta now also reads the small delta body it
    does not need — accept (tiny) or skip when the range-spool cache has the
    reconstructed object for that exact ETag.
  - Client disconnect after open → the stream drops; no leaked permits.
- **Bonus correctness:** today a peer overwrite between HEAD and GET can return
  the ETag of one version with the body of another. With metadata from the GET
  response, this mix cannot happen for passthrough. For delta, the delta body and
  its metadata come from one response too.
- **Guard:** the read verbs go through one entry point (`retrieve_stream_fresh`
  or the conditional path); source guard forbids `engine.head(` followed by
  `retrieve_stream` in `object.rs`.

### P3.2 Cold reference load: one GET
- `metadata.rs::load_verified_reference` runs `get_reference` +
  `get_reference_metadata` in parallel → `open_object(StoredObject::Reference)`.
  SHA check against the recorded checksum unchanged.

### P3.3 Routing: single-flight cold resolution, then stale-while-revalidate
- `routing.rs:513 resolve_existing_probed` has no single-flight: the two parallel
  HEADs of one GET each send a HeadBucket every `RESOLVE_TTL` (30 s); N clients →
  2N. (HeadBucket was the 7-10 s request on Hetzner.)
- **Step 1:** per-bucket gate (`tokio::Mutex` in a `DashMap`), recheck the cache
  under the gate, remove the gate entry after the probe. Errors do not cache.
- **Step 2:** after TTL, serve the stale resolution and refresh in the background
  (one refresher per bucket). Hard expiry at 5 × TTL, so a removed route never
  lives forever.
- **Edge cases:** config apply that changes routes must clear the cache (it does —
  confirm the hot-reload path); a bucket deleted on the backend → stale route
  answers until refresh → the request gets the backend's `NoSuchBucket` anyway.

### P3.4 Large-delta reference download: size from the GET response
- `retrieve.rs` `download_verified_reference` (~436) and
  `reference_file_and_out_spool` (~405) HEAD for the size, then GET. Use the GET
  response's Content-Length before reading the body; `acquire_beside` never waits,
  so the open response is not held. Filesystem keeps the hardlink path.
- **Edge case:** encrypted backend → Content-Length is ciphertext; the plaintext
  size is in the decrypted metadata from the same response; use that.

### P3.5 Encrypted ranged GET: no metadata HEAD; one GET for chunk-0 ranges
- `encrypting/mod.rs:985` HEADs for metadata that the engine already holds →
  trait change: the range call takes the resolved raw metadata (no default).
- Chunked format: when the range starts in chunk 0 (`bytes=0-`, video players),
  header fetch + body fetch are contiguous → one ranged GET.
- **Edge case:** the metadata passed in must be the raw stored metadata with the
  encryption markers, not the client-facing view. Type it (`RawStoredMeta`) so
  the compiler refuses the wrong one.

### P3.6 Small-delta GET: no rewrite of the reference to disk per request
- `codec.rs:718-722` writes the source to a new spool file per decode, even on a
  reference-cache hit (16 MiB reference → 16 MiB written per GET).
- **Change:** decode against the verified shared reference file kept by
  `range_spools` (`decode_to_writer`). The RAM reference cache then serves PUT
  encoding only.
- **Risk:** spool budget accounting (a shared file is held while N decodes read
  it: refcount the spool entry); medium. Measure disk writes before/after.

### P3.7 Passthrough range GET: parallel `.delta` HEAD
- No `If-Range` → send the ranged GET and the `.delta` HEAD in parallel; S3 answers
  416 itself and Content-Range carries the total. 3 → 2.
- **Edge case:** if `.delta` exists and is newer → discard the ranged body and serve
  the delta path.

---

## P4. Event outbox and SQLite (D2)

### P4.1 Every append goes through the batched writer
- Remove the inline `try_lock` + `insert_logged` branch (`event_outbox.rs:1174`).
  The writer task runs each batch insert in `spawn_blocking` and keeps id order.
- **Measured:** sequential PUT p50 −45%; no fsync on a tokio worker; no config-DB
  mutex taken on the request path.
- **Crash window (accepted, D2):** events queued in the last few ms. Safety nets
  that already exist: `flush_appends` at graceful shutdown; the replication
  scheduled walk copies what the event path missed; lifecycle is scan-based.
  Document in `explanation/` events page: "an event of a write acked in the last
  milliseconds before a crash can be lost; scheduled replication repairs the copy;
  a webhook receiver does not get that event."
- **Edge cases:** queue bound — the channel must be bounded with a large cap; on
  overflow, block the request briefly (backpressure) rather than drop, and count
  `deltaglider_outbox_queue_full_total`. Tests that read the outbox right after a
  PUT must wait via a new `GET /_/api/admin/...` — **no**, not a test-only route:
  use the existing outbox listing endpoint with a poll (or a monotonic
  `OUTBOX_WRITTEN` counter exposed where the IAM version is, only if a real
  operator use exists; otherwise poll the list).

### P4.2 Skip the append when nothing reads the outbox
- Readers: event delivery active (`advanced.event_delivery.enabled` + a target),
  replication enabled with any rule that consumes events, any active listener
  cursor. Compute `outbox_has_readers` at config apply (and at boot) into an
  `ArcSwap<bool>`/`AtomicBool` that `object_helpers.rs:40` reads.
- **Edge cases:**
  - Enable delivery/replication: events written before the flip are not there —
    already documented ("not kept for later"); scheduled replication covers the
    gap.
  - The admin Event outbox panel shows nothing in a default install → its empty
    state explains: "The proxy records events only while event delivery or
    replication is on."
  - Lifecycle and maintenance emit events too — same gate.
  - A listener cursor created at runtime (not config) must flip the flag at
    creation time.
- **Test:** default config PUT → 0 outbox rows; enable delivery → next PUT → 1 row.

### P4.3 Lifecycle: one outbox batch per page
- `lifecycle/worker.rs:1255` appends per expired object; buffer per page like
  `replication/worker.rs:1104`.

### P4.4 Node-local tables leave the synced blob (larger; own milestone)
- **Now:** the synced config DB carries 24 h of outbox, `replication_parity_objects`
  (one row per object per side per rule), run history, failure rings. Every IAM
  change uploads the whole file (bae52e25 cites a 338 MB prod file) and every peer
  downloads it.
- **Option A (preferred):** move node-local tables into a separate, unsynced
  SQLCipher file (`deltaglider_jobs.db`, same key), like `bucket_usage`. Schema
  migration v30: create tables in the new file, copy rows, drop from the config DB,
  VACUUM.
- **Option B:** upload a snapshot holding only synced tables. Rejected unless A
  proves hard: sync-base, generation numbers and older peers must all accept the
  reduced blob.
- **Edge cases (A):**
  - Mixed-version cluster: an old peer uploads a blob *with* the node-local
    tables; the new node's merge must ignore them (already excluded from merge —
    verify `iam_merge` only touches IAM tables) and must not re-import them.
  - Backup/restore (`POST /api/admin/backup`) — decide which file(s) a backup
    includes; keep the restore of external_identities working.
  - Recovery wizard and key rotation (`rekey_file`) must re-key both files.
  - `job_store` leases are node-local already (B3) — they move with the tables.
  - Downgrade: document that a downgrade after v30 loses job history (not IAM).
- **Risk:** medium-high. Separate design note before code.

### P4.5 (Optional, measure first) WAL on the config DB
- Rollback journal + `synchronous=FULL` ≈ 17 ms per commit here. WAL + NORMAL →
  1 fsync at checkpoint. The sync upload must checkpoint (`wal_checkpoint(TRUNCATE)`)
  under `upload_lock` before reading the file; `rekey_file` likewise.
- Only if P4.1/P4.2 leave measurable commit cost on hot paths. Otherwise skip.

---

## P5. Copy

### P5.1 Client CopyObject of a delta uses the delta fast path
- **Now:** `copy.rs:72` `read_copy_source` → `engine.retrieve` (decode) →
  `copy.rs:99` `store_client_write` (encode). Same folder: 12 requests, 2 MiB GET
  of the reference, 2 xdelta3 runs, to write a 53 B delta. REPLACE self-copy of a
  delta (metadata-only): 11 requests, 2 MiB GET, 2 xdelta3.
- **Change:** when the source is a delta and the destination deltaspace's
  reference sha equals the source's (or the destination has no reference and the
  source's can be seeded — see P5.3), call `transfer::delta_passthrough_copy`
  inside the same quota gate, per-key write lock, conditional checks
  (`x-amz-copy-source-if-*`, destination `If-None-Match`), maintenance gate and
  event emission that `store_client_write` uses today.
- **Metadata rules for the client copy (differ from replication):**
  - `COPY` directive: keep content-type and user metadata; `REPLACE`: take them
    from the request.
  - Drop `multipart_etag` (client CopyObject returns a single-part ETag today —
    keep that answer); the ETag stays the MD5 of the logical content.
  - `dg-created-at` = copy time; strip rule provenance stamps
    (`transfer.rs` stamps them for replication only).
  - Storage class / SSE request fields: same handling as today's path.
- **Metadata-only REPLACE self-copy of a delta:** rewrite the `.delta` object's
  metadata in place (`replace_metadata_in_place`, S3 self-copy with
  `copy-source-if-match`) → 1-2 requests, 0 xdelta3.
- **Edge cases:**
  - `dg-no-delta: true` in a REPLACE → destination must be passthrough → old path.
  - Destination bucket has compression disabled → old path (passthrough result).
  - Cross-backend or different encryption key id → the fast path reads raw delta
    bytes through the engine (decrypt) and writes them (encrypt) — still no
    xdelta3; confirm `delta_passthrough_copy` handles per-backend keys.
  - Destination is a `replication_target_only` bucket → 403 before any work.
  - Source changes during the copy → generation pin (fast path has it) → 503
    SlowDown, as the docs promise.
  - Max size: the delta fast path moves the small delta; keep today's
    `max_object_size` rule for a *delta* destination (logical size) so the answer
    for a too-large source does not change.
- **Test:** ledger rows for each shape; integration test on MinIO that GET of the
  copy returns identical bytes + ETag.

### P5.2 `StorageBackend::copy_within`: server-side copy
- **Now:** no backend copy primitive. Every copy (CopyObject passthrough, admin
  bulk copy/move, same-backend replication, lifecycle transition, migrate within
  one backend, reference seeding) moves the full object down and up through the
  proxy.
- **Trait:** `copy_within(src: StoredRef, dst: StoredRef, metadata: CopyMeta,
  fence: Option<Fence>) -> Result<CopyOutcome, StorageError>`; no default.
  `CopyOutcome::Copied { etag, .. }` or `CopyOutcome::Unsupported` (caller falls
  back to today's stream copy).
  - **S3:** CopyObject ≤ 5 GiB; above, UploadPartCopy in parts (part size from
    the existing multipart relay settings); metadata via `REPLACE` with the full
    DG metadata set from `self_copy_plan` (every dg-* header restated, SSE
    restated). `x-amz-copy-source-if-match` on the source ETag (generation pin).
  - **Filesystem:** `copy_file_range` (falls back to read/write on EXDEV), xattrs
    written explicitly, atomic rename, deferred fsync like other writes.
  - **Encrypting:** forward only when source and destination use the same key id
    (ciphertext is valid: the AAD binds only the chunk index,
    `encrypting/format.rs:202`); else `Unsupported`.
  - **Routing:** forward only when both buckets resolve to the same backend
    *instance*; else `Unsupported`.
  - **B2 / others:** a 501 or NotImplemented → `Unsupported`, cached per backend
    fingerprint in `BackendCapabilityCache` so the next copy skips the attempt.
- **Callers (one at a time, each its own commit + ledger rows):** CopyObject
  passthrough → admin bulk copy/move → transfer.rs (replication, lifecycle,
  migrate) → reference seeding (P5.3).
- **Edge cases:**
  - Multipart ETag of the source: S3 CopyObject returns a new single-part ETag
    for objects ≤ 5 GiB; DG stores the logical ETag in metadata
    (`dg-multipart-etag`) — the client-visible ETag comes from DG metadata, so
    restate it. Test: multipart source → copy → HEAD ETag unchanged where today's
    path keeps it.
  - Object-lock / retention headers on the backend: not used by DG; ignore.
  - Cross-region same-backend: S3 CopyObject works; cost moves to the provider.
  - Quota: the destination counter must count the logical size exactly as the
    stream path does (shared helper).
  - Events/provenance: identical to the stream path.
  - Maintenance gate and write lock: held across the server-side copy.

### P5.3 Reference seeding by server-side copy
- **Now:** seeding `reference.bin` into a new folder: 14 requests, 2 MiB down +
  2 MiB up, plus a HEAD of the source reference right after its GET
  (`transfer.rs:1342-1346`).
- **Change:** `copy_within` with `fence = IfNoneMatch` (create-only). If the backend
  refuses a conditional CopyObject (probe: AWS supports `If-None-Match` on
  CopyObject? **verify per provider**: AWS, MinIO/silo, Hetzner, R2), fall back
  to the fenced PUT. Capability recorded per backend.
- **Edge:** the lock protocol (`RefWriteProof`, `with_dest_prefix_lock`) must wrap
  the copy exactly as it wraps the PUT; the proof type must be required by the
  `copy_within` reference variant (separate method `copy_reference_fenced` that
  takes the proof, so a reference copy cannot bypass the guard).

### P5.4 Fast path: trim the HEADs
- `ObjectTransferRequest.dest_prior: Option<CounterPrior>` from the caller's HEAD
  or listing (event consumer already HEADs the destination at
  `event_consumer.rs:1307`; `fast_path_prior` at `transfer.rs:1303` HEADs again).
- `get_delta` returns the metadata of its GET response (drop the generation-pin
  HEAD at `transfer.rs:1332`; the pin is the ETag of that response).
- `verify_destination`: HEAD only the written form (1 HEAD, not `engine.head`'s 2).
- ≈ 4 HEADs per copy. A stale caller prior only skews the usage counter (and the
  next scan fixes it) — state this in the doc comment.

### P5.5 UploadPartCopy of a delta source: decode once
- Each part reconstructs the whole object (`copy.rs:309`) when the source is
  ≤ 16 MiB. Decode once into the range-spool cache, matched on source ETag.

---

## P6. Maintenance jobs

### P6.1 HEAD only the listed form
- The listing (`dedup_keep_latest`) already sees which forms exist. Pass
  `StoredForms` to the visit; HEAD both only when both exist (crash leftover).
  Reencrypt nothing-to-rewrite 2 → 1 HEAD; migrate verify 4 → 2.
- Shares `StoredForms` with P2.1 — land P2.1 first.

### P6.2 Pass the job's HEAD into the write
- `copy_object_from(Some(meta))` and on into the store as `known_prior`
  (reencrypt prototype: −2 HEAD per rewritten object). Migrate: the skip-check
  result is the prior; with a known-absent prior the store skips the DELETE.
- Write gate freezes the bucket during these jobs → prior cannot go stale from
  clients; replication into a gated bucket is deferred (existing rule) → safe.

### P6.3 Backfill: 6 → 3-4 requests per object
- Reuse the GET response headers instead of the self-copy HEAD
  (`storage/s3/objects.rs:104,125`); fence the self-copy with
  `x-amz-copy-source-if-match`; cache "ACL not supported" per bucket.

### P6.4 One listing instead of three
- Reencrypt/backfill list the bucket for counting, for objects, and
  `list_reference_prefixes` (`storage/s3/mod.rs:1231-1241`). Count from the usage
  counter when it is trusted (else count during the objects pass and show an
  indeterminate bar until done); collect reference prefixes during the walk.
- **Edge case:** progress percent semantics (`progress_percent` / `display_percent`)
  change for the first pass — UI must show "counting…" honestly.

### P6.5 Quota scan stamps the usage counter
- `object_helpers.rs:289-303` + `usage_scanner.rs:293`: a quota bucket whose
  counter is untrusted re-lists every 5 min forever (100k objects ≈ 1,200 LIST/h).
  A complete, non-truncated scan of prefix `""` stamps the counter via
  `begin_scan`/`overwrite_from_scan` (as `bucket_scan` does). Then the quota check
  is O(1).
- **Edge:** totals definitions must match (`scan_totals` vs scanner) — reuse
  `scan_totals`; writes during the scan are kept (90fad667 rule).

### P6.6 Maintenance poll
- 3 s poll takes the DB mutex 1,200×/h (0 pages written). Raise to TTL/2 and rely
  on the existing notify for new jobs. Low priority.

---

## P7. Per-request CPU and contention

### P7.1 `Arc<AuthenticatedUser>`
- 2-3 deep clones per request (`api/auth.rs` `AuthenticatedUser::from`,
  `iam/middleware.rs:113`, the s3s adapter): 2.3-10.4 µs. Store `Arc` in the IAM
  index; request extensions carry the `Arc`. Bootstrap user: build its policy
  once per config, not per request (0.5 µs).

### P7.2 Precompiled IAM matchers
- `can_with_context` 20.8 µs (3 rules) - 45.5 µs (13 rules): iam-rs rebuilds
  `StatementMatch` strings, `interpolate_variables` and `ArnMatcher::from_pattern`
  per evaluation (`iam/permissions.rs:439`).
- **Change:** per user, at IAM index build: compile action and resource matchers;
  call iam-rs only for statements with a `Condition`. `${iam:username}` /
  `${iam:access_key_id}` are per-user constants → substitute at build.
- **Lock:** differential proptest: random policies × random requests → compiled
  evaluator == iam-rs, `PROPTEST_CASES=2000` locally. Plus the existing
  permission truth tables. **Risk:** authorization semantics — this item does not
  merge without the differential test.

### P7.3 Filesystem syscalls
- GET: tell ENOENT from ENODATA on the `.delta` xattr miss (drops a `statx`);
  `open_object` reuses the metadata that `head()` read, or `fgetxattr` on the
  opened fd (also closes a TOCTOU between stat and open).
- PUT: drop the second bucket `statx`, `mkdir` → `create_dir_all` once per
  directory (cache known-existing dirs per process with invalidation on prune),
  the `dup`/`close` pair, the `.delta` fallback `statx`; merge blocking hops
  (9 → 2 `spawn_blocking`).
- **Edge case:** `prune_empty_dirs` can remove a cached-as-existing directory →
  the write must retry `create_dir_all` on ENOENT once (0aef820c covers the
  related race — keep its test).

### P7.4 Passthrough PUT and the deltaspace lock (last; highest risk in plan)
- 16 parallel passthrough PUTs into one folder: 67-135 req/s vs 890-1,250 across
  folders (`store.rs:388` holds the deltaspace lock across the fsyncs / upstream
  PUT).
- **Precondition:** every write path (client PUT, form POST, CopyObject,
  replication, lifecycle, migrate, admin bulk) takes the per-key object write
  lock. Today client paths do (`lock_and_check`); background paths do not. Add a
  source guard that every `store*` caller holds a per-key guard (type it: the
  store takes `&KeyWriteGuard`).
- **Then:** a passthrough write takes the per-key lock only.
- **Why per-key is mandatory:** a passthrough write and a delta write of the same
  key could each `drop_other_form` the other → both 200, object gone.
- **Filesystem:** `prune_empty_dirs` (`filesystem/mod.rs:958`) can remove the
  folder under a write → the write's `create_dir_all`-retry (P7.3) must exist
  first, or keep the deltaspace lock on the filesystem backend only.
- **Delta writes** keep the deltaspace lock (reference RMW).
- **Tests:** extend `race_conditions_test` with mixed passthrough/delta writes of
  one key and with writes racing a sibling DELETE's prune.

---

## P8. API contract and UI (D1, D3)

### P8.1 `metadata=true` lists storage facts with zero HEADs (D1)
- **New contract:** each `<Contents>` carries `<UserMetadata>` with
  `dg-note` (`delta` | `passthrough` | `reference`-baseline marker as today's HEAD
  shows it) and `dg-delta-size` (stored size) — from the listing alone. The
  logical size and ETag of the entry come from the facts index as today.
  User metadata (`x-amz-meta-*` of the client) is no longer listed; HeadObject
  returns it.
- **Engine:** `list_entry_needs_head` (`engine/list.rs:118`) changes to: HEAD only an
  unresolved delta stub (facts miss), which P1.6b then heals for good.
  Delta-eligible non-delta keys: no HEAD (the listing proves the form).
- **Consumers to update in the same commit:** `cli/stats.rs` detailed mode (needs
  logical sizes only — verify it reads nothing else), `iam/listing.rs`,
  `api/admin/savings.rs` (`enrich_list_metadata`), `s3_adapter_s3s/list.rs`
  (XML extension), docs `reference/s3-api-compatibility.md` (ListObjectsV2 row),
  changelog (call it out: a non-standard parameter changed meaning).
- **Edge cases:** encrypted passthrough → stored size = ciphertext size: list it as
  the HEAD does today (check what HEAD's `dg-delta-size` holds for encrypted
  passthrough — match exactly); folder markers; IAM-filtered pages; public-prefix
  anonymous readers → apply `strip_fingerprint_metadata` rules (anonymous readers
  get no `dg-tool`; decide whether anonymous get `dg-note` at all — default: no
  DG facts for anonymous, same as fingerprint stance).

### P8.2 UI table reads storage facts from the listing
- `listObjects` (`s3client.ts:168`) sends `metadata=true` via SDK middleware
  (build step adds the query param — it must be part of the signed canonical
  query, so add it before signing); a deserialize middleware reads
  `<UserMetadata>` from the raw XML before the SDK drops unknown elements.
- `headCache` is filled from the listing; the per-row HEAD queue
  (`useS3Browser.ts:300-340`, `enrichKeys`, `pumpHeads`) is deleted.
- **Measured target:** folder visit 100 HEADs (→ 200 upstream) → 0. A→B→Back: 200 → 0.
- **Guard:** vitest with `mockFetch`: open a folder of 100 files → 0 HEAD
  requests (an unmatched HEAD fails the test).
- **Edge cases:** pagination (each page carries its facts); the 60 s auto-refresh
  replaces facts; a listing without facts for an entry (older proxy is impossible —
  embedded UI; but a facts miss) → show the neutral tag, not a spinner forever.

### P8.3 Inspector: seed from the listing, HEAD once
- `InspectorPanel.tsx:301-330`: drop `cachedHead` from the effect deps (it re-runs
  when the row's facts land → 2 HEADs); seed storage stats from the listing facts;
  HEAD once on open for the full header view.

### P8.4 Write responses carry storage facts (D3)
- **Headers** on PutObject, CompleteMultipartUpload, form POST (204/201), and
  CopyObject: `x-amz-storage-type` (existing name), `x-deltaglider-stored-size`
  (existing), `x-deltaglider-ref-sha256` (new, when delta), and
  `x-deltaglider-baseline: created` (new, when this write created the folder's
  `reference.bin`).
- **Gating:** always for authenticated writers; never for anonymous (public
  prefixes cannot write anyway; open-access mode counts as authenticated-equivalent
  for this rule — decide and test). `DGP_DEBUG_HEADERS` keeps gating the GET/HEAD
  debug headers and the listing debug header (unchanged).
- **Unify:** `form_post.rs:829` already sends `x-amz-storage-type` unconditionally —
  route it through the same helper.
- **UI:** upload queue (`useUploadQueue.ts:196`) reads the headers through an SDK
  middleware that captures raw response headers for PutObject and
  CompleteMultipartUpload (`@aws-sdk/lib-storage` Upload). Drops the HEAD per
  uploaded file **and** the `listDirectObjects` "folder was empty" check: the
  server now says `baseline: created`. `uploadCreatedBaseline`
  (`savings.ts:146`) reads the header.
- **Edge cases:** multipart Complete: the facts are known only after the
  reassembled store — the headers come from the store result; a write that fails
  ratio and lands passthrough → `passthrough`; same-origin UI → no CORS change; a
  third-party browser app under `cors_permissive` needs
  `Access-Control-Expose-Headers` for these names — add them to the expose list.
- **Docs:** `reference/s3-api-compatibility.md` PutObject row + a short section on
  DG response headers; remove the UI comment "verified against 1.19, no
  storage-type header" (`savings.ts:136`).

### P8.5 Savings chip: invalidate on change; cache descendants
- `SavingsCache::invalidate` is dead code (`savings.rs:167`); a refetch after a
  bulk change returns the pre-change number for ≤ 5 min. The admin bulk handlers
  invalidate the touched prefix and its parents.
- While walking prefix P, cache totals for each descendant directory whose key range
  ended before any truncation (keys arrive sorted) → drill-down within 5 min costs 0
  upstream. Memory ≈ 120 B/dir under the 4,096-entry cap.

### P8.6 Single delete in the inspector does not re-list
- `App.tsx:798` `onDeleted={s3.mutate}` → remove the row locally, decrement counts;
  the 60 s tick reconciles.

### P8.7 Auto-refresh reads page 1 only
- `useS3Browser.ts:276-283` re-lists up to 10 pages per minute per tab. Refresh the
  first page and keep the loaded pages; manual Refresh re-lists all.
- **Edge case:** an object deleted elsewhere on page 3 stays visible until a
  manual refresh or a navigation — state it in the status bar "updated …".

### P8.8 Minor UI
- AnalyticsSection: one `GET /usage/bucket/:b` per bucket → batch endpoint
  `GET /usage/buckets` (real dashboard use, not test convenience).
- `listBuckets` calls `getBucketOrigins` outside react-query → use the shared key.
- Logs SSE pauses in hidden tabs (`adminApi/logs.ts:54`).

---

## 2. Order, dependencies, releases

```
P0 ─┬─ P1 (all independent; P1.1 + P1.2 ship first as a patch release)
    ├─ P2.1 ─┬─ P6.1/P6.2
    │        └─ P7.4 (also needs P7.3)
    ├─ P2.2, P2.3, P2.4, P2.5, P2.6 ─ P2.7
    ├─ P3.1 ─ P3.2, P3.4 ;  P3.3, P3.5, P3.6, P3.7 independent
    ├─ P4.1 ─ P4.2 ─ P4.3 ;  P4.4 (design note first) ; P4.5 optional
    ├─ P5.1 ─ P5.4 ;  P5.2 ─ P5.3 ; P5.5
    ├─ P6.3, P6.4, P6.5, P6.6 independent
    ├─ P7.1 ─ P7.2
    └─ P8.1 ─ P8.2 ─ P8.3 ;  P8.4 ; P8.5-P8.8 independent
```

| Release | Content | Why grouped |
|---|---|---|
| 2.0.9 (patch) | P1.1 nodelay, P1.2 UI loop | Both are defects with one-line fixes. |
| 2.1.0 | P0, rest of P1, P2.1-P2.3, P3.1-P3.4, P4.1-P4.3, P8.1-P8.4 | Request-count cuts on every hot verb + the API contract change (D1/D3) in one minor version with one changelog section. |
| 2.2.0 | P5 (server-side copy), P6, P2.4-P2.7, P3.5-P3.7, P8.5-P8.8 | Copy primitive needs provider verification; jobs ride on it. |
| 2.3.0 | P4.4 (node-local DB split), P7 | Schema migration and authz/lock changes deserve their own release and a soak on prod. |

Before each release: `scripts/test-prod-config.sh` against prod state, the full
`cargo test --all --locked` with MinIO, and the ledger diff in the release notes.

## 3. Risk register

| Item | Risk | Mitigation |
|---|---|---|
| P3.1 one-round-trip GET | Wrong body/ETag on a form race | Newest-wins kept; per-edge-case tests; MinIO race test with concurrent overwrite form flips. |
| P5.1/P5.2 copy | Wrong metadata on the destination (ETag, multipart ETag, created-at, encryption) | Metadata built by one shared function used by stream and server-side paths; golden test comparing HEAD of a stream copy vs a server-side copy, field by field. |
| P5.3 reference seeding | Second baseline in multi-instance | Copy takes `RefWriteProof`; conditional create; fallback to fenced PUT; lock-protocol model test extended. |
| P7.2 IAM compile | Authorization drift | Differential proptest is a merge blocker. |
| P7.4 lock removal | Lost object on mixed-form race | Per-key lock typed into `store*`; race tests; filesystem keeps deltaspace lock until P7.3 lands. |
| P4.1 queue-only outbox | Event loss on crash (accepted, D2) | Documented; scheduled replication repairs; bounded queue with backpressure. |
| P4.4 DB split | Mixed-version sync, backup, rekey | Design note + upgrade/downgrade test matrix before code. |
| P8.1 contract change | Unknown external users of `metadata=true` user metadata | Changelog call-out; HeadObject is the documented way. |
| P1.3 readiness | Ready while a backend just died | Verdict age bound 2 × interval; live probe when stale. |

## 4. Not planned (and why)

- Single-form HEAD hint (D4): breaks newest-wins after a crash.
- Skipping xdelta3 for a first delta against itself: needs a hand-built VCDIFF;
  forbidden by the "xdelta3 CLI only" decision.
- Removing the post-write fence HEAD in `put_delta`: the lock protocol needs it.
- Skipping the reference lock when the reference exists: a peer's reclaim could
  orphan the delta.
- Per-request INFO logs: no measurable effect above noise; revisit after P7.

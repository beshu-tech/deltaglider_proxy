# Rust adversarial audit — findings & realistic follow-up plan

A 60-agent adversarial audit (6 specialist auditors → per-finding refutation panel
→ synthesis) ran over the 75 KLOC Rust codebase. **13 findings confirmed, 40 refuted**
by the verification panel. The architecture themes were then individually
cost/benefit-evaluated against the real code (not taken at face value). This doc
records what shipped, what's planned, and — importantly — **what was deliberately
rejected as not worth it**, with the evidence.

## Shipped (commit `fix(audit): bound form-POST body buffer + map object 404 to NotFound`)

- **H1 (high) — form-POST body buffered with no ceiling.** `intercept_form_post_for_s3s`
  called `axum::body::to_bytes(body, usize::MAX)`. It runs *above* `DefaultBodyLimit`
  in the layer stack, and `DefaultBodyLimit` only does an eager `Content-Length`
  check — a **chunked** `multipart/form-data` POST slipped past it entirely, so a
  single oversized/chunked request could OOM the process. Now bounded by the
  hot-reloadable `max_object_size`; oversized → 413. **Also the likely root of the
  aws-cli chunked-PUT failure seen against prod.** `src/startup.rs`.
- **A4 (medium) — object-level 404 misclassified as 500.** `classify_s3_error`
  lacked `NoSuchKey`, so `CopyObject` on a concurrently-deleted source surfaced as
  HTTP 500 instead of 404. Added `NoSuchKey`/404 → `NotFound` for non-bucket-level
  ops; unit-tested both branches. `src/storage/s3.rs`.

## Planned (small, clearly worth it)

- **A5 (low) — form-POST signature length not constant-time validated.** Before the
  `ct_eq` HMAC compare, `signature.to_ascii_lowercase()` runs on unvalidated
  variable-length input, leaking the expected signature length via timing. Fix:
  `if signature.len() != 64 || !signature.bytes().all(|b| b.is_ascii_hexdigit())
  { return Err(..) }` before any timing-sensitive work. ~5 lines, defense-in-depth.
  `src/api/handlers/form_post.rs:580`.
## Done (this follow-up pass — small, clearly worth it)

- **A5 (low) — form-POST signature length now constant-time validated.** Reject any
  `signature` that isn't exactly 64 ASCII-hex chars *before* the `to_ascii_lowercase()`
  + `ct_eq`, closing the length-via-timing oracle. `src/api/handlers/form_post.rs:580`.
- **A3 cheap mitigation — "restrictive-first" ordering pinned.** The two
  bucket-derived ArcSwaps were already published public-prefix-first (the
  restrictive-bias order); added a comment documenting the invariant so it can't drift,
  since a reorder would open a torn-read window that grants *more* access during a
  bucket-privatize apply. No behavior change. `src/api/admin/config/mod.rs:157`.

## Deliberately left as-is (fix would be worse than the bug)

- **Low — replay cache transient overshoot of `MAX_REPLAY_ENTRIES`** (`src/api/auth.rs:871`).
  `prune_replay_cache` enforces the hard oldest-first cap on *every* request before
  insert, so the overshoot is bounded by concurrency and immediately re-pruned. Making
  prune+insert atomic would require a lock around the hot auth path — adding contention
  to fix a transient, self-correcting, bounded overshoot is a net-negative trade.
  Documented-acceptable.

## Rejected — evaluated and deliberately NOT doing (with evidence)

These are the audit's headline "architecture themes." Each was deep-evaluated against
the real code; all three deflate substantially. Recording the reasoning so they don't
get re-raised.

### A2 — "convert `engine.store`/`retrieve` to `BoxStream<Bytes>`" — **REJECTED**

The audit billed this as the "highest-leverage structural fix that dissolves the
H2/H3 memory-blowup class." It does not.

- **Passthrough already streams** and is the *de-prioritized* case (already-compressed
  media). `store_passthrough_chunked` (`store.rs:413`), `retrieve_stream` /
  `retrieve_stream_range` (`retrieve.rs:40,178`) hash incrementally and hand
  chunks/paths to the streaming trait methods.
- **The delta path — the product's reason to exist — is inherently buffered** and
  cannot be streamed: xdelta3 needs the whole input piped to stdin (`codec.rs:128`),
  reconstruction needs the full reference + delta, and `store_inner` computes
  `Sha256` + `Md5` over the whole body for content-addressing/ETag *before* the
  delta/passthrough decision (`store.rs:78-79`), with GET re-hashing the reconstructed
  object (`retrieve.rs:401`).
- **Every PUT caller already holds the full body** anyway, because s3s verifies
  `x-amz-content-sha256` over the complete buffer (`s3_adapter_s3s.rs:1193`). A
  streaming `store()` would be fed an already-materialized `Bytes`.
- **Cost:** ~1.5–3 weeks, ~27 call sites, breaks the object-safe `StorageBackend`
  `#[async_trait]`, and `put_delta`/`put_reference` would *still* take `&[u8]`.

Net: a multi-week, trait-breaking refactor that leaves the primary (delta) path
buffered exactly as today and only "helps" passthrough, which already streams where
it matters. The OOM is already capped by the `max_object_size` checks at
`store.rs:68` / `retrieve.rs:355`.

### A1 — "global aggregate in-flight byte budget for PUT/form-POST" — **LOW PRIORITY, NOT A FIX**

Factually correct that only multipart has a global byte budget (`multipart.rs:216`).
But:

- `ConcurrencyLimitLayer(1024)` is the **outermost** layer (`startup.rs:654`, applied
  last) and caps concurrent requests; single-shot PUT/form-POST bytes are bounded by
  *live request count*, which it already governs. (Multipart needed its own budget
  because parts accumulate across uploads with a 24 h idle TTL — bytes uncorrelated
  with active request count.)
- The 102 GB/500 GB headline stacks an **adversary on top of a misconfiguration**
  (5 GB `max_object_size` × 100 maxed concurrent uploads). At default 100 MB and
  realistic concurrency it's single-digit GB.
- The proxy **refuses to start without auth**; every write path is behind SigV4 +
  admission. The public-prefix path is **read-only**, so there's no untrusted-uploader
  vector. For the actual deployment (internal proxy, trusted CI, single instance) the
  exposure is low.

Worth a ~3–5 h `Arc<AtomicI64>` + RAII reservation guard *only if* the proxy ever
serves untrusted public uploaders or operators routinely raise `max_object_size` into
the GB range. Otherwise it's a guard on a door `ConcurrencyLimitLayer` already locks.

### A3 unification — "one `ArcSwap<ConfigSnapshot>`" — **REJECTED (the race is negligible; the fix is harmful)**

- The headline "a just-disabled user still passes" is **mis-attributed**: user
  enable/disable goes through Users-CRUD → `rebuild_iam_index` → a *single atomic*
  `iam_state.store(...)` (`users.rs:156`), read once per request via `load_full()`
  (`auth.rs:583`). No cross-snapshot inconsistency — a request sees old XOR new IAM,
  atomically.
- The only genuine cross-snapshot case is a combined bucket+IAM `/config/apply`: 2–3
  sequential atomic pointer swaps, nanoseconds apart, operator-triggered a few times a
  day, and the snapshots **fail toward the more-restrictive side** (the anon user is
  scoped to whichever prefix snapshot it read). One request, sub-microsecond window,
  not externally triggerable.
- The unification is multi-day and **fights the design**: IAM is mutated by *two*
  paths (Users-CRUD hot path + config-apply). A single `ConfigSnapshot` would force
  every one-bit user edit to clone-and-republish engine config + buckets + admission
  blocks — a regression in blast radius and complexity. The separate `iam_state`
  ArcSwap exists precisely so Users-CRUD can swap it in isolation (and version-gate via
  `IAM_VERSION`).

Verdict: document; optionally the 2-line "restrictive-first" reorder above. Do not
unify.

## What the audit confirmed is SOLID

Codec correctness, SigV4 core, IAM evaluation (Deny precedence, conditions, `*`
equivalence, `${username}` expansion), and the multipart lifecycle all survived
adversarial scrutiny — the refuted pile was dominated by "auditor missed the guard"
on these paths.

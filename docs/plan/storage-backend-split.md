# Splitting `StorageBackend` by concern

Status: decided, Option C (one trait, the stored form becomes a parameter).
Progress in §6. Written 2026-10-30.

## 1. The problem

`StorageBackend` (`src/storage/traits.rs`) has 67 methods, 30 of them with
a default. Eleven types implement it:

| Impl | Kind |
|---|---|
| `FilesystemBackend` | raw (xattr metadata, `deltaspaces/` layout) |
| `S3Backend` | raw (user-metadata headers, listing facts, CAS fences) |
| `EncryptingBackend<B>` | wrapper: seals bodies and metadata per stored form |
| `RoutingBackend` | wrapper: virtual bucket → backend + real bucket |
| `FaultyFs` | test wrapper (fault injection) |
| six test doubles | `NullInner`, `FencingFs`, `CountingBackend`, routing `TestBackend`, transfer `AbortSpy` + `SpyRef` |

`dynosaur` generates `DynStorageBackend` and the `Box<T>`/`&T` bridges.

Most methods are not object storage. They are the delta format's
vocabulary: the stored form (reference, delta, passthrough) is in the
method NAME, so each operation exists up to three times, and every wrapper
forwards each copy.

## 2. The 67 methods by concern

| Concern | Methods | n |
|---|---|---|
| Buckets | `create_bucket`, `ensure_declared_bucket`, `delete_bucket`, `list_buckets`, `list_buckets_with_dates`, `list_bucket_origins`, `head_bucket` | 7 |
| Reference reads | `get_reference`, `get_reference_to_file`, `open_reference_source`, `get_reference_metadata`, `has_reference` | 5 |
| Reference writes (unfenced) | `put_reference`, `put_reference_from_file`, `put_reference_metadata`, `delete_reference` | 4 |
| Reference fence | `reference_fence`, `reference_fence_with_metadata`, `write_reference_fenced` | 3 |
| Delta | `get_delta`, `put_delta`, `get_delta_metadata`, `delete_delta` | 4 |
| Passthrough reads | `get_passthrough`, `get_passthrough_metadata`, `get_passthrough_stream`, `get_passthrough_stream_range`, `open_passthrough_range` | 5 |
| Passthrough writes | `put_passthrough`, `put_passthrough_file`, `put_passthrough_parts`, `put_passthrough_chunked`, `file_put_spool_bytes`, `delete_passthrough` | 6 |
| Metadata rewrite in place | `get_passthrough_metadata_for_rewrite`, `put_passthrough_metadata` | 2 |
| Form-generic object ops | `open_object`, `copy_within`, `variant_version`, `delete_variant_if` | 4 |
| Multipart + capability flags | `create_multipart_upload`, `upload_part`, `complete_multipart_upload`, `abort_multipart_upload`, `multipart_storage_label`, `supports_native_multipart`, `lite_list_carries_logical_facts` | 7 |
| Routing identity | `resolved_backend_name`, `storage_identity`, `previous_storage_identity` | 3 |
| Encryption control | `write_refusal`, `read_key_claim`, `write_key_claim` | 3 |
| Deltaspace scans | `scan_deltaspace`, `scan_deltaspace_lite`, `holds_only_reference`, `list_deltaspaces`, `list_reference_prefixes` | 5 |
| Listing | `bulk_list_objects`, `bulk_list_objects_with_baselines`, `resolve_listed_sizes`, `listing_tokens`, `enrich_list_metadata`, `list_objects_delegated`, `put_directory_marker`, `total_size` | 8 |
| Durability | `flush_pending` | 1 |

Facts that shape the options:

- The newest methods already take the form as a parameter: `open_object`
  (`StoredObject`), `copy_within`, `variant_version`, `delete_variant_if`
  (`ObjectVariant`). The encrypting wrapper's seal helpers take
  `StoredObject` too (`seal_bytes`, `reseal`, `present_raw`).
- `get_passthrough_stream` and `get_passthrough_stream_range` had no
  production caller: the engine reads with `open_object` /
  `open_passthrough_range` (source guard
  `the_engine_reads_bytes_with_their_metadata`), and the encrypting wrapper
  already derived both from those two.
- The engine reaches the four unfenced reference writes from ONE place
  (`ReferenceLockGuard::write_reference` in `engine/locking.rs`), as a
  `match` over `RefWrite`; `unfenced_reference_write` repeats that match.
- The routing-only methods (`list_bucket_origins`, `resolved_backend_name`,
  `storage_identity`, `previous_storage_identity`) are asked through the
  engine's `DynStorageBackend`, which may or may not be a router.

## 3. Options

### Option A: `ObjectStore` (key → bytes) under a `DeltaStore` written once

A small trait for plain keys (get/put/head/delete/list/conditional put),
and the reference/delta/passthrough vocabulary written once above it.

- **Encrypting wrapper.** The seal binds bucket, folder, FORM and name
  (`seal.rs`). At the `ObjectStore` level the wrapper sees a key, so it
  must parse the form back out of it (`reference.bin`, `*.delta`). That
  parse is backend-specific (filesystem: `{bucket}/deltaspaces/{prefix}/`,
  folder marker file `DIR_MARKER_FILE`) and ambiguous for a user key that
  ends in `.delta`. Or the wrapper stays above `DeltaStore`, and then the
  split does not reduce its forwarding.
- **Routing wrapper.** Fits either level.
- **Fences and proofs.** `RefWriteProof` and the fence must reach the
  conditional put of `reference.bin` and of deltas. A key-level store
  cannot ask for a proof by form unless the key type carries the form,
  which is Option C.
- **Formats.** The two raw backends differ in more than the key: the
  filesystem keeps `Durability::Sync` for references and defers object
  fsyncs (`with_deferred_fsync`), hardlinks/copies reference files
  locally, reads xattrs during its directory walk, and has no ETag; S3 has
  the listing-facts namespace (`!!1`/`!!2`), listing tokens, heal, native
  multipart and CAS fences. An `ObjectStore` that keeps all of that stays
  at about 35 methods, and a generic `DeltaStore` must reproduce the exact
  per-backend layout. The format risk is high and the gain is small.
- **Round trips.** `reference_fence_with_metadata` (one HEAD for fence +
  metadata) and `open_object` (one GET for bytes + metadata) need a
  `head`/`get` that returns ETag and metadata together; possible, but every
  "one request" guarantee must be proven again for the new layer.

Verdict: rejected. It moves the vocabulary instead of removing it, and it
puts the storage formats at risk.

### Option B: one trait split into supertraits by concern

`trait StorageBackend: Buckets + References + Deltas + Passthrough + ...`.

- **dynosaur.** Verified with dynosaur 0.3.1 (scratch crate): the
  generated `DynStore` does NOT implement an async supertrait (`E0277:
  DynStore<'_>: Buckets is not satisfied`); dynosaur erases only the
  annotated trait's own methods. Every supertrait would need its own
  hand-written erasure inside one dyn type, which is the code dynosaur
  generates today (67 boxed signatures), written by hand.
- **Wrappers.** They still implement every method of every supertrait.
  The split groups impl blocks; it removes no forwarding.
- **Proofs, fences, formats, round trips.** Unchanged (nothing moves).

Verdict: rejected for now. High cost (drop dynosaur), no reduction.

### Option C (recommended): one trait; the stored form becomes a parameter

Keep one trait and dynosaur. Remove methods in two ways:

1. Delete methods that are derivable from a required method and have no
   production caller (stream reads → `open_object` /
   `open_passthrough_range`).
2. Collapse each `{reference, delta, passthrough} × operation` family into
   ONE method whose address names the form. Proofs move INTO the address
   type, so a form that needs a proof cannot be named without one:

   ```rust
   enum StoredWrite<'a> {                     // sketch
       Delta { name: &'a str, proof: &'a RefWriteProof },
       Passthrough { name: &'a str },
   }
   ```

   Reference writes keep `RefWrite` + proof (they already have one shape).
   Convenience names that callers and tests use (`put_reference`, ...)
   live in `StorageBackendExt`, a blanket extension over the trait, so
   they have exactly one implementation and no wrapper forwards them.

- **Encrypting wrapper.** It already works by `StoredObject`; each
  collapsed method is one `match` where it had three methods.
- **Routing wrapper.** One `route_existing!` per operation instead of one
  per form.
- **Fences and proofs.** `write_reference_fenced` and the fence methods
  stay as they are (no default). Each collapsed method has NO default, so
  the compiler still makes every wrapper forward it. `RefWriteProof` is
  still made only in `engine/locking.rs::reference_writes`.
- **Formats.** None changes: each `match` arm runs the old method's body
  (same key, path, xattr, headers, durability).
- **Round trips.** None changes: same bodies. Only tracing span names
  change (`write_reference` instead of `put_reference` and so on).
- **Source guards.** Guards that pin a removed name move to the new name
  in the same step, or go away when the compiler takes over the rule (a
  removed method cannot be called).

## 4. Target shape (estimate)

| Concern | Today | After |
|---|---|---|
| Reference writes | 4 | 1 (`write_reference`) |
| Passthrough stream reads | 2 | 0 |
| Metadata reads (3 forms) | 3 | 1 (`head_object(StoredObject)`) |
| Buffered reads (3 forms) | 3 | 1 (`get_object(StoredObject)`) |
| Deletes of delta/passthrough | 2 | 1 (`delete_object(ObjectVariant)`; reference delete stays in `write_reference`) |
| Puts of delta/passthrough (bytes, file, parts, chunks) | 5 | 1 (`put_object(StoredWrite, PutBody)`) |

About 67 → 55 methods, and fewer per-form copies in each wrapper. Later,
with evidence: a declarative forwarding macro for the pure forwards (the
encrypting wrapper has about 20, `SpyRef` forwards all).

## 5. Migration, one green step each

1. Remove `get_passthrough_stream` and `get_passthrough_stream_range` (no
   production caller). `open_object_by_parts` /
   `open_passthrough_range_by_parts` read the buffered `get_passthrough`.
   Tests read through `open_object` / `open_passthrough_range`.
2. `write_reference(op: RefWrite, proof)` replaces `put_reference`,
   `put_reference_from_file`, `put_reference_metadata`, `delete_reference`
   (no default). `unfenced_reference_write` and the lock guard's
   single-instance path call it. The four names live on in
   `StorageBackendExt`. The encrypting wrapper seals the op once for both
   the fenced and the unfenced write.
3. `head_object(StoredObject)` replaces the three `get_*_metadata`.
   Guard to move: `engine/metadata.rs` (`.get_delta_metadata(`,
   `.get_passthrough_metadata(`, `.get_reference_metadata(`).
4. `get_object(StoredObject)` replaces `get_reference`, `get_delta`,
   `get_passthrough`. Guard: `follow_up_reads_are_pinned_to_the_first_read`
   (`inner.get_passthrough(`).
5. `delete_object(bucket, prefix, name, ObjectVariant)` replaces
   `delete_delta` / `delete_passthrough`, next to `delete_variant_if`.
6. `put_object(StoredWrite, PutBody)` replaces `put_delta`,
   `put_passthrough`, `_file`, `_parts`, `_chunked`. Guards to move in the
   same step: `usage.rs` `COMMITS`, `conditional.rs` write list,
   `buffered_put_bodies_are_not_copied`. `file_put_spool_bytes` stays.
7. Re-measure; decide on the forwarding macro.

Each step must stop and be re-planned if it would change a storage
format, a request count, or who can make a proof.

## 6. Progress

- Step 1: done (commit "drop the two metadata-less passthrough stream reads").
- Step 2: done (commit "one unfenced reference write"). 67 → 62 methods.
- Found while doing step 2, kept as it was: the encrypting wrapper runs the
  witness check on a FENCED reference delete (`check_write_unless(.., true)`
  still calls `check_witness`), but never on an UNFENCED one (the old
  `delete_reference` forwarded as is). So a single-instance delete-reclaim
  of `reference.bin` passes a witness refusal that a multi-instance one
  does not. Decide which one is right before step 5.

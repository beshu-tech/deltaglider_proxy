# DeltaGlider Proxy Hardening Plan

Three-phase plan addressing critical-to-low severity issues from the senior engineering review.
Ordered strategically: data integrity first, then operational stability, then performance/polish.

---

## Phase 1: Data Integrity & Safety Foundation ✅ COMPLETED

**Rationale**: A storage system that can lose or corrupt data is worthless regardless of how fast it is. Every issue in this phase addresses a scenario where data can be silently lost, corrupted, or the system can deadlock.

### 1.1 ✅ Fix Dual-Mutex Deadlock Risk in ReferenceCache

**Severity**: HIGH | **Risk**: Deadlock under concurrent access
**File**: `src/deltaglider/cache.rs`

`ReferenceCache::put()` acquires `self.cache.lock()` then `self.current_size.lock()` sequentially. If lock ordering is ever reversed (by future code or compiler reordering), this deadlocks. Merge both into a single `Mutex<CacheInner>` struct.

**Changes**:
- Create `CacheInner { cache: LruCache, current_size: usize }`
- Single `Mutex<CacheInner>` replaces two separate mutexes
- All methods acquire one lock instead of two

### 1.2 ✅ Fix XML Injection in Stub Handlers

**Severity**: MEDIUM | **Risk**: XML injection via bucket name
**File**: `src/api/handlers.rs`

`list_multipart_uploads()` interpolates `bucket` directly into XML without escaping. While the extractor validates the bucket, defense-in-depth requires escaping.

**Changes**:
- Use `escape_xml()` from `xml.rs` in the `list_multipart_uploads` format string

### 1.3 ✅ Replace Blocking `path.exists()` with Async Equivalents

**Severity**: LOW-MEDIUM | **Risk**: Tokio worker thread starvation under load
**File**: `src/storage/filesystem.rs`

Multiple `StorageBackend` methods call synchronous `path.exists()` and `path.is_dir()` inside async functions. Under high concurrency, these block the Tokio runtime worker threads.

**Changes**:
- Replace `path.exists()` with `tokio::fs::try_exists(&path).await.unwrap_or(false)`
- Replace `path.is_dir()` with `tokio::fs::metadata(&path).await.map(|m| m.is_dir()).unwrap_or(false)`
- Apply to: `has_reference`, `get_reference`, `delete_reference`, `get_delta`, `delete_delta`, `get_direct`, `delete_direct`, `scan_deltaspace`, `list_deltaspaces`, `exists`, `delete`, `get_raw`, `list_prefix`, `read_metadata`, `dir_size`

### 1.4 ✅ Atomic Filesystem Writes (Write-to-Temp + Rename)

**Severity**: CRITICAL | **Risk**: Truncated files after crash = silent data corruption
**File**: `src/storage/filesystem.rs`

Every `fs::write()` is a non-atomic operation. A crash mid-write produces a truncated file that cannot be detected. Replace all data writes with the standard atomic pattern: write to a temp file in the same directory, fsync, then `rename()` (which is atomic on POSIX).

**Changes**:
- Add `atomic_write(&self, path: &Path, data: &[u8])` helper method
  - Creates a `NamedTempFile` in the same parent directory (same filesystem = atomic rename)
  - Writes data, calls `flush()` + `sync_all()`
  - Uses `persist()` (which does rename) to atomically replace the target
- Replace all `fs::write()` calls with `atomic_write()` in `put_raw`, `put_reference`, `put_delta`, `put_direct`, `write_metadata`

### 1.5 ✅ Transactional Data + Metadata Write Ordering

**Severity**: CRITICAL | **Risk**: Orphaned data or dangling metadata pointers after crash
**File**: `src/storage/filesystem.rs`

Even with atomic individual writes, the *order* matters. If we write metadata first and crash before data, the system has metadata pointing to non-existent data. If we write data first and crash before metadata, we have orphaned data (wasted disk, but no corruption -- the object simply doesn't exist yet).

**Strategy**: Data-first, metadata-as-commit-marker.
- Write data atomically
- Write metadata atomically
- On read: metadata existence = object exists. Missing metadata = incomplete write (ignore data file).

**Changes**:
- Ensure all `put_*` methods write data file FIRST, then metadata
- Verify `scan_deltaspace` only returns objects with valid `.meta` files (already the case)
- Add startup log warning for orphaned data files (data without metadata)

### 1.6 ✅ Per-DeltaSpace Concurrency Control (Striped Locking)

**Severity**: CRITICAL | **Risk**: Two concurrent PUTs to empty deltaspace → reference overwrite → data loss
**Files**: `src/deltaglider/engine.rs`, `Cargo.toml` (no new deps needed)

The `store()` method does a check-then-act (`has_reference` → `set_reference`) without synchronization. Two concurrent requests to the same empty deltaspace can both create a reference, and the loser's deltas become unrecoverable.

**Strategy**: Per-prefix async mutex using `tokio::sync::Mutex` stored in a striped lock map.

**Changes**:
- Add `prefix_locks: parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>` to `DeltaGliderEngine`
- Add `async fn acquire_prefix_lock(&self, prefix: &str) -> OwnedMutexGuard<()>` helper
- Wrap the critical section in `store()` (from `has_reference` check through `store_delta`/`store_direct`) with the per-prefix lock
- Also wrap `delete()` since it may clean up the reference
- Lock granularity: per-deltaspace-prefix, so different prefixes don't block each other

---

## Phase 2: Operational Stability Under Load

**Rationale**: Once data integrity is guaranteed, the next priority is ensuring the system doesn't fall over under production workloads. These issues cause OOM, excessive latency, or resource exhaustion.

### 2.1 Configurable SHA256 Verification on GET

**Severity**: CRITICAL (performance) | **File**: `src/deltaglider/engine.rs`, `src/config.rs`
- Add `verify_on_read: bool` to `Config` (default: `true` for safety, configurable to `false` for performance)
- Skip SHA256 computation in `retrieve()` when disabled
- Log a startup warning when verification is disabled

### 2.2 Validate xdelta3 CLI at Startup, Not Per-Request

**Severity**: HIGH | **File**: `src/deltaglider/codec.rs`, `src/main.rs`
- Add `cli_available: bool` field to `DeltaCodec`, probed once at construction via `xdelta3 -V`
- Skip CLI fallback path in `decode()` when binary not found
- Log clear warning at startup if CLI is unavailable (degraded interop mode)

### 2.3 Fix `put_reference_metadata` No-Op on S3

**Severity**: HIGH | **File**: `src/storage/s3.rs`
- Implement S3 copy-object-with-new-metadata (CopyObject with MetadataDirective=REPLACE)
- This is the standard S3 pattern for updating metadata without re-uploading data
- Without this fix, legacy migration silently fails on S3, causing every legacy `retrieve()` to re-trigger migration on every request

### 2.4 Bounded Concurrency for Delta Encoding

**Severity**: MEDIUM | **File**: `src/deltaglider/engine.rs`
- Add `tokio::sync::Semaphore` with configurable permits (default: num_cpus)
- Acquire permit before `codec.encode()` / `codec.decode()` calls
- Composes with Task 1.6's prefix locks: prefix lock = correctness, semaphore = stability
- Prevents CPU saturation and ensures health checks remain responsive

### 2.5 Zero-Copy Cache with `Bytes`

**Severity**: MEDIUM | **File**: `src/deltaglider/cache.rs`, `src/deltaglider/engine.rs`
- Change cache value type from `Vec<u8>` to `bytes::Bytes`
- `cache.get()` returns `Bytes` (cheap clone via refcount) instead of cloning the entire Vec
- `Bytes` derefs to `&[u8]`, so callers of `get_reference_cached()` need no changes
- Cleaner after Task 1.1 merged cache into single `Mutex<CacheInner>`

### 2.6 Disk-Full Detection and Reporting

**Severity**: MEDIUM | **File**: `src/storage/traits.rs`, `src/storage/filesystem.rs`, `src/api/errors.rs`
- Add `StorageError::DiskFull` variant
- Detect ENOSPC via raw OS error code (`raw_os_error() == Some(28)` on Linux/macOS) since `ErrorKind::StorageFull` is unstable in Rust
- After Task 1.4's `atomic_write()`, ENOSPC can manifest in `NamedTempFile::new_in()` or `write_all()` — error kind is preserved through `spawn_blocking`
- Map to specific S3 error with actionable message

### 2.7 XML Injection in `S3Error::to_xml()`

**Severity**: MEDIUM | **File**: `src/api/errors.rs`

Same class of vulnerability fixed in Task 1.2. `S3Error::to_xml()` interpolates `resource` (derived from user-controlled S3 keys) directly into XML. Object key validation blocks NUL, backslashes, and `..` segments but does NOT block XML metacharacters (`<`, `>`, `&`, `"`). A key like `dir/<evil>payload</evil>/file.zip` passes validation and injects XML.

**Changes**:
- Apply `escape_xml()` to `resource` in `to_xml()`, same pattern as Task 1.2

---

## Phase 3: Performance & Code Quality

**Rationale**: With data safety and stability addressed, these changes improve throughput, reduce resource waste, and clean up technical debt.

### 3.1 Incremental `list_objects_v2` (Don't Load Everything)

**Severity**: HIGH | **File**: `src/deltaglider/engine.rs`
- Refactor to scan only the deltaspaces matching the requested prefix
- Stop scanning once `max_keys` objects are collected
- Use the continuation token to skip already-returned deltaspaces

### 3.2 FileRouter Allocation Optimization

**Severity**: LOW | **File**: `src/deltaglider/file_router.rs`
- Extract extension once with `rsplit('.')`
- Look up in a `HashSet<&'static str>` instead of iterating and allocating

### 3.3 Remove Legacy `StorageType` Enum

**Severity**: LOW | **File**: `src/types.rs`
- Remove `StorageType` enum and its `From<&StorageInfo>` impl
- Confirmed: zero references outside `types.rs` — pure dead code

### ~~3.4 Drop `async_trait` for Native Async Fn in Traits~~ DROPPED

**Reason**: The codebase uses `Box<dyn StorageBackend>` via `DynEngine`. Native `async fn` in traits produces opaque `impl Future` return types that are NOT object-safe — `dyn StorageBackend` dispatch would break. The `async_trait` Box overhead (~50 bytes/call) is negligible compared to disk I/O and network I/O in every storage operation. The refactoring risk (23 methods × 3 impl blocks) far outweighs the benefit.

### 3.4 `HashSet` in `find_deltaspaces_recursive`

**Severity**: LOW | **File**: `src/storage/filesystem.rs`
- Replace `Vec<String>` + `contains()` with `HashSet<String>`
- O(1) dedup instead of O(n)

### 3.5 Size Guard on Object Copy

**Severity**: MEDIUM | **File**: `src/api/handlers.rs`
- Check source object size against `max_object_size` before storing the copy
- Prevents memory exhaustion from copying large objects

### 3.6 Remove `Clone` from `S3Error`

**Severity**: LOW | **File**: `src/api/errors.rs`
- Remove `#[derive(Clone)]` from `S3Error`
- Confirmed: `Clone` is never called on `S3Error` itself; `.clone()` calls in `to_xml()` operate on inner `String` fields

### 3.7 Prefix Lock Map Cleanup

**Severity**: LOW | **File**: `src/deltaglider/engine.rs`
- The `prefix_locks` HashMap grows unboundedly as new prefixes are accessed (~100 bytes/entry)
- Prune entries where `Arc::strong_count() == 1` (only the map holds a reference = no active lock)
- Trigger cleanup periodically or after each `delete()` operation

---

## Dependency Summary

| Phase | New Crate Dependencies | Risk Level |
|-------|----------------------|------------|
| 1     | None                 | Low (uses existing deps) |
| 2     | None (`bytes` already in dependency tree via axum/hyper) | Low |
| 3     | None                 | Low |

## Execution Notes

- Each task is independently testable and can be verified with `cargo test`
- Phase 1 tasks should be committed individually for clean git history
- Run `cargo clippy` after each task to catch regressions
- The existing test suite should pass throughout (no behavioral changes, only safety improvements)

# Changelog

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

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

DeltaGlider Proxy — an S3-compatible proxy with transparent delta compression for versioned binary artifacts. Clients see a standard S3 API; the proxy silently deduplicates using xdelta3 against a per-prefix reference baseline.

## Build & Dev Commands

```bash
# Rust
cargo build --release
cargo fmt --all                # fix formatting
cargo clippy --locked --all-targets --all-features -- -D warnings

# Demo UI (must be built before cargo build — rust-embed embeds dist/)
cd demo/s3-browser/ui && npm ci && npm run build
npm run dev                    # dev server on :5173, proxies /api to :9001

# Tests (MinIO required for S3 integration tests)
cargo test --all --locked
cargo test --test delta_test              # single test file
cargo test --test delta_test test_name    # single test
cargo test -- --nocapture                 # show println output
cargo test --lib                          # unit tests only, no integration

# Docker (multi-stage: UI build → Rust build → slim runtime)
docker build -t deltaglider-proxy .
```

CI runs: `fmt` → `clippy -D warnings` → `test` (with MinIO) → RustSec audit. All must pass.

## Architecture

```
HTTP request
  → api/handlers.rs    S3-compatible handlers (GET/PUT/HEAD/DELETE/LIST)
  → api/auth.rs        Optional SigV4 authentication middleware
  → deltaglider/engine.rs   Orchestration: route, compress, cache, reconstruct
  → storage/traits.rs       StorageBackend trait (async_trait, object-safe)
  → storage/filesystem.rs   Local filesystem impl (xattr metadata)
  → storage/s3.rs           AWS S3/MinIO impl (S3 user metadata headers)
```

**Key data flow:**
- **PUT**: FileRouter decides delta-eligible vs passthrough → compute delta against reference baseline → store if ratio < threshold, else passthrough
- **GET**: Read metadata → if delta, reconstruct from reference + delta via xdelta3 → stream to client transparently
- **Deltaspace layout**: `bucket/prefix/.dg/reference.bin` + `bucket/prefix/key[.delta]`

**Important types:**
- `StorageBackend` (trait in `storage/traits.rs`) — all storage operations; two impls: Filesystem, S3
- `SharedConfig` = `Arc<RwLock<Config>>` — hot-reloadable via admin API
- `RetrieveResponse` — enum: `Streamed` (zero-copy passthrough) vs `Buffered` (delta reconstruction)
- `FileMetadata` (in `types.rs`) — per-object metadata with DG-specific tags

**Config:** TOML file (`deltaglider_proxy.toml`) with env var overrides (`DGP_*` prefix). See `deltaglider_proxy.toml.example`.

## Frontend (demo/s3-browser/ui)

React 18 + TypeScript + Ant Design 6. Hash-based routing (`#/browse`, `#/upload`, `#/settings`). Embedded in the Rust binary via `rust-embed` and served on listen_addr + 1 (e.g., proxy on :9000, UI on :9001).

Key hooks: `useS3Browser`, `useSelection`, `useUploadQueue`. Admin API in `adminApi.ts`, S3 operations in `s3client.ts`.

## Testing

Tests in `tests/` use a `TestServer` harness (`tests/common/mod.rs`) that spawns a real proxy instance with a temp directory (filesystem backend) or MinIO (S3 backend). Port allocation uses an atomic counter starting at 19000.

S3 integration tests require MinIO running on localhost:9000. CI starts MinIO automatically; locally, use `docker run -p 9000:9000 minio/minio server /data`.

## Conventions

- Clippy warnings are errors in CI (`-D warnings`)
- The proxy is transparent: clients must not know delta compression is happening
- `x-amz-storage-type` response header exposes storage strategy (delta/passthrough/reference) for debugging
- Delta-eligible file types are defined in `deltaglider/file_router.rs`
- Passthrough files (images, video) skip delta entirely — already compressed
- Streaming is preferred for large files; delta reconstruction requires buffering the reference

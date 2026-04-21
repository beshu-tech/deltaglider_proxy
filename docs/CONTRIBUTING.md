# Contributing

*Build, test, project structure*

Thanks for your interest in contributing! Whether it's a bug report, feature idea, or code change, we appreciate your help.

## Getting Started

### Prerequisites

- **Rust stable** — install via [rustup](https://rustup.rs/) (the repo's `rust-toolchain.toml` pins the channel automatically)
- **Node.js 20+** — needed to build the embedded demo UI
- **Docker** — optional, used for running MinIO in integration tests

### Building from Source

```bash
# 1. Clone the repo
git clone https://github.com/beshu-tech/deltaglider_proxy.git
cd deltaglider_proxy

# 2. Build the demo UI (rust-embed bakes it into the binary)
cd demo/s3-browser/ui && npm install && npm run build && cd -

# 3. Build the proxy
cargo build

# 4. Run it
DGP_DATA_DIR=./data cargo run
```

The S3 API and demo UI both start on `http://localhost:9000`. The UI is available at `http://localhost:9000/_/`.

### Running Tests

```bash
# Unit tests (no external services needed)
cargo test

# Integration tests (needs Docker — MinIO is started automatically via testcontainers)
cargo test --test s3_integration_test
```

### Code Quality Checks

The CI runs these on every push — make sure they pass before submitting a PR:

```bash
cargo fmt --all -- --check                                          # Formatting
cargo clippy --locked --all-targets --all-features -- -D warnings   # Lints
cargo test --all --locked                                           # Tests
```

## Project Structure

```
src/
├── api/
│   ├── mod.rs           # API module root, S3Error type
│   ├── handlers/        # S3 API handlers (object, bucket, multipart, status)
│   ├── auth.rs          # SigV4 auth middleware + public prefix bypass
│   ├── admin/           # Admin API (login, config, users, groups, auth providers, backup)
│   ├── aws_chunked.rs   # AWS chunked transfer encoding decoder
│   ├── extractors.rs    # Axum request extractors (ValidatedBucket, ValidatedPath)
│   ├── errors.rs        # S3 error responses
│   └── xml.rs           # S3 XML response/request builders
├── deltaglider/
│   ├── engine/          # Core engine (mod, store, retrieve submodules)
│   ├── codec.rs         # xdelta3 encode/decode (subprocess)
│   ├── cache.rs         # Reference file LRU cache (moka)
│   └── file_router.rs   # File type routing (delta-eligible vs passthrough)
├── storage/
│   ├── traits.rs        # StorageBackend trait (async_trait, object-safe)
│   ├── filesystem.rs    # Local filesystem backend (xattr metadata)
│   ├── s3.rs            # S3 backend (AWS SDK)
│   ├── routing.rs       # Multi-backend routing (virtual bucket → real backend)
│   └── xattr_meta.rs    # Extended attribute helpers
├── iam/
│   ├── mod.rs           # IamState enum, IamIndex, hot-swap
│   ├── types.rs         # IamUser, Permission, AuthenticatedUser, S3Action
│   ├── permissions.rs   # ABAC evaluation (legacy + iam-rs with conditions)
│   ├── middleware.rs     # Per-request IAM authorization middleware
│   ├── keygen.rs        # Secure access key generation
│   └── external_auth/   # OAuth/OIDC providers (Google, generic OIDC)
├── config_db/           # Encrypted SQLCipher database (users, groups, providers)
├── config.rs            # Flat Config struct + ENV_VAR_REGISTRY (YAML canonical; TOML deprecated but still loads)
├── config_sections.rs   # Sectioned YAML wire shape (admission/access/storage/advanced) + shorthand expanders
├── admission/           # Pre-auth admission chain (operator-authored + synthesized blocks)
├── bucket_policy.rs     # Per-bucket policies + PublicPrefixSnapshot
├── startup.rs           # Server startup, router assembly, middleware stack
├── session.rs           # Admin session store (OsRng tokens, IP binding)
├── rate_limiter.rs      # Per-IP rate limiting (token bucket)
├── metadata_cache.rs    # Object metadata LRU cache (moka)
├── audit.rs             # Audit logging helpers
├── multipart.rs         # In-memory multipart upload state
├── types.rs             # Core types (FileMetadata, StorageInfo, etc.)
├── demo.rs              # Embedded UI (rust-embed) + admin API router
├── lib.rs               # Library root
└── main.rs              # Entry point
demo/s3-browser/ui/      # React 18 + TypeScript + Ant Design 6 admin GUI
tests/                   # Integration tests (S3 ops, auth, IAM, public prefixes)
docs/                    # Documentation
```

### Key Concepts

- **DeltaSpace**: A group of objects under the same directory prefix that share a single baseline for delta compression. For example, all objects under `releases/` form one deltaspace.
- **Reference file**: The internal baseline stored once per deltaspace. All deltas are computed against it (no chaining), so reconstruction is always O(1).
- **StorageBackend**: A trait abstracting where bytes live — local filesystem or upstream S3. Adding a new backend means implementing this trait.
- **File router**: Decides whether a file is delta-eligible based on its extension (`.zip`, `.tar.gz`, etc.) or should be stored as passthrough (`.jpg`, `.mp4`, etc.).

## Submitting Changes

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Run `cargo fmt`, `cargo clippy`, and `cargo test`
4. Open a pull request with a clear description of what and why

## Reporting Issues

Open an issue on GitHub. If it's a bug, include:

- What you expected vs. what happened
- Steps to reproduce
- DeltaGlider Proxy version (`deltaglider_proxy --version`)
- Backend type (filesystem or S3)

## License

By contributing, you agree that your contributions will be licensed under [GPL-2.0-only](LICENSE).

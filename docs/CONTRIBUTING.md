# Contributing to DeltaGlider Proxy

Thanks for your interest in contributing! Whether it's a bug report, feature idea, or code change, we appreciate your help.

## Getting Started

### Prerequisites

- **Rust 1.75+** — install via [rustup](https://rustup.rs/)
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
DELTAGLIDER_PROXY_DATA_DIR=./data cargo run
```

The S3 API starts on `http://localhost:9000` and the demo UI on `http://localhost:9001`.

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
cargo fmt --all -- --check   # Formatting
cargo clippy -- -D warnings  # Lints
cargo test --all              # Tests
```

## Project Structure

```
src/
├── api/
│   ├── handlers.rs    # S3 API endpoint handlers
│   ├── extractors.rs  # Axum request extractors
│   ├── errors.rs      # S3 error responses
│   └── xml.rs         # S3 XML response/request builders
├── deltaglider/
│   ├── engine.rs      # Core delta compression logic
│   ├── codec.rs       # xdelta3 encode/decode
│   ├── cache.rs       # Reference file LRU cache
│   └── file_router.rs # File type routing
├── storage/
│   ├── traits.rs      # StorageBackend trait
│   ├── filesystem.rs  # Local filesystem backend
│   └── s3.rs          # S3 backend
├── auth.rs            # SigV4 authentication middleware
├── config.rs          # Configuration loading
├── multipart.rs       # In-memory multipart upload state management
├── types.rs           # Core types (FileMetadata, etc)
├── demo.rs            # Embedded React demo UI (rust-embed)
└── main.rs            # Server entry point
demo/s3-browser/ui/    # React demo UI source (Vite + TypeScript)
tests/                 # Integration tests
docs/                  # Additional documentation
```

### Key Concepts

- **DeltaSpace**: A group of objects under the same directory prefix that share a single baseline for delta compression. For example, all objects under `releases/` form one deltaspace.
- **Reference file**: The internal baseline stored once per deltaspace. All deltas are computed against it (no chaining), so reconstruction is always O(1).
- **StorageBackend**: A trait abstracting where bytes live — local filesystem or upstream S3. Adding a new backend means implementing this trait.
- **File router**: Decides whether a file is delta-eligible based on its extension (`.zip`, `.tar.gz`, etc.) or should be stored directly (`.jpg`, `.mp4`, etc.).

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

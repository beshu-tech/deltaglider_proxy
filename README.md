# DeltaGlider Proxy

**S3-compatible proxy with transparent delta compression**

DeltaGlider Proxy sits between your S3 clients and backend storage, automatically deduplicating similar files using [xdelta3](https://github.com/jmacd/xdelta). Your existing tools work unchanged while storage costs drop significantly for versioned artifacts.

```
┌─────────────────┐       ┌─────────────────┐       ┌─────────────────┐
│   S3 Client     │──────▶│  DeltaGlider    │──────▶│   Backend S3    │
│   (aws-cli,     │◀──────│     Proxy       │◀──────│ (AWS/MinIO/R2)  │
│   boto3, etc)   │       │   (xdelta3)     │       │                 │
└─────────────────┘       └─────────────────┘       └─────────────────┘
```

## Why DeltaGlider Proxy?

**The problem**: You're storing versioned binary artifacts (Docker images, ML models, game builds, firmware). Each version is 90% identical to the previous, but S3 stores them as completely separate objects.

**The solution**: DeltaGlider Proxy stores only the deltas. v2.zip that's 95% similar to v1.zip? Stored as a ~5% sized delta. Clients still GET the full file - reconstruction is transparent.

```bash
# Your workflow doesn't change (aside from pointing at DeltaGlider Proxy)
DELTAGLIDER_PROXY_ENDPOINT=http://localhost:9000
aws --endpoint-url "$DELTAGLIDER_PROXY_ENDPOINT" s3 cp releases/v1.zip s3://default/releases/v1.zip  # Seeds the deltaspace baseline
aws --endpoint-url "$DELTAGLIDER_PROXY_ENDPOINT" s3 cp releases/v2.zip s3://default/releases/v2.zip  # Stored as delta (~5% size)
aws --endpoint-url "$DELTAGLIDER_PROXY_ENDPOINT" s3 cp s3://default/releases/v2.zip ./               # Reconstructed transparently
```

## Quick Start

```bash
# Build (requires Node.js for the demo UI)
cd demo/s3-browser/ui && npm install && npm run build && cd -
cargo build --release

# Run with filesystem backend (for testing)
DELTAGLIDER_PROXY_DATA_DIR=./data ./target/release/deltaglider_proxy

# Or with S3 backend (example: MinIO on :9000; run DeltaGlider Proxy on a different port)
docker compose up -d
DELTAGLIDER_PROXY_LISTEN_ADDR=127.0.0.1:9002 \
DELTAGLIDER_PROXY_S3_BUCKET=deltaglider_proxy-data \
DELTAGLIDER_PROXY_S3_ENDPOINT=http://localhost:9000 \
AWS_ACCESS_KEY_ID=minioadmin \
AWS_SECRET_ACCESS_KEY=minioadmin \
./target/release/deltaglider_proxy
```

An embedded demo UI automatically starts on **S3 port + 1** (e.g. `http://localhost:9001` or `http://localhost:9003`).

Point your S3 client at DeltaGlider Proxy (default `http://localhost:9000`, or `http://localhost:9002` in the MinIO example above) and use bucket name `default`:

```bash
# aws-cli
DELTAGLIDER_PROXY_ENDPOINT=http://localhost:9000
aws --endpoint-url "$DELTAGLIDER_PROXY_ENDPOINT" s3 cp file.zip s3://default/file.zip

# boto3
import os, boto3
s3 = boto3.client('s3', endpoint_url=os.environ.get('DELTAGLIDER_PROXY_ENDPOINT', 'http://localhost:9000'))
s3.upload_file('file.zip', 'default', 'file.zip')
```

## How It Works

DeltaGlider Proxy organizes objects into **deltaspaces** based on directory path:

```
releases/v1.zip  ─┐
releases/v2.zip  ─┼─▶ deltaspace "releases" (internal baseline + per-object deltas)
releases/v3.zip  ─┘

models/bert.tar.gz  ─┐
models/gpt.tar.gz   ─┼─▶ deltaspace "models"
```

Within each deltaspace:
1. DeltaGlider Proxy maintains one internal **baseline** (`reference.bin`) seeded by the first delta-eligible upload
2. Eligible uploads are stored as a delta against that baseline when `delta_size/original_size < max_delta_ratio`
3. Otherwise, the object is stored directly (no delta)
4. GET/HEAD operate on the original user key; reconstruction is transparent

**What gets delta-compressed?**

By default: `.zip`, `.tar`, `.tgz`, `.tar.gz`, `.tar.bz2`, `.tar.xz`, `.jar`, `.war`, `.ear`, `.rar`, `.7z`, `.dmg`, `.iso`, `.sql`, `.dump`, `.bak`, `.backup`

Files like `.jpg`, `.png`, `.mp4` are stored directly (already compressed, don't delta well).

More details:
- `docs/OPERATIONS.md`
- `docs/STORAGE_FORMAT.md`

## Configuration

### Environment Variables

```bash
# Server
DELTAGLIDER_PROXY_LISTEN_ADDR=0.0.0.0:9000
DELTAGLIDER_PROXY_DEFAULT_BUCKET=default

# DeltaGlider
DELTAGLIDER_PROXY_MAX_DELTA_RATIO=0.5      # Store as delta if ratio < 50%
DELTAGLIDER_PROXY_MAX_OBJECT_SIZE=104857600 # 100MB max (xdelta3 constraint)
DELTAGLIDER_PROXY_CACHE_SIZE_MB=100         # Reference cache for fast reconstruction

# Filesystem backend
DELTAGLIDER_PROXY_DATA_DIR=./data

# S3 backend (takes precedence if DELTAGLIDER_PROXY_S3_BUCKET is set)
DELTAGLIDER_PROXY_S3_BUCKET=deltaglider_proxy-data
DELTAGLIDER_PROXY_S3_ENDPOINT=http://localhost:9000
DELTAGLIDER_PROXY_S3_REGION=us-east-1
DELTAGLIDER_PROXY_S3_FORCE_PATH_STYLE=true
AWS_ACCESS_KEY_ID=...
AWS_SECRET_ACCESS_KEY=...
```

### Config File

```bash
./deltaglider_proxy --config deltaglider_proxy.toml
```

See [deltaglider_proxy.toml.example](deltaglider_proxy.toml.example) for all options.

### CLI

```
deltaglider_proxy 0.1.0
S3-compatible proxy with delta compression

USAGE:
    deltaglider_proxy [OPTIONS]

OPTIONS:
    -c, --config <FILE>     Path to configuration file
    -l, --listen <ADDR>     Listen address (overrides config)
    -b, --bucket <BUCKET>   Default bucket name (overrides config)
    -v, --verbose           Enable verbose logging
    -h, --help              Print help
    -V, --version           Print version
```

## Demo UI

An embedded React-based S3 browser ships inside the binary (via [rust-embed](https://crates.io/crates/rust-embed)). It starts automatically on **S3 port + 1** — no extra container needed.

```bash
# Local dev: build the UI first, then the Rust binary
cd demo/s3-browser/ui && npm install && npm run build && cd -
cargo run -- --listen 127.0.0.1:9002
# S3 API  → http://localhost:9002
# Demo UI → http://localhost:9003  (auto-connects to S3 API)

# Docker: the Dockerfile handles the Node build automatically
cd demo/s3-browser && docker compose up --build
# S3 API  → http://localhost:9002
# Demo UI → http://localhost:9003
```

The UI auto-detects the S3 endpoint (port - 1) from its own URL, so it works out of the box with zero configuration.

## Development

### Prerequisites

- Rust 1.75+
- Node.js 20+ (for demo UI build)
- Docker (for MinIO testing)

### Build & Test

```bash
# Build
cargo build

# Run unit tests
cargo test

# Run with MinIO for integration tests
docker compose up -d
cargo test -- --ignored  # Runs S3 backend tests
```

### Project Structure

```
src/
├── api/
│   ├── handlers.rs    # S3 API endpoint handlers
│   ├── extractors.rs  # Axum request extractors
│   ├── errors.rs      # S3 error responses
│   └── xml.rs         # S3 XML response builders
├── deltaglider/
│   ├── engine.rs      # Core delta compression logic
│   ├── codec.rs       # xdelta3 encode/decode
│   ├── cache.rs       # Reference file LRU cache
│   └── file_router.rs # File type routing
├── storage/
│   ├── traits.rs      # StorageBackend trait
│   ├── filesystem.rs  # Local filesystem backend
│   └── s3.rs          # S3 backend
├── config.rs          # Configuration loading
├── types.rs           # Core types (FileMetadata, etc)
├── demo.rs            # Embedded React demo UI (rust-embed, served on S3 port + 1)
└── main.rs            # Server entry point
demo/s3-browser/ui/    # React demo UI source (Vite + TypeScript)
```

## S3 API Compatibility

| Operation | Status | Notes |
|-----------|--------|-------|
| PutObject | ✅ | Full support |
| GetObject | ✅ | Full support |
| HeadObject | ✅ | Full support |
| DeleteObject | ✅ | Full support |
| ListObjectsV2 | ✅ | Full support |
| DeleteObjects | ✅ | Batch delete |
| CopyObject | ✅ | Via x-amz-copy-source header |
| CreateBucket | ✅ | Single-bucket mode |
| HeadBucket | ✅ | Single-bucket mode |
| DeleteBucket | ✅ | Must be empty |
| ListBuckets | ✅ | Returns configured bucket |

**Not implemented**: Multipart upload, versioning, ACLs, lifecycle policies.

## Performance

PUT responses include `x-amz-storage-type`:

```bash
curl -i -X PUT --data-binary @releases/v2.zip http://localhost:9000/default/releases/v2.zip
# x-amz-storage-type: delta  # or "direct"
```

Typical savings for versioned artifacts:

| Use Case | Typical Savings |
|----------|-----------------|
| Docker layers | 60-80% |
| ML model checkpoints | 70-90% |
| Game builds | 50-70% |
| Firmware images | 80-95% |

## Architecture Notes

- **No database**: State is derived from storage. Each file has a `.meta` sidecar with checksums and storage info.
- **Checksums everywhere**: SHA-256 verified on every GET. Corruption is detected immediately.
- **Async throughout**: Tokio runtime, async S3 SDK, non-blocking I/O.
- **Dynamic backends**: Same code paths for filesystem and S3. Backend is a trait object.

## License

GPLv2 (GPL-2.0-only).

See [LICENSE](LICENSE) for the full license text.

## See Also

- [xdelta3](https://github.com/jmacd/xdelta) - The delta compression library
- [rsync](https://rsync.samba.org/) - Rolling checksums, different approach
- [casync](https://github.com/systemd/casync) - Content-addressable sync

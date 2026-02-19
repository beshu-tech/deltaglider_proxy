# DeltaGlider Proxy

**S3-compatible proxy with transparent delta compression**

DeltaGlider Proxy sits between your S3 clients and backend storage, automatically deduplicating similar files using [xdelta3](https://github.com/jmacd/xdelta). Your existing tools work unchanged while storage costs drop significantly for versioned artifacts.

![DeltaGlider Proxy Architecture](docs/diagram.png)

## Why DeltaGlider Proxy?

**The problem**: You're storing versioned binary artifacts (Docker images, ML models, game builds, firmware). Each version is 90% identical to the previous, but S3 stores them as completely separate objects.

**The solution**: DeltaGlider Proxy stores only the deltas. v2.zip that's 95% similar to v1.zip? Stored as a ~5% sized delta. Clients still GET the full file - reconstruction is transparent.

```bash
# Your workflow doesn't change (aside from pointing at DeltaGlider Proxy)
DGP_ENDPOINT=http://localhost:9000
aws --endpoint-url "$DGP_ENDPOINT" s3 mb s3://mybucket                                       # Create a bucket first
aws --endpoint-url "$DGP_ENDPOINT" s3 cp releases/v1.zip s3://mybucket/releases/v1.zip  # Seeds the deltaspace baseline
aws --endpoint-url "$DGP_ENDPOINT" s3 cp releases/v2.zip s3://mybucket/releases/v2.zip  # Stored as delta (~5% size)
aws --endpoint-url "$DGP_ENDPOINT" s3 cp s3://mybucket/releases/v2.zip ./               # Reconstructed transparently
```

## Quick Start

```bash
# Build (requires Node.js for the demo UI)
cd demo/s3-browser/ui && npm install && npm run build && cd -
cargo build --release

# Run with filesystem backend (for testing)
DGP_DATA_DIR=./data ./target/release/deltaglider_proxy

# Or with S3 backend (example: MinIO on :9000; run DeltaGlider Proxy on a different port)
docker compose up -d
DGP_LISTEN_ADDR=127.0.0.1:9002 \
DGP_S3_ENDPOINT=http://localhost:9000 \
DGP_BE_AWS_ACCESS_KEY_ID=minioadmin \
DGP_BE_AWS_SECRET_ACCESS_KEY=minioadmin \
./target/release/deltaglider_proxy
```

An embedded demo UI automatically starts on **S3 port + 1** (e.g. `http://localhost:9001` or `http://localhost:9003`).

Point your S3 client at DeltaGlider Proxy (default `http://localhost:9000`, or `http://localhost:9002` in the MinIO example above). Create a bucket first, then upload:

```bash
# aws-cli
DGP_ENDPOINT=http://localhost:9000
aws --endpoint-url "$DGP_ENDPOINT" s3 mb s3://mybucket
aws --endpoint-url "$DGP_ENDPOINT" s3 cp file.zip s3://mybucket/file.zip

# boto3
import os, boto3
s3 = boto3.client('s3', endpoint_url=os.environ.get('DGP_ENDPOINT', 'http://localhost:9000'))
s3.create_bucket(Bucket='mybucket')
s3.upload_file('file.zip', 'mybucket', 'file.zip')
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
3. Otherwise, the object is stored as passthrough (original file unchanged)
4. GET/HEAD operate on the original user key; reconstruction is transparent

**What gets delta-compressed?**

By default: `.zip`, `.tar`, `.tgz`, `.tar.gz`, `.tar.bz2`, `.tar.xz`, `.jar`, `.war`, `.ear`, `.rar`, `.7z`, `.dmg`, `.iso`, `.sql`, `.dump`, `.bak`, `.backup`

Files like `.jpg`, `.png`, `.mp4` are stored as passthrough (already compressed, don't delta well).

More details:
- `docs/OPERATIONS.md`
- `docs/STORAGE_FORMAT.md`

## Configuration

### Authentication

DeltaGlider Proxy supports optional SigV4 authentication via both the standard `Authorization` header and presigned URLs (query string auth). When configured, all requests must be signed with the proxy's credentials:

```bash
export DGP_ACCESS_KEY_ID=myaccesskey
export DGP_SECRET_ACCESS_KEY=mysecretkey
```

Standard S3 tools (aws-cli, boto3, Terraform) and presigned URLs (`aws s3 presign`) work out of the box — just configure them with the proxy's credentials. The proxy verifies client signatures, then re-signs upstream requests with separate backend credentials. See [docs/AUTHENTICATION.md](docs/AUTHENTICATION.md) for details including the presigned URL flow diagram.

### Environment Variables

```bash
# Server
DGP_LISTEN_ADDR=0.0.0.0:9000

# Authentication (optional — both must be set to enable)
DGP_ACCESS_KEY_ID=...
DGP_SECRET_ACCESS_KEY=...

# Logging
DGP_LOG_LEVEL=deltaglider_proxy=info,tower_http=info  # Overridden by RUST_LOG; changeable at runtime via admin GUI

# DeltaGlider
DGP_MAX_DELTA_RATIO=0.5      # Store as delta if ratio < 50%
DGP_MAX_OBJECT_SIZE=104857600 # 100MB max (xdelta3 constraint)
DGP_CACHE_MB=100              # Reference cache for fast reconstruction

# Filesystem backend
DGP_DATA_DIR=./data

# S3 backend
DGP_S3_ENDPOINT=http://localhost:9000
DGP_S3_REGION=us-east-1
DGP_S3_PATH_STYLE=true
DGP_BE_AWS_ACCESS_KEY_ID=...
DGP_BE_AWS_SECRET_ACCESS_KEY=...
```

### Config File

```bash
./deltaglider_proxy --config deltaglider_proxy.toml
```

See [deltaglider_proxy.toml.example](deltaglider_proxy.toml.example) for all options.

### CLI

```
deltaglider_proxy 0.1.3
S3-compatible proxy with delta compression

USAGE:
    deltaglider_proxy [OPTIONS]

OPTIONS:
    -c, --config <FILE>     Path to configuration file
    -l, --listen <ADDR>     Listen address (overrides config)
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

The UI auto-detects the S3 endpoint (port - 1) from its own URL, so it works out of the box with zero configuration. Features include dark/light theme switching (persisted to localStorage), multi-environment management for connecting to different S3 backends, an admin settings panel, and drag-and-drop uploads with delta compression stats.

## Contributing

See [CONTRIBUTING.md](docs/CONTRIBUTING.md) for build instructions, project structure, and how to submit changes.

## S3 API Compatibility

DeltaGlider Proxy intercepts the S3 operations it needs for delta compression (PUT, GET, HEAD, DELETE, LIST, COPY, multipart upload) and handles them directly. Other S3 operations are not currently proxied through to the backend.

| Operation | Status | Notes |
|-----------|--------|-------|
| PutObject | ✅ | Delta compression applied when eligible |
| GetObject | ✅ | Transparent reconstruction from deltas |
| HeadObject | ✅ | Returns original object metadata |
| DeleteObject | ✅ | Cleans up deltas and references |
| ListObjectsV2 | ✅ | Returns logical keys (hides delta internals) |
| DeleteObjects | ✅ | Batch delete |
| CopyObject | ✅ | Via x-amz-copy-source header |
| CreateBucket | ✅ | Multi-bucket support |
| HeadBucket | ✅ | Multi-bucket support |
| DeleteBucket | ✅ | Must be empty |
| ListBuckets | ✅ | Lists all buckets |
| CreateMultipartUpload | ✅ | `POST /{bucket}/{key}?uploads` |
| UploadPart | ✅ | `PUT /{bucket}/{key}?partNumber=N&uploadId=X` |
| CompleteMultipartUpload | ✅ | `POST /{bucket}/{key}?uploadId=X` |
| AbortMultipartUpload | ✅ | `DELETE /{bucket}/{key}?uploadId=X` |
| ListParts | ✅ | `GET /{bucket}/{key}?uploadId=X` |
| ListMultipartUploads | ✅ | `GET /{bucket}?uploads` |
| Versioning | ❌ | Not supported |
| ACLs, lifecycle | ❌ | Not supported |

## Performance

PUT responses include `x-amz-storage-type`:

```bash
curl -i -X PUT --data-binary @releases/v2.zip http://localhost:9000/default/releases/v2.zip
# x-amz-storage-type: delta  # or "passthrough"
```

Typical savings for versioned artifacts:

| Use Case | Typical Savings |
|----------|-----------------|
| Docker layers | 60-80% |
| ML model checkpoints | 70-90% |
| Game builds | 50-70% |
| Firmware images | 80-95% |

## Architecture Notes

- **No database**: State is derived from storage. Each file carries checksums and storage info in metadata (xattr on filesystem, S3 user metadata headers on S3).
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

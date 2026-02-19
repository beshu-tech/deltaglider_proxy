# Operations

DeltaGlider Proxy is a single-process S3-compatible HTTP server. Clients speak “normal S3” (mostly), while DeltaGlider Proxy stores data as full objects or delta patches in a backend (filesystem or S3).

## Running

### Filesystem backend (local dev)

```bash
cargo run --release
# or
DGP_DATA_DIR=./data cargo run --release
```

By default DeltaGlider Proxy listens on `127.0.0.1:9000`. Create buckets via the S3 API (`CreateBucket`) or the demo UI.

### S3 backend (MinIO example)

Run MinIO on `:9000`, run DeltaGlider Proxy on a different port (example `:9002`):

```bash
docker compose up -d

DGP_LISTEN_ADDR=127.0.0.1:9002 \
DGP_S3_ENDPOINT=http://127.0.0.1:9000 \
DGP_BE_AWS_ACCESS_KEY_ID=minioadmin \
DGP_BE_AWS_SECRET_ACCESS_KEY=minioadmin \
cargo run --release
```

Point S3 clients at DeltaGlider Proxy (`:9002` in the example), not at MinIO.

## Configuration

DeltaGlider Proxy loads configuration in this order:

1. `DGP_CONFIG` (explicit TOML path)
2. `./deltaglider_proxy.toml`
3. `/etc/deltaglider_proxy/config.toml`
4. Environment variables (see `deltaglider_proxy.toml.example`)

CLI flags override anything loaded from the file/env:

```bash
./target/release/deltaglider_proxy --config deltaglider_proxy.toml --listen 0.0.0.0:9000
```

## Demo UI

An embedded React-based S3 browser starts automatically on **S3 port + 1**. For example, if DeltaGlider Proxy listens on `:9002`, the demo UI is available at `http://localhost:9003`.

The UI auto-detects the S3 endpoint from its own URL (port - 1), so no manual configuration is needed. It supports browsing objects, uploading files, viewing delta compression stats, navigating folders, and dark/light theme switching (persisted to localStorage). An admin panel is available for tuning proxy settings and managing authentication.

To build for local development:

```bash
cd demo/s3-browser/ui && npm install && npm run build && cd -
cargo build
```

The Docker build handles the Node.js UI build automatically via a multi-stage Dockerfile.

## Health & Observability

- `GET /health` returns JSON with `status` and `version`.
- Logging uses `tracing`. The log level is resolved in this priority order:
  1. `RUST_LOG` env var (standard tracing-subscriber)
  2. `DGP_LOG_LEVEL` env var (e.g. `DGP_LOG_LEVEL=deltaglider_proxy=warn,tower_http=warn`)
  3. `--verbose` CLI flag (sets trace level)
  4. Default: `deltaglider_proxy=debug,tower_http=debug`

```bash
# Using RUST_LOG
RUST_LOG=deltaglider_proxy=debug,tower_http=info cargo run --release

# Using DGP_LOG_LEVEL
DGP_LOG_LEVEL=deltaglider_proxy=warn cargo run --release
```

- **Runtime log level changes**: The log level can be changed at runtime through the admin GUI (Settings page) without restarting the server. Changes take effect immediately for all new log messages.

## Security model (read this twice)

- **Optional SigV4 authentication**: When `DGP_ACCESS_KEY_ID` and `DGP_SECRET_ACCESS_KEY` are both set, all requests must be signed with valid AWS Signature V4 credentials — either via the `Authorization` header or via presigned URL query parameters. Standard S3 tools (aws-cli, boto3, Terraform) and presigned URLs (`aws s3 presign`) work out of the box. The proxy verifies client signatures, then re-signs upstream requests with separate backend credentials via the AWS SDK. See [AUTHENTICATION.md](AUTHENTICATION.md) for details and the presigned URL flow diagram.
- **Without authentication**: If credentials are not configured, DeltaGlider Proxy accepts all requests. Treat it like an internal service and put it behind network policy / a trusted reverse proxy.
- **Admin GUI**: The embedded demo UI includes an admin interface protected by a separate password hash (`DGP_ADMIN_PASSWORD_HASH`). Admin sessions use in-memory tokens with 24-hour TTL, independent of S3 SigV4 auth.
- Keys are validated to reject `..` path segments and backslashes, but you should still avoid exposing the proxy directly to untrusted clients.

## Performance knobs

- `DGP_MAX_OBJECT_SIZE`: hard cutoff for delta processing (and currently for uploads in general).
- `DGP_MAX_DELTA_RATIO`: if `delta_size/original_size` is >= this value, DeltaGlider Proxy stores the object as passthrough (unchanged, with original filename).
- `DGP_CACHE_MB`: LRU cache for reference baselines to avoid re-fetching on hot reads.


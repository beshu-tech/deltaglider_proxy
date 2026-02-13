# Operations

DeltaGlider Proxy is a single-process S3-compatible HTTP server. Clients speak “normal S3” (mostly), while DeltaGlider Proxy stores data as full objects or delta patches in a backend (filesystem or S3).

## Running

### Filesystem backend (local dev)

```bash
cargo run --release
# or
DELTAGLIDER_PROXY_DATA_DIR=./data cargo run --release
```

By default DeltaGlider Proxy listens on `127.0.0.1:9000` and exposes a single bucket named `default`.

### S3 backend (MinIO example)

Run MinIO on `:9000`, run DeltaGlider Proxy on a different port (example `:9002`):

```bash
docker compose up -d

DELTAGLIDER_PROXY_LISTEN_ADDR=127.0.0.1:9002 \
DELTAGLIDER_PROXY_S3_ENDPOINT=http://127.0.0.1:9000 \
AWS_ACCESS_KEY_ID=minioadmin \
AWS_SECRET_ACCESS_KEY=minioadmin \
cargo run --release
```

Point S3 clients at DeltaGlider Proxy (`:9002` in the example), not at MinIO.

## Configuration

DeltaGlider Proxy loads configuration in this order:

1. `DELTAGLIDER_PROXY_CONFIG` (explicit TOML path)
2. `./deltaglider_proxy.toml`
3. `/etc/deltaglider_proxy/config.toml`
4. Environment variables (see `deltaglider_proxy.toml.example`)

CLI flags override anything loaded from the file/env:

```bash
./target/release/deltaglider_proxy --config deltaglider_proxy.toml --listen 0.0.0.0:9000 --bucket default
```

## Demo UI

An embedded React-based S3 browser starts automatically on **S3 port + 1**. For example, if DeltaGlider Proxy listens on `:9002`, the demo UI is available at `http://localhost:9003`.

The UI auto-detects the S3 endpoint from its own URL (port - 1), so no manual configuration is needed. It supports browsing objects, uploading files, viewing delta compression stats, and navigating folders.

To build for local development:

```bash
cd demo/s3-browser/ui && npm install && npm run build && cd -
cargo build
```

The Docker build handles the Node.js UI build automatically via a multi-stage Dockerfile.

## Health & Observability

- `GET /health` returns JSON with `status` and `version`.
- Logging uses `tracing` and respects `RUST_LOG`. Use `--verbose` for a noisy default filter, or set your own:

```bash
RUST_LOG=deltaglider_proxy=debug,tower_http=info cargo run --release
```

## Security model (read this twice)

- **Optional SigV4 authentication**: When `DELTAGLIDER_PROXY_ACCESS_KEY_ID` and `DELTAGLIDER_PROXY_SECRET_ACCESS_KEY` are both set, all requests must be signed with valid AWS Signature V4 credentials. Standard S3 tools (aws-cli, boto3, Terraform) work out of the box. See [AUTHENTICATION.md](AUTHENTICATION.md) for details.
- **Without authentication**: If credentials are not configured, DeltaGlider Proxy accepts all requests. Treat it like an internal service and put it behind network policy / a trusted reverse proxy.
- Keys are validated to reject `..` path segments and backslashes, but you should still avoid exposing the proxy directly to untrusted clients.

## Performance knobs

- `DELTAGLIDER_PROXY_MAX_OBJECT_SIZE`: hard cutoff for delta processing (and currently for uploads in general).
- `DELTAGLIDER_PROXY_MAX_DELTA_RATIO`: if `delta_size/original_size` is >= this value, DeltaGlider Proxy stores the object “direct”.
- `DELTAGLIDER_PROXY_CACHE_SIZE_MB`: LRU cache for reference baselines to avoid re-fetching on hot reads.


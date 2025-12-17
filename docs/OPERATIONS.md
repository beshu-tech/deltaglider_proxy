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
DELTAGLIDER_PROXY_S3_BUCKET=deltaglider_proxy-data \
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

## Health & Observability

- `GET /health` returns JSON with `status` and `version`.
- Logging uses `tracing` and respects `RUST_LOG`. Use `--verbose` for a noisy default filter, or set your own:

```bash
RUST_LOG=deltaglider_proxy=debug,tower_http=info cargo run --release
```

## Security model (read this twice)

- No request authentication (no SigV4). Treat DeltaGlider Proxy like an internal service and put it behind network policy / a trusted reverse proxy.
- Keys are validated to reject `..` path segments and backslashes, but you should still avoid exposing the proxy directly to untrusted clients.

## Performance knobs

- `DELTAGLIDER_PROXY_MAX_OBJECT_SIZE`: hard cutoff for delta processing (and currently for uploads in general).
- `DELTAGLIDER_PROXY_MAX_DELTA_RATIO`: if `delta_size/original_size` is >= this value, DeltaGlider Proxy stores the object “direct”.
- `DELTAGLIDER_PROXY_CACHE_SIZE_MB`: LRU cache for reference baselines to avoid re-fetching on hot reads.


#!/usr/bin/env bash
# Start a minimal DeltaGlider instance (filesystem backend, open auth) and run
# Playwright smoke tests against the embedded `/_/` UI + `/_/health`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${DELTAGLIDER_PROXY_BIN:-$ROOT/target/release/deltaglider_proxy}"
# Avoid fixed ports: a stale local proxy or parallel run must not mask a failed start.
if [[ -n "${E2E_PORT:-}" ]]; then
  PORT="$E2E_PORT"
else
  PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
fi

if [[ ! -f "$BIN" ]]; then
  echo "error: binary not found: $BIN (set DELTAGLIDER_PROXY_BIN or build --release)" >&2
  exit 1
fi

DIR="$(mktemp -d)"
DATA="$DIR/data"
mkdir -p "$DATA"
CONFIG="$DIR/e2e.yaml"
cat > "$CONFIG" <<EOF
listen_addr: "127.0.0.1:$PORT"
authentication: "none"
backend:
  type: filesystem
  path: "$DATA"
EOF

cleanup() {
  if [[ -n "${PID:-}" ]]; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rm -rf "$DIR"
}
trap cleanup EXIT

# Deterministic admin password for Playwright (same as tests/common/mod.rs).
export DGP_BOOTSTRAP_PASSWORD_HASH='$2b$04$s7/yy6Z363jZoQodArpuDeP00U.zE1QPi0bxM/o9BOZDs6tDbss5q'

cd "$ROOT"
DGP_CONFIG="$CONFIG" "$BIN" &
PID=$!

ready=0
for _ in $(seq 1 100); do
  if ! kill -0 "$PID" 2>/dev/null; then
    wait "$PID" || true
    echo "error: proxy process exited before healthy (port $PORT)" >&2
    exit 1
  fi
  if curl -sf "http://127.0.0.1:${PORT}/_/health" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.2
done
if [[ "$ready" -ne 1 ]]; then
  echo "error: proxy did not become healthy on port $PORT" >&2
  kill "$PID" 2>/dev/null || true
  exit 1
fi

export PLAYWRIGHT_BASE_URL="http://127.0.0.1:${PORT}"
cd "$ROOT/demo/s3-browser/ui"
# Not `exec`: that would drop the EXIT trap and leave the proxy running.
npx playwright test e2e/

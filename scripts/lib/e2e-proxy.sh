# shellcheck shell=bash
# Shared by scripts/e2e-smoke.sh and scripts/qa-e2e.sh: start one
# DeltaGlider proxy (filesystem backend, free port) for a Playwright run.
#
#   source scripts/lib/e2e-proxy.sh
#   e2e_start_proxy open|bootstrap   # exports PLAYWRIGHT_BASE_URL, sets an EXIT trap
#
# bootstrap mode: SigV4 bootstrap creds in `access:` (E2E_ACCESS_KEY /
# E2E_SECRET_KEY) and the `qa-public` bucket with `public_prefixes: ["pub/"]`,
# the shape `demo/s3-browser/ui/e2e/qa-regression.spec.ts` expects.
# Both modes use the admin password `testpass` (tests/common/mod.rs).

E2E_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
E2E_BIN="${DELTAGLIDER_PROXY_BIN:-$E2E_ROOT/target/release/deltaglider_proxy}"
export E2E_ACCESS_KEY="${E2E_ACCESS_KEY:-qa-admin-key}"
export E2E_SECRET_KEY="${E2E_SECRET_KEY:-qa-admin-secret-0123456789}"

e2e_cleanup() {
  if [[ -n "${E2E_PID:-}" ]]; then
    kill "$E2E_PID" 2>/dev/null || true
    wait "$E2E_PID" 2>/dev/null || true
  fi
  if [[ -n "${E2E_DIR:-}" ]]; then
    # Keep the proxy log for a failed run (CI uploads it).
    if [[ -n "${E2E_LOG_OUT:-}" && -f "$E2E_DIR/proxy.log" ]]; then
      cp "$E2E_DIR/proxy.log" "$E2E_LOG_OUT" || true
    fi
    rm -rf "$E2E_DIR"
  fi
}

e2e_start_proxy() {
  local mode="$1"
  if [[ ! -f "$E2E_BIN" ]]; then
    echo "error: binary not found: $E2E_BIN (set DELTAGLIDER_PROXY_BIN or build --release)" >&2
    exit 1
  fi
  # Avoid fixed ports: a stale local proxy or parallel run must not mask a failed start.
  local port="${E2E_PORT:-}"
  if [[ -z "$port" ]]; then
    port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
  fi

  E2E_DIR="$(mktemp -d)"
  mkdir -p "$E2E_DIR/data"
  local config="$E2E_DIR/e2e.yaml"
  case "$mode" in
    open)
      cat > "$config" <<YAML
listen_addr: "127.0.0.1:$port"
authentication: "none"
backend:
  type: filesystem
  path: "$E2E_DIR/data"
YAML
      ;;
    bootstrap)
      cat > "$config" <<YAML
access:
  access_key_id: "$E2E_ACCESS_KEY"
  secret_access_key: "$E2E_SECRET_KEY"
storage:
  filesystem: "$E2E_DIR/data"
  buckets:
    qa-public:
      public_prefixes: ["pub/"]
advanced:
  listen_addr: "127.0.0.1:$port"
YAML
      ;;
    *)
      echo "error: unknown e2e auth mode '$mode' (open|bootstrap)" >&2
      exit 2
      ;;
  esac
  trap e2e_cleanup EXIT

  # cwd = the temp dir: the proxy writes state files next to its cwd.
  (
    cd "$E2E_DIR"
    # Deterministic admin password `testpass` (same hash as tests/common/mod.rs).
    DGP_CONFIG="$config" \
      DGP_BOOTSTRAP_PASSWORD_HASH='$2b$04$s7/yy6Z363jZoQodArpuDeP00U.zE1QPi0bxM/o9BOZDs6tDbss5q' \
      DGP_BOOT_BACKEND_PROBE=off \
      RUST_LOG="${E2E_RUST_LOG:-deltaglider_proxy=info}" \
      exec "$E2E_BIN" > "$E2E_DIR/proxy.log" 2>&1
  ) &
  E2E_PID=$!

  local ready=0
  for _ in $(seq 1 150); do
    if ! kill -0 "$E2E_PID" 2>/dev/null; then
      wait "$E2E_PID" || true
      echo "error: proxy exited before healthy (port $port); log:" >&2
      tail -40 "$E2E_DIR/proxy.log" >&2 || true
      exit 1
    fi
    if curl -sf "http://127.0.0.1:${port}/_/health" >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 0.2
  done
  if [[ "$ready" -ne 1 ]]; then
    echo "error: proxy did not become healthy on port $port; log:" >&2
    tail -40 "$E2E_DIR/proxy.log" >&2 || true
    exit 1
  fi
  export PLAYWRIGHT_BASE_URL="http://127.0.0.1:${port}"
  export E2E_AUTH="$mode"
}

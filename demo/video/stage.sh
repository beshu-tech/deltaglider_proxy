#!/usr/bin/env bash
# Stage a clean throwaway DeltaGlider Proxy instance for the demo recording.
#
# Boots a release binary in bootstrap/GUI mode on :9220 (filesystem backend,
# admin/testpassword123), seeds a `releases` bucket with three versioned
# tarballs that delta against each other, and leaves the encrypted backend /
# demo bucket / IAM user+group UNCREATED — those are the live actions the
# video performs.
#
# Idempotent: wipes prior state and restores a pristine config every run.
#
# Usage: demo/video/stage.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/release/deltaglider_proxy"
SHOT="${DGP_DEMO_DIR:-/private/tmp/dgp-demo-video}"
PORT=9220
EP="http://127.0.0.1:$PORT"

mkdir -p "$SHOT/seed" "$SHOT/data" "$SHOT/video"

# 1. Pristine config (a fresh copy every run — applies mutate it).
cat > "$SHOT/deltaglider_proxy.yaml" <<YAML
storage:
  backend: filesystem
  filesystem_path: ./data
advanced:
  listen_addr: 127.0.0.1:$PORT
YAML

# 2. Bootstrap hash sidecar for a password we know (testpassword123).
htpasswd -nbB admin testpassword123 2>/dev/null | cut -d: -f2 > "$SHOT/.deltaglider_bootstrap_hash"

# 3. Fabricate three ~6MB versioned tarballs that delta well (base + ~2% drift),
#    once — reused across runs.
if [ ! -f "$SHOT/seed/fw-1.3.0.tar" ]; then
  python3 - "$SHOT/seed" <<'PY'
import os, sys, hashlib, tarfile
d = sys.argv[1]
base = bytearray()
i = 0
while len(base) < 6 * 1024 * 1024:
    base += hashlib.sha256(b"deltaglider-firmware-block-" + str(i).encode()).digest()
    i += 1
def write(name, buf):
    p = os.path.join(d, name.replace('.tar', '.bin'))
    open(p, 'wb').write(buf)
    with tarfile.open(os.path.join(d, name), 'w') as t:
        t.add(p, arcname=os.path.basename(p))
write('fw-1.1.0.tar', base)
v2 = bytearray(base); mid = len(v2)//2
for j in range(mid, mid + len(v2)//50): v2[j] = (v2[j] + 7) & 0xff
write('fw-1.2.0.tar', v2)
v3 = bytearray(v2)
for j in range(1000, 1000 + len(v3)//80): v3[j] = (v3[j] ^ 0x5a) & 0xff
write('fw-1.3.0.tar', v3)
print("seed tarballs ready")
PY
fi

# 4. Reset live state (data + IAM DB + scan caches), restart the binary.
lsof -ti ":$PORT" | xargs kill 2>/dev/null || true
sleep 1
rm -rf "$SHOT/data" "$SHOT"/deltaglider_config.db "$SHOT"/.deltaglider_scans
mkdir -p "$SHOT/data"
( cd "$SHOT" && DGP_CONFIG="$SHOT/deltaglider_proxy.yaml" \
    DGP_ACCESS_KEY_ID=admin DGP_SECRET_ACCESS_KEY=testpassword123 \
    nohup "$BIN" --listen "127.0.0.1:$PORT" > "$SHOT/proxy.log" 2>&1 & )
for _ in $(seq 1 20); do curl -fsS "$EP/_/health" >/dev/null 2>&1 && break; sleep 0.5; done
curl -fsS "$EP/_/health" >/dev/null || { echo "proxy failed to boot"; tail -8 "$SHOT/proxy.log"; exit 1; }

# 5. Seed releases bucket with the versioned tarballs (real deltas).
export AWS_ACCESS_KEY_ID=admin AWS_SECRET_ACCESS_KEY=testpassword123 AWS_DEFAULT_REGION=us-east-1
aws --endpoint-url "$EP" s3 mb s3://releases >/dev/null 2>&1 || true
for v in 1 2 3; do
  aws --endpoint-url "$EP" s3 cp "$SHOT/seed/fw-1.$v.0.tar" \
    "s3://releases/firmware/widget-3000/fw-1.$v.0.tar" >/dev/null 2>&1
done
echo "staged on $EP — releases/firmware/widget-3000/ seeded:"
aws --endpoint-url "$EP" s3 ls s3://releases/firmware/widget-3000/

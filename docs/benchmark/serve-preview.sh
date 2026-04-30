#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
PORT="${PORT:-8765}"
cd "$ROOT"

if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "Port $PORT is already in use (often a zombie http.server)." >&2
  echo "Free it:  lsof -nP -iTCP:$PORT -sTCP:LISTEN" >&2
  echo "Then:     kill -9 \$(lsof -ti :$PORT)" >&2
  exit 1
fi

echo "Serving $ROOT"
echo "  http://127.0.0.1:${PORT}/visual-three.html"
echo "  http://127.0.0.1:${PORT}/visual-preview.html"
echo ""
exec python3 -m http.server "$PORT" --bind 127.0.0.1

#!/usr/bin/env bash
# One-shot: stage → record → compose the demo video.
# Output: deltaglider-demo-90s.mp4 at the repo root.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DIR/../.." && pwd)"

"$DIR/stage.sh"
( cd "$ROOT/demo/s3-browser/ui" && node ../../video/demo.mjs )
"$DIR/compose.sh"

# Tear down the throwaway instance.
lsof -ti :9220 | xargs kill 2>/dev/null || true
echo "demo video ready: $ROOT/deltaglider-demo-90s.mp4"

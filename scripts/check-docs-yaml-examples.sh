#!/usr/bin/env bash
# Validate every `# validate`-tagged YAML block in product docs.
#
# Shape we expect:
#
#     ```yaml
#     # validate
#     storage:
#       backend:
#         type: s3
#     ```
#
# Every such block is extracted to a temp file and fed to
# `deltaglider_proxy config lint`. If any block fails, CI fails —
# prevents drift between documentation examples and the real schema.
#
# Requires `deltaglider_proxy` on PATH (built from `cargo build`).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PRODUCT_DIR="$ROOT/docs/product"
BIN="${DELTAGLIDER_PROXY_BIN:-deltaglider_proxy}"

if ! command -v "$BIN" >/dev/null 2>&1; then
  echo "ERROR: '$BIN' not on PATH." >&2
  echo "  Set DELTAGLIDER_PROXY_BIN=./target/release/deltaglider_proxy (or similar)." >&2
  exit 2
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

fail=0
total=0

# Walk every .md under docs/product/. Python extracts fenced `yaml`
# blocks whose first line after the fence is `# validate`; each block
# is written as a separate file in $tmpdir.
#
# The choice of Python (vs bash/awk) is deliberate: markdown fenced
# blocks can nest or have leading whitespace, and a simple sed script
# misidentifies blocks inside lists. Python's regex is readable +
# correct; we already require python3 for other CI jobs.
while IFS= read -r -d '' md; do
  rel="${md#"$ROOT"/}"
  python3 - "$md" "$tmpdir" "$rel" <<'PY'
import re, sys, os

src_path, tmp_dir, rel = sys.argv[1], sys.argv[2], sys.argv[3]
with open(src_path) as f:
    text = f.read()

pattern = re.compile(
    r'```yaml\s*\n(?P<body># validate\s*\n[\s\S]*?)```',
    re.MULTILINE,
)

basename = rel.replace('/', '__').replace('.md', '')
for idx, m in enumerate(pattern.finditer(text)):
    body = m.group('body')
    # Drop the `# validate` marker line.
    body = re.sub(r'^# validate\s*\n', '', body, count=1)
    out = os.path.join(tmp_dir, f"{basename}__{idx}.yaml")
    with open(out, 'w') as f:
        f.write(body)
    # Print rel|path so the shell loop can invoke config lint.
    print(f"{rel}|{out}")
PY
done < <(find "$PRODUCT_DIR" -type f -name '*.md' -print0) > "$tmpdir/index.txt"

while IFS='|' read -r rel yaml; do
  total=$((total + 1))
  if out=$("$BIN" config lint "$yaml" 2>&1); then
    :  # valid, no warnings → 0
  else
    echo "INVALID: $rel (block extracted to $yaml)" >&2
    echo "$out" | sed 's/^/    /' >&2
    fail=1
  fi
done < "$tmpdir/index.txt"

if [ "$fail" -eq 0 ]; then
  echo "docs YAML examples OK: $total blocks validated"
fi

exit "$fail"

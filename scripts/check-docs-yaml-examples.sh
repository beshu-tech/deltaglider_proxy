#!/usr/bin/env bash
# Check every ```yaml block in docs/product/ (docs/dev/writing-task-pages.md).
#
# The first line of each block is a marker:
#
#     # validate                   a complete proxy config: it must pass `config lint`
#     # fragment                   part of a proxy config, shown for reading
#     # not-proxy-config: <kind>   Helm values, Kubernetes, Compose, Prometheus, ...
#
# A block without a marker fails the check. For a `# validate` block, every
# `${env:NAME}` reference without a default gets a placeholder value in the
# environment of `config lint`, because the lint expands references and fails
# on an unset one. changelog.md is skipped: gen-changelog-doc.sh generates it
# from CHANGELOG.md, whose old entries predate the markers.
#
# Requires the proxy binary: DELTAGLIDER_PROXY_BIN, or `deltaglider_proxy` on PATH.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${DELTAGLIDER_PROXY_BIN:-deltaglider_proxy}"

if ! command -v "$BIN" >/dev/null 2>&1; then
  echo "ERROR: '$BIN' not on PATH." >&2
  echo "  Set DELTAGLIDER_PROXY_BIN=./target/release/deltaglider_proxy (or similar)." >&2
  exit 2
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

python3 - "$ROOT/docs/product" "$tmpdir" "$(command -v "$BIN")" <<'PY'
import os, re, subprocess, sys, textwrap
from pathlib import Path

product, tmp, binary = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
FENCE = re.compile(r'^([ \t]*)```ya?ml[^\n]*\n(.*?)^\1```', re.M | re.S)
REF = re.compile(r'\$\{env:([A-Za-z_][A-Za-z0-9_]*)\}')  # no `:-default`
AES_HEX = '0123456789abcdef' * 4

def placeholder(name: str) -> str:
    # An AES key must be 64 hex characters; any other string field takes text.
    return AES_HEX if 'ENCRYPTION_KEY' in name or name.endswith('_AES_KEY') else f'docs-placeholder-{name.lower()}'

errors, validated = [], 0
for md in sorted(product.rglob('*.md')):
    rel = md.relative_to(product.parent.parent)
    if md.name == 'changelog.md' and md.parent == product:
        continue
    text = md.read_text()
    for n, m in enumerate(FENCE.finditer(text)):
        line = text.count('\n', 0, m.start()) + 1
        body = textwrap.dedent(m.group(2))
        first = next((l.strip() for l in body.splitlines() if l.strip()), '')
        if first == '# validate':
            pass
        elif first == '# fragment' or re.fullmatch(r'# not-proxy-config: \S.*', first):
            continue
        else:
            errors.append(f'{rel}:{line}: yaml block without a marker (# validate, # fragment, # not-proxy-config: <kind>)')
            continue
        validated += 1
        body = re.sub(r'^[ \t]*# validate[ \t]*\n', '', body, count=1)
        f = tmp / f'{rel.as_posix().replace("/", "__")}__{n}.yaml'
        f.write_text(body)
        env = {'PATH': os.environ['PATH'], 'HOME': os.environ.get('HOME', '/tmp')}
        env.update({name: placeholder(name) for name in REF.findall(body)})
        r = subprocess.run([binary, 'config', 'lint', str(f)], env=env, capture_output=True, text=True)
        if r.returncode != 0:
            out = (r.stdout + r.stderr).strip().replace('\n', '\n    ')
            errors.append(f'{rel}:{line}: `config lint` failed\n    {out}')

for e in errors:
    print(e, file=sys.stderr)
if errors:
    sys.exit(1)
print(f'docs YAML examples OK: {validated} blocks validated, every block marked')
PY

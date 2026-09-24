#!/usr/bin/env bash
# Integration tests build into ONE binary (tests/all.rs). Every tests/<name>.rs
# (except all.rs and common/) must be:
#   1. declared as a module in tests/all.rs, and
#   2. selected in .github/workflows/ci.yml (the PR merge gate) by the libtest
#      filter `<name>::` on `cargo test --test all`,
# and every such filter in ci.yml must name an existing tests/<name>.rs.
# libtest filters are substring matches, so no module name may make `<a>::`
# a substring of `<b>::` (that would silently run extra tests in a group).
# Nightly `cargo test --all` is supplementary.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

shopt -s nullglob
names=()
for f in tests/*.rs; do
  name=$(basename "$f" .rs)
  [[ "$name" == "all" ]] && continue
  names+=("$name")
done

err=0
for name in "${names[@]}"; do
  if ! grep -qE "^mod ${name};$" tests/all.rs; then
    echo "error: tests/${name}.rs is not declared in tests/all.rs (add: #[path = \"${name}.rs\"] mod ${name};)" >&2
    err=1
  fi
  if ! grep -qE "(^|[^a-z0-9_])${name}::( |\\\\|$)" .github/workflows/ci.yml; then
    echo "error: tests/${name}.rs is not selected in .github/workflows/ci.yml (add filter: ${name}::)" >&2
    err=1
  fi
  for other in "${names[@]}"; do
    if [[ "$other" != "$name" && "${other}::" == *"${name}::" ]]; then
      echo "error: filter '${name}::' also matches module '${other}' — rename one of them" >&2
      err=1
    fi
  done
done
# Reverse direction: a filter naming no test module would silently run zero
# tests (the old `--test <name>` form failed loudly on a missing target).
while read -r filter; do
  if [[ ! -f "tests/${filter}.rs" ]]; then
    echo "error: ci.yml filter '${filter}::' matches no tests/${filter}.rs (it would run zero tests)" >&2
    err=1
  fi
done < <(awk '/cargo test .*--test all/ {c=1} c {print; if ($0 !~ /\\$/) c=0}' .github/workflows/ci.yml \
          | grep -oE '[a-z0-9_]+::' | sed 's/::$//' | sort -u)

((err == 0)) || exit 1

echo "OK: all ${#names[@]} tests/*.rs modules are in tests/all.rs and selected in CI."

#!/usr/bin/env bash
# Integration tests build into ONE binary (tests/all.rs). Every tests/<name>.rs
# (except all.rs and common/) must be:
#   1. declared as a module in tests/all.rs, and
#   2. selected in .github/workflows/ci.yml (the PR merge gate) by the libtest
#      filter `<name>::` on `cargo test --test all`.
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
((err == 0)) || exit 1

echo "OK: all ${#names[@]} tests/*.rs modules are in tests/all.rs and selected in CI."

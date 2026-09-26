#!/usr/bin/env bash
# Start a minimal DeltaGlider instance (filesystem backend) and run the
# Playwright `e2e/` suite (smoke + full flow) against the embedded `/_/` UI.
#
#   E2E_AUTH=open      (default) authentication: none
#   E2E_AUTH=bootstrap bootstrap SigV4 creds + admin password; the full-flow
#                      spec for that mode logs in instead of auto-connecting.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=lib/e2e-proxy.sh
source "$ROOT/scripts/lib/e2e-proxy.sh"
e2e_start_proxy "${E2E_AUTH:-open}"

cd "$ROOT/demo/s3-browser/ui"
# Not `exec`: that would drop the EXIT trap and leave the proxy running.
npx playwright test e2e/

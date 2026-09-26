#!/usr/bin/env bash
# Run the QA regression pass (demo/s3-browser/ui/e2e/qa-regression.spec.ts)
# against a release binary in bootstrap-auth mode. Nightly CI runs this; it
# is not in the PR gate (it takes minutes and drives the whole admin UI).
#
#   DELTAGLIDER_PROXY_BIN=target/release/deltaglider_proxy ./scripts/qa-e2e.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=lib/e2e-proxy.sh
source "$ROOT/scripts/lib/e2e-proxy.sh"
e2e_start_proxy bootstrap

export QA_ACCESS_KEY="$E2E_ACCESS_KEY" QA_SECRET_KEY="$E2E_SECRET_KEY" QA_PUBLIC_BUCKET=qa-public
cd "$ROOT/demo/s3-browser/ui"
npm run e2e:qa

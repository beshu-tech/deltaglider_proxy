#!/usr/bin/env bash
# =============================================================================
# check-no-committed-secrets.sh
# -----------------------------------------------------------------------------
# CI backstop for the IaC deployment artifacts. The iac-prod/ workflow ships a
# secret-free YAML + a gitignored secrets.env; this guards against the gitignore
# being bypassed (e.g. `git add -f`) and against an obvious live secret landing
# anywhere in the tracked tree. Pairs with iac-prod/.gitignore — defence in depth.
#
# Scans the COMMITTED (git-tracked) files only. Exits non-zero on any hit.
# =============================================================================
set -euo pipefail

fail=0
note() { printf '  ✗ %s\n' "$*" >&2; fail=1; }

# 1. Files that must never be committed (live secrets / backups / DBs / keys).
#    These are gitignored under iac-prod/; this catches a forced add anywhere.
#    A basename glob matches at any depth: ':(glob)**/x' also matches a root file.
forbidden=$(git ls-files \
  ':(glob)**/secrets.env' \
  ':(glob)**/.env' \
  ':(glob)**/.env.*' \
  ':(glob)**/*.zip' \
  ':(glob)**/deltaglider_config.db' \
  ':(glob)**/deltaglider_jobs.db' \
  ':(glob)**/*.db' \
  ':(glob)**/*.db.bak' \
  ':(glob)**/*.db.key' \
  ':(glob)**/*.sqlite' \
  ':(glob)**/.deltaglider_bootstrap_hash' \
  ':(glob)**/*.pem' \
  ':(glob)**/*.key' \
  ':(glob)**/*.p12' \
  ':(glob)**/*.pfx' \
  ':(glob)**/*.jks' \
  ':(glob)**/id_rsa' \
  ':(glob)**/id_ed25519' \
  ':(glob)**/id_ecdsa' \
  ':(glob)iac-prod/config.yaml' \
  ':(glob)iac-prod/iam.json' \
  ':(glob)iac-prod/secrets.json' \
  2>/dev/null || true)
if [[ -n "$forbidden" ]]; then
  while IFS= read -r f; do note "forbidden file committed: $f"; done <<<"$forbidden"
fi

# 2. Live-secret patterns in any tracked text file. Deliberately narrow to
#    avoid false positives on docs/examples (which use placeholders).
#    - Slack token: xox[abprs]-<digits>-...
#    - PEM private key block
#    - AWS access key id: AKIA/ASIA + 16 base32 chars (real ids never hold 0, 1,
#      8 or 9; the fake ids in docs and tests do, or spell EXAMPLE)
#    - GitHub token (ghp_/gho_/ghu_/ghs_/ghr_ + 36, or github_pat_), GitLab
#      glpat-, Stripe sk_live_, Google API key AIza..., Anthropic sk-ant-
#    Only an AWS id that itself spells EXAMPLE (AWS's own documentation key)
#    is exempt. Each match is checked alone (`-o`), so a real token on a line
#    that says EXAMPLE, or beside an EXAMPLE id, still fails.
patterns='xox[abprs]-[0-9]{6,}-|-----BEGIN [A-Z ]*PRIVATE KEY-----|(AKIA|ASIA)[A-Z2-7]{16}([^A-Z2-7]|$)|gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{22,}|glpat-[A-Za-z0-9_-]{20}|sk_live_[0-9A-Za-z]{20,}|AIza[0-9A-Za-z_-]{35}|sk-ant-[A-Za-z0-9_-]{20,}'
hits=$(git grep -nIoE "$patterns" -- \
  ':(exclude)*.lock' \
  ':(exclude)demo/s3-browser/ui/dist/**' \
  2>/dev/null | grep -vE ':(AKIA|ASIA)[A-Z2-7]{0,16}EXAMPLE' || true)
if [[ -n "$hits" ]]; then
  while IFS= read -r line; do note "possible live secret: $line"; done <<<"$hits"
fi

if [[ "$fail" -ne 0 ]]; then
  echo "ERROR: potential committed secret(s) found — see above." >&2
  exit 1
fi
echo "OK: no committed secret artifacts or obvious live-secret patterns."

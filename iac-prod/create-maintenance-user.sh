#!/usr/bin/env bash
#
# One-time: create a scoped 'maintenance' admin IAM user in prod (GUI mode) and
# take a first admin export. Run LOCALLY. Uses legacy-admin's IAM creds to get an
# admin session via login-as — secrets are referenced by variable, never printed.
#
# Prereqs: iac-prod/secrets.env present (provides DGP_USER_LEGACY_ADMIN_SECRET),
#          jq, curl. Run from the repo root:  bash iac-prod/create-maintenance-user.sh
#
set -euo pipefail

# --- config you must set ---
BASE="https://dgp.serve.beshu.tech"                 # admin host (or files.readonlyrest.com)
LEGACY_ADMIN_AKID="REPLACE_WITH_legacy-admin_ACCESS_KEY_ID"   # NOT a secret — from your config
# ---------------------------

[[ "$LEGACY_ADMIN_AKID" == REPLACE_* ]] && { echo "Set LEGACY_ADMIN_AKID first." >&2; exit 1; }
source iac-prod/secrets.env
: "${DGP_USER_LEGACY_ADMIN_SECRET:?not found in secrets.env}"

CJ="$(mktemp)"; trap 'rm -f "$CJ"' EXIT

# 1. login-as legacy-admin -> admin session cookie. Abort if it's not 200.
code=$(curl -sS -c "$CJ" -o /dev/null -w '%{http_code}' \
  -X POST "$BASE/_/api/admin/login-as" -H 'Content-Type: application/json' \
  -d "{\"access_key_id\":\"$LEGACY_ADMIN_AKID\",\"secret_access_key\":\"$DGP_USER_LEGACY_ADMIN_SECRET\"}")
echo "login-as: HTTP $code"
[[ "$code" == 200 ]] || { echo "login-as failed (is legacy-admin an admin? are creds current?)" >&2; exit 1; }

# 2. Create the 'maintenance' admin user. Returns its full secret ONCE — save it.
echo "== creating maintenance user =="
curl -sS -b "$CJ" -X POST "$BASE/_/api/admin/users" -H 'Content-Type: application/json' \
  -d '{"name":"maintenance","permissions":[{"effect":"Allow","actions":["*"],"resources":["*"]}]}' \
  | tee iac-prod/maintenance-user.json | jq '{name, access_key_id, secret_access_key, permissions}'
echo ">> Saved iac-prod/maintenance-user.json (CONTAINS THE SECRET — hand these creds to ton77v, then delete)."

# 3. First admin export WITH secrets — the seed artifact for the IaC loop.
curl -sS -b "$CJ" "$BASE/_/api/admin/config/export?include_secrets=true" \
  -o iac-prod/prod-export.yaml -w 'export: HTTP %{http_code}\n'
echo ">> Wrote iac-prod/prod-export.yaml (CONTAINS SECRETS — treat like secrets.env; both are gitignored)."

echo
echo "Done. Next: give ton77v the maintenance access_key_id + secret_access_key from"
echo "iac-prod/maintenance-user.json. Their headless job then does:"
echo "  login-as (maintenance creds) -> GET /config/export?include_secrets=true"

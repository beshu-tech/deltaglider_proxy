#!/usr/bin/env bash
# Restore production parity from a GET /_/api/admin/backup zip (manifest +
# config.yaml + iam.json + secrets.json).
#
# Prerequisites:
#   - Proxy is running with the same bundle already driving env (see run-from-backup-dir.sh):
#     DGP_CONFIG, DGP_S3_*, DGP_BE_*, access keys, DGP_BOOTSTRAP_PASSWORD_HASH from secrets.
#   - You know the bootstrap PLAINTEXT password (login verifies bcrypt; the zip only has the hash).
#   - access.iam_mode is `gui` (default). Declarative mode blocks IAM mutations including POST backup.
#
# Usage:
#   export DGP_ADMIN_PASSWORD='your-bootstrap-plaintext'
#   ./scripts/restore-full-backup-zip.sh http://127.0.0.1:9000 /path/to/dgp-backup-v*.zip
#
# Optional — wipe local SQLCipher DB before restarting the proxy (same host as DGP_CONFIG parent dir):
#   ./scripts/restore-full-backup-zip.sh --rm-db /path/to/dir/with/config.yaml/parent http://127.0.0.1:9000 backup.zip
#   Stops nothing; deletes dir/deltaglider_config.db and dir/deltaglider_config.db.bak if present.
#
set -euo pipefail

RM_DB_DIR=""
if [[ "${1:-}" == "--rm-db" ]]; then
  RM_DB_DIR="${2:?}"
  shift 2
fi

BASE="${1:?usage: $0 [--rm-db CONFIG_PARENT_DIR] <base-url> <backup.zip>}"
ZIP="${2:?}"

if [[ -n "$RM_DB_DIR" ]]; then
  for f in "$RM_DB_DIR/deltaglider_config.db" "$RM_DB_DIR/deltaglider_config.db.bak"; do
    if [[ -f "$f" ]]; then
      echo "Removing $f"
      rm -f "$f"
    fi
  done
fi

PASS="${DGP_ADMIN_PASSWORD:?set DGP_ADMIN_PASSWORD to the bootstrap plaintext password}"
[[ -f "$ZIP" ]] || { echo "error: zip not found: $ZIP" >&2; exit 1; }

COOKIE_JAR="$(mktemp)"
TMP_LOGIN="$(mktemp)"
TMP_BODY="$(mktemp)"
trap 'rm -f "$COOKIE_JAR" "$TMP_LOGIN" "$TMP_BODY"' EXIT

CODE="$(curl -sS -o "$TMP_LOGIN" -w '%{http_code}' -c "$COOKIE_JAR" \
  -X POST "${BASE%/}/_/api/admin/login" \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg p "$PASS" '{password:$p}')")"

if [[ "$CODE" != "200" ]]; then
  echo "login failed: HTTP $CODE" >&2
  cat "$TMP_LOGIN" >&2 || true
  exit 1
fi
CODE="$(curl -sS -o "$TMP_BODY" -w '%{http_code}' -b "$COOKIE_JAR" \
  -X POST "${BASE%/}/_/api/admin/backup" \
  -H 'Content-Type: application/zip' \
  --data-binary @"$ZIP")"
BODY="$(cat "$TMP_BODY")"

if [[ "$CODE" != "200" ]]; then
  echo "POST backup failed: HTTP $CODE" >&2
  echo "$BODY" >&2
  exit 1
fi

echo "$BODY" | jq .
echo "Restore OK. IAM counters above; check admin Users/Groups or GET /_/api/admin/users (session)."

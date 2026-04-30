#!/usr/bin/env bash
# Run deltaglider_proxy using an admin-export zip unpacked to a directory
# (manifest.json, config.yaml, iam.json, secrets.json).
#
# Why DGP_S3_*: `apply_env_overrides` only injects DGP_BE_AWS_* when DGP_S3_ENDPOINT
# or DGP_S3_REGION is set; otherwise YAML `access_key_id: null` stays null.
#
# Usage:
#   unzip -d ~/.local-dgp-backup ~/Downloads/dgp-backup-v*.zip
#   ./scripts/run-from-backup-dir.sh ~/.local-dgp-backup
#
# Optional second arg: path to release binary (default: ./target/release/deltaglider_proxy).
#
# Production parity (IAM + merged config + secrets): after boot, POST the original
# backup zip with an admin session — see scripts/restore-full-backup-zip.sh
# (needs bootstrap plaintext in DGP_ADMIN_PASSWORD).

set -euo pipefail

BACKUP_DIR="${1:?usage: $0 <path-to-unzipped-backup-dir> [path-to-binary]}"
BIN="${2:-./target/release/deltaglider_proxy}"

if [[ ! -f "$BACKUP_DIR/config.yaml" ]] || [[ ! -f "$BACKUP_DIR/secrets.json" ]]; then
  echo "error: expected $BACKUP_DIR/{config.yaml,secrets.json}" >&2
  exit 1
fi

export DGP_CONFIG="$BACKUP_DIR/config.yaml"

# Trigger env-based backend credential injection (see src/config.rs apply_env_overrides).
export DGP_S3_ENDPOINT
export DGP_S3_REGION
export DGP_S3_PATH_STYLE
DGP_S3_ENDPOINT="$(awk '/^[[:space:]]*endpoint:/{print $2; exit}' "$BACKUP_DIR/config.yaml")"
DGP_S3_REGION="$(awk '/^[[:space:]]*region:/{print $2; exit}' "$BACKUP_DIR/config.yaml")"
if grep -q '^[[:space:]]*force_path_style:[[:space:]]*false' "$BACKUP_DIR/config.yaml"; then
  DGP_S3_PATH_STYLE=false
else
  DGP_S3_PATH_STYLE=true
fi
[[ -n "$DGP_S3_ENDPOINT" ]]
[[ -n "$DGP_S3_REGION" ]]

eval "$(jq -r '.storage | ["export DGP_BE_AWS_ACCESS_KEY_ID=" + (.access_key_id|@sh), "export DGP_BE_AWS_SECRET_ACCESS_KEY=" + (.secret_access_key|@sh)] | .[]' "$BACKUP_DIR/secrets.json")"
eval "$(jq -r '.access | ["export DGP_ACCESS_KEY_ID=" + (.access_key_id|@sh), "export DGP_SECRET_ACCESS_KEY=" + (.secret_access_key|@sh)] | .[]' "$BACKUP_DIR/secrets.json")"

if jq -e '.bootstrap_password_hash // empty|length>0' "$BACKUP_DIR/secrets.json" >/dev/null 2>&1; then
  eval "$(jq -r '"export DGP_BOOTSTRAP_PASSWORD_HASH=" + (.bootstrap_password_hash|@sh)' "$BACKUP_DIR/secrets.json")"
fi

exec "$BIN"

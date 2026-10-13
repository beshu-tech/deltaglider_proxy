#!/usr/bin/env bash
# Prod-config regression harness: prove that WHATEVER configuration prod
# currently runs still works on the CURRENT branch.
#
# Boots the current branch's release binary in a throwaway dir against the
# real prod state and asserts, DYNAMICALLY (expectations are derived from
# the prod YAML itself, so the script keeps working as prod evolves):
#
#   1. `config lint` accepts the prod YAML.
#   2. The proxy boots and reports healthy.
#   3. Bootstrap admin login works; IAM users / groups / OIDC providers
#      visible through the admin API match the YAML (declarative mode).
#   4. Bucket routing matches the YAML (every bucket on its backend).
#   5. The unified /jobs endpoint lists the YAML replication rules.
#   6. `config export` output passes `config lint` (round-trip).
#   7. S3 with bootstrap creds: ListBuckets shows every YAML bucket, and a
#      PUT+GET sha256 round-trip succeeds on a FILESYSTEM-routed bucket.
#   8. A non-admin declarative user is denied a write outside its grants
#      (fail-closed check — denied before any backend I/O).
#
# SAFETY: only filesystem-backed buckets are ever written to (their path
# lives inside the throwaway dir). Buckets on s3-type backends (real
# remote storage) are never touched beyond appearing in list responses.
#
# State source (first match wins):
#   $1                          — a prod backup zip (restored via the API,
#                                 mode=preserve-bootstrap, like
#                                 restart-local-with-prod-backup.sh)
#   $TMP_ROOT/dgp-backup-*.zip (newest)
#   $TMP_ROOT/dgp-prod-local — clone of the already-restored local dir
# TMP_ROOT is /tmp (on macOS that is /private/tmp).
#
# Local-only (needs the prod backup / restored state + its secrets); not
# part of CI. CI covers the sanitized shape via `prod_shape_tests` in
# src/config.rs. Run this before releases, like `cargo test --all`.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/target/release/deltaglider_proxy"
PORT="${PORT:-9450}"
BASE="http://127.0.0.1:${PORT}"
PW="testpassword123"
TMP_ROOT="${TMP_ROOT:-/tmp}"
DIR="$(mktemp -d "${TMP_ROOT}/dgp-prodconf-test.XXXXXX")"
COOKIES="${DIR}/cookies.txt"

PASS=0
FAIL=0
note() { printf '    %s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '\033[32m ok \033[0m %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '\033[31mFAIL\033[0m %s\n' "$*"; }
check(){ if "$@" >/dev/null 2>&1; then ok "${ASSERT}"; else bad "${ASSERT}"; fi; }

cleanup() {
  [[ -f "${DIR}/proxy.pid" ]] && kill "$(cat "${DIR}/proxy.pid")" 2>/dev/null || true
  rm -rf "${DIR}"
}
trap cleanup EXIT

[[ -x "${BIN}" ]] || { echo "ERROR: ${BIN} missing — run 'cargo build --release' first" >&2; exit 1; }
command -v aws >/dev/null || { echo "ERROR: aws cli required" >&2; exit 1; }

# Is something listening on the port? lsof on macOS, ss on Linux.
port_in_use() {
  if command -v lsof >/dev/null; then
    lsof -ti ":$1" -sTCP:LISTEN >/dev/null 2>&1
  else
    ss -Hltn "sport = :$1" | grep -q .
  fi
}

# ── Acquire prod state ──────────────────────────────────────────────────
BACKUP="${1:-$(ls -t ${TMP_ROOT}/dgp-backup-*.zip 2>/dev/null | head -1 || true)}"
start_proxy() {
  # Fail loudly if something already holds the port — otherwise the health
  # probe below would happily talk to a STALE instance and every assertion
  # would test the wrong process (same trap tests/common/mod.rs guards).
  if port_in_use "${PORT}"; then
    echo "ERROR: port ${PORT} already in use — kill the stray instance or set PORT=" >&2
    return 1
  fi
  # `&` on the proxy command alone: `$!` must be the proxy's PID, not a
  # subshell's (killing a subshell leaves the proxy running with prod
  # credentials, and the next start finds the port taken).
  ( cd "${DIR}" || exit 1
    DGP_LISTING_FACTS_GC=false nohup "${BIN}" --listen "127.0.0.1:${PORT}" > "${DIR}/proxy.log" 2>&1 &
    echo $! > "${DIR}/proxy.pid" )
  for _ in $(seq 1 40); do
    curl -fsS -o /dev/null "${BASE}/_/health" 2>/dev/null && return 0
    sleep 0.5
  done
  echo "ERROR: proxy did not become healthy; log tail:" >&2
  tail -20 "${DIR}/proxy.log" >&2
  return 1
}

# SAFETY: the restored config holds the REAL backend credentials. Turn off
# every background job that writes on its own (replication, lifecycle, event
# delivery) BEFORE any proxy loads it: the restore applies the config to a
# running proxy at once, and prod runs as one instance with no
# cross-instance locks. The rules stay in the YAML, so step 5 still sees them.
neutralize_yaml() {  # neutralize_yaml <in.yaml> <out.yaml>
  python3 - "$1" "$2" <<'PY'
import sys, yaml
cfg = yaml.safe_load(open(sys.argv[1])) or {}
storage = cfg.get('storage') or {}
for section in ('replication', 'lifecycle'):
    if isinstance(storage.get(section), dict):
        storage[section]['enabled'] = False
advanced = cfg.get('advanced') or {}
if isinstance(advanced.get('event_delivery'), dict):
    advanced['event_delivery']['enabled'] = False
yaml.safe_dump(cfg, open(sys.argv[2], 'w'), sort_keys=False)
PY
}
neutralize_backup() {  # neutralize_backup <in.zip> <out.zip>
  local work="${DIR}/backup-src"
  mkdir -p "${work}" && unzip -q "$1" -d "${work}"
  neutralize_yaml "${work}/config.yaml" "${work}/config.yaml"
  python3 - "${work}" <<'PY'
import hashlib, json, os, sys
d = sys.argv[1]
m = json.load(open(os.path.join(d, 'manifest.json')))
for f in m.get('files', []):
    data = open(os.path.join(d, f['name']), 'rb').read()
    f['bytes'], f['sha256'] = len(data), hashlib.sha256(data).hexdigest()
json.dump(m, open(os.path.join(d, 'manifest.json'), 'w'), indent=2)
PY
  ( cd "${work}" && zip -q -X "$2" manifest.json config.yaml iam.json secrets.json )
  cp "${work}/iam.json" "${DIR}/backup-iam.json"
  rm -rf "${work}"
}

if [[ -n "${BACKUP}" && -f "${BACKUP}" ]]; then
  echo "==> Source: backup zip ${BACKUP}"
  neutralize_backup "${BACKUP}" "${DIR}/backup-safe.zip"
  BACKUP="${DIR}/backup-safe.zip"
  mkdir -p "${DIR}/data"
  cat > "${DIR}/deltaglider_proxy.yaml" <<EOF
access:
  access_key_id: admin
  secret_access_key: seed-placeholder-overwritten-by-restore
storage:
  backend:
    type: filesystem
    path: ${DIR}/data
EOF
  ( cd "${DIR}" && printf '%s\n' "${PW}" | "${BIN}" --set-bootstrap-password >/dev/null )
  start_proxy
  curl -fsS -c "${COOKIES}" -X POST "${BASE}/_/api/admin/login" \
    -H 'Content-Type: application/json' -d "{\"password\":\"${PW}\"}" -o /dev/null
  curl -fsS -b "${COOKIES}" -X POST "${BASE}/_/api/admin/backup?mode=preserve-bootstrap" \
    -H 'Content-Type: application/zip' --data-binary "@${BACKUP}" -o /dev/null
  kill "$(cat "${DIR}/proxy.pid")" 2>/dev/null || true
  for _ in $(seq 1 20); do
    port_in_use "${PORT}" || break
    sleep 0.5
  done
elif [[ -d ${TMP_ROOT}/dgp-prod-local ]]; then
  echo "==> Source: clone of ${TMP_ROOT}/dgp-prod-local (no backup zip found)"
  cp ${TMP_ROOT}/dgp-prod-local/deltaglider_proxy.yaml "${DIR}/"
  cp ${TMP_ROOT}/dgp-prod-local/deltaglider_config.db "${DIR}/" 2>/dev/null || true
  # The bootstrap hash lives in a hidden sidecar file — without it the
  # proxy generates a fresh password and quarantines the cloned DB.
  cp ${TMP_ROOT}/dgp-prod-local/.deltaglider_bootstrap_hash "${DIR}/" 2>/dev/null || true
  neutralize_yaml "${DIR}/deltaglider_proxy.yaml" "${DIR}/deltaglider_proxy.yaml"
  mkdir -p "${DIR}/data"
else
  echo "ERROR: no prod state found (no backup zip, no ${TMP_ROOT}/dgp-prod-local)" >&2
  exit 1
fi

YAML="${DIR}/deltaglider_proxy.yaml"

# Filesystem backends in the restored YAML may point at the source dir's
# relative ./data — make sure relative paths resolve inside OUR dir (they
# do: the proxy runs with CWD=${DIR}).

# ── Derive expectations from the prod YAML itself ───────────────────────
EXPECT_JSON="${DIR}/expect.json"
python3 - "$YAML" > "${EXPECT_JSON}" <<'PY'
import json, os, re, sys, yaml
raw = open(sys.argv[1]).read()
# Resolve ${env:NAME} / ${env:NAME:-default} like the proxy does at load —
# once prod's YAML moves to secret-free templates, expectations (incl. the
# non-admin denial probe's credentials) must use the resolved values.
def _resolve(m):
    name, default = m.group(1), m.group(2)
    val = os.environ.get(name, '')
    if val:
        return val
    if default is not None:
        return default
    print(f'warning: ${{env:{name}}} unset while deriving expectations', file=sys.stderr)
    return m.group(0)
raw = re.sub(r'\$\{env:([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}', _resolve, raw)
cfg = yaml.safe_load(raw)
access = cfg.get('access') or {}
storage = cfg.get('storage') or {}
backends = {b['name']: b for b in (storage.get('backends') or [])}
fs_backends = [n for n, b in backends.items() if b.get('type') == 'filesystem']
buckets = storage.get('buckets') or {}
default_backend = storage.get('default_backend')
out = {
    'users': sorted(u['name'] for u in (access.get('iam_users') or [])),
    'user_keys': {u['name']: [u['access_key_id'], u['secret_access_key']]
                  for u in (access.get('iam_users') or [])},
    'nonadmin_users': sorted(
        u['name'] for u in (access.get('iam_users') or [])
        if 'Administrators' not in (u.get('groups') or [])
        and not any('*' in (p.get('actions') or []) and '*' in (p.get('resources') or [])
                    for p in (u.get('permissions') or []))),
    'groups': sorted(g['name'] for g in (access.get('iam_groups') or [])),
    'providers': sorted(p['name'] for p in (access.get('auth_providers') or [])),
    'iam_mode': access.get('iam_mode', 'gui'),
    'buckets': {name: (pol or {}).get('backend') or default_backend
                for name, pol in buckets.items()},
    'fs_buckets': sorted(name for name, pol in buckets.items()
                         if ((pol or {}).get('backend') or default_backend) in fs_backends),
    'replication_rules': [r['name'] for r in ((storage.get('replication') or {}).get('rules') or [])],
    'lifecycle_rules': [r['name'] for r in ((storage.get('lifecycle') or {}).get('rules') or [])],
}
# GUI-mode IAM lives in the database, not the YAML: the backup's iam.json
# holds what the restore must bring back.
iam_path = os.path.join(os.path.dirname(sys.argv[1]), 'backup-iam.json')
if out['iam_mode'] != 'declarative' and os.path.exists(iam_path):
    iam = json.load(open(iam_path))
    out['users'] = sorted(u['name'] for u in iam.get('users', [])
                          if u.get('auth_source', 'local') != 'external')
    out['groups'] = sorted(g['name'] for g in iam.get('groups', []))
    out['providers'] = sorted(p['name'] for p in iam.get('auth_providers', []))
    out['iam_source'] = 'backup iam.json'
else:
    out['iam_source'] = 'YAML'
json.dump(out, sys.stdout)
PY
note "expectations: $(python3 -c "import json;d=json.load(open('${EXPECT_JSON}'));print(f\"IAM from {d['iam_source']}: {len(d['users'])} users, {len(d['groups'])} groups, {len(d['providers'])} providers, {len(d['buckets'])} buckets, {len(d['replication_rules'])} repl rules\")")"

# ── 1. config lint ──────────────────────────────────────────────────────
ASSERT="config lint accepts the prod YAML"
check "${BIN}" config lint "${YAML}"

# ── 2. boot + health ────────────────────────────────────────────────────
start_proxy
ASSERT="proxy boots healthy on the prod config"
check curl -fsS "${BASE}/_/health"

# ── 3. admin login + IAM surface ────────────────────────────────────────
ASSERT="bootstrap admin login"
check curl -fsS -c "${COOKIES}" -X POST "${BASE}/_/api/admin/login" \
  -H 'Content-Type: application/json' -d "{\"password\":\"${PW}\"}"

# Filesystem-routed buckets have no directory yet in a fresh clone —
# create them through the S3 API (safe: their backend path lives inside
# the throwaway dir). Remote s3-backed buckets are NEVER created/touched.
export AWS_ACCESS_KEY_ID=admin AWS_SECRET_ACCESS_KEY="${PW}" AWS_EC2_METADATA_DISABLED=true
EP=(--endpoint-url "${BASE}")
while read -r fsb; do
  [[ -n "$fsb" ]] && aws "${EP[@]}" s3 mb "s3://${fsb}" >/dev/null 2>&1 || true
done <<<"$(python3 -c "import json;d=json.load(open('${EXPECT_JSON}'));print('\n'.join(d['fs_buckets']))")"

api_matches() { # <assert-label> <url> <python-predicate over (resp, expect)>
  local label="$1" url="$2" pred="$3"
  if curl -fsS -b "${COOKIES}" "${url}" -o "${DIR}/resp.json" 2>/dev/null \
     && python3 -c "
import json
resp = json.load(open('${DIR}/resp.json'))
expect = json.load(open('${EXPECT_JSON}'))
assert ${pred}, (resp, expect)
" 2>"${DIR}/pred.err"; then
    ok "${label}"
  else
    bad "${label} $(head -c 200 "${DIR}/pred.err" 2>/dev/null)"
  fi
}

# Declarative-mode contract: LOCAL users exactly mirror the YAML;
# OAuth-born users (auth_source == 'external') are preserved by the
# reconciler and may appear in addition.
api_matches "IAM users match the expectations (externals allowed)" "${BASE}/_/api/admin/users" \
  "(lambda users: sorted(u['name'] for u in users if u.get('auth_source') != 'external') == expect['users'])(resp if isinstance(resp, list) else resp.get('users', []))"
api_matches "IAM groups match the expectations" "${BASE}/_/api/admin/groups" \
  "set(expect['groups']) <= set(g['name'] for g in (resp if isinstance(resp, list) else resp.get('groups', [])))"
api_matches "OIDC providers match the expectations" "${BASE}/_/api/admin/ext-auth/providers" \
  "sorted(p['name'] for p in (resp if isinstance(resp, list) else resp.get('providers', []))) == expect['providers']"

# ── 4. bucket routing ───────────────────────────────────────────────────
api_matches "bucket→backend routing matches the YAML" "${BASE}/_/api/admin/buckets" \
  "(lambda rows: all(any(b['name'] == name and b.get('backend_name') == origin for b in rows) for name, origin in expect['buckets'].items() if name in expect['fs_buckets']) and all(b.get('backend_name') == expect['buckets'][b['name']] for b in rows if b['name'] in expect['buckets']))(resp.get('buckets', []))"

# ── 5. jobs endpoint lists the replication rules ────────────────────────
api_matches "unified /jobs lists YAML replication rules" "${BASE}/_/api/admin/jobs" \
  "all(any(('replication' in str(j.get('id','')) and rule in str(j.get('id',''))) or rule == j.get('name') for j in (resp.get('jobs') if isinstance(resp, dict) else resp)) for rule in expect['replication_rules'])"

# ── 6. export → lint round-trip ─────────────────────────────────────────
ASSERT="config export passes config lint (round-trip)"
if curl -fsS -b "${COOKIES}" "${BASE}/_/api/admin/config/export" -o "${DIR}/export.yaml" \
   && "${BIN}" config lint "${DIR}/export.yaml" >/dev/null 2>&1; then
  ok "${ASSERT}"
else
  bad "${ASSERT}"
fi

# ── 7. S3 surface with bootstrap creds ──────────────────────────────────
ASSERT="S3 ListBuckets shows every filesystem-routed YAML bucket"
if LIST="$(aws "${EP[@]}" s3api list-buckets --query 'Buckets[].Name' --output text 2>/dev/null)" \
   && python3 -c "
import json, sys
expect = json.load(open('${EXPECT_JSON}'))
got = set('''${LIST:-}'''.split())
missing = set(expect['fs_buckets']) - got
assert not missing, f'missing buckets: {missing}'
remote_missing = set(expect['buckets']) - set(expect['fs_buckets']) - got
if remote_missing:
    print(f'note: remote-backed buckets not listed (backend unreachable?): {sorted(remote_missing)}', file=sys.stderr)
"; then
  ok "${ASSERT}"
else
  bad "${ASSERT}"
fi

FS_BUCKET="$(python3 -c "import json;d=json.load(open('${EXPECT_JSON}'));print(d['fs_buckets'][0] if d['fs_buckets'] else '')")"
if [[ -n "${FS_BUCKET}" ]]; then
  ASSERT="PUT+GET sha256 round-trip on filesystem bucket '${FS_BUCKET}'"
  dd if=/dev/urandom of="${DIR}/blob.bin" bs=1048576 count=2 2>/dev/null
  SUM1="$(shasum -a 256 "${DIR}/blob.bin" | cut -d' ' -f1)"
  if aws "${EP[@]}" s3 cp "${DIR}/blob.bin" "s3://${FS_BUCKET}/__prodconf_test/blob.bin" --no-progress >/dev/null 2>&1 \
     && aws "${EP[@]}" s3 cp "s3://${FS_BUCKET}/__prodconf_test/blob.bin" "${DIR}/blob.out" --no-progress >/dev/null 2>&1 \
     && [[ "$(shasum -a 256 "${DIR}/blob.out" | cut -d' ' -f1)" == "${SUM1}" ]]; then
    ok "${ASSERT}"
    aws "${EP[@]}" s3 rm "s3://${FS_BUCKET}/__prodconf_test/blob.bin" >/dev/null 2>&1 || true
  else
    bad "${ASSERT}"
  fi
else
  note "skip: no filesystem-routed bucket for write round-trip"
fi

# ── 8. non-admin declarative user is denied outside its grants ──────────
read -r NA_USER NA_KEY NA_SECRET <<<"$(python3 -c "
import json
d = json.load(open('${EXPECT_JSON}'))
for name in d['nonadmin_users']:
    k, s = d['user_keys'][name]
    print(name, k, s); break
" )" || true
if [[ -n "${NA_USER:-}" && -n "${FS_BUCKET}" ]]; then
  ASSERT="non-admin '${NA_USER}' denied writing ${FS_BUCKET}/__prodconf_denied (fail-closed)"
  if AWS_ACCESS_KEY_ID="${NA_KEY}" AWS_SECRET_ACCESS_KEY="${NA_SECRET}" \
     aws "${EP[@]}" s3 cp "${DIR}/blob.bin" "s3://${FS_BUCKET}/__prodconf_denied/blob.bin" --no-progress >/dev/null 2>&1; then
    bad "${ASSERT} — write unexpectedly SUCCEEDED"
    AWS_ACCESS_KEY_ID=admin AWS_SECRET_ACCESS_KEY="${PW}" \
      aws "${EP[@]}" s3 rm "s3://${FS_BUCKET}/__prodconf_denied/blob.bin" >/dev/null 2>&1 || true
  else
    ok "${ASSERT}"
  fi
else
  note "skip: no non-admin user or no fs bucket for the denial probe"
fi

echo
echo "── ${PASS} passed, ${FAIL} failed ──"
[[ ${FAIL} -eq 0 ]]

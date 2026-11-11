#!/usr/bin/env bash
# The Helm chart sets DGP_SPOOL_MAX_BYTES below the size limit of the /tmp
# emptyDir that holds the spool, for the default values and every example.
set -euo pipefail
chart=charts/deltaglider-proxy
to_bytes() {
  case "$1" in
    *Gi) echo $(( ${1%Gi} * 1073741824 )) ;;
    *Mi) echo $(( ${1%Mi} * 1048576 )) ;;
    *) echo "unknown size unit: $1" >&2; exit 1 ;;
  esac
}
check() {
  local what=$1; shift
  local out limit spool
  out=$(helm template ci "$chart" "$@")
  limit=$(awk '/- name: tmp$/{t=1} t && /sizeLimit:/{print $2; exit}' <<<"$out")
  spool=$(awk '/name: DGP_SPOOL_MAX_BYTES/{getline; gsub(/"/, "", $2); print $2; exit}' <<<"$out")
  [ -n "$limit" ] || { echo "$what: no tmp sizeLimit" >&2; exit 1; }
  [ -n "$spool" ] || { echo "$what: no DGP_SPOOL_MAX_BYTES" >&2; exit 1; }
  limit=$(to_bytes "$limit")
  if [ "$spool" -le 0 ] || [ "$spool" -ge "$limit" ]; then
    echo "$what: DGP_SPOOL_MAX_BYTES=$spool is not below the /tmp limit $limit" >&2
    exit 1
  fi
  echo "$what: DGP_SPOOL_MAX_BYTES=$spool < /tmp limit $limit"
}
check "default values"
check "tmp.sizeLimit=512Mi" --set tmp.sizeLimit=512Mi
for f in "$chart"/examples/*.yaml; do
  check "$f" -f "$f"
done
if helm template ci "$chart" --set tmp.sizeLimit=2G >/dev/null 2>&1; then
  echo "a sizeLimit without Mi or Gi must fail the render" >&2
  exit 1
fi

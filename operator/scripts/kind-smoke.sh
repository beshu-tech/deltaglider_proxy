#!/usr/bin/env bash
# Cluster smoke for the operator, run by operator-ci.yml before publish.
# Needs: kubectl pointed at an empty kind (or k3d) cluster, and the image
# named in deploy/operator.yaml already loaded into it.
#
# 1. Install the CRD and the operator from deploy/ (the real RBAC).
# 2. Pre-create a bootstrap Secret without dbKey, as an operator before
#    0.3.24 left it. The operator must patch it (needs `patch` on secrets).
# 3. Create one DeltaGliderProxy and wait for phase Ready.
# 4. Change spec.image and wait for the StatefulSet rollout and Ready again.
set -euo pipefail

cd "$(dirname "$0")/.."
NS=dgp-smoke
CR=dgp
TIMEOUT=${SMOKE_TIMEOUT:-300s}

diagnose() {
  echo "::group::diagnostics"
  kubectl get dgp,sts,deploy,pods,pvc,secrets -A -o wide || true
  kubectl -n "$NS" get dgp "$CR" -o yaml || true
  kubectl -n "$NS" describe pods || true
  kubectl get events -A --sort-by=.lastTimestamp | tail -50 || true
  kubectl -n deltaglider-system logs deploy/deltaglider-operator --tail=200 || true
  kubectl -n "$NS" logs "statefulset/$CR" --tail=100 || true
  echo "::endgroup::"
}
trap diagnose ERR

kubectl apply -f deploy/crd.yaml
kubectl wait --for=condition=Established crd/deltagliderproxies.deltaglider.beshu.tech --timeout=60s
kubectl apply -f deploy/operator.yaml
kubectl -n deltaglider-system rollout status deploy/deltaglider-operator --timeout=120s

kubectl create namespace "$NS"
kubectl -n "$NS" create secret generic dgp-env \
  --from-literal=DGP_ACCESS_KEY_ID=smoke \
  --from-literal=DGP_SECRET_ACCESS_KEY=smoke-secret-not-used-elsewhere
# bcrypt("correct horse battery staple", 12), base64: the specimen of the
# operator's own bootstrap_hash_tests. A test value, not a credential.
kubectl -n "$NS" create secret generic "$CR-bootstrap" \
  --from-literal=password='correct horse battery staple' \
  --from-literal=hash='JDJiJDEyJDEzZzhBSEpLTkZ2aGguU0lORnlUSGVMdEpzUzBSM2FkdjROTEd6eFZzQ1FJR3c5U0dLc1ht'

kubectl apply -f - <<YAML
apiVersion: deltaglider.beshu.tech/v1alpha1
kind: DeltaGliderProxy
metadata:
  name: $CR
  namespace: $NS
spec:
  replicas: 1
  configYaml: |
    storage:
      filesystem: /data/storage
    advanced:
      listen_addr: "0.0.0.0:9000"
  envFromSecret: dgp-env
  bootstrapPassword:
    autoGenerate: true
  storage:
    size: 1Gi
  router:
    replicas: 1
YAML

kubectl -n "$NS" wait --for=jsonpath='{.status.phase}'=Ready "dgp/$CR" --timeout="$TIMEOUT"
db_key=$(kubectl -n "$NS" get secret "$CR-bootstrap" -o jsonpath='{.data.dbKey}')
if [[ -z "$db_key" ]]; then
  echo "::error::the operator did not add dbKey to the existing bootstrap Secret"
  diagnose
  exit 1
fi
echo "ok: phase Ready, dbKey added to the old bootstrap Secret"

# Upgrade step: the same release under its fully qualified name is a new
# spec.image string, so the pod template changes and the StatefulSet rolls.
old_image=$(kubectl -n "$NS" get sts "$CR" -o jsonpath='{.spec.template.spec.containers[0].image}')
new_image="docker.io/${old_image#docker.io/}"
[[ "$new_image" != "$old_image" ]] || { echo "::error::image $old_image is already fully qualified"; exit 1; }
gen=$(kubectl -n "$NS" get dgp "$CR" -o jsonpath='{.metadata.generation}')
kubectl -n "$NS" patch dgp "$CR" --type=merge -p "{\"spec\":{\"image\":\"$new_image\"}}"

# Wait until the operator wrote the new template, then for the rollout.
end=$((SECONDS + 120))
until [[ "$(kubectl -n "$NS" get sts "$CR" -o jsonpath='{.spec.template.spec.containers[0].image}')" == "$new_image" ]]; do
  if (( SECONDS > end )); then echo "::error::the StatefulSet never got image $new_image"; diagnose; exit 1; fi
  sleep 2
done
kubectl -n "$NS" rollout status "statefulset/$CR" --timeout="$TIMEOUT"
end=$((SECONDS + 120))
until (( $(kubectl -n "$NS" get dgp "$CR" -o jsonpath='{.status.observedGeneration}') > gen )); do
  if (( SECONDS > end )); then echo "::error::status.observedGeneration never passed $gen"; diagnose; exit 1; fi
  sleep 2
done
kubectl -n "$NS" wait --for=jsonpath='{.status.phase}'=Ready "dgp/$CR" --timeout="$TIMEOUT"
echo "ok: spec.image change rolled out to $new_image and the phase is Ready"

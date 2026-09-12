#!/usr/bin/env bash
# Destructive, explicitly requested development reset. This script intentionally
# leaves control-plane/gateway stopped; deploy-local.sh restores desired replicas
# only after fresh images and manifests are ready.
set -euo pipefail
if [[ "$(kubectl config current-context)" != "docker-desktop" ]]; then
  echo "Local reset is restricted to the docker-desktop context." >&2
  exit 1
fi
kubectl -n xscope-system get statefulset/postgres deployment/redis >/dev/null
kubectl -n xscope-system scale deployment/control-plane deployment/gateway --replicas=0
kubectl -n xscope-system wait --for=delete pod -l app.kubernetes.io/name=control-plane --timeout=120s
kubectl -n xscope-system wait --for=delete pod -l app.kubernetes.io/name=gateway --timeout=120s
kubectl -n xscope-system exec statefulset/postgres -- \
  psql -v ON_ERROR_STOP=1 -U xscope -d keycloak -c 'DROP SCHEMA IF EXISTS xscope CASCADE;'
kubectl -n xscope-system exec deployment/redis -- sh -ec '
  redis-cli --scan --pattern "xscope:quota:*" | while IFS= read -r key; do
    redis-cli UNLINK "$key" >/dev/null
  done
'
echo "Removed XScope business schema and development quota state; Keycloak identities and historical evidence PVCs remain."

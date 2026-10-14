#!/usr/bin/env bash
# Run Go's own `net` (or net/http) package tests under patina (D73): a thin
# wrapper around scripts/test-gopkg.sh (D74), kept so the Phase 4 command
# still works.
#
#   scripts/test-gonet.sh [--pkg net|net/http] [aarch64] [x86_64]   (default: net, both)
#
# Environment: GONET_TEST_RUN (-test.run regex), GONET_TEST_SHORT=0 (drop
# -test.short), GONET_TEST_TIMEOUT (go -test.timeout), plus everything
# test-gopkg.sh takes (PATINA_CPUS, PATINA_MEM, PATINA_PARAMS, ...).
# Skip list testapps/skiplist/<pkg>.txt; logs in out/test/gopkg/.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
pkg=net
if [[ ${1:-} == --pkg ]]; then
  pkg=$2
  shift 2
fi
arches=$(IFS=,; echo "$*")
opts=(--arch "${arches:-both}")
[[ -n ${GONET_TEST_RUN:-} ]] && opts+=(--run "$GONET_TEST_RUN")
[[ ${GONET_TEST_SHORT:-1} == 0 ]] && opts+=(--no-short)
[[ -n ${GONET_TEST_TIMEOUT:-} ]] && export GOPKG_TEST_TIMEOUT=$GONET_TEST_TIMEOUT
exec "$root/scripts/test-gopkg.sh" "$pkg" "${opts[@]}"

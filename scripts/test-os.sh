#!/usr/bin/env bash
# Run Go's own `os` package tests under patina (D65): a thin wrapper around
# scripts/test-gopkg.sh (D74), kept so the Phase 3 command still works.
#
#   scripts/test-os.sh [aarch64] [x86_64]     (default: both)
#
# Environment: OS_TEST_RUN (-test.run regex), OS_TEST_SHORT=0 (drop
# -test.short), OS_TEST_TIMEOUT (go -test.timeout), plus everything
# test-gopkg.sh takes (PATINA_CPUS, PATINA_MEM, PATINA_PARAMS, ...).
# Skip list testapps/skiplist/os.txt; logs in out/test/gopkg/os-<arch>-cpuN.log.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
arches=$(IFS=,; echo "$*")
opts=(--arch "${arches:-both}")
[[ -n ${OS_TEST_RUN:-} ]] && opts+=(--run "$OS_TEST_RUN")
[[ ${OS_TEST_SHORT:-1} == 0 ]] && opts+=(--no-short)
[[ -n ${OS_TEST_TIMEOUT:-} ]] && export GOPKG_TEST_TIMEOUT=$OS_TEST_TIMEOUT
exec "$root/scripts/test-gopkg.sh" os "${opts[@]}"

#!/usr/bin/env bash
# Run the in-process VZ tests of cli/internal/vmm with an entitlement-signed
# test binary (plain `go test` binaries are unsigned, so VZ refuses them and
# those tests skip).
#
#   cli/scripts/vztest.sh [-test.run regexp] [-test.v] ...
#   PATINA_VZ_PROBE=1 cli/scripts/vztest.sh -test.run TestVZProbeDTB -test.v
#
# macOS on Apple Silicon only.
set -euo pipefail

cli=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/patina-vztest.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

cd "$cli"
CGO_ENABLED=1 go test -c -o "$tmp/vmm.test" ./internal/vmm
codesign --force --sign - --entitlements "$cli/scripts/patina.entitlements" "$tmp/vmm.test"
# Tests read testdata/ relative to the package directory.
cd "$cli/internal/vmm"
"$tmp/vmm.test" "$@"

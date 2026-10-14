#!/usr/bin/env bash
# Run the in-process VZ tests of cli/internal/vmm with an entitlement-signed
# test binary (plain `go test` binaries are unsigned, so VZ refuses them and
# those tests skip).
#
#   cli/scripts/vztest.sh [-test.run regexp] [-test.v] ...
#   PATINA_VZ_PROBE=1 cli/scripts/vztest.sh -test.run TestVZProbeDTB -test.v
#
# With --e2e it instead runs the CLI end-to-end tests against the real kernel
# (cli/vz_integration_test.go: hello exit codes, ctl stop/ping, --net user
# + HTTP from the host at the guest's DHCP address). They build their own
# signed CLI; the kernel must be built (scripts/build-kernel.sh aarch64).
# Then it runs the in-process AF_VSOCK test (TestVZVsock, decision D76:
# testapps/vsockapp, the host dialing the guest's listener on port 5000).
#
#   cli/scripts/vztest.sh --e2e [go test flags...]     (default: -v)
#
# macOS on Apple Silicon only.
set -euo pipefail

cli=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [[ ${1:-} == --e2e ]]; then
  shift
  cd "$cli"
  [[ $# -gt 0 ]] || set -- -v
  PATINA_VZ_E2E=1 go test -count=1 -timeout 20m -run 'TestVZ(BootHello|E2E)' "$@" .
  # AF_VSOCK end to end (D76): in-process (the host side dials the guest's
  # listener through the framework), so it runs from a signed vmm.test.
  exec "$0" -test.run 'TestVZVsock$' -test.v -test.timeout 5m
fi
tmp=$(mktemp -d "${TMPDIR:-/tmp}/patina-vztest.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

cd "$cli"
CGO_ENABLED=1 go test -c -o "$tmp/vmm.test" ./internal/vmm
codesign --force --sign - --entitlements "$cli/scripts/patina.entitlements" "$tmp/vmm.test"
# Tests read testdata/ relative to the package directory.
cd "$cli/internal/vmm"
"$tmp/vmm.test" "$@"

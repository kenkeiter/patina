#!/usr/bin/env bash
# Boot the patina kernel + an initrd under Apple Virtualization.framework,
# with the same interface as scripts/run-qemu.sh (which execs this script
# when PATINA_VMM=vz):
#
#   scripts/run-vz.sh aarch64 [initrd.cpio] [-- [params... --] args...]
#
# The kernel and initrd are packed into a temporary .patina image
# (`patina image pack`, target vz-aarch64) and run with
# `patina run --vmm vz` (doc/cli-contract.md "Apple Virtualization.framework").
# Parameter/argument handling matches run-qemu.sh: words before an inner
# `--` are kernel params, the rest are program args, and raw `k=v` words
# among the args are kernel params too (they are moved to the params here,
# since `patina run` percent-encodes `=` in real args).
#
# Environment (the run-qemu.sh names):
#   PATINA_CPUS     vCPUs (default 1)            -> --cpus
#   PATINA_MEM      MiB (default 512)            -> --mem
#   PATINA_TIMEOUT  seconds (default 60)         -> --timeout (plus a hard
#                   outer timeout 30 s later)
#   PATINA_KERNEL   kernel Image (default out/kernel-aarch64.Image)
#   PATINA_LOG      file capturing the guest console (default: a temp file)
#   PATINA_NET      user -> --net user (VZ NAT + DHCP; the CLI adds
#                   ip=dhcp, so `ip=` params from the caller are dropped:
#                   QEMU slirp's 10.0.2.x addresses do not exist on VZ's
#                   NAT). user:H-G forwards are NOT available on VZ (no port
#                   forwarding; the host reaches the guest at its leased
#                   address) -> error.
#   PATINA_CTL_SOCK made a symlink to the run's control proxy socket
#                   (<run dir>/ctl.sock, raw NDJSON like QEMU's chardev
#                   socket; the proxy replays the last ready event to late
#                   clients). The link is dangling until `patina run` has
#                   started the VM, so clients must retry (ctlclient.py does).
#   PATINA_CONSOLE  ignored: the VZ console is always hvc0.
#   PATINA_VZ_PARAMS extra kernel params for the VZ path only (default
#                   empty), e.g. a temporary workaround knob.
#   PATINA_CLI      patina binary (default cli/bin/patina; (re)built with
#                   cli/scripts/build.sh, ad-hoc signed, when missing or older than cli/**/*.go)
# Not mappable on VZ (warned about, then ignored): PATINA_CTL_WAIT (the VM
# cannot be held until a client connects: events emitted before the client
# attaches, other than `ready`, are not seen by it), PATINA_ACCEL,
# PATINA_MACHINE, PATINA_VIRTIO, PATINA_QEMU_EXTRA.
#
# The console is streamed to stdout and captured; `patina run -v`
# diagnostics go to stderr after the run. Exit status as run-qemu.sh:
#   N    the guest's exit code (exit event, else the console sentinel)
#   124  timeout
#   125  no exit code (the CLI reported none, or failed)
set -euo pipefail

usage() { echo "usage: $0 aarch64 [initrd.cpio] [-- kernel args...]" >&2; exit 2; }
[[ $# -ge 1 ]] || usage
arch=$1; shift
[[ $arch == aarch64 ]] || { echo "error: VZ runs aarch64 guests only" >&2; exit 2; }
initrd=
if [[ $# -gt 0 && $1 != "--" ]]; then initrd=$1; shift; fi
if [[ $# -gt 0 ]]; then
  [[ $1 == "--" ]] || usage
  shift
fi
params=()
if [[ " $* " == *" -- "* ]]; then
  while [[ $# -gt 0 && $1 != "--" ]]; do params+=("$1"); shift; done
  shift
fi
args=()
for a in "$@"; do
  if [[ $a == *=* ]]; then params+=("$a"); else args+=("$a"); fi
done

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cpus=${PATINA_CPUS:-1}
mem=${PATINA_MEM:-512}
tmo=${PATINA_TIMEOUT:-60}
kernel=${PATINA_KERNEL:-$root/out/kernel-aarch64.Image}
cli=${PATINA_CLI:-$root/cli/bin/patina}
TIMEOUT_BIN=$(command -v timeout || command -v gtimeout || true)
[[ -n $TIMEOUT_BIN ]] || { echo "error: coreutils 'timeout' not found" >&2; exit 2; }
[[ -f $kernel ]] || { echo "error: kernel $kernel not found (run scripts/build-kernel.sh aarch64)" >&2; exit 2; }
if [[ -n $initrd && ! -f $initrd ]]; then echo "error: initrd $initrd not found" >&2; exit 2; fi
if [[ -z ${PATINA_CLI:-} ]] && { [[ ! -x $cli ]] || [[ -n $(find "$root/cli" -name '*.go' -newer "$cli" -print -quit) ]]; }; then
  "$root/cli/scripts/build.sh" >&2 || { echo "error: cannot build $cli" >&2; exit 2; }
fi

for v in PATINA_CTL_WAIT PATINA_ACCEL PATINA_MACHINE PATINA_VIRTIO PATINA_QEMU_EXTRA; do
  if [[ -n ${!v:-} && ${!v} != 0 ]]; then
    echo "[run-vz] note: $v=${!v} has no VZ equivalent, ignored" >&2
  fi
done

runflags=(--vmm vz --cpus "$cpus" --mem "$mem" --timeout "${tmo}s" --grace 5s -v)
if [[ -n ${PATINA_NET:-} ]]; then
  [[ ${PATINA_NET%%:*} == user ]] || { echo "error: PATINA_NET must be user on VZ" >&2; exit 2; }
  if [[ $PATINA_NET == *:* ]]; then
    echo "error: PATINA_NET=$PATINA_NET: port forwarding is not available on VZ (reach the guest at its DHCP address)" >&2
    exit 2
  fi
  runflags+=(--net user)
  kept=()
  for p in ${params[@]+"${params[@]}"}; do [[ $p == ip=* ]] || kept+=("$p"); done
  params=(${kept[@]+"${kept[@]}"})
fi
# shellcheck disable=SC2206
[[ -n ${PATINA_VZ_PARAMS:-} ]] && params+=(${PATINA_VZ_PARAMS})

tmp=$(mktemp -d "${TMPDIR:-/tmp}/patina-vz.XXXXXX")
name=rvz-$$-$RANDOM
log=${PATINA_LOG:-$tmp/console.log}
ctl_sock=${PATINA_CTL_SOCK:-}
cleanup() {
  rm -rf "$tmp"
  [[ -n $ctl_sock && -L $ctl_sock ]] && rm -f "$ctl_sock"
  true
}
trap cleanup EXIT

if [[ -z $initrd ]]; then
  # An empty newc archive (just the trailer): `patina run` needs an initrd.
  initrd=$tmp/empty.cpio
  printf '070701%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X%08X%08XTRAILER!!!\0\0\0\0' \
    0 0 0 0 1 0 0 0 0 0 0 11 0 >"$initrd"
fi
img=$tmp/img.patina
"$cli" image pack --kernel "$kernel" --initrd "$initrd" --target vz-aarch64 --name "$name" -o "$img" >/dev/null

if [[ -n $ctl_sock ]]; then
  if [[ -n ${XDG_RUNTIME_DIR:-} ]]; then base=$XDG_RUNTIME_DIR/patina; else base=$HOME/.patina/run; fi
  rm -f "$ctl_sock"
  ln -s "$base/$name/ctl.sock" "$ctl_sock"
fi

echo "[run-vz] PATINA_PARAMS='${params[*]:-}' $cli run $img --name $name ${runflags[*]} -- ${args[*]:-}" >&2
set +e
PATINA_PARAMS="${params[*]:-}" "$TIMEOUT_BIN" -k 5 $((tmo + 30)) \
  "$cli" run "$img" --name "$name" "${runflags[@]}" -- ${args[@]+"${args[@]}"} \
  </dev/null 2>"$tmp/stderr" | tee "$log"
rc=${PIPESTATUS[0]}
set -e
cat "$tmp/stderr" >&2

if [[ $rc -eq 124 ]]; then
  echo "[run-vz] timeout after ${tmo}s" >&2
  exit 124
fi
if [[ $rc -eq 125 ]] || grep -q 'guest ended without reporting an exit code' "$tmp/stderr"; then
  echo "[run-vz] no exit code (patina run status $rc)" >&2
  exit 125
fi
exit "$rc"

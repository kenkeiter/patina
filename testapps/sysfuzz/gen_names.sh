#!/bin/sh
# Regenerates nrnames_<arch>.go (the fuzzer's syscall table) from the
# nr_<arch>.go constants. Run from testapps/sysfuzz (go generate does).
set -eu
for a in arm64 amd64; do
  {
    echo "// Code generated from nr_$a.go by gen_names.sh. DO NOT EDIT."
    echo
    echo "package main"
    echo
    echo "// sysTable lists every syscall number the arch table names."
    echo "var sysTable = []sysEnt{"
    awk '/^\tSYS_[A-Z0-9_]+ = [0-9]+$/ { printf "\t{%s, \"%s\"},\n", $1, tolower(substr($1, 5)) }' "nr_$a.go"
    echo "}"
  } >"nrnames_$a.go"
done

#!/bin/sh
# Regenerates sample.newc.cpio with the host's cpio (bsdcpio/libarchive or
# GNU cpio). The checked-in archive was made with bsdcpio 3.5.3 on macOS;
# tests/fixture.rs asserts on its contents (uid/gid/ino are whatever the
# generating host used, and the test reads them from the archive itself).
#
# usage: mkfixture.sh <output.cpio>
set -eu
out=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

mkdir -p root/etc root/dir/sub root/empty-dir
printf 'hello from patina\n' > root/app
printf '127.0.0.1 localhost\n' > root/etc/hosts
printf 'abcde' > root/dir/five
: > root/dir/sub/empty
printf 'x' > root/odd-name-len
ln -s /app root/link-to-app
ln root/app root/app.hard
chmod 0755 root/app
chmod 0600 root/dir/five
chmod 0700 root/empty-dir

# Fixed mtime: 2025-01-02 03:04:05 UTC.
find root -exec touch -h -t 202501020304.05 {} +

cd root
find . | LC_ALL=C sort | cpio -o --format newc > "$out" 2>/dev/null
echo "wrote $out"

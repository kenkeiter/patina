#!/bin/sh
# Download Debian's linux-libc-dev (sanitized Linux UAPI headers for every
# Debian architecture) into an EMPTY directory outside the repo and unpack
# it there. Prints the include directory to pass to extract.sh.
#
#   gen/linux-headers/fetch-headers.sh /tmp/linux-uapi
#
# The package is treated as data: nothing in it is executed.
set -eu

DEB=linux-libc-dev_7.2.10-1_all.deb
SHA256=f99e80a157ea51a3f9f2c55d160e7fac9df774034d600e6aaf60ef92fca45fdb
URL=https://deb.debian.org/debian/pool/main/l/linux/$DEB

dir=${1:?usage: fetch-headers.sh <empty-dir>}
mkdir -p "$dir"
if [ -n "$(ls -A "$dir")" ]; then
	echo "fetch-headers.sh: $dir is not empty" >&2
	exit 1
fi
cd "$dir"
curl -sSfL -o "$DEB" "$URL"
echo "$SHA256  $DEB" | shasum -a 256 -c - >&2
mkdir root
# A .deb is an ar archive; bsdtar reads both layers.
bsdtar -xOf "$DEB" 'data.tar*' | bsdtar -xf - -C root
echo "$dir/root/usr/include"

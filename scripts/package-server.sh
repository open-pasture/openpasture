#!/usr/bin/env bash
# Package a release build of the server into out/<name>.tar.gz.
#
#   cargo build --release --target <target> -p op-cli -p collar-sim
#   scripts/package-server.sh <target> openpasture-server-<os>-<arch>
set -euo pipefail

target="$1"
name="$2"
root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$root/target/$target/release"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
cp "$bin/openpasture" "$bin/collar-sim" "$stage/$name/"
cp "$root/LICENSE" "$stage/$name/LICENSE"
cp "$root/packaging/README-server.md" "$stage/$name/README.md"

mkdir -p "$root/out"
COPYFILE_DISABLE=1 tar -C "$stage/$name" -czf "$root/out/$name.tar.gz" .
echo "$root/out/$name.tar.gz"

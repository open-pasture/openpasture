#!/usr/bin/env bash
# The one way to move op-protocol's vectors to an opencollar commit: copy
# protocol/vectors/ at <ref> over crates/op-protocol/vectors/ and write the
# commit's full SHA to crates/op-protocol/OPENCOLLAR_REF, so the copy and the
# commit it came from always change together.
#
#   scripts/sync-vectors.sh <opencollar git checkout> <ref>
#
# Then run the op-protocol tests (they check this implementation against every
# case) and scripts/compare-vectors.sh, and commit both in one pull request.
# When op-protocol writes the vectors (OP_WRITE_VECTORS=1), copy them into
# opencollar first (vectors/README.md), push that, and sync to its commit.
set -euo pipefail
repo=${1:?usage: sync-vectors.sh <opencollar git checkout> <ref>}
ref=${2:?usage: sync-vectors.sh <opencollar git checkout> <ref>}
here=$(cd "$(dirname "$0")/.." && pwd)
sha=$(git -C "$repo" rev-parse --verify "$ref^{commit}")
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git -C "$repo" archive "$sha" protocol/vectors | tar -x -C "$tmp"
rm -rf "$here/crates/op-protocol/vectors"
cp -R "$tmp/protocol/vectors" "$here/crates/op-protocol/vectors"
printf '# The opencollar commit whose protocol/vectors/ crates/op-protocol/vectors/ copies.\n# Written by scripts/sync-vectors.sh; checked by scripts/compare-vectors.sh in CI.\n%s\n' "$sha" > "$here/crates/op-protocol/OPENCOLLAR_REF"
echo "vectors from opencollar ${sha:0:7}; now run: cargo test -p op-protocol && scripts/compare-vectors.sh $repo"

#!/usr/bin/env bash
# op-protocol's test vectors are the collar firmware's and the protocol
# spec's too: every one must be byte-identical to each copy in opencollar
# at the commit pinned in crates/op-protocol/OPENCOLLAR_REF.
#
#   scripts/compare-vectors.sh <opencollar git checkout> [ref]
#
# The files are read from the checkout's git history at the pinned commit (or
# [ref], e.g. origin/main for the drift watch), never from its working tree,
# so the answer doesn't depend on what that checkout has checked out. Same
# command locally and in CI. To move the pin, use scripts/sync-vectors.sh.
#
# v1 (crates/op-protocol/vectors/*.json): opencollar's firmware/tests/host/vectors/,
#   protocol/vectors/ and protocol/vectors/v1/, whichever exist; at least one must.
# v2 (crates/op-protocol/vectors/v2/*.json): opencollar's protocol/vectors/v2/.
set -euo pipefail
repo=${1:?usage: compare-vectors.sh <opencollar git checkout> [ref]}
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
pinfile=crates/op-protocol/OPENCOLLAR_REF
pin=$(grep -Ev '^[[:space:]]*(#|$)' "$pinfile" 2>/dev/null | head -1 | tr -d '[:space:]' || true)
if ! [[ $pin =~ ^[0-9a-f]{40}$ ]]; then
  echo "::error file=$pinfile::$pinfile must hold the full SHA of the opencollar commit these vectors match (got '${pin}'). The comparison is never skipped: run scripts/sync-vectors.sh <opencollar checkout> <ref> to copy the vectors and write the pin."
  exit 1
fi
ref=${2:-$pin}
if ! git -C "$repo" cat-file -e "$ref^{commit}" 2>/dev/null; then
  echo "::error::$repo has no commit $ref; fetch it (git -C $repo fetch origin) or check out opencollar at it"
  exit 1
fi
oc=$(mktemp -d)
trap 'rm -rf "$oc"' EXIT
paths=()
for d in firmware/tests/host/vectors protocol/vectors; do
  git -C "$repo" cat-file -e "$ref:$d" 2>/dev/null && paths+=("$d")
done
[ ${#paths[@]} -gt 0 ] && git -C "$repo" archive "$ref" "${paths[@]}" | tar -x -C "$oc"
echo "opencollar at $(git -C "$repo" rev-parse --short "$ref")${2:+ ($2)}; pin $pinfile = ${pin:0:7}"
status=0
shopt -s nullglob

compare() { # compare <ours> <copies...>
  local ours=$1; shift
  local name; name=$(basename "$ours")
  if [ $# -eq 0 ]; then
    echo "::error::${ours#crates/op-protocol/} has no copy in opencollar"
    status=1
    return
  fi
  for theirs in "$@"; do
    if cmp -s "$ours" "$theirs"; then
      echo "same: ${ours#crates/op-protocol/} = opencollar/${theirs#"$oc"/}"
    else
      echo "::error::${ours#crates/op-protocol/} differs from opencollar/${theirs#"$oc"/}"
      status=1
    fi
  done
}

v1=(crates/op-protocol/vectors/*.json)
v2=(crates/op-protocol/vectors/v2/*.json)
[ ${#v1[@]} -gt 0 ] || { echo "::error::no vectors in crates/op-protocol/vectors"; exit 1; }
for ours in "${v1[@]}"; do
  name=$(basename "$ours")
  copies=()
  for d in firmware/tests/host/vectors protocol/vectors protocol/vectors/v1; do
    [ -f "$oc/$d/$name" ] && copies+=("$oc/$d/$name")
  done
  compare "$ours" ${copies[@]+"${copies[@]}"}
done
for ours in ${v2[@]+"${v2[@]}"}; do
  name=$(basename "$ours")
  copies=()
  [ -f "$oc/protocol/vectors/v2/$name" ] && copies+=("$oc/protocol/vectors/v2/$name")
  compare "$ours" ${copies[@]+"${copies[@]}"}
done
# A v2 vector opencollar has and we don't is a gap too.
for theirs in "$oc"/protocol/vectors/v2/*.json; do
  [ -f "crates/op-protocol/vectors/v2/$(basename "$theirs")" ] || { echo "::error::opencollar/protocol/vectors/v2/$(basename "$theirs") has no copy in crates/op-protocol/vectors/v2"; status=1; }
done
if [ $status -ne 0 ] && [ -z "${2:-}" ]; then
  echo "::error::op-protocol's vectors differ from opencollar at the pin. Regenerate them here (OP_WRITE_VECTORS=1 cargo test -p op-protocol --test vectors --test vectors_v2), then copy them into opencollar, or bring opencollar's into this repo with scripts/sync-vectors.sh <checkout> <ref>, and move the pin in the same pull request."
elif [ $status -ne 0 ]; then
  echo "::error::opencollar $2 has vectors that differ from the pinned ones: op-protocol and the firmware have drifted. Bring the two together (vectors/README.md) and move the pin with scripts/sync-vectors.sh."
fi
exit $status

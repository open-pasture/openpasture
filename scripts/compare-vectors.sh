#!/usr/bin/env bash
# op-protocol's test vectors are the collar firmware's and the protocol
# spec's too: every one must be byte-identical to each copy in opencollar.
#
#   scripts/compare-vectors.sh <opencollar checkout>
#
# v1 (crates/op-protocol/vectors/*.json): opencollar's firmware/tests/host/vectors/,
#   protocol/vectors/ and protocol/vectors/v1/, whichever exist; at least one must.
# v2 (crates/op-protocol/vectors/v2/*.json): opencollar's protocol/vectors/v2/.
set -euo pipefail
oc=${1:?usage: compare-vectors.sh <opencollar checkout>}
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
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
      echo "same: ${ours#crates/op-protocol/} = ${theirs#"$oc"/}"
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
exit $status

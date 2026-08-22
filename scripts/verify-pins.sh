#!/usr/bin/env bash
#
# toolchain.env is the single authority for the toolchain pins. A few consumers
# cannot read it -- a workflow `uses:` ref and a Cargo git `rev` must both be
# literals, and docs/toolchain.md states the pin in prose. Every one of those
# copies is asserted here instead of being trusted, so a repin that misses one
# fails closed rather than shipping a split toolchain.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# shellcheck source=toolchain.env
. ./toolchain.env

: "${HEW_REVISION:?toolchain.env must define HEW_REVISION}"
: "${HEW_VERSION:?toolchain.env must define HEW_VERSION}"

fail() {
  echo "pin drift: $*" >&2
  exit 1
}

for workflow in .github/workflows/publish.yml .github/workflows/ci.yml; do
  grep -q "setup-llvm@${HEW_REVISION}\$" "$workflow" \
    || fail "$workflow does not pin setup-llvm at $HEW_REVISION"
done

grep -q "^hew-cabi = .* rev = \"${HEW_REVISION}\" }\$" Cargo.toml \
  || fail "Cargo.toml does not pin hew-cabi at $HEW_REVISION"

# docs/toolchain.md states the revision more than once (prose and a clone
# recipe). Checking that the pin appears somewhere would pass with a stale
# second copy, so every 40-hex string in the file must be the pin.
documented_revisions="$(grep -oE '[0-9a-f]{40}' docs/toolchain.md | sort -u || true)"
[ -n "$documented_revisions" ] || fail "docs/toolchain.md states no revision"
while read -r revision; do
  [ "$revision" = "$HEW_REVISION" ] \
    || fail "docs/toolchain.md states revision $revision, expected $HEW_REVISION"
done <<<"$documented_revisions"

grep -q "$HEW_VERSION" docs/toolchain.md \
  || fail "docs/toolchain.md does not state version $HEW_VERSION"

echo "verified toolchain pins: $HEW_VERSION at $HEW_REVISION"

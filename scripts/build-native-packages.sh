#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
. ./toolchain.env
: "${HEW:?set HEW to the pinned compiler}"
: "${HEW_NATIVE_ARCHIVE_DIR:?set HEW_NATIVE_ARCHIVE_DIR to the output directory}"
test "$("$HEW" --version)" = "hew $HEW_VERSION"
scripts/verify-pins.sh
python3 scripts/verify_package_contract.py
mkdir -p "$HEW_NATIVE_ARCHIVE_DIR"
archive_dir="$(cd "$HEW_NATIVE_ARCHIVE_DIR" && pwd)"
manifests="$(mktemp)"
trap 'rm -f "$manifests"' EXIT
python3 scripts/verify_package_contract.py --native-publish-order > "$manifests"
count=0

while IFS=$'\t' read -r manifest name version; do
  if [[ -n "${HEW_NATIVE_PACKAGES:-}" && " ${HEW_NATIVE_PACKAGES} " != *" ${name} "* ]]; then
    continue
  fi
  arguments=(native build --output "$archive_dir/$name-$version.tar.zst")
  if [[ -n "${HEW_NATIVE_TARGET:-}" ]]; then
    arguments+=(--target "$HEW_NATIVE_TARGET")
  fi
  (cd "$(dirname "$manifest")" && "$HEW" "${arguments[@]}")
  count=$((count + 1))
done < "$manifests"
[[ "$count" -gt 0 ]] || { echo "no selected native packages" >&2; exit 1; }

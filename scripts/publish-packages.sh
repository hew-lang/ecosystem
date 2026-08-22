#!/usr/bin/env bash
#
# Publish every tracked package manifest whose version is not yet in the
# registry. This is the only authority for the publish action: the workflow
# calls it, and so does `make publish-local`. Inputs are files in this
# repository plus the environment, so it runs identically on a runner and on a
# maintainer's machine.
#
# Required in the environment:
#   HEW  path to a hew binary whose --version matches the HEW_VERSION pin
#
# Authentication resolves to exactly one source, or the script exits:
#   * OIDC                       when ACTIONS_ID_TOKEN_REQUEST_URL is set
#   * ~/.hew/credentials.toml    when it already exists
# Both present is an ambiguity and neither present is a missing login; both
# fail closed, and no unauthenticated publish is ever attempted.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# shellcheck source=toolchain.env
. ./toolchain.env

: "${HEW_VERSION:?toolchain.env must define HEW_VERSION}"
: "${REGISTRY_API:?toolchain.env must define REGISTRY_API}"
: "${REGISTRY_AUDIENCE:?toolchain.env must define REGISTRY_AUDIENCE}"

# Paths under the Hew home are reported as `~/.hew/...`: the absolute form
# carries the account name, and these messages are read from CI logs.
hew_home="$HOME/.hew"
credentials="$hew_home/credentials.toml"
signing_key="$hew_home/keys/id_ed25519"

fail() {
  echo "$*" >&2
  exit 1
}

sha256_hex() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

# Every temporary file this script creates is registered here before first use,
# so an exit at any assertion leaves nothing behind.
manifests_file="$(mktemp)"
publish_log="$(mktemp)"
response="$(mktemp)"
archive="$(mktemp)"
trap 'rm -f "$manifests_file" "$publish_log" "$response" "$archive"' EXIT

# 1. Contracts first: never touch the registry with a drifted pin or with a
#    manifest set that does not satisfy the dotted-name and version contract.
scripts/verify-pins.sh
python3 scripts/verify_package_contract.py

# 2. The toolchain must be the pinned one.
hew_bin="${HEW:-}"
[ -n "$hew_bin" ] || fail "HEW is unset; set it to the pinned hew binary (make toolchain builds one)"
[ -x "$hew_bin" ] || fail "HEW is not executable: $hew_bin"
actual_version="$("$hew_bin" --version)"
echo "Using $actual_version"
[ "$actual_version" = "hew $HEW_VERSION" ] \
  || fail "toolchain mismatch: expected 'hew $HEW_VERSION', found '$actual_version'"

# 3. Authentication: exactly one source resolves.
have_oidc=0
have_credentials=0
if [ -n "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ]; then
  have_oidc=1
fi
if [ -f "$credentials" ]; then
  have_credentials=1
fi

if [ "$have_oidc" -eq 1 ] && [ "$have_credentials" -eq 1 ]; then
  fail "ambiguous credentials: OIDC is available and ~/.hew/credentials.toml already exists; remove one"
elif [ "$have_oidc" -eq 1 ]; then
  echo "Authenticating via GitHub Actions OIDC"
  : "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:?OIDC request URL is set but ACTIONS_ID_TOKEN_REQUEST_TOKEN is not}"
  "$hew_bin" key generate
  public_key="$(<"$signing_key.pub")"
  oidc_token="$(
    curl --fail --silent --show-error \
      --header "Authorization: Bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
      "${ACTIONS_ID_TOKEN_REQUEST_URL}&audience=$(printf '%s' "$REGISTRY_AUDIENCE" | jq -sRr @uri)" \
      | jq --exit-status --raw-output '.value'
  )"
  grant="$(
    jq --null-input \
      --arg oidc_token "$oidc_token" \
      --arg public_key "$public_key" \
      '{oidc_token: $oidc_token, public_key: $public_key}' \
      | curl --fail-with-body --silent --show-error \
          --header "Content-Type: application/json" \
          --data-binary @- \
          "$REGISTRY_API/login/github-actions"
  )"
  registry_token="$(jq --exit-status --raw-output '.token' <<<"$grant")"
  [ "$(jq --exit-status --raw-output '.identity' <<<"$grant")" = "hew-ecosystem" ] \
    || fail "trusted publishing grant is not the hew-ecosystem identity"
  [ "$(jq --exit-status --raw-output '.expires_in' <<<"$grant")" = "900" ] \
    || fail "trusted publishing grant did not carry the expected 900s lifetime"
  mkdir -p "$hew_home"
  (
    umask 077
    printf '[registry]\ntoken = "%s"\ngithub_user = "hew-ecosystem"\n' \
      "$registry_token" > "$credentials"
  )
elif [ "$have_credentials" -eq 1 ]; then
  echo "Authenticating with the existing registry credentials at ~/.hew/credentials.toml"
  [ -f "$signing_key" ] \
    || fail "no signing key at ~/.hew/keys/id_ed25519; run: hew key generate"
else
  fail "no registry credentials: run 'hew login' to authenticate, or run this on a runner with OIDC"
fi

# The fingerprint of the key that will sign every publish below. The registry
# records it per version, so it is what the read-back is checked against.
key_fingerprint="$("$hew_bin" key list | sed -n 's/^[[:space:]]*Fingerprint:[[:space:]]*//p' | head -1)"
case "$key_fingerprint" in
  SHA256:?*) ;;
  *) fail "could not read the local signing key fingerprint from 'hew key list'" ;;
esac
echo "Signing with $key_fingerprint"

# 4. Publish every manifest version the registry does not already have.
manifest_rows() {
  python3 - <<'PY'
import pathlib
import subprocess
import tomllib

manifests = subprocess.check_output(
    ["git", "ls-files", "*hew.toml"], text=True
).splitlines()
if not manifests:
    raise SystemExit("no package manifests found")
for manifest in sorted(manifests):
    data = tomllib.loads(pathlib.Path(manifest).read_text())
    package = data["package"]
    print(f"{manifest}\t{package['name']}\t{package['version']}")
PY
}

manifest_rows > "$manifests_file"

# The name grammar is defined once, in the contract script; this asks it rather
# than re-implementing a weaker check.
while IFS=$'\t' read -r manifest name version; do
  python3 scripts/verify_package_contract.py --check-name "$name" \
    || fail "$manifest: package name is not publishable: $name"
done < "$manifests_file"

while IFS=$'\t' read -r manifest name version; do
  package_path="${name//./\/}"
  package_url="$REGISTRY_API/packages/$package_path"
  status="$(
    curl --silent --show-error \
      --output "$response" \
      --write-out '%{http_code}' \
      "$package_url"
  )"

  case "$status" in
    200)
      jq --exit-status '.versions | type == "array"' "$response" >/dev/null
      if jq --exit-status --arg version "$version" \
        'any(.versions[]; .vers == $version)' "$response" >/dev/null; then
        echo "Skipping $name@$version: already published"
        continue
      fi
      ;;
    404)
      ;;
    *)
      cat "$response" >&2
      fail "Registry query for $name failed with HTTP $status"
      ;;
  esac

  echo "Publishing $name@$version from $manifest"
  (
    cd "$(dirname "$manifest")"
    "$hew_bin" publish
  ) | tee "$publish_log"

  # The checksum hew computed over the archive it just packed.
  local_cksum="$(sed -n 's/^Checksum: //p' "$publish_log" | tail -1)"
  case "$local_cksum" in
    sha256:????????????????????????????????????????????????????????????????) ;;
    *) fail "$name@$version: hew publish did not report a sha256 checksum" ;;
  esac

  # Read back the stored record and check every field that identifies what was
  # published: a 200 alone would accept another publisher's version.
  curl --fail-with-body --silent --show-error --output "$response" "$package_url/$version"
  jq --exit-status \
    --arg name "$package_path" \
    --arg version "$version" \
    --arg cksum "$local_cksum" \
    --arg key_fp "$key_fingerprint" \
    '.version
     | (.name == $name)
       and (.vers == $version)
       and (.cksum == $cksum)
       and (.key_fp == $key_fp)
       and (.sig | type == "string" and startswith("ed25519:") and (length > 9))' \
    "$response" >/dev/null \
    || {
      jq '.version | {name, vers, cksum, key_fp, sig}' "$response" >&2 || cat "$response" >&2
      fail "$name@$version: registry record does not match what was published (expected name $package_path, checksum $local_cksum, key $key_fingerprint)"
    }

  # And check the stored bytes against that same checksum, so the record is
  # verified against the archive the registry will actually serve.
  curl --fail-with-body --silent --show-error --location \
    --output "$archive" "$package_url/$version/download"
  stored_cksum="sha256:$(sha256_hex "$archive")"
  [ "$stored_cksum" = "$local_cksum" ] \
    || fail "$name@$version: stored archive hashes to $stored_cksum, expected $local_cksum"

  echo "Verified $name@$version: $local_cksum"
done < "$manifests_file"

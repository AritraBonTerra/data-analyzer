#!/usr/bin/env bash
# Signs a release DMG with the update key, checks the signature against the
# public key the app ships with, and prints latest.json for the release.
#
#   scripts/release-manifest.sh <dmg> <version> <private key> <public key> > latest.json
#
# The public key is normally packaging/update-key.pub. Verifying against it means
# a wrong signing key fails the release instead of shipping updates no app accepts.
set -euo pipefail

dmg=${1:?usage: release-manifest.sh <dmg> <version> <private key> <public key>}
version=${2:?missing version}
key=${3:?missing private key}
pub=${4:?missing public key}

namespace=data-analyzer-update
base=https://github.com/AritraBonTerra/data-analyzer/releases

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Sign the DMG's bytes from stdin; the signature does not depend on the file name.
ssh-keygen -q -Y sign -f "$key" -n "$namespace" <"$dmg" >"$work/sig"

# Verify exactly as the app does: one allowed signer named data-analyzer.
echo "data-analyzer $(cut -d' ' -f1,2 "$pub")" >"$work/allowed_signers"
ssh-keygen -Y verify -f "$work/allowed_signers" -I data-analyzer -n "$namespace" \
  -s "$work/sig" <"$dmg" >&2

jq -n \
  --arg version "$version" \
  --arg dmg "$base/download/v$version/$(basename "$dmg")" \
  --argjson size "$(stat -f %z "$dmg")" \
  --arg sha256 "$(shasum -a 256 "$dmg" | cut -d' ' -f1)" \
  --rawfile signature "$work/sig" \
  --arg notes "$base/tag/v$version" \
  '{version: $version, dmg: $dmg, size: $size, sha256: $sha256, signature: $signature, notes: $notes}'

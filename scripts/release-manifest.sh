#!/usr/bin/env bash
# Signs a release asset with the update key, checks the signature against the
# public key the app ships with, and prints the release's manifest for it:
# latest.json for the macOS DMG, latest-linux-<arch>.json for an AppImage.
#
#   scripts/release-manifest.sh <asset> <version> <private key> <public key> > <manifest>
#
# The public key is normally packaging/update-key.pub. Verifying against it means
# a wrong signing key fails the release instead of shipping updates no app accepts.
# RELEASE_BASE replaces the GitHub releases URL (for a local test server).
set -euo pipefail

asset=${1:?usage: release-manifest.sh <asset> <version> <private key> <public key>}
version=${2:?missing version}
key=${3:?missing private key}
pub=${4:?missing public key}

namespace=data-analyzer-update
base=${RELEASE_BASE:-https://github.com/AritraBonTerra/data-analyzer/releases}

# The manifest key names the asset's kind; each platform's app reads its own.
case $asset in
  *.dmg) kind=dmg ;;
  *.AppImage) kind=appimage ;;
  *) echo "Don't know what kind of asset $asset is." >&2; exit 1 ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Sign the asset's bytes from stdin; the signature does not depend on the file name.
ssh-keygen -q -Y sign -f "$key" -n "$namespace" <"$asset" >"$work/sig"

# Verify exactly as the app does: one allowed signer named data-analyzer.
echo "data-analyzer $(cut -d' ' -f1,2 "$pub")" >"$work/allowed_signers"
ssh-keygen -Y verify -f "$work/allowed_signers" -I data-analyzer -n "$namespace" \
  -s "$work/sig" <"$asset" >&2

# sha256sum on Linux, shasum on macOS.
if command -v sha256sum >/dev/null; then
  sha256=$(sha256sum "$asset" | cut -d' ' -f1)
else
  sha256=$(shasum -a 256 "$asset" | cut -d' ' -f1)
fi

jq -n \
  --arg version "$version" \
  --arg kind "$kind" \
  --arg url "$base/download/v$version/$(basename "$asset")" \
  --argjson size "$(wc -c <"$asset" | tr -d ' ')" \
  --arg sha256 "$sha256" \
  --rawfile signature "$work/sig" \
  --arg notes "$base/tag/v$version" \
  '{version: $version, ($kind): $url, size: $size, sha256: $sha256, signature: $signature, notes: $notes}'

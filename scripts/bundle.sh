#!/usr/bin/env bash
# Builds "dist/Data Analyzer.app" (Apple Silicon) and a DMG, signed with your
# Developer ID. With --notarize, also notarizes and staples both.
#
#   scripts/bundle.sh               # build + sign
#   scripts/bundle.sh --notarize    # build + sign + notarize + staple
#
# Env:
#   SIGN_IDENTITY   codesign identity (default: first "Developer ID Application");
#                   "-" signs ad hoc for local testing (no notarization possible)
#   NOTARY_PROFILE  notarytool keychain profile (default: data-analyzer). Create once:
#                   xcrun notarytool store-credentials data-analyzer \
#                     --apple-id you@example.com --team-id TEAMID --password <app-specific>
set -euo pipefail
cd "$(dirname "$0")/.."
# macOS's bash 3.2 exits on a failed `source` even with `|| true`, so test first.
[[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"

notarize=false
[[ "${1:-}" == "--notarize" ]] && notarize=true

version=$(grep -m1 '^version' crates/app/Cargo.toml | cut -d'"' -f2)
build=$(git rev-list --count HEAD)
identity=${SIGN_IDENTITY:-$(security find-identity -v -p codesigning |
  sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' | head -1)}
profile=${NOTARY_PROFILE:-data-analyzer}
app="dist/Data Analyzer.app"
dmg="dist/Data-Analyzer-$version.dmg"

if [[ -z "$identity" ]]; then
  echo "No Developer ID Application identity found; set SIGN_IDENTITY." >&2
  exit 1
fi

# Apple's timestamp server only applies to real identities.
timestamp=--timestamp
[[ "$identity" == "-" ]] && timestamp=--timestamp=none

echo "==> Building release binary"
cargo build --release -p data-analyzer

echo "==> Assembling $app"
rm -rf "$app" "$dmg"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/data-analyzer "$app/Contents/MacOS/"
sed -e "s/__VERSION__/$version/" -e "s/__BUILD__/$build/" packaging/Info.plist >"$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null

echo "==> Rendering icon"
work=$(mktemp -d)
iconset="$work/AppIcon.iconset"
mkdir -p "$iconset"
sips -s format png assets/AppIcon.svg --out "$work/1024.png" >/dev/null
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$work/1024.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$work/1024.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$work"

echo "==> Signing with $identity"
codesign --force --options runtime "$timestamp" --sign "$identity" "$app"
codesign --verify --strict --verbose=2 "$app"

notarize_and_staple() {
  local target=$1 upload=$1
  if [[ -d "$target" ]]; then
    upload="$work.zip"
    ditto -c -k --keepParent "$target" "$upload"
  fi
  xcrun notarytool submit "$upload" --keychain-profile "$profile" --wait
  xcrun stapler staple "$target"
  [[ "$upload" != "$target" ]] && rm -f "$upload"
  return 0
}

if $notarize; then
  echo "==> Notarizing app"
  notarize_and_staple "$app"
fi

echo "==> Building $dmg"
staging=$(mktemp -d)
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
hdiutil create -quiet -volname "Data Analyzer" -srcfolder "$staging" -ov -format UDZO "$dmg"
rm -rf "$staging"
codesign --force "$timestamp" --sign "$identity" "$dmg"

if $notarize; then
  echo "==> Notarizing DMG"
  notarize_and_staple "$dmg"
  spctl --assess --type open --context context:primary-signature -v "$dmg"
fi

echo "==> Done: $app, $dmg"

#!/usr/bin/env bash
# Builds dist/Data-Analyzer-<arch>.AppImage on Linux (x86_64 or aarch64): the
# release binary with its desktop entry and icon, packed into a squashfs image
# behind the AppImage runtime. That runtime links FUSE 3 statically, so users
# don't need libfuse2. The binary links only libraries every desktop has
# (glibc, libstdc++, xcb, xkbcommon), so none are bundled; build on the oldest
# glibc you want to support (CI uses Ubuntu 22.04, glibc 2.35).
#
#   scripts/appimage.sh
#
# Needs: cargo, mksquashfs (squashfs-tools), curl. Uses desktop-file-validate
# when present.
#
# Env:
#   APPIMAGE_RUNTIME  a type2 runtime file to use instead of downloading the pinned one
set -euo pipefail
cd "$(dirname "$0")/.."

arch=$(uname -m)
id=dev.ari3.DataAnalyzer
out="dist/Data-Analyzer-$arch.AppImage"

# github.com/AppImage/type2-runtime, release 20251108.
runtime_url=https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-$arch
case $arch in
  x86_64) runtime_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d ;;
  aarch64) runtime_sha256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444 ;;
  *) echo "No AppImage runtime for $arch." >&2; exit 1 ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "==> Building release binary"
cargo build --release -p data-analyzer

echo "==> Assembling AppDir"
appdir="$work/AppDir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" \
  "$appdir/usr/share/icons/hicolor/scalable/apps"
install -m 755 "${CARGO_TARGET_DIR:-target}/release/data-analyzer" "$appdir/usr/bin/"
ln -s usr/bin/data-analyzer "$appdir/AppRun"
cp "packaging/linux/$id.desktop" "$appdir/"
cp "packaging/linux/$id.desktop" "$appdir/usr/share/applications/"
cp assets/AppIcon.svg "$appdir/$id.svg"
cp assets/AppIcon.svg "$appdir/usr/share/icons/hicolor/scalable/apps/$id.svg"
ln -s "$id.svg" "$appdir/.DirIcon"
if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$appdir/$id.desktop"
fi

echo "==> Fetching the AppImage runtime"
runtime=${APPIMAGE_RUNTIME:-$work/runtime}
if [[ -z "${APPIMAGE_RUNTIME:-}" ]]; then
  curl -fsSL "$runtime_url" -o "$runtime"
  echo "$runtime_sha256  $runtime" | sha256sum -c --quiet
fi

echo "==> Packing $out"
mkdir -p dist
rm -f "$out"
mksquashfs "$appdir" "$work/image.squashfs" -quiet -root-owned -noappend \
  -comp zstd -Xcompression-level 19 -b 1M
cat "$runtime" "$work/image.squashfs" >"$out"
chmod 755 "$out"

echo "==> Done: $out ($(du -h "$out" | cut -f1))"

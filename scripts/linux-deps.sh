#!/usr/bin/env bash
# Installs what building on Linux needs beyond Rust (Debian/Ubuntu): the X11,
# Wayland and font libraries GPUI links, and squashfs-tools for the AppImage.
# CI runs it; run it once on a new machine.
set -euo pipefail
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential pkg-config \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev libx11-xcb-dev \
  libfontconfig1-dev libfreetype6-dev \
  squashfs-tools desktop-file-utils

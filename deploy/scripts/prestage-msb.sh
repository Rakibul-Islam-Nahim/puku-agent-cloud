#!/usr/bin/env bash
# Pre-stage the microsandbox toolchain (msb binary + libkrunfw) into
# /opt/puku/msb so production workers never download at build or first-run
# time. Pin the version here; bump deliberately.
set -euo pipefail

MSB_VERSION="${MSB_VERSION:-0.6.9}"
DEST="${DEST:-/opt/puku/msb}"

arch="$(uname -m)"
case "$arch" in
  x86_64) msb_arch="x86_64" ;;
  aarch64|arm64) msb_arch="aarch64" ;;
  *) echo "unsupported arch: $arch" >&2; exit 1 ;;
esac

mkdir -p "$DEST/bin" "$DEST/lib"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Release tag is `v<version>`; the bundle asset carries no version in its
# name. Contents are flat: `msb` and `libkrunfw.so.<x.y.z>`.
url="https://github.com/superradcompany/microsandbox/releases/download/v${MSB_VERSION}/microsandbox-linux-${msb_arch}.tar.gz"
echo "fetching $url"
curl -fsSL "$url" -o "$tmp/msb.tar.gz"
tar -xzf "$tmp/msb.tar.gz" -C "$tmp"

install -m 0755 "$tmp/msb" "$DEST/bin/msb"
find "$tmp" -maxdepth 1 -name 'libkrunfw.so.*' -type f -exec install -m 0755 {} "$DEST/lib/" \;

# The SDK looks for the versioned file (libkrunfw.so.<x.y.z>) and dlopens via
# the soname; preflight.sh checks the bare .so. Build both links, relative so
# the tree stays relocatable: libkrunfw.so -> libkrunfw.so.5 -> .so.5.6.1
real_fw="$(basename "$(ls "$DEST"/lib/libkrunfw.so.*.* | sort -V | tail -1)")"
abi="${LIBKRUNFW_ABI:-5}"
ln -sf "$real_fw" "$DEST/lib/libkrunfw.so.${abi}"
ln -sf "libkrunfw.so.${abi}" "$DEST/lib/libkrunfw.so"

echo "staged msb $("$DEST/bin/msb" --version 2>/dev/null || echo "$MSB_VERSION") into $DEST"

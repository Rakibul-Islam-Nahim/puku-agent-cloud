#!/usr/bin/env bash
# Pre-stage the Cloud Hypervisor toolchain into /opt/puku/ch so a worker with
# PUKU_ENGINE_CLOUD_HYPERVISOR=true never downloads anything at boot:
#
#   /opt/puku/ch/bin/cloud-hypervisor   the VMM (static release binary)
#   /opt/puku/ch/bin/virtiofsd          host side of the guest's shared dirs
#   /opt/puku/ch/bin/puku-guestd        guest init, copied into every rootfs
#   /opt/puku/ch/vmlinux                the guest kernel (Image on aarch64)
#
# Pinned; bump deliberately, and check each pin against its upstream release
# page when you do -- a wrong pin fails here, loudly, not at first boot.
#
# Usage (as root, from the repo):
#   ./deploy/scripts/prestage-ch.sh
#   KERNEL_URL=https://.../vmlinux ./deploy/scripts/prestage-ch.sh   # a prebuilt kernel
set -euo pipefail

CH_VERSION="${CH_VERSION:-v53.0}"
KERNEL_REPO="${KERNEL_REPO:-https://github.com/cloud-hypervisor/linux.git}"
KERNEL_REF="${KERNEL_REF:-ch-6.16.9}"
DEST="${DEST:-/opt/puku/ch}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

say() { printf '\n== %s ==\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }

arch="$(uname -m)"
case "$arch" in
  x86_64) ch_asset="cloud-hypervisor-static"; kimage="vmlinux"; karch="x86"; musl="x86_64-unknown-linux-musl" ;;
  aarch64|arm64) ch_asset="cloud-hypervisor-static-aarch64"; kimage="Image"; karch="arm64"; musl="aarch64-unknown-linux-musl" ;;
  *) die "unsupported arch: $arch" ;;
esac

mkdir -p "$DEST/bin"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# ------------------------------------------------------------- the VMM
say "cloud-hypervisor $CH_VERSION"
url="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CH_VERSION}/${ch_asset}"
echo "fetching $url"
curl -fsSL "$url" -o "$tmp/cloud-hypervisor" || die "could not fetch $url (is CH_VERSION right?)"
install -m 0755 "$tmp/cloud-hypervisor" "$DEST/bin/cloud-hypervisor"
"$DEST/bin/cloud-hypervisor" --version

# ----------------------------------------------------------- virtiofsd
# The distro package is the maintained build; Debian 12 and Ubuntu 23.04+
# ship it. It installs outside PATH, so look where packages put it.
say "virtiofsd"
vfs="$(command -v virtiofsd || true)"
for p in /usr/libexec/virtiofsd /usr/lib/qemu/virtiofsd; do
  [ -z "$vfs" ] && [ -x "$p" ] && vfs="$p"
done
if [ -z "$vfs" ] && command -v apt-get >/dev/null; then
  apt-get install -y virtiofsd >/dev/null
  vfs="$(command -v virtiofsd || true)"
  [ -z "$vfs" ] && [ -x /usr/libexec/virtiofsd ] && vfs=/usr/libexec/virtiofsd
fi
[ -n "$vfs" ] || die "virtiofsd not found; install the virtiofsd package or set it up by hand"
ln -sf "$vfs" "$DEST/bin/virtiofsd"
"$DEST/bin/virtiofsd" --version || true

# --------------------------------------------------------------- kernel
# An uncompressed kernel for direct boot. Built from Cloud Hypervisor's own
# tree and config unless a prebuilt one is supplied; either way the options
# puku-guestd depends on are forced on.
say "guest kernel"
if [ -n "${KERNEL_URL:-}" ]; then
  curl -fsSL "$KERNEL_URL" -o "$tmp/$kimage" || die "could not fetch $KERNEL_URL"
else
  command -v make >/dev/null && command -v gcc >/dev/null && command -v flex >/dev/null && command -v bison >/dev/null \
    || die "building the kernel needs build-essential flex bison libelf-dev libssl-dev bc; or pass KERNEL_URL"
  git clone --depth 1 --branch "$KERNEL_REF" "$KERNEL_REPO" "$tmp/linux" || die "no branch $KERNEL_REF in $KERNEL_REPO"
  (
    cd "$tmp/linux"
    make ARCH="$karch" ch_defconfig
    for opt in VIRTIO_FS FUSE_FS VSOCKETS VIRTIO_VSOCKETS OVERLAY_FS EXT4_FS \
               HW_RANDOM_VIRTIO VIRTIO_CONSOLE VIRTIO_BLK VIRTIO_NET DEVTMPFS TMPFS \
               CGROUPS NAMESPACES; do
      ./scripts/config --enable "$opt"
    done
    make ARCH="$karch" olddefconfig
    make ARCH="$karch" -j"$(nproc)" "$kimage"
    cp "$( [ "$karch" = x86 ] && echo vmlinux || echo arch/arm64/boot/Image )" "$tmp/$kimage"
  )
fi
install -m 0644 "$tmp/$kimage" "$DEST/vmlinux"

# ----------------------------------------------------------- puku-guestd
# Static, so it runs in any guest userland: alpine, debian, ubuntu.
say "puku-guestd"
command -v cargo >/dev/null || die "cargo is needed to build puku-guestd"
rustup target add "$musl" >/dev/null
(cd "$ROOT" && cargo build --release -p puku-guestd --target "$musl")
install -m 0755 "$ROOT/target/$musl/release/puku-guestd" "$DEST/bin/puku-guestd"

say "staged into $DEST"
ls -l "$DEST" "$DEST/bin"

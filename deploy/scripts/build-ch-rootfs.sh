#!/usr/bin/env bash
# Turn an OCI image into the boot disk a Cloud Hypervisor VM runs from.
#
# msb pulls images into a store of its own; Cloud Hypervisor boots a disk.
# This writes, for an image reference IMAGE:
#
#   $IMAGES_DIR/<key>/rootfs.ext4         the image's filesystem, plus
#                                         /sbin/puku-guestd as init
#   $IMAGES_DIR/<key>/image-config.json   its ENV/WORKDIR/USER (also inside
#                                         the disk, where puku-guestd reads it)
#   $IMAGES_DIR/<key>/image.json          which image this is: ref, id and
#                                         registry digests. workerd reports the
#                                         staged list, and controld only places
#                                         a machine where its image is
#
# <key> is the reference made filesystem-safe -- the same rule as
# `vm::ch::image::key` in workerd, so the exact string controld dispatches
# (PUKU_AGENT_IMAGE, a machine's `image`) finds the disk. The swap is atomic:
# VMs already running keep the old disk open, new ones get the new one.
#
# Run it wherever `msb load` runs for the same image (deploy-guest-image.sh,
# upgrade-box.sh), so both engines always boot the same bits.
#
# Usage (as root -- ownership inside the image must survive):
#   ./deploy/scripts/build-ch-rootfs.sh puku-agent:latest
#   ./deploy/scripts/build-ch-rootfs.sh pukubot-computer:latest
set -euo pipefail

IMAGE="${1:?usage: build-ch-rootfs.sh <image-ref>}"
IMAGES_DIR="${IMAGES_DIR:-/var/lib/puku/images}"
GUESTD="${GUESTD:-/opt/puku/ch/bin/puku-guestd}"

say() { printf '\n== %s ==\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }
key_for() { printf '%s' "$1" | sed 's/[^A-Za-z0-9._-]/_/g'; }

[ "$(id -u)" = 0 ] || die "run as root: file ownership inside the image must be preserved"
command -v docker >/dev/null || die "docker is not on PATH"
command -v mkfs.ext4 >/dev/null || die "mkfs.ext4 is not on PATH (e2fsprogs)"
[ -x "$GUESTD" ] || die "puku-guestd is not staged at $GUESTD (run prestage-ch.sh)"

KEY="$(key_for "$IMAGE")"
OUT="$IMAGES_DIR/$KEY"
tmp="$(mktemp -d "$IMAGES_DIR/.build-XXXXXX" 2>/dev/null || { mkdir -p "$IMAGES_DIR"; mktemp -d "$IMAGES_DIR/.build-XXXXXX"; })"
trap 'rm -rf "$tmp"' EXIT

say "exporting $IMAGE"
docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull "$IMAGE"
cid="$(docker create "$IMAGE" /bin/true)"
mkdir -p "$tmp/root"
docker export "$cid" | tar -x -C "$tmp/root" --numeric-owner
docker rm "$cid" >/dev/null
# `docker export` keeps the filesystem and drops the config; puku-guestd
# needs the ENV (PATH above all) and WORKDIR back.
docker image inspect --format '{{json .Config}}' "$IMAGE" > "$tmp/image-config.json"
# Which bits these are, for the worker's heartbeat (and, later, for matching
# a snapshot to the exact base it was taken on).
printf '{"ref":"%s","id":%s,"repo_digests":%s,"built_at":"%s"}\n' \
  "$IMAGE" \
  "$(docker image inspect --format '{{json .Id}}' "$IMAGE")" \
  "$(docker image inspect --format '{{json .RepoDigests}}' "$IMAGE")" \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$tmp/image.json"

say "adding puku-guestd"
install -D -m 0755 "$GUESTD" "$tmp/root/sbin/puku-guestd"
install -D -m 0644 "$tmp/image-config.json" "$tmp/root/etc/puku/image-config.json"
# Mount points puku-guestd uses on a read-only root, including /mnt, the
# scratch space it builds the overlay in.
for d in proc sys dev run tmp mnt; do mkdir -p "$tmp/root/$d"; done
# An empty resolv.conf is overwritten at boot with the VM's gateway.
: > "$tmp/root/etc/resolv.conf"

say "building the disk"
used_mb="$(du -sm "$tmp/root" | cut -f1)"
size_mb=$(( used_mb * 13 / 10 + 512 ))
[ "$size_mb" -lt 1024 ] && size_mb=1024
truncate -s "${size_mb}M" "$tmp/rootfs.ext4"
mkfs.ext4 -q -F -L puku-root -d "$tmp/root" "$tmp/rootfs.ext4"
echo "  ${used_mb} MiB of files in a ${size_mb} MiB disk"

say "installing as $OUT"
mkdir -p "$tmp/out"
mv "$tmp/rootfs.ext4" "$tmp/image-config.json" "$tmp/image.json" "$tmp/out/"
if [ -d "$OUT" ]; then
  mv "$OUT" "$OUT.old.$$"
fi
mv "$tmp/out" "$OUT"
rm -rf "$OUT.old.$$"
ls -l "$OUT"

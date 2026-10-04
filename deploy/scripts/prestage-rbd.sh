#!/usr/bin/env bash
# Pre-stage RBD (RADOS Block Device) kernel modules and tooling onto a
# production worker so first-use doesn't shell out to download.
#
# Per docs/RELIABILITY-REBUILD.md §5.3: RBD is the primary fence; the
# blocklist command must work even when the controld is unreachable
# (otherwise a fenced host that boots the new kernel module loads
# nothing and the fence was a no-op).
#
# The script is a no-op when /sys/module/rbd is already present, which is
# the case on every Ubuntu/Debian cloud image we ship; it exists so a
# custom kernel can be pre-staged and so the install is documented.
set -euo pipefail

DEST="${DEST:-/opt/puku/rbd}"
KVERSION="${KVERSION:-$(uname -r)}"

if [ -d /sys/module/rbd ]; then
  echo "rbd already loaded on kernel $KVERSION; nothing to stage" >&2
  exit 0
fi

mkdir -p "$DEST"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# rbd + libceph modules live in the kernel tree; if a custom kernel lacks
# them, fall back to the distribution package.
if [ -d "/lib/modules/$KVERSION/kernel/drivers/block/rbd.ko" ]; then
  install -m 0644 "/lib/modules/$KVERSION/kernel/drivers/block/rbd.ko" \
    "$DEST/rbd.ko"
  install -m 0644 "/lib/modules/$KVERSION/kernel/libceph.ko" \
    "$DEST/libceph.ko" 2>/dev/null || true
  echo "staged rbd.ko from running kernel $KVERSION" >&2
else
  echo "rbd kernel module not present in $KVERSION; install linux-modules-extra-$KVERSION" >&2
  exit 1
fi

# rbd CLI tool. The script does NOT install rbd here; that's a ceph-common
# package and the operator's choice. We only verify it is on PATH.
if ! command -v rbd >/dev/null 2>&1; then
  echo "warning: 'rbd' not on PATH; install ceph-common to enable fencing" >&2
fi

echo "rbd pre-stage complete: $DEST" >&2

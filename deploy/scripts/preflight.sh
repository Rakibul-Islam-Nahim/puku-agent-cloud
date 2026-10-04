#!/usr/bin/env bash
# Worker preflight: refuse to start puku-workerd on a host that can't run
# microVMs. Engine-aware: the checks follow PUKU_ENGINE_LIBKRUN and
# PUKU_ENGINE_CLOUD_HYPERVISOR from the unit's environment.
#
# libkrun problems are fatal when libkrun is on: it is every worker's
# default engine. Cloud Hypervisor problems are warnings: workerd runs the
# same checks itself and simply does not advertise the engine, so a box
# whose CH toolchain is incomplete keeps serving libkrun sessions.
set -euo pipefail

fail() { echo "preflight FAILED: $*" >&2; exit 1; }
warn() { echo "preflight WARNING: $*" >&2; }
truthy() { case "$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')" in 1|true|yes|y|on) return 0 ;; *) return 1 ;; esac; }

LIBKRUN="${PUKU_ENGINE_LIBKRUN:-true}"
CH="${PUKU_ENGINE_CLOUD_HYPERVISOR:-false}"

[ "$(uname -s)" = "Linux" ] || fail "workers must be Linux (KVM)"
[ -e /dev/kvm ] || fail "/dev/kvm missing — need bare metal or nested virt"
[ -r /dev/kvm ] && [ -w /dev/kvm ] || fail "/dev/kvm not read/writable by $(id -un)"

if truthy "$LIBKRUN"; then
  MSB="${MSB_PATH:-/opt/puku/msb/bin/msb}"
  [ -x "$MSB" ] || fail "msb binary not staged at $MSB (run prestage-msb.sh)"
  FW="${MSB_LIBKRUNFW_PATH:-/opt/puku/msb/lib/libkrunfw.so}"
  [ -e "$FW" ] || fail "libkrunfw not staged at $FW (run prestage-msb.sh)"
  "$MSB" doctor || fail "msb doctor reported problems"
  echo "preflight OK (libkrun): $("$MSB" --version)"
fi

if truthy "$CH"; then
  ok=1
  for dev in /dev/vhost-vsock /dev/net/tun; do
    [ -e "$dev" ] || { warn "$dev missing (modprobe vhost_vsock tun)"; ok=0; }
  done
  for f in "${PUKU_CH_BIN:-/opt/puku/ch/bin/cloud-hypervisor}" \
           "${PUKU_CH_VIRTIOFSD:-/opt/puku/ch/bin/virtiofsd}" \
           "${PUKU_CH_KERNEL:-/opt/puku/ch/vmlinux}" \
           /opt/puku/ch/bin/puku-guestd; do
    [ -e "$f" ] || { warn "$f missing (run prestage-ch.sh)"; ok=0; }
  done
  for tool in nft ip mkfs.ext4; do
    command -v "$tool" >/dev/null || { warn "$tool not on PATH"; ok=0; }
  done
  if [ "$ok" = 1 ]; then
    echo "preflight OK (cloud_hypervisor): $("${PUKU_CH_BIN:-/opt/puku/ch/bin/cloud-hypervisor}" --version)"
  else
    warn "cloud_hypervisor will not be advertised until the above is fixed"
  fi
fi

truthy "$LIBKRUN" || truthy "$CH" || fail "no engine enabled (PUKU_ENGINE_LIBKRUN / PUKU_ENGINE_CLOUD_HYPERVISOR)"

# Reliability-rebuild preflight (R6). Each check is a soft warning rather
# than a hard fail: a single-tenant dev box that never fences still works
# without RBD / BMC, and refusing to start would be worse than degraded
# recovery. The unit file picks up the warnings via journald.
if [ -e /sys/module/rbd ]; then
  echo "preflight OK (rbd): kernel module loaded"
else
  warn "rbd kernel module missing; fencing will be a no-op until linux-modules-extra is installed (run prestage-rbd.sh)"
fi
if command -v rbd >/dev/null 2>&1; then
  echo "preflight OK (rbd-cli): $(rbd --version | head -1)"
else
  warn "rbd CLI not on PATH; install ceph-common"
fi
if [ -d /sys/class/ipmi_msghandler ] || [ -d /sys/class/redfish ]; then
  echo "preflight OK (bmc): ipmi/redfish interface present"
else
  warn "no IPMI/Redfish interface; fence will rely on Ceph blocklist alone"
fi

#!/usr/bin/env bash
# One-shot provisioning for a fresh Ubuntu 24.04 bare-metal worker+control box.
# Run as root. Idempotent-ish; review before running on a shared host.
set -euo pipefail

apt-get update
apt-get install -y --no-install-recommends \
  postgresql-17 docker.io curl ca-certificates git jq \
  build-essential pkg-config libcap-ng-dev
# libcap-ng-dev: the capng crate (pulled in by msb_krun_devices, i.e. the
# workerd side of microsandbox) emits a bare `-lcap-ng` with no pkg-config
# probe, so the dev symlink must exist or puku-workerd fails at link time.

# Users and directories.
id -u puku >/dev/null 2>&1 || useradd -r -m -d /var/lib/puku-controld puku
mkdir -p /opt/puku/bin /var/lib/puku/sessions /etc/puku
[ -f /etc/puku/worker-token ] || head -c 32 /dev/urandom | base64 > /etc/puku/worker-token
chmod 600 /etc/puku/worker-token

# Database.
sudo -u postgres psql -tc "SELECT 1 FROM pg_roles WHERE rolname='puku'" | grep -q 1 || \
  sudo -u postgres psql -c "CREATE ROLE puku LOGIN PASSWORD 'puku'"
sudo -u postgres psql -tc "SELECT 1 FROM pg_database WHERE datname='puku_cloud'" | grep -q 1 || \
  sudo -u postgres createdb -O puku puku_cloud

# microsandbox toolchain.
"$(dirname "$0")/prestage-msb.sh"

# Binaries are deployed separately (scp or CI artifact) into /opt/puku/bin,
# then: cp deploy/systemd/*.service /etc/systemd/system/ && systemctl daemon-reload
#       systemctl enable --now puku-controld puku-workerd
echo "provisioned. Deploy binaries to /opt/puku/bin and enable the units."

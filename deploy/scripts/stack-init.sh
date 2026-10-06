#!/usr/bin/env bash
# Make (or finish) .env for docker-compose.yml, from .env.example:
#
#   ./deploy/scripts/stack-init.sh
#
# Fills every <generated> value with a fresh secret and every
# <this host's LAN IP> with this host's address, never touching a value that is
# already there, then lists what is still yours to fill. Safe to re-run.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ENV=$ROOT/.env

if [ ! -f "$ENV" ]; then
  cp "$ROOT/.env.example" "$ENV"
  echo "made $ENV from .env.example"
fi
chmod 600 "$ENV"

hex() {
  if command -v openssl >/dev/null 2>&1; then openssl rand -hex "$1"
  else head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; fi
}

# Each <generated> gets its own value. Hex only: POSTGRES_PASSWORD sits in a URL.
n=0
while grep -q '<generated>' "$ENV"; do
  line=$(grep -n '<generated>' "$ENV" | head -1 | cut -d: -f1)
  sed -i "${line}s/<generated>/$(hex 32)/" "$ENV"
  n=$((n + 1))
done
[ "$n" = 0 ] || echo "generated $n secret(s)"

if grep -q "<this host's LAN IP>" "$ENV"; then
  ip=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for (i = 1; i < NF; i++) if ($i == "src") print $(i + 1)}' | head -1 || true)
  if [ -n "$ip" ]; then
    sed -i "s/<this host's LAN IP>/$ip/g" "$ENV"
    echo "this host's LAN IP: $ip (MINIO_BIND, PUKU_R2_ENDPOINT); change it in .env if that is the wrong network"
  fi
fi

left=$(grep -nE '<[^>]+>' "$ENV" | grep -v '^[0-9]*:#' || true)
if [ -n "$left" ]; then
  echo
  echo "Still to fill in $ENV:"
  printf '%s\n' "$left" | sed 's/^/  line /'
  echo
  echo "Then: docker compose up -d"
else
  echo "$ENV is complete. Next: docker compose up -d"
fi

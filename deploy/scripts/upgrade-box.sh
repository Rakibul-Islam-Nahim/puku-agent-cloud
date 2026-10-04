#!/usr/bin/env bash
# Bring a deployed box up to the current checkout, in the right order.
#
# Ordering is the whole point of this script. Two steps are easy to forget and
# both fail confusingly:
#
#   * msb keeps its own image store and never consults the Docker daemon, so a
#     rebuilt guest image is invisible until it is `msb load`ed. Skip it and
#     sessions fail at boot with "Not authorized: index.docker.io/...", which
#     reads like a registry problem and is not one.
#   * The SDK runner needs puku-agent-sdk staged into the image before the
#     build, not after.
#
# Migrations are NOT a step: they are embedded in the controld binary and run
# at startup, so recreating the container applies them.
#
# Safe to re-run. Nothing here deletes data.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SKILLS_ROOT_REPO="$(cd "$ROOT/.." && pwd)/puku-skills-service"
TAG="${TAG:-0.1.0}"
SKIP_PULL="${SKIP_PULL:-0}"

say() { printf '\n== %s ==\n' "$*"; }

# ---------------------------------------------------------------------- pull
if [ "$SKIP_PULL" != "1" ]; then
  say "pulling"
  git -C "$ROOT" pull --ff-only
  [ -d "$SKILLS_ROOT_REPO/.git" ] && git -C "$SKILLS_ROOT_REPO" pull --ff-only || true
fi

# --------------------------------------------------------------- stage & build
# The SDK is committed under images/puku-agent/vendor (see its README), so a
# fresh clone can build a guest image with no second checkout and no network.
# Re-stage only when asked, e.g. VENDOR_SDK=1 to pick up a local SDK edit.
if [ "${VENDOR_SDK:-0}" = "1" ]; then
  say "re-staging puku-agent-sdk from a checkout"
  "$ROOT/deploy/scripts/vendor-sdk.sh"
else
  say "puku-agent-sdk $(cat "$ROOT/images/puku-agent/vendor/sdk/VERSION" 2>/dev/null || echo '?') (vendored)"
fi

say "building guest images"
docker build -t "puku-agent:$TAG" "$ROOT/images/puku-agent"
docker build -f "$ROOT/images/puku-agent-office/Dockerfile" \
  --build-arg "BASE=puku-agent:$TAG" -t "puku-agent-office:$TAG" "$ROOT"

say "loading guest images into msb"
# msb has its own store; a docker build alone leaves it pulling from the
# registry. Re-run this every time an image is rebuilt or the guest silently
# keeps running the old one.
export MSB_HOME="${MSB_HOME:-/opt/puku/msb}"
export PATH="${MSB_HOME}/bin:$PATH"
for img in "puku-agent:$TAG" "puku-agent-office:$TAG"; do
  echo "  $img"
  # Remove first: `msb load` onto a tag that already exists reports
  # "✓ Loaded" and keeps the old image. Every guest-side change -- a runner
  # fix, a new flag -- would build, load, report success, and never reach a
  # VM. The tag stays constant across releases here, so this is the norm
  # rather than an edge case.
  msb image rm "$img" >/dev/null 2>&1 || true
  docker save "$img" | msb load -t "$img"
done
msb image list

say "building the control plane image"
docker build -t "poridhi/puku-controld:$TAG" "$ROOT"
if [ -d "$SKILLS_ROOT_REPO" ]; then
  docker build -t "poridhi/puku-skills-service:$TAG" "$SKILLS_ROOT_REPO"
fi

# ------------------------------------------------------------------- restart
say "restarting services"
# Skills first: controld resolves packs against it at dispatch.
if [ -f "$SKILLS_ROOT_REPO/deploy/bm/docker-compose.yml" ]; then
  (cd "$SKILLS_ROOT_REPO/deploy/bm" && docker compose up -d)
fi
# Recreating controld is what applies any new migrations.
(cd "$ROOT/deploy/bm" && docker compose up -d --force-recreate controld)

say "rebuilding and restarting the worker"
# rustup puts cargo in ~/.cargo/bin and wires it up from ~/.bashrc, which a
# non-interactive script never sources -- and `sudo` drops the caller's PATH
# on top of that. So cargo is on the PATH when you type it and missing when
# this runs, which is a confusing way to lose the last step of an upgrade.
if ! command -v cargo >/dev/null 2>&1; then
  for c in "$HOME/.cargo/env" /root/.cargo/env "${SUDO_USER:+/home/$SUDO_USER/.cargo/env}"; do
    [ -n "$c" ] && [ -f "$c" ] && . "$c" && break
  done
fi
command -v cargo >/dev/null 2>&1 || {
  echo "cargo not found. Install rustup, or point CARGO at it:" >&2
  echo "  sudo env PATH=\"\$HOME/.cargo/bin:\$PATH\" ./deploy/scripts/upgrade-box.sh" >&2
  exit 1
}
(cd "$ROOT" && cargo build --release -p puku-workerd)
install -m755 "$ROOT/target/release/puku-workerd" /opt/puku/bin/puku-workerd

# The unit file is NOT synced: the deployed copy carries per-host edits
# (agent image, egress policy, msb paths) that the template does not, so
# overwriting it would silently undo them. But the reverse is just as
# silent -- a new Environment= or EnvironmentFile= line in the repo never
# reaches the box, and the feature simply does not switch on. That is how
# Sentry stayed off on the worker while the DSN sat in /etc/puku ready to
# be read. So: report the drift and let a human apply it.
UNIT=/etc/systemd/system/puku-workerd.service
TEMPLATE="$ROOT/deploy/systemd/puku-workerd.service"
if [ -f "$UNIT" ] && [ -f "$TEMPLATE" ]; then
  missing=$(grep -E '^(Environment|EnvironmentFile)=' "$TEMPLATE" \
    | grep -vxF -f <(grep -E '^(Environment|EnvironmentFile)=' "$UNIT") || true)
  if [ -n "$missing" ]; then
    say "the deployed unit is missing settings the template has"
    echo "$missing" | sed 's/^/    /'
    echo "    add them to $UNIT, then: systemctl daemon-reload && systemctl restart puku-workerd"
  fi
fi

systemctl restart puku-workerd

# -------------------------------------------------------------------- verify
say "health"
sleep 3
curl -fsS "http://127.0.0.1:7770/health?deep=1" | ${JQ:-jq} . || true
echo
echo "worker:"; systemctl is-active puku-workerd || true

cat <<'NEXT'

Next: the SDK runner is built but NOT the default. To run the gate on one
worker without touching the rest of the fleet:

  sudo systemctl edit puku-workerd     # add:
    [Service]
    Environment="PUKU_RUNNER_CMD=exec node /opt/puku/runner.mjs"

  The quotes matter: systemd Environment= is a space-separated list of
  assignments, so unquoted it sets PUKU_RUNNER_CMD=exec and silently drops
  the rest, and the runner never starts.

  sudo systemctl restart puku-workerd
  ./skills/deployment-test/scripts/sdk-gate.sh

full-system-test.sh is a different job and wants a different override: it
asserts exact billing values, so it needs the deterministic CLI and costs
nothing to run.

    Environment="PUKU_RUNNER_CMD=exec env PUKU_CLI_PATH=/opt/puku/fake-puku-cli.sh node /opt/puku/runner.mjs"

Sessions on that binary emit a platform.warning saying so. Revert by deleting
the override and restarting -- left in place, every session returns canned
output and reports completed.
NEXT

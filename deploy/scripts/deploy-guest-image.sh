#!/usr/bin/env bash
# Rebuild the guest image and actually get it into a VM.
#
# Both halves of that sentence are load-bearing, and each has its own trap.
# This script exists because we hit both, in sequence, over four days of
# testing a runner fix that was correct the whole time.
#
#   1. msb keeps its own image store and never consults the Docker daemon.
#      `docker build` alone changes nothing a guest will ever see. Worse,
#      `msb load` onto a tag that already exists prints "✓ Loaded" and keeps
#      the OLD image -- so the obvious fix reports success and does nothing.
#      Hence `msb image rm` first, every time.
#
#   2. CONTROLD chooses the guest image, not workerd. `PUKU_AGENT_IMAGE`
#      exists on both services and only controld's copy reaches a session
#      spec (api/mod.rs -> cfg.agent_image). Editing the workerd unit -- the
#      one that looks like it owns the VM -- changes nothing at all. So the
#      tag is READ from the running controld rather than assumed.
#
# controld is never restarted here: it serves live traffic and it does not
# need to be, because the tag it already asks for is the tag we load into.
#
# Usage:
#   ./deploy/scripts/deploy-guest-image.sh              # read the live tag, rebuild, load
#   SKIP_PULL=1 ./deploy/scripts/deploy-guest-image.sh  # deploy the working tree
#   LIVE_TAG=puku-agent-office:0.1.0 ./deploy/scripts/deploy-guest-image.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BM="$ROOT/deploy/bm"
SKIP_PULL="${SKIP_PULL:-0}"
export MSB_HOME="${MSB_HOME:-/opt/puku/msb}"
export PATH="$MSB_HOME/bin:$PATH"

say() { printf '\n== %s ==\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }

# ------------------------------------------------------------------ preflight
say "preflight"
command -v docker >/dev/null || die "docker is not on PATH"
command -v msb    >/dev/null || die "msb is not on PATH (MSB_HOME=$MSB_HOME)"
echo "  MSB_HOME: $MSB_HOME"

# ------------------------------------------------- which image does controld want
# Read it, do not assume it. The whole class of bug this script prevents is
# "the tag I edited is not the tag that is used".
say "asking controld which image it dispatches"
if [ -n "${LIVE_TAG:-}" ]; then
  echo "  LIVE_TAG override: $LIVE_TAG"
else
  CID="$(docker ps -q --filter 'label=com.docker.compose.service=controld' | head -1)"
  [ -n "$CID" ] || die "no running controld container; pass LIVE_TAG=<image:tag> instead"
  LIVE_TAG="$(docker exec "$CID" env | sed -n 's/^PUKU_AGENT_IMAGE=//p' | head -1)"
  [ -n "$LIVE_TAG" ] || die "controld has no PUKU_AGENT_IMAGE set; pass LIVE_TAG=<image:tag>"
  echo "  controld dispatches: $LIVE_TAG"
fi
case "$LIVE_TAG" in
  puku-agent-office:*) VARIANT=office ;;
  puku-agent:*)        VARIANT=lean ;;
  *) die "unrecognised guest image '$LIVE_TAG' -- this script builds puku-agent and puku-agent-office" ;;
esac
BUILD_TAG="${LIVE_TAG##*:}"

# ---------------------------------------------------------------------- pull
if [ "$SKIP_PULL" != "1" ]; then
  say "pulling"
  git -C "$ROOT" pull --ff-only
fi
echo "  HEAD: $(git -C "$ROOT" rev-parse --short HEAD)"

# --------------------------------------------------------------------- build
# The office image is a DIFFERENT Dockerfile that builds FROM the lean one.
# Building images/puku-agent and tagging the result puku-agent-office silently
# ships a guest with none of the document tooling -- it boots, it works, and
# the missing half only shows up when a session tries to render a deck.
say "building the guest image"
docker build -t "puku-agent:$BUILD_TAG" "$ROOT/images/puku-agent"
if [ "$VARIANT" = office ]; then
  docker build -f "$ROOT/images/puku-agent-office/Dockerfile" \
    --build-arg "BASE=puku-agent:$BUILD_TAG" -t "$LIVE_TAG" "$ROOT"
fi
BUILT="$(docker image inspect -f '{{.Id}}' "$LIVE_TAG")"
echo "  built $LIVE_TAG -> $BUILT"

# ---------------------------------------------------------------------- load
# rm before load. `msb load` onto an existing tag reports success and keeps
# the old image, which is indistinguishable from a working deploy right up
# until a guest runs week-old code.
say "loading into the msb store"
msb image rm "$LIVE_TAG" >/dev/null 2>&1 || true
docker save "$LIVE_TAG" | msb load -t "$LIVE_TAG"

say "verifying msb holds the tag controld asks for"
msb image list
msb image list | grep -q "$(printf '%s' "${LIVE_TAG%%:*}")" \
  || die "$LIVE_TAG is not in the msb store after load"

# ------------------------------------------------------------------- restart
# Safe: workerd owns no long-lived client connections. controld is untouched.
say "restarting puku-workerd"
if systemctl list-unit-files puku-workerd.service >/dev/null 2>&1; then
  systemctl restart puku-workerd
  echo "  restarted"
else
  echo "  no puku-workerd unit on this host; skipped"
fi

# --------------------------------------------------------------------- verify
# Nothing above proves a GUEST runs the new code -- only a session does. The
# runner prints a line before it touches memory; that line is the fingerprint.
say "next: prove it reached a guest"
cat <<EOF
Run one session from your laptop, wait for it to finish, then here:

  B=\$(ls -dt /var/lib/puku/sessions/*/ | head -1); echo "\$B"
  grep -E "pre-memory|write FAILED|memory preamble" "\$B/session/runner.stderr"
  ls -la "\$B/workspace/"

'runner: pre-memory ...' means the new rootfs booted. Its ABSENCE means the
guest is still on an old image no matter what this script printed -- and that
is the only signal that has ever been trustworthy here.

Read the session directory only AFTER the run finishes. Reading it during
[bootstrapping] shows an empty workspace and no runner.stderr, and that has
already been mistaken for a failure twice.
EOF

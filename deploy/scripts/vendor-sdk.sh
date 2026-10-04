#!/usr/bin/env bash
# Update the vendored copy of puku-agent-sdk.
#
# The staged result is COMMITTED (images/puku-agent/vendor -- see its README),
# so this is a maintenance tool, not a build step. Deployments do not run it;
# they use what is in the repo.
#
# Two sources, because the version we need is not the version on npm:
#
#   --local <path>    copy from a checkout (default: ../puku-cli-sdk)
#   --npm <version>   npm pack a published release
#
# Why local is the default: the runner uses `spawnPukuCliProcess` to tap the
# CLI's stderr and read its real exit code, and that option is only honoured
# from 3.0.1 onward. The published 3.0.0 silently ignores it, which would cost
# us /session/runner.stderr and turn a failing session into a completed one.
# Verified against both.
set -euo pipefail

DEST="${DEST:-$(cd "$(dirname "$0")/../.." && pwd)/images/puku-agent/vendor/sdk}"
SRC_MODE=local
SRC_LOCAL="$(cd "$(dirname "$0")/../../.." && pwd)/puku-cli-sdk"
SRC_VERSION=""

while [ $# -gt 0 ]; do
  case "$1" in
    --local) SRC_MODE=local; [ $# -ge 2 ] && [ "${2#--}" = "$2" ] && { SRC_LOCAL="$2"; shift; }; shift ;;
    --npm)   SRC_MODE=npm; SRC_VERSION="${2:?--npm needs a version}"; shift 2 ;;
    *) echo "usage: $0 [--local <path>] [--npm <version>]" >&2; exit 2 ;;
  esac
done

mkdir -p "$DEST"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

if [ "$SRC_MODE" = npm ]; then
  echo "fetching puku-agent-sdk@$SRC_VERSION from npm"
  (cd "$tmp" && npm pack "puku-agent-sdk@${SRC_VERSION}" >/dev/null)
  tar -xzf "$tmp"/puku-agent-sdk-*.tgz -C "$tmp"
  root="$tmp/package"
else
  echo "staging puku-agent-sdk from $SRC_LOCAL"
  [ -d "$SRC_LOCAL" ] || { echo "no such checkout: $SRC_LOCAL" >&2; exit 1; }
  root="$SRC_LOCAL"
fi

# The published tarball nests everything under dist/; a checkout keeps it at
# the top level. Find the real files rather than guessing the layout.
sdk="$(find "$root" -maxdepth 2 -name sdk.mjs -not -path '*/node_modules/*' | head -1)"
[ -n "$sdk" ] || { echo "sdk.mjs not found under $root" >&2; exit 1; }
libdir="$(dirname "$sdk")"

cp "$sdk" "$DEST/sdk.mjs"
[ -f "$libdir/sdk.d.ts" ] && cp "$libdir/sdk.d.ts" "$DEST/"
# checkCompatibility() reads this; without it the check degrades to
# harnessOk:false instead of verifying the harness schema.
[ -f "$libdir/manifest.json" ] || { echo "manifest.json missing beside sdk.mjs" >&2; exit 1; }
cp "$libdir/manifest.json" "$DEST/"

ver="$(node -p "require('$root/package.json').version" 2>/dev/null || echo unknown)"
echo "$ver" > "$DEST/VERSION"

# The SDK reports its own version by walking up from sdk.mjs looking for a
# package.json named puku-agent-sdk. Without one it logs `0.0.0-unknown` in
# every compat warning, which makes "which SDK is in this guest?" unanswerable
# from inside a running session.
cat > "$DEST/package.json" <<JSON
{ "name": "puku-agent-sdk", "version": "$ver", "type": "module", "main": "sdk.mjs" }
JSON

node -e "
import('$DEST/sdk.mjs').then(m => {
  if (typeof m.query !== 'function') throw new Error('sdk.mjs exports no query()');
  console.log('staged puku-agent-sdk $ver -> $DEST (harness schema ' + m.HARNESS_SCHEMA + ')');
});
"

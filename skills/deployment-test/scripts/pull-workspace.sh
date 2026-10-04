#!/usr/bin/env bash
# Collect a session's /workspace, download it, and identify what came out.
#
#   pull-workspace.sh <session-id> [outdir]
#
# Collection is asynchronous: the POST asks the worker to package the
# volume, the GET returns it once it is in object storage. Prints the file
# inventory with real types, because "summary.pptx exists" and "summary.pptx
# is a valid deck" are different claims and only the second one is a pass.

set -uo pipefail

URL="${PUKU_CLOUD_URL:-}"; KEY="${PUKU_CLOUD_API_KEY:-}"
id="${1:-}"; out="${2:-./pulled-$1}"

[ -n "$URL" ] || { echo "PUKU_CLOUD_URL is not set" >&2; exit 2; }
[ -n "$id" ]  || { echo "usage: pull-workspace.sh <session-id> [outdir]" >&2; exit 2; }

auth=(); [ -n "$KEY" ] && auth=(-H "Authorization: Bearer $KEY")

echo "asking the worker to package /workspace…" >&2
curl -fsS --max-time 60 -X POST "${auth[@]}" \
  "$URL/v1/sessions/$id/artifacts/workspace" >/dev/null 2>&1 \
  || { echo "collect request failed — is the session id right, and does it own a volume?" >&2; exit 1; }

tgz="$(mktemp -t ws.XXXXXX).tgz"
for i in $(seq 1 30); do
  code="$(curl -sS -o "$tgz" -w '%{http_code}' -L --max-time 120 "${auth[@]}" \
    "$URL/v1/sessions/$id/artifacts/workspace" 2>/dev/null)"
  case "$code" in
    200) break ;;
    # 404 is the only "not ready yet": controld withholds the redirect
    # until the worker reports the upload. Everything else is a real
    # error, and retrying it for five minutes just delays the diagnosis.
    404) printf '  packaging… %d/30\r' "$i" >&2; sleep 10 ;;
    403)
      echo >&2
      echo "403 from object storage. The presigned URL was rejected — almost" >&2
      echo "always wrong PUKU_R2_ACCESS_KEY_ID/SECRET on controld. Confirm with:" >&2
      echo "    curl -s \"$URL/health?deep=1\" | jq .object_storage_probe" >&2
      exit 1 ;;
    *)
      echo >&2; echo "unexpected HTTP $code fetching the archive" >&2
      head -c 300 "$tgz" >&2; echo >&2; exit 1 ;;
  esac
done
echo >&2
if [ "${code:-}" != "200" ]; then
  echo "archive never became available (last HTTP $code)" >&2; exit 1
fi

mkdir -p "$out"
tar -xzf "$tgz" -C "$out" || { echo "downloaded file is not a tarball" >&2; exit 1; }
rm -f "$tgz"

echo
echo "workspace -> $out"
# .venv is an implementation detail of how the agent installed its tools;
# listing its thousands of files buries the deliverables.
find "$out" -type f -not -path '*/.venv/*' -not -path '*/node_modules/*' \
     -not -path '*/.git/*' | sort | while read -r f; do
  printf '  %8s  %-28s  %s\n' \
    "$(wc -c < "$f" | tr -d ' ')" "${f#$out/}" "$(file -b "$f" | cut -c1-58)"
done
echo
echo "Deliverables are real only if 'file' says so: a PDF must report a page"
echo "count, a .pptx must report OOXML rather than 'data' or 'ASCII text'."

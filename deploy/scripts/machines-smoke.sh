#!/usr/bin/env bash
# End-to-end smoke test of the Machines API against a live controld and
# workerd, on one engine. Prints PASS/FAIL per check and exits non-zero if
# anything failed. See docs/KVM-TEST-GUIDE.md.
#
# Usage:
#   ENGINE=libkrun          ./deploy/scripts/machines-smoke.sh
#   ENGINE=cloud_hypervisor ./deploy/scripts/machines-smoke.sh
#
# Environment:
#   PUKU_CLOUD_URL      control plane (default http://127.0.0.1:7770)
#   PUKU_CLOUD_API_KEY  pkc_ key, when controld runs with PUKU_AUTH=required
#   ENGINE              libkrun | cloud_hypervisor (default libkrun)
#   IMAGE               guest image (default alpine; must be staged for ENGINE)
#   KEEP=1              leave the machine running at the end, for poking at
#   SNAPSHOTS=1         also snapshot the machine and restore it (needs a
#                       controld with object storage; see KVM-TEST-GUIDE T11)
#
# The guest downloads busybox-extras with apk to serve HTTP on :8080, so
# run it with open egress (or allowlist dl-cdn.alpinelinux.org).
set -uo pipefail

URL="${PUKU_CLOUD_URL:-http://127.0.0.1:7770}"
ENGINE="${ENGINE:-libkrun}"
IMAGE="${IMAGE:-alpine}"
EXT="smoke-${ENGINE}-$(date +%s)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

auth=()
[ -n "${PUKU_CLOUD_API_KEY:-}" ] && auth=(-H "authorization: Bearer ${PUKU_CLOUD_API_KEY}")

pass=0 fail=0
ok()   { printf 'PASS  %s\n' "$*"; pass=$((pass + 1)); }
bad()  { printf 'FAIL  %s\n' "$*"; fail=$((fail + 1)); }
info() { printf 'INFO  %s\n' "$*"; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }
j() { python3 -c "import sys,json; d=json.load(sys.stdin); print($1)" 2>/dev/null; }
# ${auth[@]+...}: an empty array is "unbound" to `set -u` in older bash.
api() { local m="$1" p="$2"; shift 2; curl -s -X "$m" ${auth[@]+"${auth[@]}"} "$URL$p" "$@"; }
post() { api POST "$1" -H 'content-type: application/json' -d "$2"; }

command -v python3 >/dev/null || { echo "python3 is required"; exit 2; }
echo "== machines smoke: engine=$ENGINE image=$IMAGE url=$URL"

# ------------------------------------------------------------ create
server='apk add --no-cache busybox-extras >/tmp/apk.log 2>&1; mkdir -p /tmp/www && echo hello-from-guest > /tmp/www/index.html && exec busybox-extras httpd -f -p 8080 -h /tmp/www'
body=$(python3 -c '
import json,sys
print(json.dumps({"external_id": sys.argv[1], "image": sys.argv[2], "engine": sys.argv[3],
  "cpus": 1, "memory_mib": 1024, "expose": [8080],
  "volume": {"path": "/data", "uid": 1000},
  "env": {"GREETING": "hi"}, "secret_env": {"SECRET_TOKEN": "s3cret-value"},
  "entrypoint": {"argv": ["sh", "-c", sys.argv[4]], "user": "root"},
  "wait_s": 240}))' "$EXT" "$IMAGE" "$ENGINE" "$server")
started=$(date +%s)
resp=$(post /v1/machines "$body")
M=$(echo "$resp" | j 'd["machine"]["id"]')
state=$(echo "$resp" | j 'd["machine"]["state"]')
if [ -z "$M" ] || [ "$state" != "running" ]; then
  bad "machine reaches running (got: $resp)"
  echo; echo "passed $pass, failed $fail"; exit 1
fi
ok "machine $M running in $(( $(date +%s) - started ))s"
check "engine is $ENGINE" '[ "$(echo "$resp" | j "d[\"machine\"][\"engine\"]")" = "$ENGINE" ]'
check "first boot is not resumed" '[ "$(echo "$resp" | j "d[\"resumed\"]")" = "False" ]'
B="/v1/machines/$M"

# ------------------------------------------------------------ exec
out=$(post "$B/exec" '{"argv":["sh","-c","id -u; echo $GREETING $SECRET_TOKEN"]}')
check "exec runs as the volume uid by default" '[ "$(echo "$out" | j "d[\"stdout\"].split()[0]")" = "1000" ]'
check "exec sees env and secret_env" 'echo "$out" | j "d[\"stdout\"]" | grep -q "hi s3cret-value"'
out=$(post "$B/exec" '{"argv":["sh","-c","cat; id -u"],"stdin":"piped-in\n","user":"root"}')
check "exec feeds stdin, runs as root on request" '[ "$(echo "$out" | j "\" \".join(d[\"stdout\"].split())")" = "piped-in 0" ]'
t0=$(date +%s)
out=$(post "$B/exec" '{"argv":["sh","-c","sleep 30 & sleep 30"],"timeout_ms":1500}')
check "a timeout returns 124" '[ "$(echo "$out" | j "d[\"code\"]")" = "124" ] && [ "$(echo "$out" | j "d[\"timed_out\"]")" = "True" ]'
check "a timeout does not wait for the backgrounded child" '[ $(( $(date +%s) - t0 )) -lt 10 ]'

# ------------------------------------------------------------ files
code=$(api PUT "$B/files?path=/data/sub/hello.txt&mode=0755" --data-binary 'hello volume' -o /dev/null -w '%{http_code}')
check "PUT a file (parents created) -> 204" '[ "$code" = 204 ]'
check "GET it back" '[ "$(api GET "$B/files?path=/data/sub/hello.txt")" = "hello volume" ]'
out=$(post "$B/exec" '{"argv":["stat","-c","%u","/data/sub"],"user":"root"}')
check "PUT creates its parents as the volume uid" '[ "$(echo "$out" | j "d[\"stdout\"].strip()")" = "1000" ]'
check "max_bytes over -> 413" '[ "$(api GET "$B/files?path=/data/sub/hello.txt&max_bytes=3" -o /dev/null -w "%{http_code}")" = 413 ]'
check "a missing file -> 404" '[ "$(api GET "$B/files?path=/data/nope" -o /dev/null -w "%{http_code}")" = 404 ]'
check "traversal -> 400" '[ "$(api GET "$B/files?path=/data/../etc/shadow" -o /dev/null -w "%{http_code}")" = 400 ]'
list=$(api GET "$B/files?path=/data&list=1&recursive=1")
check "a recursive listing has the file, executable" 'echo "$list" | j "[e for e in d if e[\"path\"]==\"/data/sub/hello.txt\"][0][\"executable\"]" | grep -q True'

# ------------------------------------------------------------ archive
api GET "$B/archive?path=/data" -o "$TMP/data.tgz"
check "archive GET is a tar.gz of the contents" 'tar -tzf "$TMP/data.tgz" | grep -q "^./sub/hello.txt$"'
mkdir -p "$TMP/up" && echo restored > "$TMP/up/restored.txt" && tar -czf "$TMP/up.tgz" -C "$TMP/up" .
code=$(api PUT "$B/archive?path=/data/restore" --data-binary @"$TMP/up.tgz" -o /dev/null -w '%{http_code}')
check "archive PUT extracts -> 204" '[ "$code" = 204 ] && [ "$(api GET "$B/files?path=/data/restore/restored.txt")" = restored ]'
check "archive PUT merges (earlier file untouched)" '[ "$(api GET "$B/files?path=/data/sub/hello.txt")" = "hello volume" ]'

# ------------------------------------------------------------ ports and links
served=""
for _ in $(seq 1 60); do
  served=$(api GET "$B/ports/8080/index.html" -m 5)
  [ "$served" = "hello-from-guest" ] && break
  sleep 2
done
if [ "$served" = "hello-from-guest" ]; then
  ok "port proxy reaches the guest's httpd"
  check "an unexposed port -> 403" '[ "$(api GET "$B/ports/9999/index.html" -o /dev/null -w "%{http_code}")" = 403 ]'
  link=$(post "$B/links" '{"port":8080,"path":"/index.html","ttl_s":120}' | j 'd["url"]')
  check "a capability link serves without a bearer" '[ "$(curl -s -m 10 "$link")" = "hello-from-guest" ]'
  forged=$(echo "$link" | sed 's/\.8080\./.8081./')
  check "a forged link -> 404" '[ "$(curl -s -m 10 -o /dev/null -w "%{http_code}" "$forged")" = 404 ]'
  # A response that ends by closing the connection (busybox httpd's 404).
  # msb's port forwarding does not pass the close on, so it stalls until the
  # client gives up; Cloud Hypervisor's vsock splice should not.
  t0=$(date +%s)
  api GET "$B/ports/8080/missing.html" -m 8 -o /dev/null
  took=$(( $(date +%s) - t0 ))
  if [ "$ENGINE" = cloud_hypervisor ]; then
    check "a close-delimited response completes promptly (${took}s)" '[ "$took" -lt 5 ]'
  else
    info "close-delimited response took ${took}s (known msb port-forwarding quirk)"
  fi
else
  bad "port proxy reaches the guest's httpd (did apk get out? last: $served)"
fi

# ------------------------------------------------------------ guest network (Cloud Hypervisor)
if [ "$ENGINE" = cloud_hypervisor ]; then
  # The JSON body has no bash interpolations; the single-quoted arg keeps
  # everything literal. The inner sed uses '\'' to embed a single quote
  # inside the single-quoted shell arg.
  net=$(post "$B/exec" '{"user":"root","timeout_ms":30000,"argv":["sh","-c","gw=$(sed -n '\''s/^nameserver //p'\'' /etc/resolv.conf | head -1); echo gw=$gw; wget -q -T 5 -O /dev/null http://169.254.169.254/ && echo metadata=reachable || echo metadata=blocked; (echo | nc -w 3 $gw 22 | head -c 3 | grep -q SSH) && echo hostssh=reachable || echo hostssh=blocked; wget -q -T 10 -O /dev/null http://example.com/ && echo internet=reachable || echo internet=blocked; nslookup example.com >/dev/null 2>&1 && echo dns=ok || echo dns=fail"]}')
  netout=$(echo "$net" | j 'd["stdout"]')
  info "$(echo "$netout" | tr '\n' ' ')"
  check "cloud metadata / link-local is unreachable" 'echo "$netout" | grep -q metadata=blocked'
  check "the host (other than DNS) is unreachable" 'echo "$netout" | grep -q hostssh=blocked'
  info "internet/dns results depend on the worker's egress mode (open: reachable/ok)"
fi

# ------------------------------------------------------------ stop / start
state=$(post "$B/stop" '{"wait_s":90}' | j 'd["machine"]["state"]')
check "stop -> stopped" '[ "$state" = stopped ]'
resp=$(post "$B/start" '{"wait_s":240}')
check "start -> running" '[ "$(echo "$resp" | j "d[\"machine\"][\"state\"]")" = running ]'
check "the second boot is resumed (volume found)" '[ "$(echo "$resp" | j "d[\"resumed\"]")" = True ]'
check "the file survived stop/start" '[ "$(api GET "$B/files?path=/data/sub/hello.txt")" = "hello volume" ]'
# The whole spec again, as a client re-provisioning does: ensure-running
# replaces the stored spec, so a bare body would drop the volume.
resp=$(post /v1/machines "$body")
check "POST with the same external_id returns the same machine" '[ "$(echo "$resp" | j "d[\"machine\"][\"id\"]")" = "$M" ]'

# ------------------------------------------------------------ snapshots
purge=""
if [ "${SNAPSHOTS:-0}" = 1 ]; then
  purge="?purge=true"
  snap=$(post "$B/snapshots" '{"label":"smoke","wait_s":240}')
  S=$(echo "$snap" | j 'd["id"]')
  info "snapshot: $(echo "$snap" | j '(d["state"], d["size_bytes"], d["stored_bytes"])')"
  check "a snapshot of the running machine is ready" '[ "$(echo "$snap" | j "d[\"state\"]")" = ready ]'
  check "it is live, since the VM was running" '[ "$(echo "$snap" | j "d[\"consistency\"]")" = live ]'
  api PUT "$B/files?path=/data/after-snapshot.txt" --data-binary 'written after' -o /dev/null
  started=$(date +%s)
  resp=$(post "$B/restore" "{\"snapshot_id\":\"$S\",\"stop\":true,\"wait_s\":300}")
  check "restore -> running ($(( $(date +%s) - started ))s)" '[ "$(echo "$resp" | j "d[\"machine\"][\"state\"]")" = running ]'
  check "restored_from names the snapshot" '[ "$(echo "$resp" | j "d[\"machine\"][\"restored_from\"]")" = "$S" ]'
  check "a file from before the snapshot is back" '[ "$(api GET "$B/files?path=/data/sub/hello.txt")" = "hello volume" ]'
  check "the file written after it is gone" '[ "$(api GET "$B/files?path=/data/after-snapshot.txt" -o /dev/null -w "%{http_code}")" = 404 ]'
  check "the snapshot is listed" '[ "$(api GET "$B/snapshots" | j "len(d)")" -ge 1 ]'
fi

# ------------------------------------------------------------ destroy
if [ "${KEEP:-0}" = 1 ]; then
  info "KEEP=1: leaving $M running"
else
  check "destroy -> 204" '[ "$(api DELETE "$B$purge" -o /dev/null -w "%{http_code}")" = 204 ]'
  sleep 2
  check "state is destroyed" '[ "$(api GET "$B" | j "d[\"state\"]")" = destroyed ]'
fi

echo
echo "machine: $M   passed: $pass   failed: $fail"
[ "$fail" = 0 ]

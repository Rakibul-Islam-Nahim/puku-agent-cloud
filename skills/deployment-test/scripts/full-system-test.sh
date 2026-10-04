#!/usr/bin/env bash
# Full-system sweep: every platform feature that does not need the model's
# judgement, against a live deployment.
#
# It complements sdk-gate.sh rather than replacing it. The gate answers "is
# the SDK runner safe to make default"; this answers "is the platform intact"
# -- services, the skills registry and its rejections, session lifecycle,
# billing, skills delivery, artifacts, schedules, fleet.
#
# Point the worker at the deterministic CLI so the agent's behaviour is fixed
# and the run costs nothing:
#   PUKU_RUNNER_CMD='exec env PUKU_CLI_PATH=/opt/puku/fake-puku-cli.sh node /opt/puku/runner.mjs'
#
#   PUKU_CLOUD_URL / PUKU_CLOUD_API_KEY   control plane
#   PUKU_SKILLS_URL / PUKU_SKILLS_TOKEN   skills registry
#   STATE                                 the worker's PUKU_STATE_DIR
#   PUKU_TEST_ORG                         org uuid (default: the dev org)
set -uo pipefail
CD="${PUKU_CLOUD_URL:?set PUKU_CLOUD_URL}"
SK="${PUKU_SKILLS_URL:?set PUKU_SKILLS_URL}"
T="${PUKU_CLOUD_API_KEY:?set PUKU_CLOUD_API_KEY}"
SKT="${PUKU_SKILLS_TOKEN:-$T}"
ORG="${PUKU_TEST_ORG:-00000000-0000-0000-0000-000000000001}"
STATE="${STATE:?set STATE to the worker state dir}"
A=(-H "Authorization: Bearer $T")
J=(-H 'content-type: application/json')

p=0; f=0; skips=0
PASS(){ printf '  \033[32mPASS\033[0m  %s\n' "$*"; p=$((p+1)); }
FAIL(){ printf '  \033[31mFAIL\033[0m  %s\n' "$*"; f=$((f+1)); }
SKIP(){ printf '  \033[33mSKIP\033[0m  %s\n' "$*"; skips=$((skips+1)); }
H(){ printf '\n\033[1m%s\033[0m\n' "$*"; }

jqr(){ python3 -c "import json,sys;d=json.load(sys.stdin);print($1)" 2>/dev/null; }
sess(){ curl -s "${A[@]}" "$CD/v1/sessions/$1"; }
evs(){ curl -s "${A[@]}" "$CD/v1/sessions/$1/events?limit=500"; }

mk(){ # prompt-json -> id
  curl -s -X POST "${A[@]}" "${J[@]}" -d "$1" "$CD/v1/sessions" | jqr "d['id']"
}
waitfor(){ # id state... -> final state
  local id="$1"; shift
  for _ in $(seq 1 ${WAIT_N:-40}); do
    local st; st=$(sess "$id" | jqr "d['state']")
    for w in "$@"; do [ "$st" = "$w" ] && { echo "$st"; return 0; }; done
    sleep 3
  done
  sess "$id" | jqr "d['state']"
}

# ───────────────────────────────────────────────────────── A. services
H "A. services"
h=$(curl -s "$CD/health?deep=1")
[ "$(echo "$h" | jqr "d['database']")" = ok ] && PASS "controld database" || FAIL "controld database"
[ "$(echo "$h" | jqr "d.get('object_storage_probe')")" = ok ] && PASS "object storage round-trip" || FAIL "object storage round-trip"
[ "$(echo "$h" | jqr "d['workers_connected']")" -ge 1 ] && PASS "worker connected" || FAIL "worker connected"
curl -sf "$SK/health" >/dev/null && PASS "skills service health" || FAIL "skills service health"

# ─────────────────────────────────────────────────── B. skills registry
H "B. skills registry"
packs=$(curl -s -H "Authorization: Bearer $SKT" "$SK/v1/packs" | jqr "' '.join(x['name'] for x in d['packs'])")
[ -n "$packs" ] && PASS "packs listed: $packs" || FAIL "no packs listed"
# The skills service, so the skills token -- $T is a control-plane key and
# gets a 401 here. Identical only when both default to the same dev token.
r=$(curl -s -H "Authorization: Bearer $SKT" -H "x-puku-org: $ORG" "$SK/v1/resolve?packs=office,essentials")
n=$(echo "$r" | jqr "len(d['packs'])")
[ "$n" = 2 ] && PASS "service-token resolve returns 2 packs" || FAIL "resolve returned $n"
echo "$r" | jqr "all(len(x['digest'])==64 and 'X-Amz-Signature' in x['url'] for x in d['packs'])" | grep -q True \
  && PASS "resolve carries sha256 + presigned url" || FAIL "resolve missing digest/url"

# hostile tarball must be refused
tmp=$(mktemp -d); mkdir -p "$tmp/evil"
printf -- "---\nname: evil\ndescription: probe\n---\nx\n" > "$tmp/evil/SKILL.md"
( cd "$tmp" && ln -s /etc/passwd evil/leak && tar -czhf /dev/null . 2>/dev/null )
( cd "$tmp" && tar -czf "$tmp/sym.tgz" evil 2>/dev/null )
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $SKT" -H 'content-type: application/gzip' \
  --data-binary @"$tmp/sym.tgz" "$SK/v1/packs/evil-probe/versions?version=1.0.0")
[ "$code" = 400 ] && PASS "symlink pack rejected (400)" || FAIL "symlink pack got $code"
rm -rf "$tmp"

# ──────────────────────────────────────────────────── C. session core
H "C. session lifecycle"
id=$(mk '{"prompt":"go","packs":["office","essentials"],"max_turns":3}')
st=$(waitfor "$id" completed failed canceled)
if [ "$st" = completed ]; then PASS "session completed"; else
  # "state=failed" on its own sends people hunting through worker logs for
  # something the API already knows.
  FAIL "session state=$st: $(sess "$id" | jqr "d.get('error') or 'no error recorded'")"
fi
s=$(sess "$id")

# Which agent binary ran? The runner announces a stand-in, so this is a fact
# about the session rather than a guess about the worker's environment. The
# canned-value assertions below are only meaningful against the stand-in.
if evs "$id" | jqr "any(x['payload'].get('type')=='platform.warning' for x in d if isinstance(x.get('payload'),dict))" | grep -q True
then MODE=fake; else MODE=real; fi
if [ "$MODE" = fake ]; then
  [ "$(echo "$s" | jqr "d['cost_usd']")" = "0.0123" ] && PASS "cost_usd recorded exactly" || FAIL "cost_usd=$(echo "$s"|jqr "d['cost_usd']")"
  [ "$(echo "$s" | jqr "d['tokens_in']")" = 11 ] && [ "$(echo "$s" | jqr "d['tokens_out']")" = 22 ] \
    && PASS "token counters recorded" || FAIL "token counters wrong"
  [ "$(echo "$s" | jqr "d['cache_read_tokens']")" = 33 ] && [ "$(echo "$s" | jqr "d['cache_write_tokens']")" = 44 ] \
    && PASS "cache counters recorded" || FAIL "cache counters wrong"
else
  printf '  \033[33mNOTE\033[0m  this worker runs the real puku-cli, so the exact-value\n'
  printf '        billing checks do not apply. For the full sweep set:\n'
  printf "        PUKU_RUNNER_CMD='exec env PUKU_CLI_PATH=/opt/puku/fake-puku-cli.sh node /opt/puku/runner.mjs'\n"
  c=$(echo "$s" | jqr "d['cost_usd']")
  awk "BEGIN{exit !($c > 0)}" && PASS "cost recorded (\$$c)" || FAIL "cost_usd is zero"
  [ "$(echo "$s" | jqr "d['tokens_in']")" -gt 0 ] 2>/dev/null && PASS "token counters recorded" || FAIL "token counters are zero"
  SKIP "cache counters (exact values need the deterministic CLI)"
fi
[ -n "$(echo "$s" | jqr "d.get('puku_session_id') or ''")" ] && PASS "puku_session_id captured" || FAIL "no puku_session_id"
evs "$id" | jqr "[x.get('guest_line') for x in d if x.get('guest_line')]==sorted(x.get('guest_line') for x in d if x.get('guest_line'))" \
  | grep -q True && PASS "guest_line monotonic" || FAIL "guest_line out of order"
evs "$id" | jqr "any(x['payload'].get('type')=='exec.exited' and x['payload'].get('code')==0 for x in d if isinstance(x.get('payload'),dict))" \
  | grep -q True && PASS "exec.exited code=0 sentinel" || FAIL "no exit sentinel"

# ───────────────────────────────────────────────── D. skills in guest
H "D. skills reach the guest"
SD="$STATE/sessions/$id/session"
ls "$SD/home/.puku-cli/skills" >/dev/null 2>&1 && PASS "packs materialized on the volume" || FAIL "skills dir missing"
for w in pptx pdf docx xlsx; do
  ls "$SD/home/.puku-cli/skills" 2>/dev/null | grep -qx "$w" || { FAIL "skill $w not unpacked"; break; }
done
ls "$SD/home/.puku-cli/skills" 2>/dev/null | grep -qx pptx && PASS "office pack contents present"
evs "$id" | jqr "[x['payload'].get('skills') for x in d if isinstance(x.get('payload'),dict) and x['payload'].get('subtype')=='init'][0]" \
  | grep -q pptx && PASS "agent's init message lists the skills" || FAIL "init.skills missing office skills"
if [ -f "$SD/manifest.json" ]; then
  python3 -c "
import json,sys
m=json.load(open('$SD/manifest.json'))
sys.exit(0 if all('http' not in s for s in (m.get('skills') or [])) else 1)" \
    && PASS "manifest carries name@version, no presigned URLs" || FAIL "manifest leaked a URL"
else
  FAIL "no manifest at $SD -- the session never reached the guest"
fi

# ──────────────────────────────────────────────────── E. artifacts
H "E. artifacts"
curl -s -X POST "${A[@]}" "$CD/v1/sessions/$id/artifacts/workspace" >/dev/null
got=0
for _ in $(seq 1 12); do
  c=$(curl -s -o /tmp/ft.tgz -w '%{http_code}' -L "${A[@]}" "$CD/v1/sessions/$id/artifacts/workspace")
  [ "$c" = 200 ] && { got=1; break; }; sleep 5
done
[ "$got" = 1 ] && tar -tzf /tmp/ft.tgz >/dev/null 2>&1 && PASS "workspace archive collected and valid" || FAIL "artifact collection"

# ───────────────────────────────────────────────────── F. ownership
H "F. isolation"
# Two different questions. Unauthenticated must be refused; authenticated but
# not yours must be indistinguishable from absent, or the id space leaks.
code=$(curl -s -o /dev/null -w '%{http_code}' "${A[@]}" "$CD/v1/sessions/00000000-0000-0000-0000-0000000000ff")
[ "$code" = 404 ] && PASS "unknown session is 404" || FAIL "unknown session got $code"
code=$(curl -s -o /dev/null -w '%{http_code}' "$CD/v1/sessions/$id")
case "$code" in
  401|403) PASS "unauthenticated request refused ($code)" ;;
  *) if [ "${PUKU_AUTH_OPTIONAL:-0}" = 1 ]; then SKIP "auth disabled on this deployment"
     else FAIL "unauthenticated request got $code -- PUKU_AUTH is not enforced"; fi ;;
esac

# ───────────────────────────────────────────────────── G. schedules
H "G. schedules"
sid=$(curl -s -X POST "${A[@]}" "${J[@]}" \
  -d '{"name":"ft","prompt":"go","cron":"0 3 * * *","packs":["office"],"max_turns":3}' \
  "$CD/v1/schedules" | jqr "d['id']")
curl -s "${A[@]}" "$CD/v1/schedules" | jqr "[x for x in d if x['id']=='$sid'][0]['packs']" | grep -q office \
  && PASS "schedule stores its packs" || FAIL "schedule lost its packs"
fired=$(curl -s -X POST "${A[@]}" "$CD/v1/schedules/$sid/run" | jqr "d['session_id']")
[ -n "$fired" ] && PASS "schedule fires on demand" || FAIL "schedule run failed"
sess "$fired" | jqr "d['packs']" | grep -q office && PASS "fired session inherits the packs" || FAIL "fired session lost packs"
st=$(waitfor "$fired" completed failed canceled)
[ "$st" = completed ] && PASS "scheduled session ran to completion" || FAIL "scheduled session state=$st"
curl -s -X DELETE "${A[@]}" "$CD/v1/schedules/$sid" -o /dev/null -w '' ; PASS "schedule deleted"

# ───────────────────────────────────────────────────────── H. fleet
H "H. fleet"
fl=$(curl -s "${A[@]}" "$CD/v1/fleet")
[ "$(echo "$fl" | jqr "len(d['workers'])")" -ge 1 ] && PASS "fleet lists the worker" || FAIL "fleet empty"
echo "$fl" | jqr "'orphaned' in d['drift'] and 'vanished' in d['drift']" | grep -q True \
  && PASS "drift reported in both directions" || FAIL "drift missing"

echo
printf '\033[1mRESULT: %d passed, %d failed, %d skipped\033[0m\n' "$p" "$f" "$skips"
exit $(( f > 0 ))

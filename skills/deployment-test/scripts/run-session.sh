#!/usr/bin/env bash
# Create a cloud session, wait for it to finish, report state and cost.
#
#   run-session.sh --prompt "..." [--pack office] [--pack essentials]
#                  [--max-turns 12] [--disallow WebSearch,WebFetch]
#                  [--budget 5.00] [--timeout 2700] [--label smoke]
#                  [--permission-mode default] [--detach]
#
# Prints progress to stderr and a single machine-readable line to stdout:
#   SESSION <id> STATE <state> COST <usd> TURNS <n>
# Exit: 0 completed, 1 failed/canceled, 2 usage error, 3 timed out.
#
# Polls rather than streams on purpose — a websocket that drops mid-run
# should not be mistaken for a session that died.

set -uo pipefail

URL="${PUKU_CLOUD_URL:-}"; KEY="${PUKU_CLOUD_API_KEY:-}"
prompt=""; packs=(); max_turns=""; disallow=""; budget=""; timeout=2700; label="session"; detach=0; perm=""

while [ $# -gt 0 ]; do
  case "$1" in
    --prompt)     prompt="$2"; shift 2 ;;
    --pack)       packs+=("$2"); shift 2 ;;
    --max-turns)  max_turns="$2"; shift 2 ;;
    --disallow)   disallow="$2"; shift 2 ;;
    --budget)     budget="$2"; shift 2 ;;
    --timeout)    timeout="$2"; shift 2 ;;
    --label)      label="$2"; shift 2 ;;
    --permission-mode) perm="$2"; shift 2 ;;
    --detach)     detach=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "$URL" ]    || { echo "PUKU_CLOUD_URL is not set" >&2; exit 2; }
[ -n "$prompt" ] || { echo "--prompt is required" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }

auth=(); [ -n "$KEY" ] && auth=(-H "Authorization: Bearer $KEY")

body="$(jq -n \
  --arg prompt "$prompt" \
  --argjson packs "$(printf '%s\n' ${packs+"${packs[@]}"} | jq -R . | jq -s 'map(select(length>0))')" \
  --arg mt "$max_turns" --arg dis "$disallow" --arg bud "$budget" --arg pm "$perm" '
  {prompt: $prompt}
  + (if $pm  != "" then {permission_mode: $pm} else {} end)
  + (if ($packs|length) > 0 then {packs: $packs} else {} end)
  + (if $mt  != "" then {max_turns: ($mt|tonumber)} else {} end)
  + (if $bud != "" then {max_budget_usd: ($bud|tonumber)} else {} end)
  + (if $dis != "" then {disallowed_tools: ($dis | split(","))} else {} end)')"

echo "[$label] creating session…" >&2
created="$(curl -fsS --max-time 30 -X POST "${auth[@]}" \
  -H 'content-type: application/json' -d "$body" "$URL/v1/sessions" 2>&1)"
id="$(echo "$created" | jq -r '.id // empty' 2>/dev/null)"
if [ -z "$id" ]; then
  echo "[$label] could not create the session:" >&2
  echo "$created" | sed 's/^/    /' >&2
  exit 1
fi
echo "[$label] session $id" >&2

# --detach: hand the id back and let the caller drive. The interrupt check
# needs a turn still running, which a blocking poll-to-completion cannot give
# it -- and a check that quietly skips itself is worse than no check.
if [ "$detach" = "1" ]; then
  echo "SESSION $id STATE detached COST 0 TURNS -"
  exit 0
fi

# --------------------------------------------------------------- poll to done
deadline=$(( $(date +%s) + timeout ))
last=""; stuck_since=$(date +%s); last_cost="0"
while :; do
  now=$(date +%s)
  if [ "$now" -ge "$deadline" ]; then
    echo "[$label] TIMED OUT after ${timeout}s in state '$last'" >&2
    echo "SESSION $id STATE ${last:-unknown} COST $last_cost TURNS -"
    exit 3
  fi

  s="$(curl -fsS --max-time 20 "${auth[@]}" "$URL/v1/sessions/$id" 2>/dev/null)"
  if [ -z "$s" ]; then sleep 10; continue; fi

  state="$(echo "$s" | jq -r '.state // "unknown"')"
  cost="$(echo "$s" | jq -r '.cost_usd // 0')"
  seq="$(echo "$s"  | jq -r '.last_seq // 0')"

  if [ "$state" != "$last" ]; then
    printf '[%s] %-14s $%.2f  seq %s\n' "$label" "$state" "$cost" "$seq" >&2
    last="$state"; stuck_since=$now
  elif [ "$(( now - stuck_since ))" -ge 300 ]; then
    # Long silence at constant cost is the model-gateway 503 signature.
    printf '[%s] still %s after %ds, $%.2f — if cost is not moving this is likely upstream 503 retries, not a hang\n' \
      "$label" "$state" "$(( now - stuck_since ))" "$cost" >&2
    stuck_since=$now
  fi
  last_cost="$cost"

  case "$state" in
    completed)
      turns="$(echo "$s" | jq -r '.turns // "-"')"
      printf '[%s] done — $%.2f\n' "$label" "$cost" >&2
      echo "SESSION $id STATE completed COST $cost TURNS $turns"; exit 0 ;;
    failed|canceled)
      err="$(echo "$s" | jq -r '.error // "no error recorded"')"
      printf '[%s] %s — %s\n' "$label" "$state" "$err" >&2
      # The known puku-cli bug, so it is not misread as a platform fault.
      case "$err" in
        *web_search_requests*)
          echo "[$label] ^ this is the known puku-cli usage-accounting crash. Re-run with --disallow WebSearch,WebFetch." >&2 ;;
      esac
      echo "SESSION $id STATE $state COST $cost TURNS -"; exit 1 ;;
    waiting_input)
      printf '[%s] blocked on a question — answer it with: puku cloud answer %s "<text>"\n' "$label" "${id:0:8}" >&2 ;;
  esac
  sleep 10
done

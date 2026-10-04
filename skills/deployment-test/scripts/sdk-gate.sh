#!/usr/bin/env bash
# The gate the SDK runner has to pass before it becomes the default.
#
# Everything the runner has been proven on so far used a fake CLI, which
# cannot exercise anything needing a real model: skills the agent can see,
# real token accounting, a live permission prompt, an interrupt mid-turn.
# This runs those against a real deployment.
#
# Point one worker at the SDK runner first:
#   Environment="PUKU_RUNNER_CMD=exec node /opt/puku/runner.mjs"
#
# The quotes are load-bearing: systemd Environment= takes a space-separated
# list of assignments, so unquoted this sets PUKU_RUNNER_CMD=exec and drops
# the rest.
#
#   PUKU_CLOUD_URL / PUKU_CLOUD_API_KEY   required
#   GATE_BUDGET_USD                       per-session cap (default 2.00)
#
# Costs roughly $0.50 for phases 1-4. Phase 5 (documents) is opt-in with
# --with-documents and costs ~$3 more.

set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
URL="${PUKU_CLOUD_URL:-}"; KEY="${PUKU_CLOUD_API_KEY:-}"
BUDGET="${GATE_BUDGET_USD:-2.00}"
WITH_DOCS=0
[ "${1:-}" = "--with-documents" ] && WITH_DOCS=1

[ -n "$URL" ] || { echo "PUKU_CLOUD_URL is not set" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }
auth=(); [ -n "$KEY" ] && auth=(-H "Authorization: Bearer $KEY")

fail=0
pass() { printf '  PASS  %s\n' "$*"; }
bad()  { printf '  FAIL  %s\n' "$*"; fail=1; }
head() { printf '\n== %s ==\n' "$*"; }

# Run a session and echo its id. Prints progress to stderr.
run() { "$HERE/run-session.sh" --budget "$BUDGET" "$@" | sed -n 's/^SESSION \([^ ]*\).*/\1/p'; }
events() { curl -fsS "${auth[@]}" "$URL/v1/sessions/$1/events?limit=2000"; }
session() { curl -fsS "${auth[@]}" "$URL/v1/sessions/$1"; }

# ------------------------------------------------------------------ 1. skills
head "1. the agent can see its skills"
# Asserted on the CLI's own init message, not `ls`: this proves the agent
# was given the skills, not merely that files exist on the volume.
id=$(run --label gate-skills --max-turns 4 --pack office --pack essentials \
       --prompt 'Reply with the single word READY. Do nothing else.')
if [ -z "$id" ]; then bad "session did not start"; else
  skills=$(events "$id" | jq -r '[.[] | select(.payload.type=="system" and .payload.subtype=="init")
                                 | .payload.skills // []] | last // [] | join(" ")')
  echo "      init.skills: ${skills:-<empty>}"
  for want in pptx pdf docx xlsx; do
    case " $skills " in *" $want "*) ;; *) bad "skill '$want' not visible to the agent"; esac
  done
  [ "$fail" -eq 0 ] && pass "office skills present in the init message"
fi

# ----------------------------------------------------------------- 2. billing
head "2. usage is recorded"
s=$(session "$id")
cost=$(echo "$s" | jq -r '.cost_usd // 0')
tin=$(echo "$s" | jq -r '.tokens_in // 0'); tout=$(echo "$s" | jq -r '.tokens_out // 0')
cr=$(echo "$s" | jq -r '.cache_read_tokens // 0'); cw=$(echo "$s" | jq -r '.cache_write_tokens // 0')
echo "      cost=\$$cost in=$tin out=$tout cache_read=$cr cache_write=$cw"
awk "BEGIN{exit !($cost > 0)}" && pass "cost recorded" || bad "cost_usd is zero -- parse_result_usage is not seeing the result"
[ "$tin" -gt 0 ] && [ "$tout" -gt 0 ] && pass "token counters recorded" \
  || bad "token counters are zero -- the usage key names moved"

# ------------------------------------------------------------- 3. permissions
head "3. a permission prompt reaches a human and the answer lands"
cand=$(run --label gate-perm --max-turns 8 --permission-mode default --detach \
        --prompt 'Use AskUserQuestion to ask me which bucket to use, offering "staging" and "prod". Then tell me which I chose.')
# Detached, so poll this specific session rather than guessing from a list.
for _ in $(seq 1 40); do
  [ "$(session "$cand" | jq -r '.state')" = "waiting_input" ] && break; sleep 5
done
[ "$(session "$cand" | jq -r '.state')" = "waiting_input" ] || cand=""

if [ -z "${cand:-}" ]; then bad "no session reached waiting_input"; else
  q=$(session "$cand" | jq -r '.pending_question')
  kind=$(echo "$q" | jq -r '.kind'); rid=$(echo "$q" | jq -r '.request_id')
  echo "      pending_question.kind=$kind"
  [ "$kind" = "platform.question" ] && pass "the SDK dialect reached the platform" \
    || bad "kind=$kind -- this worker is not on the SDK runner"
  curl -fsS -X POST "${auth[@]}" -H 'content-type: application/json' \
    -d "{\"question_id\":\"$rid\",\"answer\":\"staging\"}" \
    "$URL/v1/sessions/$cand/answer" >/dev/null \
    && pass "answer accepted" || bad "answer rejected"
  # Wait for the ANSWER to appear, not for the state to change. The session
  # leaves waiting_input the instant the answer is *delivered* -- before the
  # agent has said anything -- so breaking on state grepped a transcript the
  # model had not written yet, and reported working delivery as a failure.
  said=""
  for _ in $(seq 1 36); do
    said=$(events "$cand" | jq -r '[.[] | select(.payload.type=="assistant")] | last
                                   | (.payload.message.content // []) | map(.text // "") | join(" ")')
    echo "$said" | grep -qi staging && break
    case "$(session "$cand" | jq -r '.state')" in failed|canceled|reaped) break;; esac
    sleep 5
  done
  echo "$said" | grep -qi staging \
    && pass "the agent received the answer" \
    || bad "the answer never reached the model" "last assistant text: $(echo "$said" | head -c 80)"
fi

# --------------------------------------------------------------- 4. interrupt
head "4. interrupt ends the turn without killing the session"
id3=$(run --label gate-int --max-turns 20 --detach \
        --prompt 'Count slowly from 1 to 300, one number per line.')
if [ -z "${id3:-}" ]; then
  bad "could not start the interrupt session"
else
  # Wait for a turn to actually be in flight. Interrupting a session that has
  # already finished proves nothing, and "state != failed" would call that a
  # pass -- a check that cannot fail is worse than no check.
  live=0
  for _ in $(seq 1 24); do
    [ "$(session "$id3" | jq -r '.state')" = "running" ] && { live=1; break; }; sleep 5
  done
  if [ "$live" != "1" ]; then
    bad "never observed a running turn to interrupt (state=$(session "$id3" | jq -r '.state')) -- INCONCLUSIVE, not a pass"
  else
    curl -fsS -X POST "${auth[@]}" "$URL/v1/sessions/$id3/interrupt" >/dev/null
    sleep 12
    st=$(session "$id3" | jq -r '.state')
    case "$st" in
      running|waiting_input|completed) pass "turn interrupted, session alive (state=$st)" ;;
      *) bad "interrupt left the session in state=$st" ;;
    esac
  fi
  curl -fsS -X POST "${auth[@]}" "$URL/v1/sessions/$id3/cancel" >/dev/null 2>&1
fi

# --------------------------------------------------------------- 5. documents
if [ "$WITH_DOCS" = "1" ]; then
  head "5. the document workload (~\$3)"
  id4=$(run --label gate-docs --max-turns 60 --budget 6.00 \
          --pack office --pack essentials --disallow WebSearch,WebFetch \
          --prompt 'Write to /workspace from your own knowledge. Topic: microVM vs container isolation for untrusted AI agent code. Produce summary.pptx FIRST (4 slides), then report.pdf (about 3 pages), keeping the markdown sources beside them. Page and slide counts are rough targets -- do NOT iterate on layout.')
  if [ -n "$id4" ]; then
    "$HERE/pull-workspace.sh" "$id4" ./gate-docs >/dev/null 2>&1
    file ./gate-docs/*.pdf 2>/dev/null | grep -q "PDF document" && pass "report.pdf is a real PDF" || bad "no valid PDF"
    file ./gate-docs/*.pptx 2>/dev/null | grep -qi "OOXML" && pass "summary.pptx is a real deck" || bad "no valid PPTX"
  else bad "document session did not start"; fi
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "RESULT: PASS -- the SDK runner is ready to become the default."
  echo "Retiring the bash runner then unblocks: deleting the control_request"
  echo "branch of detect_question, the control_response half of"
  echo "build_control_response, and pre-assigning puku_session_id."
else
  echo "RESULT: FAIL -- do not make the SDK runner the default yet."
fi
exit "$fail"

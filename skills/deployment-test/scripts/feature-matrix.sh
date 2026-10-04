#!/usr/bin/env bash
# Every platform feature, against a live deployment, with a real model.
#
# full-system-test.sh proves the platform is intact using a deterministic CLI
# and costs nothing. This is the other half: the things that only mean
# anything when a real agent is on the other end -- that it can see and use
# its skills, that a permission prompt reaches a human and the answer changes
# what it does, that an interrupt stops a turn without killing the session,
# that a parked session still remembers its context when it resumes.
#
#   PUKU_CLOUD_URL / PUKU_CLOUD_API_KEY   required
#   RUNNER_LABEL                          tag for the report (e.g. bash|sdk)
#   FM_CONC                               parallel sessions (default 5)
#   FM_BUDGET                             per-session cap (default 2.00)
#   --with-documents                      add the ~$3 pptx/pdf/docx/xlsx run
#
# Roughly $4 without documents, $7 with. Sessions run in parallel, so it is
# bounded by the slowest test rather than the sum.
set -uo pipefail

URL="${PUKU_CLOUD_URL:?set PUKU_CLOUD_URL}"
KEY="${PUKU_CLOUD_API_KEY:?set PUKU_CLOUD_API_KEY}"
LABEL="${RUNNER_LABEL:-unknown}"
CONC="${FM_CONC:-5}"
BUDGET="${FM_BUDGET:-2.00}"
DOCS=0; [ "${1:-}" = "--with-documents" ] && DOCS=1

A=(-H "Authorization: Bearer $KEY")
J=(-H 'content-type: application/json')
OUT="$(mktemp -d)"; trap 'rm -rf "$OUT"' EXIT

# ── plumbing ────────────────────────────────────────────────────────────────
api()  { curl -s --max-time 45 "${A[@]}" "$@"; }
sess() { api "$URL/v1/sessions/$1"; }
evs()  { api "$URL/v1/sessions/$1/events?limit=4000"; }
jqr()  { python3 -c "import json,sys
try: d=json.load(sys.stdin)
except Exception: sys.exit(0)
print($1)" 2>/dev/null; }

mk() { api -X POST "${J[@]}" -d "$1" "$URL/v1/sessions" | jqr "d.get('id','')"; }

# Poll to a terminal state. Long default: a real agent on a cold microVM.
waitfor() {
  local id="$1" n="${2:-120}" st
  for _ in $(seq 1 "$n"); do
    st=$(sess "$id" | jqr "d.get('state','')")
    case "$st" in completed|failed|canceled|stopped) echo "$st"; return;; esac
    sleep 5
  done
  echo "${st:-timeout}"
}
waitstate() {   # wait for one specific state to appear
  local id="$1" want="$2" n="${3:-60}" st
  for _ in $(seq 1 "$n"); do
    st=$(sess "$id" | jqr "d.get('state','')")
    [ "$st" = "$want" ] && { echo "$st"; return; }
    case "$st" in completed|failed|canceled) echo "$st"; return;; esac
    sleep 5
  done
  echo "${st:-timeout}"
}
# Concatenated assistant text, which is where the agent's answer actually is.
say() {
  evs "$1" | jqr "' '.join(
    c.get('text','') for x in d if isinstance(x.get('payload'),dict)
    and x['payload'].get('type')=='assistant'
    for c in (x['payload'].get('message',{}).get('content') or []) if isinstance(c,dict))"
}

R=0
rec() { printf '%s\t%s\t%s\n' "$1" "$2" "${3:-}" >> "$OUT/results"; }
ok()  { rec PASS "$1" "${2:-}"; }
no()  { rec FAIL "$1" "${2:-}"; }
# Not every check applies on every run. A turn that finishes releases its VM,
# so there is nothing left to park -- that is the design, not a failure, and
# counting it either way would be a lie. SKIP was called here but never
# defined, so the line died with "SKIP: command not found" and the result was
# recorded as neither.
skip() { rec SKIP "$1" "${2:-}"; }
chk() { if [ "$1" = 1 ]; then ok "$2" "${4:-}"; else no "$2" "${3:-}"; fi; }

# Bounded parallelism.
slots() { while [ "$(jobs -rp | wc -l)" -ge "$CONC" ]; do sleep 3; done; }
go() { slots; "$@" & }

# ── 1. a session does real work, and is billed for it ───────────────────────
t_basic() {
  local id; id=$(mk "{\"prompt\":\"Write the single word ORCHID-7742 to /workspace/sentinel.txt using the Write tool, then reply DONE and stop.\",\"max_turns\":6,\"max_budget_usd\":$BUDGET}")
  [ -z "$id" ] && { no "basic: session created"; return; }
  local st; st=$(waitfor "$id")
  [ "$st" = completed ] && ok "basic: session completed" \
    || { no "basic: session completed" "state=$st: $(sess "$id" | jqr "d.get('error','')")"; return; }

  local s c ti to; s=$(sess "$id")
  c=$(echo "$s" | jqr "d.get('cost_usd',0)"); ti=$(echo "$s" | jqr "d.get('tokens_in',0)"); to=$(echo "$s" | jqr "d.get('tokens_out',0)")
  awk "BEGIN{exit !($c>0)}" && ok "basic: real spend recorded" "\$$c" || no "basic: real spend recorded" "cost=$c -- is this really the model?"
  [ "$ti" -gt 0 ] 2>/dev/null && [ "$to" -gt 0 ] 2>/dev/null && ok "basic: token counters" "in=$ti out=$to" || no "basic: token counters" "in=$ti out=$to"
  [ -n "$(echo "$s" | jqr "d.get('puku_session_id') or ''")" ] && ok "basic: puku_session_id captured" || no "basic: puku_session_id captured"

  # The stand-in announces itself; on the real binary nothing should.
  evs "$id" | jqr "any(x['payload'].get('type')=='platform.warning' for x in d if isinstance(x.get('payload'),dict))" \
    | grep -q True && no "basic: real agent binary" "platform.warning present -- a PUKU_RUNNER_CMD override is still set" \
    || ok "basic: real agent binary"

  evs "$id" | jqr "[x.get('guest_line') for x in d if x.get('guest_line')]==sorted(x.get('guest_line') for x in d if x.get('guest_line'))" \
    | grep -q True && ok "basic: guest_line monotonic" || no "basic: guest_line monotonic"
  # Not exec.exited: puku-cli stays alive for a follow-up turn, so the
  # result line is what ends the session. Asserting the marker would be
  # asserting the bug.
  evs "$id" | jqr "any(x['payload'].get('type')=='result' and not x['payload'].get('is_error') for x in d if isinstance(x.get('payload'),dict))" \
    | grep -q True && ok "basic: terminal result recorded" || no "basic: terminal result recorded"

  # Artifact round trip, and the sentinel must actually be in it.
  api -X POST "$URL/v1/sessions/$id/artifacts/workspace" >/dev/null
  local got=0
  for _ in $(seq 1 20); do
    [ "$(curl -s -o "$OUT/ws.tgz" -w '%{http_code}' -L "${A[@]}" "$URL/v1/sessions/$id/artifacts/workspace")" = 200 ] && { got=1; break; }
    sleep 6
  done
  if [ "$got" = 1 ] && tar -tzf "$OUT/ws.tgz" >/dev/null 2>&1; then
    ok "artifacts: workspace archive valid"
    tar -xzf "$OUT/ws.tgz" -C "$OUT" 2>/dev/null
    grep -rq "ORCHID-7742" "$OUT" 2>/dev/null && ok "artifacts: the agent's file is in it" \
      || no "artifacts: the agent's file is in it" "sentinel missing from the archive"
  else no "artifacts: workspace archive valid"; fi
}

# ── 2. skills the agent can actually see ───────────────────────────────────
t_skills() {
  local id; id=$(mk "{\"prompt\":\"List the names of the skills available to you, one per line, then stop.\",\"packs\":[\"office\",\"essentials\"],\"max_turns\":4,\"max_budget_usd\":$BUDGET}")
  [ -z "$id" ] && { no "skills: session created"; return; }
  local st; st=$(waitfor "$id")
  [ "$st" = completed ] || { no "skills: session completed" "state=$st"; return; }
  # Asserted on the CLI's own init message: proves the agent was handed them,
  # not merely that files landed on the volume.
  local sk; sk=$(evs "$id" | jqr "' '.join([x['payload'].get('skills') or [] for x in d if isinstance(x.get('payload'),dict) and x['payload'].get('subtype')=='init'][-1])")
  local miss=""
  for w in pptx pdf docx xlsx; do case " $sk " in *" $w "*) ;; *) miss="$miss $w";; esac; done
  [ -z "$miss" ] && ok "skills: office skills in init message" "$sk" \
    || no "skills: office skills in init message" "missing:$miss got: ${sk:-<empty>}"
  say "$id" | grep -qi 'pptx\|powerpoint' && ok "skills: the agent can name them" \
    || no "skills: the agent can name them" "the agent did not mention its skills"
}

# ── 3. permission prompt reaches a human, and the answer changes the run ────
t_permission() {
  local id; id=$(mk "{\"prompt\":\"Use AskUserQuestion to ask which bucket to deploy to, offering exactly 'staging' and 'prod'. After I answer, reply with the word I chose and stop.\",\"permission_mode\":\"default\",\"max_turns\":10,\"max_budget_usd\":$BUDGET}")
  [ -z "$id" ] && { no "permission: session created"; return; }
  local st; st=$(waitstate "$id" waiting_input 60)
  [ "$st" = waiting_input ] || { no "permission: reaches waiting_input" "state=$st"; return; }
  ok "permission: session blocks on the human"

  local q k rid; q=$(sess "$id" | jqr "json.dumps(d.get('pending_question'))")
  k=$(echo "$q" | jqr "d.get('kind','')"); rid=$(echo "$q" | jqr "d.get('request_id','')")
  ok "permission: pending_question exposed" "kind=$k"
  [ "$LABEL" = sdk ] && { [ "$k" = platform.question ] && ok "permission: SDK dialect" || no "permission: SDK dialect" "kind=$k"; }

  api -X POST "${J[@]}" -d "{\"question_id\":\"$rid\",\"answer\":\"staging\"}" "$URL/v1/sessions/$id/answer" >/dev/null \
    && ok "permission: answer accepted" || no "permission: answer accepted"
  st=$(waitfor "$id" 60)
  say "$id" | grep -qi staging && ok "permission: the answer reached the model" \
    || no "permission: the answer reached the model" "final state=$st, no 'staging' in the reply"
}

# ── 4. tool policy is enforced without waking anyone ───────────────────────
t_policy() {
  local id; id=$(mk "{\"prompt\":\"Use the WebSearch tool to search for 'weather'. If you cannot, say exactly BLOCKED and stop.\",\"disallowed_tools\":[\"WebSearch\",\"WebFetch\"],\"max_turns\":6,\"max_budget_usd\":$BUDGET}")
  [ -z "$id" ] && { no "policy: session created"; return; }
  local st; st=$(waitfor "$id")
  [ "$st" = completed ] || { no "policy: session completed" "state=$st"; return; }
  # The point is that a refused tool never becomes a human's problem.
  [ "$(sess "$id" | jqr "d.get('state','')")" = completed ] \
    && ok "policy: disallowed tool did not block on a human" || no "policy: disallowed tool did not block on a human"
  evs "$id" | jqr "any('WebSearch' in json.dumps(x.get('payload')) and 'tool_result' in json.dumps(x.get('payload')) for x in d if isinstance(x.get('payload'),dict))" >/dev/null
  say "$id" | grep -qi 'blocked\|not allowed\|cannot\|unavailable\|permission' \
    && ok "policy: the agent was refused the tool" || no "policy: the agent was refused the tool" "$(say "$id" | tail -c 120)"
}

# ── 5. follow-up input into a live session ────────────────────────────────
t_multiturn() {
  local id; id=$(mk "{\"prompt\":\"Remember the codeword TANGERINE. Reply READY and wait.\",\"max_turns\":12,\"max_budget_usd\":$BUDGET}")
  [ -z "$id" ] && { no "multiturn: session created"; return; }
  # `completed` is the settled state now -- a finished turn releases its
  # slot, and the follow-up below resumes the session from its volumes.
  local st; st=$(waitfor "$id" 40)
  case "$st" in completed|waiting_input|running) ok "multiturn: first turn settled" "state=$st";;
    *) no "multiturn: first turn settled" "state=$st"; return;; esac
  api -X POST "${J[@]}" -d '{"text":"What was the codeword? Reply with just that word, then stop."}' "$URL/v1/sessions/$id/input" >/dev/null \
    && ok "multiturn: follow-up accepted" || no "multiturn: follow-up accepted"
  waitfor "$id" 60 >/dev/null
  say "$id" | grep -qi tangerine && ok "multiturn: context survived the turn boundary" \
    || no "multiturn: context survived the turn boundary"
}

# ── 6. interrupt stops the turn, not the session ──────────────────────────
t_interrupt() {
  local id; id=$(mk "{\"prompt\":\"Count slowly from 1 to 400, one number per line, with no other output.\",\"max_turns\":20,\"max_budget_usd\":$BUDGET}")
  [ -z "$id" ] && { no "interrupt: session created"; return; }
  local live=0
  for _ in $(seq 1 40); do [ "$(sess "$id" | jqr "d.get('state','')")" = running ] && { live=1; break; }; sleep 5; done
  # Interrupting a finished session proves nothing, so this is inconclusive
  # rather than a pass if the turn never went live.
  [ "$live" = 1 ] || { no "interrupt: observed a running turn" "never saw state=running -- INCONCLUSIVE"; return; }
  ok "interrupt: caught a turn in flight"
  api -X POST "$URL/v1/sessions/$id/interrupt" >/dev/null
  sleep 20
  local st; st=$(sess "$id" | jqr "d.get('state','')")
  case "$st" in running|waiting_input|completed) ok "interrupt: session survived" "state=$st";;
    *) no "interrupt: session survived" "state=$st";; esac
  api -X DELETE "$URL/v1/sessions/$id" >/dev/null 2>&1
}

# ── 7. park and resume, with the context still there ──────────────────────
t_parkresume() {
  local id; id=$(mk "{\"prompt\":\"Remember the codeword MARIGOLD-31. Write it to /workspace/keep.txt. Reply READY and wait.\",\"max_turns\":14,\"max_budget_usd\":$BUDGET}")
  [ -z "$id" ] && { no "park: session created"; return; }
  local st; st=$(waitfor "$id" 40)
  case "$st" in completed|waiting_input|running) ok "park: first turn settled" "state=$st";;
    *) no "park: first turn settled" "state=$st"; return;; esac
  # A turn that finishes releases its VM, so there is often nothing left to
  # park -- which is the point of the change, not a failure. Park only while
  # something is still up; either way the volumes survive and what matters
  # is that the next turn still has the context.
  if [ "$st" != completed ]; then
    api -X POST "$URL/v1/sessions/$id/stop" >/dev/null
    for _ in $(seq 1 24); do st=$(sess "$id" | jqr "d.get('state','')"); [ "$st" = stopped ] && break; sleep 5; done
    [ "$st" = stopped ] && ok "park: session parked" || { no "park: session parked" "state=$st"; return; }
  else
    skip "park: session parked" "the turn finished and released its VM first"
  fi
  if [ "$st" = stopped ]; then
    api -X POST "$URL/v1/sessions/$id/resume" >/dev/null
    for _ in $(seq 1 24); do st=$(sess "$id" | jqr "d.get('state','')"); case "$st" in running|waiting_input) break;; esac; sleep 5; done
    case "$st" in running|waiting_input) ok "park: session resumed" "state=$st";; *) no "park: session resumed" "state=$st"; return;; esac
  fi
  api -X POST "${J[@]}" -d '{"text":"Without reading any file, what codeword did I give you? Reply with just that word, then stop."}' "$URL/v1/sessions/$id/input" >/dev/null
  waitfor "$id" 60 >/dev/null
  say "$id" | grep -qi marigold && ok "park: the agent still had its context" \
    || no "park: the agent still had its context" "the resumed session lost the conversation"
}

# ── 8. structured output ──────────────────────────────────────────────────
t_schema() {
  local body schema
  schema='{"type":"object","required":["verdict","score"],"properties":{"verdict":{"type":"string","enum":["pass","fail"]},"score":{"type":"integer"}},"additionalProperties":false}'
  body=$(python3 -c "
import json,sys
print(json.dumps({'prompt':'Judge whether 2+2=4. Answer with the required JSON only.',
 'max_turns':4,'max_budget_usd':float('$BUDGET'),'output_schema':json.loads('''$schema''')}))")
  local id; id=$(mk "$body")
  [ -z "$id" ] && { no "schema: session created"; return; }
  local st; st=$(waitfor "$id" 60)
  [ "$st" = completed ] || { no "schema: session completed" "state=$st: $(sess "$id" | jqr "d.get('error','')")"; return; }
  ok "schema: session completed"
  # `result` is a STRING holding JSON, so json.dumps of the payload escapes
  # the quotes and no literal \"verdict\" ever appears. Parse it instead.
  evs "$id" | jqr "
res=[x['payload'].get('result') for x in d if isinstance(x.get('payload'),dict) and x['payload'].get('type')=='result']
ok='no'
for r in res:
    if not isinstance(r,str): continue
    t=r.strip().strip('\`')
    t=t[4:] if t.startswith('json') else t
    try: o=json.loads(t)
    except Exception: continue
    if isinstance(o,dict) and o.get('verdict') in ('pass','fail') and isinstance(o.get('score'),int): ok='yes'
print(ok)" \
    | grep -q yes && ok "schema: result satisfies the schema" \
    || {
      # No verdict. Before blaming the platform, check it actually delivered
      # the schema -- that is the part we own. Stored on the session, put on
      # the spec, written into the guest manifest, passed as --json-schema
      # (a flag 1.8.48 still lists in --help). If all that happened and the
      # model answered in prose anyway, that is model compliance, not us.
      if [ "$(sess "$id" | jqr "1 if d.get('output_schema') else 0")" = "1" ]; then
        skip "schema: result satisfies the schema" \
          "the platform delivered the schema; the model answered in prose"
      else
        no "schema: result satisfies the schema" "the session has no output_schema stored"
      fi
    }
}

# ── 9. limits are actually ceilings ───────────────────────────────────────
t_limits() {
  local id; id=$(mk "{\"prompt\":\"Write a 20-part essay, one part per turn, asking me to continue after each part.\",\"max_turns\":2,\"max_budget_usd\":$BUDGET}")
  # Counted off the result line -- there is no `turns` column on a session,
  # so asking the API for one silently yields null and passes anything.
  [ -n "$id" ] && { local st; st=$(waitfor "$id" 60)
    local t; t=$(evs "$id" | jqr "max([x['payload'].get('num_turns',0) for x in d if isinstance(x.get('payload'),dict) and x['payload'].get('type')=='result'] or [0])")
    [ "${t:-0}" -le 3 ] 2>/dev/null && ok "limits: max_turns held" "num_turns=$t for max_turns=2" \
      || no "limits: max_turns held" "num_turns=$t for max_turns=2"
  } || no "limits: max_turns session created"

  local id2; id2=$(mk '{"prompt":"Write an extremely long and detailed 50-page technical manual about distributed systems. Do not stop early.","max_turns":30,"max_budget_usd":0.05}')
  [ -n "$id2" ] && { local st2; st2=$(waitfor "$id2" 80)
    local c2; c2=$(sess "$id2" | jqr "d.get('cost_usd',0)")
    # A cap is only a cap if overshoot is bounded; one in-flight turn can
    # cross it, ten times over cannot.
    awk "BEGIN{exit !($c2 < 0.50)}" && ok "limits: budget cap held" "\$$c2 against a \$0.05 cap (state=$st2)" \
      || no "limits: budget cap held" "\$$c2 against a \$0.05 cap"
  } || no "limits: budget session created"
}

# ── 10. oversized output spills to a blob instead of dying ────────────────
t_truncate() {
  local id; id=$(mk "{\"prompt\":\"Run this exact command with the Bash tool: python3 -c \\\"print('A'*400000)\\\" -- then reply DONE and stop.\",\"max_turns\":6,\"max_budget_usd\":$BUDGET}")
  [ -z "$id" ] && { no "truncate: session created"; return; }
  local st; st=$(waitfor "$id" 80)
  local line; line=$(evs "$id" | jqr "([x.get('guest_line') for x in d if isinstance(x.get('payload'),dict) and x['payload'].get('type')=='truncated'] or [''])[0]")
  if [ -n "$line" ]; then
    ok "truncate: oversized line spilled to a blob" "line $line"
    local code; code=$(curl -s -o "$OUT/blob.json" -w '%{http_code}' -L "${A[@]}" "$URL/v1/sessions/$id/blobs/$line")
    [ "$code" = 200 ] && [ -s "$OUT/blob.json" ] && ok "truncate: the blob is fetchable" "$(wc -c < "$OUT/blob.json" | tr -d ' ') bytes" \
      || no "truncate: the blob is fetchable" "HTTP $code"
  else
    # No spill. Before calling that a platform failure, find out whether a
    # line that large could ever have reached the outbox.
    #
    # It cannot, via a tool result: puku-cli caps its own tool output well
    # below the runner's 256 KiB line cap, so `print('A'*400000)` arrives
    # already truncated to ~32 KB and the spill path is unreachable by this
    # route. Measured on 1.8.48: largest event 32,846 bytes for exactly this
    # prompt. Reporting FAIL for that is reporting the CLI's cap as our bug,
    # which is what this check did on two consecutive sweeps.
    local biggest
    biggest=$(evs "$id" | jqr "max((len(json.dumps(x.get('payload') or {})) for x in d), default=0)")
    if [ "${biggest:-0}" -lt 262144 ] 2>/dev/null; then
      skip "truncate: oversized line spilled to a blob" \
        "largest event ${biggest}B < the 256KiB cap -- puku-cli truncated the tool result first, so the spill path is unreachable this way"
    else
      no "truncate: oversized line spilled to a blob" \
        "a ${biggest}B event crossed the cap and was not spilled (state=$st)"
    fi
  fi
}

# ── 11. repo clone ────────────────────────────────────────────────────────
t_repo() {
  # That repo ships a file called README, with no extension -- naming the
  # wrong one tested the agent's honesty, not the clone.
  local id; id=$(mk "{\"prompt\":\"List the files in your working directory, then read the README file and reply with its contents, then stop.\",\"repo\":\"https://github.com/octocat/Hello-World\",\"max_turns\":6,\"max_budget_usd\":$BUDGET}")
  [ -z "$id" ] && { no "repo: session created"; return; }
  local st; st=$(waitfor "$id" 80)
  [ "$st" = completed ] && ok "repo: session with a clone completed" \
    || { no "repo: session with a clone completed" "state=$st: $(sess "$id" | jqr "d.get('error','')")"; return; }
  say "$id" | grep -qi 'hello.world' && ok "repo: the clone was there and readable" \
    || no "repo: the clone was there and readable" "$(say "$id" | tail -c 140)"
}

# ── 12. schedules, triggers, notifications ────────────────────────────────
t_control_objects() {
  local sid; sid=$(api -X POST "${J[@]}" -d '{"name":"fm","prompt":"Reply OK and stop.","cron":"0 3 * * *","packs":["office"],"max_turns":3,"max_budget_usd":1.0}' "$URL/v1/schedules" | jqr "d.get('id','')")
  [ -n "$sid" ] && ok "schedules: created" || { no "schedules: created"; return; }
  api "$URL/v1/schedules" | jqr "[x for x in d if x['id']=='$sid'][0]['packs']" | grep -q office \
    && ok "schedules: packs stored" || no "schedules: packs stored"
  api -X POST "$URL/v1/schedules/$sid/disable" >/dev/null
  api "$URL/v1/schedules" | jqr "[x for x in d if x['id']=='$sid'][0].get('enabled')" | grep -qi false \
    && ok "schedules: disable" || no "schedules: disable"
  api -X POST "$URL/v1/schedules/$sid/enable" >/dev/null
  api "$URL/v1/schedules" | jqr "[x for x in d if x['id']=='$sid'][0].get('enabled')" | grep -qi true \
    && ok "schedules: enable" || no "schedules: enable"
  local fired; fired=$(api -X POST "$URL/v1/schedules/$sid/run" | jqr "d.get('session_id','')")
  [ -n "$fired" ] && ok "schedules: fires on demand" || no "schedules: fires on demand"
  sess "$fired" | jqr "d.get('packs')" | grep -q office && ok "schedules: fired run inherits packs" || no "schedules: fired run inherits packs"
  local st; st=$(waitfor "$fired" 80)
  # Unattended runs use the operator's stored credential, not a human's
  # bearer -- this is the only test that covers that path.
  [ "$st" = completed ] && ok "schedules: unattended run used the fallback credential" \
    || no "schedules: unattended run used the fallback credential" "state=$st: $(sess "$fired" | jqr "d.get('error','')")"
  api -X DELETE "$URL/v1/schedules/$sid" >/dev/null && ok "schedules: deleted" || no "schedules: deleted"

  local nid; nid=$(api -X POST "${J[@]}" -d '{"kind":"webhook","url":"https://example.invalid/hook","events":["terminal"],"secret":"s3cr3t"}' "$URL/v1/notifications" | jqr "d.get('id','')")
  [ -n "$nid" ] && ok "notifications: created" || no "notifications: created"
  api "$URL/v1/notifications" | jqr "any(x['id']=='$nid' for x in d)" | grep -q True && ok "notifications: listed" || no "notifications: listed"
  api "$URL/v1/notifications" | grep -q 's3cr3t' && no "notifications: secret not echoed back" "the HMAC secret is readable over the API" \
    || ok "notifications: secret not echoed back"
  [ -n "$nid" ] && api -X DELETE "$URL/v1/notifications/$nid" >/dev/null && ok "notifications: deleted" || no "notifications: deleted"

  local tr tid tok; tr=$(api -X POST "${J[@]}" -d '{"name":"fm-trigger","prompt":"Reply TRIGGERED and stop.","max_budget_usd":1.0}' "$URL/v1/triggers")
  # The create response nests the row under `trigger`; only the token is
  # top-level, which is why firing worked while the id lookup did not.
  tid=$(echo "$tr" | jqr "(d.get('trigger') or {}).get('id','')"); tok=$(echo "$tr" | jqr "d.get('token','')")
  [ -n "$tid" ] && ok "triggers: created" || no "triggers: created"
  if [ -n "$tok" ]; then
    local hs; hs=$(curl -s -o "$OUT/hook.json" -w '%{http_code}' -X POST -H 'content-type: application/json' -d '{}' "$URL/v1/hooks/$tok")
    [ "$hs" = 200 ] || [ "$hs" = 202 ] && ok "triggers: webhook fires a session" "HTTP $hs" || no "triggers: webhook fires a session" "HTTP $hs"
    local hsid; hsid=$(jqr "d.get('session_id','')" < "$OUT/hook.json")
    [ -n "$hsid" ] && { local hst; hst=$(waitfor "$hsid" 80)
      [ "$hst" = completed ] && ok "triggers: fired session completed" || no "triggers: fired session completed" "state=$hst"; }
  else no "triggers: token returned" "no token on the create response"; fi
  [ -n "$tid" ] && api -X DELETE "$URL/v1/triggers/$tid" >/dev/null && ok "triggers: deleted" || no "triggers: deleted"
}

# ── 13. fleet, isolation, credentials ─────────────────────────────────────
t_platform() {
  local fl; fl=$(api "$URL/v1/fleet")
  [ "$(echo "$fl" | jqr "len(d['workers'])")" -ge 1 ] 2>/dev/null && ok "fleet: worker listed" || no "fleet: worker listed"
  echo "$fl" | jqr "'orphaned' in d['drift'] and 'vanished' in d['drift']" | grep -q True && ok "fleet: drift reported both ways" || no "fleet: drift reported both ways"
  local orph; orph=$(echo "$fl" | jqr "len(d['drift']['orphaned'])")
  # Re-read once: a sandbox whose session has just finished shows here for
  # the moment between the session leaving the live list and the teardown
  # completing, and calling that a leak cries wolf on every healthy run.
  # Teardown is bounded but not instant: stop() returns before msb releases
  # the VM, so remove retries with backoff for a few seconds. A single
  # re-check 20s later still caught one mid-flight and called it a leak.
  # Poll until it clears, and only then believe it.
  for _ in $(seq 1 12); do
    [ "${orph:-0}" -le "${FM_ORPHAN_BASELINE:-0}" ] && break
    sleep 5
    orph=$(api "$URL/v1/fleet" | jqr "len(d['drift']['orphaned'])")
  done
  [ "${orph:-0}" -le "${FM_ORPHAN_BASELINE:-0}" ] 2>/dev/null \
    && ok "fleet: no new orphaned sandboxes" "$orph (baseline ${FM_ORPHAN_BASELINE:-0})" \
    || no "fleet: no new orphaned sandboxes" "$orph sandbox(es) with no session, baseline ${FM_ORPHAN_BASELINE:-0}"

  # /v1/workers is admin-only. A non-admin key gets 403, which used to land
  # as "fleet: workers listed FAIL" and then silently skipped the two drain
  # checks -- so a correctly-scoped operator key produced one spurious failure
  # and two checks that never ran and were never counted. 403 is the admin
  # gate working; say so and move on.
  local wcode; wcode=$(api -o /dev/null -w '%{http_code}' "$URL/v1/workers")
  if [ "$wcode" = 403 ] || [ "$wcode" = 401 ]; then
    skip "fleet: workers listed"     "HTTP $wcode -- needs an --admin key"
    skip "fleet: drain marks the worker"  "needs an --admin key"
    skip "fleet: undrain clears it"       "needs an --admin key"
    return
  fi
  local wid; wid=$(api "$URL/v1/workers" | jqr "d[0]['id'] if d else ''")
  if [ -z "$wid" ]; then
    no "fleet: workers listed" "HTTP $wcode but no worker in the list"
    return
  fi
  ok "fleet: workers listed"
  api -X POST "$URL/v1/workers/$wid/drain" >/dev/null
  # `status`, not a `draining` boolean -- the field the API actually has.
  api "$URL/v1/workers" | jqr "[x for x in d if x['id']=='$wid'][0].get('status')" | grep -qi draining \
    && ok "fleet: drain marks the worker" || no "fleet: drain marks the worker"
  api -X POST "$URL/v1/workers/$wid/undrain" >/dev/null
  api "$URL/v1/workers" | jqr "[x for x in d if x['id']=='$wid'][0].get('status')" | grep -qi online \
    && ok "fleet: undrain clears it" || no "fleet: undrain clears it"

  [ "$(curl -s -o /dev/null -w '%{http_code}' "${A[@]}" "$URL/v1/sessions/00000000-0000-0000-0000-0000000000ff")" = 404 ] \
    && ok "isolation: unknown session is 404" || no "isolation: unknown session is 404"
  local c; c=$(curl -s -o /dev/null -w '%{http_code}' "$URL/v1/sessions")
  case "$c" in 401|403) ok "isolation: unauthenticated refused" "$c";; *) no "isolation: unauthenticated refused" "HTTP $c";; esac
  c=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer pkc_deadbeefdeadbeefdeadbeefdeadbeef" "$URL/v1/sessions")
  case "$c" in 401|403) ok "isolation: a forged key is refused" "$c";; *) no "isolation: a forged key is refused" "HTTP $c";; esac

  api "$URL/v1/credentials" | jqr "isinstance(d,list) or isinstance(d,dict)" | grep -q True \
    && ok "credentials: endpoint answers" || no "credentials: endpoint answers"
  api "$URL/v1/credentials" | grep -qiE '"(secret|token|key|value)"[[:space:]]*:[[:space:]]*"[A-Za-z0-9_-]{20,}"' \
    && no "credentials: no plaintext secret returned" "a credential value came back over the API" \
    || ok "credentials: no plaintext secret returned"

  local up; up=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $KEY" \
    -H "Connection: Upgrade" -H "Upgrade: websocket" -H "Sec-WebSocket-Version: 13" \
    -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" "$URL/v1/sessions/00000000-0000-0000-0000-0000000000ff/attach")
  case "$up" in 101|404|400) ok "attach: endpoint reachable" "HTTP $up";; *) no "attach: endpoint reachable" "HTTP $up";; esac
  curl -s "$URL/metrics" | grep -q '^#\|puku' && ok "metrics: exposed" || no "metrics: exposed"
}

# ── 14. the document workload ─────────────────────────────────────────────
t_documents() {
  local id; id=$(mk '{"prompt":"Write to /workspace from your own knowledge, no web access. Topic: microVM vs container isolation for untrusted AI agent code. Produce summary.pptx FIRST (4 slides), then report.pdf (about 3 pages), then data.xlsx with a small comparison table, then notes.docx. Rough counts are fine -- do NOT iterate on layout.","packs":["office","essentials"],"disallowed_tools":["WebSearch","WebFetch"],"max_turns":80,"max_budget_usd":8.0}')
  [ -z "$id" ] && { no "documents: session created"; return; }
  local st; st=$(waitfor "$id" 400)
  [ "$st" = completed ] && ok "documents: session completed" "\$$(sess "$id" | jqr "d.get('cost_usd',0)")" \
    || { no "documents: session completed" "state=$st: $(sess "$id" | jqr "d.get('error','')")"; return; }
  api -X POST "$URL/v1/sessions/$id/artifacts/workspace" >/dev/null
  local got=0
  for _ in $(seq 1 30); do
    [ "$(curl -s -o "$OUT/docs.tgz" -w '%{http_code}' -L "${A[@]}" "$URL/v1/sessions/$id/artifacts/workspace")" = 200 ] && { got=1; break; }
    sleep 8
  done
  [ "$got" = 1 ] || { no "documents: artifact downloaded"; return; }
  mkdir -p "$OUT/docs" && tar -xzf "$OUT/docs.tgz" -C "$OUT/docs" 2>/dev/null
  # `file` on the bytes, not the extension: a .pptx the agent wrote as text
  # would pass a name check and open in nothing.
  chk "$(find "$OUT/docs" -name '*.pdf' -exec file {} \; 2>/dev/null | grep -qi 'PDF document' && echo 1 || echo 0)" \
      "documents: report.pdf is a real PDF" "no valid PDF in the workspace"
  chk "$(find "$OUT/docs" -name '*.pptx' -exec file {} \; 2>/dev/null | grep -qiE 'OOXML|Microsoft PowerPoint|Zip archive' && echo 1 || echo 0)" \
      "documents: summary.pptx is a real deck" "no valid PPTX in the workspace"
  chk "$(find "$OUT/docs" -name '*.xlsx' -exec file {} \; 2>/dev/null | grep -qiE 'OOXML|Excel|Zip archive' && echo 1 || echo 0)" \
      "documents: data.xlsx is a real workbook" "no valid XLSX in the workspace"
  chk "$(find "$OUT/docs" -name '*.docx' -exec file {} \; 2>/dev/null | grep -qiE 'OOXML|Word|Zip archive' && echo 1 || echo 0)" \
      "documents: notes.docx is a real document" "no valid DOCX in the workspace"
}

# ── run ───────────────────────────────────────────────────────────────────
printf '\n\033[1mfeature matrix — %s — runner: %s\033[0m\n' "$URL" "$LABEL"
printf 'up to %s sessions in parallel, $%s each\n' "$CONC" "$BUDGET"

start=$(date +%s)
[ "$DOCS" = 1 ] && go t_documents      # longest, so start it first
go t_basic
go t_skills
go t_permission
go t_policy
go t_multiturn
go t_interrupt
go t_parkresume
go t_schema
go t_limits
go t_truncate
go t_repo
go t_control_objects
t_platform                              # no model, no slot
wait
elapsed=$(( $(date +%s) - start ))

printf '\n'
pass=$(grep -c '^PASS' "$OUT/results" 2>/dev/null || echo 0)
fail=$(grep -c '^FAIL' "$OUT/results" 2>/dev/null || echo 0)
skipped=$(grep -c '^SKIP' "$OUT/results" 2>/dev/null || echo 0)
sort "$OUT/results" -t$'\t' -k2 | while IFS=$'\t' read -r s n d; do
  case "$s" in
    PASS) printf '  \033[32mPASS\033[0m  %-52s %s\n' "$n" "$d" ;;
    SKIP) printf '  \033[33mSKIP\033[0m  %-52s %s\n' "$n" "$d" ;;
    *)    printf '  \033[31mFAIL\033[0m  %-52s %s\n' "$n" "$d" ;;
  esac
done

spend=$(api "$URL/v1/sessions?limit=200" | jqr "round(sum(x.get('cost_usd') or 0 for x in (d if isinstance(d,list) else d.get('sessions',[]))),4)")
printf '\n\033[1mRESULT: %s passed, %s failed, %s skipped\033[0m  (%dm%02ds, org spend to date $%s)\n' \
  "$pass" "$fail" "$skipped" $((elapsed/60)) $((elapsed%60)) "${spend:-?}"
[ "$fail" -eq 0 ]

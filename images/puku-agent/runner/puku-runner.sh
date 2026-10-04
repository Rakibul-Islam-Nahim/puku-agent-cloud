#!/usr/bin/env bash
# In-guest session supervisor. Started by puku-workerd via exec after boot.
#
# Contract with the host:
#   /session/manifest.json   read : session parameters (written by workerd)
#   /session/stdin.fifo      read : stream-json input lines arrive here
#   /session/events.ndjson   write: every puku-cli stream-json line, 1/line —
#                                   the outbox the host tails; line number is
#                                   the event's identity, so append-only, never
#                                   rewrite
#   /session/home            puku-cli state ($HOME/.puku-cli), survives resume
#   /workspace               working directory, survives resume
set -uo pipefail

SESSION=/session
MANIFEST="$SESSION/manifest.json"
EVENTS="$SESSION/events.ndjson"
FIFO="$SESSION/stdin.fifo"

# Cap any single event line; oversized payloads spill to a blob file the
# host uploads (M2), keeping Postgres rows bounded.
MAX_LINE_BYTES=262144

die() { echo "puku-runner: $*" >&2; exit 70; }

[ -f "$MANIFEST" ] || die "manifest missing"

jqm() { jq -r "$1 // empty" "$MANIFEST"; }

PROMPT="$(jqm .prompt)"
REPO="$(jqm .repo)"
BRANCH="$(jqm .branch)"
GIT_TOKEN="$(jqm .git_token)"
MODEL="$(jqm .model)"
BUDGET="$(jqm .max_budget_usd)"
RESUME="$(jqm .resume)"
PUKU_SESSION_ID="$(jqm .puku_session_id)"

# Durable HOME so puku-cli session state survives park/resume.
mkdir -p "$SESSION/home" "$SESSION/blobs"
export HOME="$SESSION/home"
cd /workspace

# Clone on first run only; the workspace volume persists across resumes.
if [ -n "$REPO" ] && [ ! -d /workspace/.git ] && [ "$RESUME" != "true" ]; then
  clone_url="$REPO"
  if [ -n "$GIT_TOKEN" ]; then
    clone_url="$(echo "$REPO" | sed "s#https://#https://x-access-token:${GIT_TOKEN}@#")"
  fi
  git clone ${BRANCH:+--branch "$BRANCH"} "$clone_url" /workspace/repo \
    || die "git clone failed"
  # Keep the token out of .git/config and out of the manifest on disk.
  (cd /workspace/repo && git remote set-url origin "$REPO")
  cd /workspace/repo
fi
[ -d /workspace/repo ] && cd /workspace/repo

# Teleport: a transcript lifted from a local puku-cli run, so `--resume`
# continues that conversation here instead of starting an empty one.
#
# puku-cli resolves a resumed transcript to
#   $HOME/.puku-cli/projects/<sanitize($PWD)>/<session-id>.jsonl
# and looks ONLY in the directory derived from the current working
# directory (sessionStorage.ts: sessionIdExists / getProjectDir). The laptop's
# cwd and this VM's cwd differ, so the file has to be re-homed under the
# guest's own slug. sanitizePath is `replace(/[^a-zA-Z0-9]/g, '-')`, which is
# exactly the sed below. This runs after the clone, so $PWD is final.
IMPORTED="$SESSION/import/transcript.jsonl"
if [ -f "$IMPORTED" ] && [ -n "$PUKU_SESSION_ID" ]; then
  slug="$(printf '%s' "$PWD" | sed 's/[^a-zA-Z0-9]/-/g')"
  project_dir="$HOME/.puku-cli/projects/$slug"
  mkdir -p "$project_dir"
  # Never clobber a transcript the session already accumulated: on a resume
  # after park, the volume's own copy is newer than the imported one.
  if [ ! -f "$project_dir/$PUKU_SESSION_ID.jsonl" ]; then
    cp "$IMPORTED" "$project_dir/$PUKU_SESSION_ID.jsonl"
    echo "puku-runner: imported transcript -> $project_dir/$PUKU_SESSION_ID.jsonl" >&2
  fi
fi

# Shred the token now that the clone is done.
if [ -n "$GIT_TOKEN" ]; then
  jq 'del(.git_token)' "$MANIFEST" > "$MANIFEST.tmp" && mv "$MANIFEST.tmp" "$MANIFEST"
fi

# Egress explainer proxy. The VM's network policy is the real enforcement
# and denies at the DNS layer, which reaches the agent as "Could not resolve
# host" -- indistinguishable from a broken network, so it retries and invents
# workarounds. Routing through a local proxy that answers 403 makes a policy
# denial say so. Allowed domains go in NO_PROXY and never touch the proxy, so
# nothing is intercepted and no MITM CA is needed.
EGRESS_ALLOW="$(jq -r '(.egress_allow // []) | join(",")' "$MANIFEST")"
if [ -n "$EGRESS_ALLOW" ] && [ -x /usr/local/bin/puku-egress-proxy ]; then
  PUKU_EGRESS_ALLOW="$EGRESS_ALLOW" PUKU_EGRESS_PROXY_PORT=3128 \
    /usr/local/bin/puku-egress-proxy >>"$SESSION/proxy.log" 2>&1 &
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    (exec 9<>/dev/tcp/127.0.0.1/3128) 2>/dev/null && break
    sleep 0.2
  done
  export HTTP_PROXY=http://127.0.0.1:3128 HTTPS_PROXY=http://127.0.0.1:3128
  export http_proxy="$HTTP_PROXY" https_proxy="$HTTPS_PROXY"
  # Allowed hosts bypass the proxy entirely; localhost must never be proxied.
  export NO_PROXY="localhost,127.0.0.1,::1,$EGRESS_ALLOW"
  export no_proxy="$NO_PROXY"
fi

# Skill packs are materialized into the guest HOME by workerd before this
# runs. puku-cli discovers $HOME/.puku-cli/skills on its own, but the skills
# themselves reference shared files as $SKILLS_ROOT/... — the office pack's
# pptx skill calls $SKILLS_ROOT/scripts/extract-text — so the variable has
# to exist or every one of those paths is empty.
export SKILLS_ROOT="$HOME/.puku-cli/skills"
if [ -d "$SKILLS_ROOT" ]; then
  echo "puku-runner: skills at $SKILLS_ROOT: $(ls -1 "$SKILLS_ROOT" 2>/dev/null | tr '\n' ' ')" >&2
fi

# stdin fifo: held open on fd 3 so puku-cli never sees EOF between inputs.
[ -p "$FIFO" ] || mkfifo "$FIFO"

# --permission-prompt-tool stdio is what ROUTES permission asks to the
# stdin control channel as `control_request` frames. Without it puku-cli
# auto-decides every ask (safe -> allow, out-of-bounds -> deny) and
# AskUserQuestion/ExitPlanMode auto-deny, so the platform's whole
# waiting_input flow never fires. It is a hidden flag (absent from --help,
# accepted by 1.8.43) -- verified before this was wired in.
args=(
  -p
  --output-format stream-json
  --input-format stream-json
  --permission-prompt-tool stdio
  --include-partial-messages
  --verbose
)
[ -n "$MODEL" ] && args+=(--model "$MODEL")
[ -n "$BUDGET" ] && args+=(--max-budget-usd "$BUDGET")

# Tool policy. controld has already clamped these to the deployment's
# ceiling -- the guest applies, it never decides. An absent permission_mode
# means the platform did not express one; fall back to the previous
# behaviour so an old controld keeps working through one release.
PERMISSION_MODE="$(jqm .permission_mode)"
if [ -n "$PERMISSION_MODE" ]; then
  args+=(--permission-mode "$PERMISSION_MODE")
else
  args+=(--god-mode)
fi

# --allowed-tools/--disallowed-tools are VARIADIC: they swallow every
# following token until the next --flag, so each list is emitted as one
# comma-joined argument (puku-cli accepts "comma or space-separated").
ALLOWED_TOOLS="$(jq -r '(.allowed_tools // []) | join(",")' "$MANIFEST")"
DISALLOWED_TOOLS="$(jq -r '(.disallowed_tools // []) | join(",")' "$MANIFEST")"
[ -n "$ALLOWED_TOOLS" ] && args+=(--allowed-tools "$ALLOWED_TOOLS")
[ -n "$DISALLOWED_TOOLS" ] && args+=(--disallowed-tools "$DISALLOWED_TOOLS")

# Brokered connectors. Written as an --mcp-config file rather than into
# settings.json because puku-cli loads it regardless of what PUKU_CONFIG_DIR
# ends up being (the same reason puku-cowork switched). --strict-mcp-config
# means ONLY these servers load: a cloned repo can ship its own .mcp.json,
# and that file is attacker-controlled input as far as this VM is concerned.
#
# The Authorization header holds the literal `${PUKU_API_KEY}`, which
# puku-cli expands from the env — no credential is written to this file.
if [ "$(jq -r '(.mcp_servers // []) | length' "$MANIFEST")" -gt 0 ]; then
  jq '{mcpServers: (.mcp_servers | map({(.name): {type: .type, url: .url, headers: .headers}}) | add)}' \
    "$MANIFEST" > "$SESSION/mcp.json"
  args+=(--mcp-config "$SESSION/mcp.json" --strict-mcp-config)
fi

# DIAGNOSTIC (Wave 1 / Task 1.1, removed at 1.7): see runner.mjs above for why.
echo "puku-runner: pre-memory cwd=$(pwd) workspace=/workspace session=$SESSION" >&2

# Memory recalled from previous sessions on this repository, delivered as
# PROJECT CONTEXT rather than as a system prompt.
#
# This passed --append-system-prompt-file, and the model never saw a word of it:
# the gateway discards the caller's system prompt and substitutes its own.
# Measured, with nothing of ours in the path, a 47,526-byte system field
# produces the same input_tokens (796) as sending none at all. So
# --append-system-prompt[-file] and --system-prompt are inert there.
#
# puku-cli walks up from its working directory for PUKU.md, so writing it in
# the workspace root reaches the model from above the checkout -- and a project
# that keeps its own PUKU.md gets BOTH, measured. Never write it INSIDE the
# checkout: the agent could stage and commit it, and it would overwrite what
# the team wrote. --add-dir was tried and does not load a PUKU.md at all.
#
# The framing is load-bearing, not decoration. This text was synthesised from
# earlier sessions that ran model-authored code and may have read untrusted
# pages, so it is presented as BACKGROUND. It must never read as a command.
#
# Kept in step with runner/runner.mjs on purpose: this runner ships in the image
# and PUKU_RUNNER_CMD can select it, and a silent divergence between the two is
# how memory was lost here in the first place.
MEMORY_PREAMBLE="$(jqm .memory_preamble)"
if [ -n "$MEMORY_PREAMBLE" ]; then
  printf '%s\n' "$MEMORY_PREAMBLE" > "$SESSION/memory.md"
  # DIAGNOSTIC (Wave 1 / Task 1.2, removed at 1.7): bash continues on a
  # redirection failure under `set +e`, but $? carries the failure code only
  # if we read it on the same line. Without this, the host-side failure mode
  # is "stderr says NNN bytes, workspace is empty" with no error visible.
  if ! printf '%s\n' "$MEMORY_PREAMBLE" > /workspace/PUKU.md; then
    rc=$?
    echo "puku-runner: PUKU.md write FAILED path=/workspace/PUKU.md rc=$rc" >&2
  fi
  # info/exclude is local to the clone and never committed, so the page cannot
  # be staged in the one layout where the workspace root is itself a checkout.
  [ -d /workspace/.git ] && printf '\n/PUKU.md\n' >> /workspace/.git/info/exclude 2>/dev/null
  echo "puku-runner: memory preamble ${#MEMORY_PREAMBLE} bytes" >&2
fi

# Without --max-turns puku-cli yields after a single model response, which
# for an unattended cloud session reads as "the agent did nothing".
# It also terminates the variadic --mcp-config above.
MAX_TURNS="$(jqm .max_turns)"
[ -n "$MAX_TURNS" ] && args+=(--max-turns "$MAX_TURNS")

# Structured output. The API accepts output_schema, stores it, and puts it on
# the spec; this runner used to drop it on the floor, so a caller who asked
# for validated JSON got prose and nothing said otherwise. Observed: a
# session with a {verdict, score} schema answered {"answer": true}.
#
# Passed compact on one line -- the flag takes the schema itself, not a path.
OUTPUT_SCHEMA="$(jq -c '.output_schema // empty' "$MANIFEST")"
[ -n "$OUTPUT_SCHEMA" ] && args+=(--json-schema "$OUTPUT_SCHEMA")

if [ "$RESUME" = "true" ] && [ -n "$PUKU_SESSION_ID" ]; then
  args+=(--resume "$PUKU_SESSION_ID")
fi

# line-capper: truncate oversized lines, spill full content to /session/blobs.
cap_lines() {
  local n=0
  while IFS= read -r line; do
    n=$((n + 1))
    if [ "${#line}" -gt "$MAX_LINE_BYTES" ]; then
      printf '%s' "$line" > "$SESSION/blobs/line-$n.json"
      printf '{"type":"truncated","blob":"line-%s.json","bytes":%s}\n' "$n" "${#line}"
    else
      printf '%s\n' "$line"
    fi
  done
}

# Belt-and-braces secret redaction, scrubbing the stream before it leaves
# the VM. Under secret_env the guest holds only `$MSB_…` placeholders, so
# this matters mainly for PUKU_INSECURE_PLAIN_KEY runs and for the session
# file workerd plants in HOME.
#
# Puku credentials are opaque strings with no distinguishing prefix, so
# shape regexes can't find them; match the literal values we can read at
# startup instead. Exact, but blind to a token puku-cli refreshes later in
# the session — the placeholder mechanism is the real defence.
redact_exprs=()
add_secret() {
  [ -n "${1:-}" ] || return 0
  [ "${#1}" -ge 12 ] || return 0
  redact_exprs+=(-e "s|$(printf '%s' "$1" | sed -e 's/[]\/$*.^[]/\\&/g')|[redacted-puku-credential]|g")
}
add_secret "${PUKU_AI_API_KEY:-}"
add_secret "${PUKU_CLI_OAUTH_TOKEN:-}"
if [ -f "$HOME/.config/pukucode/session.json" ]; then
  add_secret "$(jq -r '.accessToken // empty' "$HOME/.config/pukucode/session.json")"
  add_secret "$(jq -r '.refreshToken // empty' "$HOME/.config/pukucode/session.json")"
fi

redact() {
  # ${a[@]+"${a[@]}"}: an empty array is an unbound variable under `set -u`
  # on bash < 4.4, and this runs with no credentials at all in stub mode.
  sed -uE ${redact_exprs[@]+"${redact_exprs[@]}"} \
    -e 's/sk-ant-[A-Za-z0-9_-]{8,}/[redacted-anthropic-key]/g' \
    -e 's/(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}/[redacted-github-token]/g' \
    -e 's/github_pat_[A-Za-z0-9_]{20,}/[redacted-github-token]/g' \
    -e 's/AKIA[0-9A-Z]{16}/[redacted-aws-key]/g' \
    -e 's/xox[baprs]-[A-Za-z0-9-]{10,}/[redacted-slack-token]/g'
}

# The launch wrapper (written by workerd) appends the exec.exited marker to
# the outbox when this script exits; the exit code must be puku-cli's.
exec 3<>"$FIFO"

# With --input-format stream-json, puku-cli ignores positional prompts and
# reads user turns from stdin — so the task goes in as the first message.
#
# This is gated on the prompt alone, NOT on `resume`. A resumed session
# usually has no prompt (park/resume just continues), but a teleported one
# does: it carries the follow-up turn that motivated moving to the cloud.
# Skipping delivery there left the agent parked on an empty stdin forever,
# looking "running" while doing nothing.
if [ -n "$PROMPT" ]; then
  jq -cn --arg t "$PROMPT" \
    '{type:"user",message:{role:"user",content:[{type:"text",text:$t}]}}' >&3
fi
puku-cli "${args[@]}" <&3 2>>"$SESSION/runner.stderr" | redact | cap_lines >> "$EVENTS"
code=${PIPESTATUS[0]}
exec 3<&-
exit "$code"

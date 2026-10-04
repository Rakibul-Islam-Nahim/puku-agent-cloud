#!/usr/bin/env bash
# A deterministic stand-in for puku-cli, used only by the runner parity
# harness. It speaks just enough of the stream-json contract to prove that
# two runners produce the same outbox: an init line carrying session_id, an
# assistant turn, an oversized line to exercise blob truncation, and a
# terminal result with the usage keys the platform bills on.
#
# Nothing here needs a credential or a network, so the parity gate can run in
# CI. Both runners are pointed at it the same way puku-cli would be found:
# the bash runner by PATH, the SDK runner by PUKU_CLI_PATH.
set -u

SID="${FAKE_SESSION_ID:-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}"

# The SDK always passes --session-id for a fresh session and the real CLI
# adopts it, so honour that or the two runners disagree on session_id alone.
prev=""
for a in "$@"; do
  case "$prev" in
    --session-id|--resume) SID="$a" ;;
  esac
  prev="$a"
done

emit() { printf '%s\n' "$1"; }

# Record the argv we were invoked with, so the harness can assert on flags
# without parsing a process table.
[ -n "${FAKE_ARGV_OUT:-}" ] && printf '%s\n' "$*" > "$FAKE_ARGV_OUT"


# Report whatever skills were actually materialized, the way the real CLI
# does, so the harness can assert on init.skills rather than on `ls`.
skills_json="[]"
if [ -d "$HOME/.puku-cli/skills" ]; then
  skills_json=$(ls -1 "$HOME/.puku-cli/skills" 2>/dev/null \
    | grep -v '\.md$' \
    | sed 's/.*/"&"/' | paste -sd, - )
  skills_json="[${skills_json}]"
fi

emit "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SID\",\"puku_cli_version\":\"${FAKE_CLI_VERSION:-1.8.49}\",\"model\":\"fake\",\"tools\":[],\"skills\":${skills_json},\"mcp_servers\":[],\"cwd\":\"$PWD\",\"permissionMode\":\"default\",\"apiKeySource\":\"none\",\"uuid\":\"11111111-1111-1111-1111-111111111111\"}"

# Leak mode: an agent that prints its own credentials, plus a few
# recognisable third-party shapes. Redaction must catch all of them before
# the bytes leave the VM.
if [ "${FAKE_LEAK:-0}" = "1" ]; then
  emit "{\"type\":\"assistant\",\"session_id\":\"$SID\",\"parent_tool_use_id\":null,\"error\":null,\"uuid\":\"88888888-8888-8888-8888-888888888888\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"env=${PUKU_AI_API_KEY:-none} anth=sk-ant-abcdefghijklmnop gh=ghp_0123456789012345678901234567890123 aws=AKIAIOSFODNN7EXAMPLE slack=xoxb-1234567890-abcdefg\"}]}}"
fi

# Garbage mode: a non-JSON line on stdout, which is what a banner, a
# progress bar or a stray warning from the real CLI would look like.
[ "${FAKE_GARBAGE:-0}" = "1" ] && printf '%s\n' "WARNING: this is not JSON at all"

emit "{\"type\":\"assistant\",\"session_id\":\"$SID\",\"parent_tool_use_id\":null,\"error\":null,\"uuid\":\"22222222-2222-2222-2222-222222222222\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ORCHID\"}]}}"

# One line past MAX_LINE_BYTES (262144) so both runners must spill a blob and
# replace it with the same `truncated` envelope.
if [ "${FAKE_BIG:-1}" = "1" ]; then
  big="$(head -c 300000 /dev/zero | tr '\0' 'x')"
  emit "{\"type\":\"assistant\",\"session_id\":\"$SID\",\"parent_tool_use_id\":null,\"error\":null,\"uuid\":\"33333333-3333-3333-3333-333333333333\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"$big\"}]}}"
fi

# Hang mode: keep the turn open so an interrupt has something to interrupt,
# and answer the SDK's interrupt control_request the way the real CLI does --
# otherwise interrupt() waits forever for an ack that never comes.
#
# Foreground with a timed read, deliberately: a backgrounded `( ... ) &` gets
# /dev/null as stdin in a non-interactive shell, so `read` returns EOF at once
# and the turn does not stay open at all.
if [ "${FAKE_HANG:-0}" = "1" ]; then
  n=0
  while [ "$n" -lt "${FAKE_HANG_SECS:-45}" ]; do
    if IFS= read -r -t 1 line; then
      case "$line" in
        *'"interrupt"'*)
          rid=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
          emit "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$rid\",\"response\":{}}}"
          emit "{\"type\":\"assistant\",\"session_id\":\"$SID\",\"parent_tool_use_id\":null,\"error\":null,\"uuid\":\"77777777-7777-7777-7777-777777777777\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"INTERRUPTED\"}]}}"
          break ;;
      esac
    fi
    n=$((n + 1))
  done
fi

# Ask mode: emit a can_use_tool control_request and block until the answering
# control_response arrives on stdin, which is exactly what the real CLI does
# under --permission-prompt-tool stdio. Lets the permission path be tested
# end to end with no credential and no network.
if [ "${FAKE_ASK:-0}" = "1" ]; then
  emit "{\"type\":\"control_request\",\"request_id\":\"req-1\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"${FAKE_ASK_TOOL:-AskUserQuestion}\",\"tool_use_id\":\"tu-1\",\"input\":{\"questions\":[{\"header\":\"Bucket\",\"question\":\"which bucket?\"}]}}}"
  while IFS= read -r line; do
    case "$line" in
      *'"control_response"'*)
        esc=$(printf '%s' "$line" | sed 's/\\/\\\\/g; s/"/\\"/g')
        emit "{\"type\":\"assistant\",\"session_id\":\"$SID\",\"parent_tool_use_id\":null,\"error\":null,\"uuid\":\"66666666-6666-6666-6666-666666666666\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"answered:$esc\"}]}}"
        break ;;
    esac
  done
fi

# Echo mode: read stdin turns and mirror them back, so the harness can prove
# the host->guest input path reaches the CLI at all.
if [ "${FAKE_ECHO:-0}" = "1" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"__stop__"'*) break ;;
    esac
    esc=$(printf '%s' "$line" | sed 's/\\/\\\\/g; s/"/\\"/g')
    emit "{\"type\":\"assistant\",\"session_id\":\"$SID\",\"parent_tool_use_id\":null,\"error\":null,\"uuid\":\"55555555-5555-5555-5555-555555555555\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"echo:$esc\"}]}}"
  done
fi

emit "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"$SID\",\"duration_ms\":1,\"duration_api_ms\":1,\"num_turns\":1,\"result\":\"ORCHID\",\"stop_reason\":\"end_turn\",\"total_cost_usd\":0.0123,\"usage\":{\"input_tokens\":11,\"output_tokens\":22,\"cache_read_input_tokens\":33,\"cache_creation_input_tokens\":44},\"modelUsage\":{},\"permission_denials\":[],\"fast_mode_state\":\"off\",\"uuid\":\"44444444-4444-4444-4444-444444444444\"}"

exit "${FAKE_EXIT:-0}"

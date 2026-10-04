# Driving the guest agent with puku-agent-sdk

State of the migration from `puku-runner.sh` (bash, hand-rolled stream-json)
to `runner.mjs` (the SDK). **`runner.mjs` is the default**
(`DEFAULT_RUNNER_CMD` in `crates/puku-workerd/src/session_actor.rs`).

`sdk-gate.sh` against the live deployment, puku-cli 1.8.48:

| Gate item | Result |
| --- | --- |
| 1. Agent can see its skills (from `system/init`) | PASS |
| 2. Cost and token counters recorded | PASS |
| 3. Permission prompt reaches a human **and the answer lands** | PASS |
| 4. Interrupt ends the turn without killing the session | PASS |

`feature-matrix.sh` on the same worker is at parity with the bash runner.

**Item 3 failed once, and the gate was wrong, not the runner.** The check
grepped the transcript for the agent's reply the instant the session left
`waiting_input` -- which happens when the answer is *delivered*, before the
model has written anything. On the strength of that FAIL the default was
reverted for a day. The failing session's own transcript ends "You chose
**staging**.", and the gate's own expression returns exactly that when run
after the fact. Fixed to poll for the answer rather than race it.

**Why prefer the SDK runner:** `AskUserQuestion`. The bash runner cannot
route it on 1.8.48 -- `--permission-prompt-tool stdio` is still *accepted*
but silently stopped routing that one tool, so the agent asks, nothing
reaches the outbox, and the CLI answers itself ("the question wasn't
answered"). That flag was only ever verified here against 1.8.43. The SDK
runner owns the control channel through `canUseTool` and does not depend on
it.

`puku-runner.sh` remains in the image. `PUKU_RUNNER_CMD='exec
/usr/local/bin/puku-runner'` selects it again with a restart, not a rebuild;
both permission dialects are still supported on the host, so falling back
costs nothing.

---

## Why

The SDK spawns the same binary with the same flags the bash runner passes by
hand, so this is not an architecture change — it replaces a hand-rolled
client of the stream-json protocol with the vendor's typed one.

The prize is the permission path. `control_request` frames arriving on stdout
had to be matched back up by `request_id`, with answers keyed by question
*header*, across two services. `canUseTool` answers in the process that holds
the question.

## Compatibility

### Works

| Check | Result |
| --- | --- |
| SDK under the guest's node | v22.23.2, 67 exports, `HARNESS_SCHEMA=1` |
| `checkCompatibility()` | `harnessOk: true`, no errors |
| **Message shapes** | **byte-identical to raw CLI lines** — `system`, `assistant`, `result` key-for-key. The dashboard, `attach.rs`, `inspect_line` and `parse_result_usage` are untouched |
| **Billing** | `total_cost_usd` + `usage.{input,output,cache_read_input,cache_creation_input}_tokens` — exactly what `parse_result_usage` reads |
| Options | 16 of 17 tested pass |
| Warm mode | a fresh session can be a `WarmQuery` by naming `sessionId`; the CLI adopts our UUID |

### Does not work

| Problem | Evidence | What the runner does |
| --- | --- | --- |
| **`settingSources` kills the session** | `unknown option '--setting-source'` | Never passes it. Also unnecessary — a skill in `~/.puku-cli/skills` appears in the init message's `skills` either way |
| **`bypassPermissions` fails SDK validation** | `options failed validation: allowDangerouslySkipPermissions` | Sets `allowDangerouslySkipPermissions` alongside. This is the default ceiling, so it is the common path |
| **`dontAsk` / `auto` rejected** | `options failed validation: permissionMode` | `extraArgs: {'permission-mode': mode}` |
| **`outputFormat` destroys the stream** | appends a second `--output-format json`; CLI: `--input-format=stream-json requires output-format=stream-json` | Structured output goes through `extraArgs: {'json-schema': …}` instead |
| **Exit code swallowed after a result** | SDK suppresses the exit error once it has seen a terminal `result` | Reads the child's real code. The host maps non-zero to `failed`, so this would have turned failures into completions |
| **`Options.stderr` is a no-op** | not wired in the SDK's transport | Taps the pipe via `spawnPukuCliProcess` |
| **Non-permission hooks never fire** | no `hook_callback` handler in the SDK | Policy lives in `canUseTool`, not `PreToolUse` |
| **npm's 3.0.0 ignores `spawnPukuCliProcess`** | verified against both | `vendor-sdk.sh` stages from a checkout by default |
| **SDK session-path helpers differ from the CLI** | SDK: `~/.local/share/puku-cli/projects`, `/`→`-`. CLI: `$HOME/.puku-cli/projects`, `[^a-zA-Z0-9]`→`-` | Teleport keeps the proven sanitiser |

A malformed line on stdout also kills the SDK's NDJSON parser outright, where
the bash pipeline tolerated it. Worth knowing if the CLI ever prints a banner.

## How the runner works

`images/puku-agent/runner/runner.mjs` reads **`/session/manifest.json`** —
not `spec.json`, which is host-only and carries credentials — and maps it to
`Options`. The host contract is unchanged: append-only `events.ndjson`, the
`truncated` blob envelope, `/session/stdin.fifo`, and the `exec.exited`
marker the launch wrapper writes.

Two things in the port are fussier than they look:

- **Reading the fifo.** `fs.createReadStream` reads fine but survives
  `process.exit()` and hangs the runner; `net.Socket` on the same fd exits
  cleanly but never yields data, silently dropping follow-up turns. A `cat`
  child on the already-open fd does both.
- **Blob numbering.** The bash runner counted from 1 per invocation, so a
  resumed session could overwrite a blob before the uploader read it. The
  port counts from the outbox's real length.

## Permission asks

Two dialects are live at once so both runners work during the parity period.
`kind` on the pending question records which one asked, and controld answers
in the same one.

```
bash runner   CLI emits control_request  -> detect_question -> pending_question
              controld build_control_response -> control_response line

SDK runner    SDK routes the ask to canUseTool inside the guest
              runner emits `platform.question`  -> detect_question -> pending_question
              controld emits `platform.answer`  -> runner builds the PermissionResult
```

Everything the human sees is unchanged: `waiting_input`, the
`session.question` event, notifications, the dashboard, and the same
`POST /v1/sessions/{id}/answer` shape. What disappears is controld
reconstructing `updatedInput` blind from a projection.

**There is no passthrough mode, and there cannot be one.** The runner always
runs warm (it names `sessionId` so `interrupt()` exists), and in warm mode the
SDK creates its ControlProtocol unconditionally and intercepts every control
envelope. With no handler registered it answers the CLI itself with
`{"subtype":"error","error":"no handler registered for … can_use_tool"}` — the
ask never reaches the host and the agent gets an error where a human answer
belonged. Verified by running exactly that.

So `canUseTool` is mandatory, and the permission half of Phase 1 was never
achievable as parity: the two dialects coexist for the *bash* runner's sake,
not as a fallback for the SDK one.

## What is built

| | Status |
| --- | --- |
| `runner.mjs` at parity, selected by `PUKU_RUNNER_CMD` | done |
| `vendor-sdk.sh` staging the SDK into the image | done |
| Deterministic `fake-puku-cli.sh` for offline parity | done |
| Interrupt via the fifo instead of `pkill -INT -f puku-cli` | done |
| `canUseTool` + the `platform.question`/`platform.answer` dialect | done — and mandatory, see above |
| Tool-policy enforcement in `canUseTool` | done |
| Structured output (`output_schema` → `--json-schema`) | done |

Proven offline, no credential required:

- Both runners produce identical event types and field values against the
  same manifest. The only difference is `session_id`, deliberately: the
  runner names the session so `interrupt()` exists.
- Exit codes propagate (0 and 3). Follow-up turns round-trip through the fifo.
- A `can_use_tool` ask becomes `platform.question`; an answer on the fifo
  returns to the CLI as a `control_response` carrying
  `updatedInput.answers.Bucket=staging`; **no `control_request` reaches the
  outbox**.
- A disallowed tool is refused without waking a human.
- argv carries exactly one `--output-format`, no `--setting-source`.

## The gate before flipping the default

The fake CLI cannot cover anything that needs a real model. On the box, with
`PUKU_RUNNER_CMD='exec node /opt/puku/runner.mjs'` on one worker:

1. **Skills** — the `system/init` message's `skills` array contains the
   pack's skills. Better than `ls $SKILLS_ROOT`: it proves the *agent* sees
   them.
2. **Billing** — one session per runner; `cost_usd`, `tokens_in/out` and both
   cache counters non-zero and comparable.
3. **Permissions** — `--permission-mode default`, force a prompt, answer and
   deny from `puku cloud`.
4. **Teleport** — the `ORCHID-7742` sentinel from `CLI-WALKTHROUGH.md` §3.
5. **Park/resume** — resume keeps context and `/workspace`.
6. **Interrupt** — stops the turn without killing the session.
7. **Document workload** — PDF + PPTX, as §10.5 of `DEPLOYMENT.md`.

Only then make `runner.mjs` the default and retire the bash runner, its
`control_request` branch in `detect_question`, and the control-response half
of `build_control_response`.

## Not built, and why

**In-process MCP tools** (`createSdkMcpServer` / `tool`). Verified feasible in
the guest — the server constructs and the bridge writes its `--mcp-config`.
Not built because there is no tool to put in it: adding 24 MB and 91 packages
to the guest image, plus a loopback HTTP listener in every session, needs a
named use case first. When one exists, the peer deps are `zod` and
`@modelcontextprotocol/sdk`, and the open question is how the bridge's config
merges with the brokered connectors already passed as `mcpServers`.

**Per-schedule `output_schema`.** Sessions carry one; schedules do not yet.
The column exists (`migrations/0014`), so it is the same one-field path
`packs` took.

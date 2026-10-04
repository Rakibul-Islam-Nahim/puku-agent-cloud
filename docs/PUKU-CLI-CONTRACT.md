# The puku-cli headless contract, as measured

Everything here was **verified against a real `puku-cli 1.8.43`** by driving
a headless session and reading `dist/cli.mjs` — not inferred from Claude Code
docs, and not copied from `puku-cowork`. The platform's human-in-the-loop
flow depends on these details being exactly right, so re-run the checks in
§5 when puku-cli is bumped.

## 1. The invocation

```
puku-cli -p \
  --output-format stream-json \
  --input-format stream-json \
  --permission-prompt-tool stdio \
  --include-partial-messages \
  --verbose \
  [--model M] [--max-budget-usd N] \
  [--permission-mode MODE] [--allowed-tools A,B] [--disallowed-tools C] \
  [--max-turns N] [--resume SESSION_ID]
```

**`--permission-prompt-tool stdio` is not optional.** It is what routes
permission asks to the stdin control channel as `control_request` frames.
Without it the CLI decides for itself (safe → allow, out of bounds → deny)
and `AskUserQuestion` / `ExitPlanMode` **auto-deny** — the platform's
`waiting_input` state would never fire and the agent would silently work
around every question. It is a hidden flag: absent from `--help`, accepted
by 1.8.43 (checked).

**`--max-turns` matters more than it looks.** Unset, puku-cli yields after a
single model response, which in an unattended cloud session reads as "the
agent did nothing". controld always fills it in (`PUKU_DEFAULT_MAX_TURNS`).

**Variadic flags are greedy.** `--allowed-tools`, `--disallowed-tools`,
`--add-dir` and `--mcp-config` swallow every following token until the next
`--flag`. The runner passes each list as one comma-joined argument
("comma or space-separated" per `--help`), which sidesteps the problem.

**`--permission-mode`** accepts exactly: `acceptEdits`, `bypassPermissions`,
`default`, `dontAsk`, `plan`, `auto`. `--god-mode` is the old spelling of
`bypassPermissions`.

## 2. Asking the human

`AskUserQuestion`'s `checkPermissions` returns `behavior: "ask"`
**unconditionally**, so it always reaches the host. The frame:

```json
{"type":"control_request",
 "request_id":"81cc6dd1-…",
 "request":{"subtype":"can_use_tool",
            "tool_name":"AskUserQuestion",
            "tool_use_id":"call_f07f…",
            "input":{"questions":[{"question":"…","header":"Indentation",
                                   "options":[{"label":"Tabs"},{"label":"Spaces"}],
                                   "multiSelect":false}]}}}
```

The matching assistant `tool_use` block arrives too. **Do not treat it as the
question** — the CLI is blocked on the control channel, so answering the
block is impossible and detecting it would raise a second, unanswerable
question for the same ask.

## 3. Answering

A pending question is a blocked `control_request`. **A user message does not
clear it.** The reply is:

```json
{"type":"control_response",
 "response":{"subtype":"success",
             "request_id":"81cc6dd1-…",
             "response":{"behavior":"allow",
                         "updatedInput":{"questions":[…],
                                         "answers":{"Indentation":"Tabs"}}}}}
```

- `answers` is keyed by each question's **`header`**, falling back to `Q1`,
  `Q2`, … (`cli.mjs`: `_?.header || \`Q${q+1}\``). The CLI renders it back to
  the model as `"Indentation"="Tabs"`.
- An `allow` with **no** `answers` produces the tool_result
  `User has answered your questions: .` — an empty answer the model will
  cheerfully invent a value for. controld refuses to send that.
- `behavior: "deny"` is a legitimate answer: it becomes an errored
  tool_result and the agent routes around it.
- Deny is still `subtype: "success"` — the *request* was handled; `behavior`
  carries the decision.

**There is no reply deadline.** Measured: the CLI waited 75 s with no
timeout (`duration_ms: 84980`, `duration_api_ms: 9988`). This is what makes
a cloud session that waits for a human on their phone possible at all.

## 4. Everything else on the control channel

- Other `control_request` subtypes (e.g. `interrupt`) are bookkeeping, not
  questions. Parking on them freezes a healthy session.
- Interrupt is sent *to* the CLI as
  `{"type":"control_request","request_id":…,"request":{"subtype":"interrupt"}}`.
- `{"type":"result"}` **without** a `subtype` is a subagent finishing, not
  the turn ending. Only a result with a subtype is terminal.
- Safe tool calls never reach the host: a plain `echo` under
  `--permission-mode default` executed with no `control_request` at all.

## 5. Re-verifying after a puku-cli bump

```sh
puku-cli --help | grep -E 'permission-mode|allowed-tools|max-turns'
puku-cli --permission-prompt-tool stdio --version   # hidden flag still accepted?
```

Then drive one headless session that calls `AskUserQuestion`, answer it with
`updatedInput.answers` keyed by header, and confirm the tool_result echoes
`"<header>"="<answer>"`. The fixtures in
`crates/puku-workerd/src/session_actor.rs` (`question_tests`) and
`crates/puku-controld/src/api/mod.rs` (`answer_tests`) are captured from
exactly that run — if the shapes change, those tests fail first.

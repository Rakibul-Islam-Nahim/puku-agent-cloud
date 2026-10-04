# Driving the cloud from puku-cli

How to shift work to your bare-metal cloud, run scheduled jobs, and confirm
each one actually did what it claims. Companion to
[DEPLOYMENT.md](DEPLOYMENT.md), which covers standing the services up;
this covers using them from the CLI.

- [0. Build the CLI — required](#0-build-the-cli--required)
- [1. Point it at your cloud](#1-point-it-at-your-cloud)
- [2. First run](#2-first-run)
- [3. Shift a local session to the cloud](#3-shift-a-local-session-to-the-cloud-teleport)
- [4. Driving a live session](#4-driving-a-live-session)
- [5. Scheduled jobs](#5-scheduled-jobs)
- [6. The document workload](#6-the-document-workload)
- [7. What to check when something looks wrong](#7-what-to-check-when-something-looks-wrong)
- [8. A full verification pass](#8-a-full-verification-pass--about-1-15-minutes)

---

## 0. Build the CLI — required

**The `puku cloud` commands are not in the published 1.8.49 you have
installed.** They live on the `feat/cloud-sessions` branch of
`puku-code-cli`, which has never been built. Everything else here fails at
`unknown command 'cloud'` until you do this.

```bash
# The build is Bun-only — it calls Bun.build, Bun.$ and a bun:bundle shim,
# so node cannot substitute.
curl -fsSL https://bun.sh/install | bash     # or: brew install oven-sh/bun/bun

cd puku-code-cli
git checkout feat/cloud-sessions
bun install
bun run build                                # produces dist/cli.mjs
```

Then use that build rather than the global one:

```bash
alias pukud="node $(pwd)/dist/cli.mjs"       # 'd' for dev
pukud cloud --help
```

Put that in `~/.zshrc`. It is an alias, not a binary, so a fresh terminal
without it fails with `command not found: pukud` — which looks like a broken
install and is not one.

Your `puku auth login` carries over: both builds read the same
`~/.config/pukucode/session.json`.

You should see all fifteen verbs: `run, ls, attach, answer, input, stop,
resume, cancel, interrupt, push, pull, watch, schedule, credentials`. If
`cloud` is missing you are still on the global 1.8.49 — check
`which puku-cli`.

The build is verified: all 22 cloud subcommands register and respond to
`--help`.

Once you are happy, install it over the global one:

```bash
npm install -g .        # from the puku-code-cli directory
```

## 1. Point it at your cloud

```bash
export PUKU_CLOUD_URL=https://agent.api.puku.sh
puku auth login                        # your own identity; nothing to store
```

**Prefer your login to a `pkc_` key.** A bearer identifies *you*: your own
org, your own quota, your own bill. A `pkc_` key carries no user, so
everyone sharing one shares an identity and a session list — keep those for
CI and operator tooling.

```bash
export PUKU_CLOUD_API_KEY=pkc_...      # only for CI / operator use
```

`--url` beats `PUKU_CLOUD_URL` beats the built-in default; `--api-key` beats
`PUKU_CLOUD_API_KEY` beats your `puku auth login`. Put the exports in your
shell profile so every command below picks them up.

Confirm the CLI and the cloud agree before anything else:

```bash
pukud cloud ls
```

An empty list is a pass. An auth error here is a wrong key, not a broken
deployment — check `curl "$PUKU_CLOUD_URL/health?deep=1"` separately.

## 2. First run

```bash
pukud cloud run --max-turns 5 \
  "Write /workspace/hello.txt containing the word ORCHID, then stop."
```

You get a live stream: tool calls, output, cost. This is the same rendering
as a local session, driven by the control plane's event feed.

**Pass:** it reaches `completed` within seconds of the agent's last word,
with a non-zero cost *and* non-zero token counts, and you saw a Write tool
call.

Two things worth knowing, because both were broken until recently and the
symptoms are unmistakable:

- A session that finishes reports **`completed`** and releases its slot at
  once. If you see it sit at `running` for 15 minutes and then report
  `stopped`, the worker is on an old build.
- `cost_usd` and the token counters should agree. Cost with `in=0 out=0`
  means the worker predates the `modelUsage` fix — spend you cannot account
  for.

Useful variants:

```bash
pukud cloud run --detach "…"              # print the id and exit
pukud cloud run --repo https://github.com/you/proj --branch main "Fix the flaky test"
pukud cloud run --max-budget-usd 2.00 --max-turns 20 "…"
pukud cloud run --pack office --pack essentials "…"
```

`--repo` clones into the session; if the agent commits, the platform pushes
a branch and opens a PR.

## 3. Shift a local session to the cloud (teleport)

This is the interesting one: take a conversation you are having **locally**
and continue it in a microVM, with its history intact.

### How it works

`puku cloud push` reads the local transcript, uploads it, and starts a cloud
session seeded with that history. The agent in the VM remembers everything
you discussed on your laptop.

### Two rules that trip people up

1. **Run it from the directory the session ran in.** Transcripts are stored
   per working directory, at
   `~/.puku-cli/projects/<sanitized-cwd>/<session-id>.jsonl`. Push from
   elsewhere and it cannot find the transcript, and says so rather than
   starting an empty session pretending to be your old one.
2. **The session needs history.** Pushing a transcript with nothing in it is
   refused, because an "imported" session that remembers nothing looks
   exactly like an agent that forgot.

### The walkthrough

```bash
cd ~/code/my-project
pukud                                     # start a normal local session
```

In that session, establish something the cloud could not otherwise know:

```
Remember this token for later: ORCHID-7742. Just acknowledge it.
```

Exit the session. Then, **from the same directory**:

```bash
pukud cloud push --prompt "What was the token I asked you to remember?"
```

**Pass:** the cloud session answers `ORCHID-7742`. That is proof the
transcript crossed the boundary — nothing else in the pipeline could supply
it.

You will also see a line like `pushing 34 KB of transcript ·
https://github.com/you/my-project#main` before it starts: git remote and
branch are detected from the directory, so a repo session clones itself in
the cloud automatically.

### Pushing a specific session

If you are not inside a session, name one. Find it by its transcript file:

```bash
ls -lt ~/.puku-cli/projects/$(pwd | sed 's/[^a-zA-Z0-9]/-/g')/*.jsonl | head
pukud cloud push --session <session-id> --prompt "Carry on in the cloud."
```

The `sed` is the same sanitisation the CLI uses — every non-alphanumeric
character becomes `-`.

### Options

```bash
pukud cloud push \
  --prompt "Carry on: finish the migration and open a PR." \
  --repo https://github.com/you/proj \   # override the detected remote
  --branch feature/x \
  --model <model> \
  --max-turns 40 \
  --detach                                # id and exit, do not stream
```

## 4. Driving a live session

A cloud session is not fire-and-forget. These are what make it usable:

```bash
pukud cloud watch                    # everything that needs you, refreshing
pukud cloud ls --state running
pukud cloud attach <id>              # replay history, then follow live
pukud cloud input <id> "also add tests for the error path"
pukud cloud interrupt <id>           # end the current turn, keep the session
pukud cloud stop <id>                # park: VM stops, volumes kept
pukud cloud resume <id>              # continue where it parked
pukud cloud cancel <id>              # done with it, discard
```

### Answering permission prompts

When the agent needs a decision, the session goes to `waiting_input` and
**waits indefinitely** — there is no timeout, so a session blocked overnight
is fine.

```bash
pukud cloud watch                              # shows what is blocked
pukud cloud answer <id> "yes, use the staging bucket"
pukud cloud answer <id> --deny --message "not in production"
```

`attach` also surfaces the question inline and lets you answer without a
second terminal.

### Interrupt

`interrupt` ends the turn the agent is in the middle of. The session stays
alive and reports `completed`, not `failed` — a stop you asked for is not an
error. Send `input` afterwards to carry on.

### Park, resume, and following up

`stop` parks a running session: the microVM shuts down, `/workspace` and
`$HOME` persist, and `resume` continues the same conversation with the same
files.

Since a finished turn releases its VM, there is often **nothing left to
park** — `stop` on a `completed` session has nothing to do, and that is the
system working rather than a fault. Just send the next message:

```bash
pukud cloud input <id> "now add tests"
```

A follow-up to a finished session resumes it from its volumes
automatically, so continuing a conversation is one command whether the
session is still running or finished an hour ago. The agent keeps its
context across that boundary — test it with a codeword rather than trusting
it.

Sessions also park themselves after their idle timeout (15 minutes by
default, 3 for scheduled runs).

## 5. Scheduled jobs

```bash
pukud cloud schedule create \
  --cron "0 3 * * *" \
  --name nightly-brief \
  --pack office --pack essentials \
  --max-turns 30 \
  --max-budget-usd 5.00 \
  "Summarise yesterday's commits on main and write /workspace/brief.pdf."
```

Five fields, **UTC**, no seconds. `puku cloud schedule ls` shows the packs
and the next fire time.

### Do this first, or every run fails at 03:00

An unattended run has no live caller whose credentials it can borrow, so it
needs a stored key:

```bash
SF=~/.config/pukucode/session.json
TOKEN=$(python3 -c "import json;print(json.load(open('$SF'))['accessToken'])")
RT=$(python3 -c "import json;print(json.load(open('$SF'))['refreshToken'])")
curl -sS -X POST -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d "{\"kind\":\"refresh\",\"value\":\"$RT\"}" "$PUKU_CLOUD_URL/v1/credentials"
pukud cloud credentials ls                     # confirms it, never prints values
```

Store the **refresh** token. controld mints a short-lived bearer from it at
each dispatch, so your schedules keep running on your own identity and bill
after your login lapses. `credentials set` stores an api_key instead, which
a puku gateway does not accept.

Without a stored credential the run fails before booting a VM, naming what
to store. It does not fall back to the operator's key — a shared deployment
has no such fallback.

### Test it now instead of waiting for 03:00

```bash
pukud cloud schedule ls                # get the id
pukud cloud schedule run <id>          # fire it now
```

That returns the `session_id` it created, without touching `next_run_at` —
the 03:00 run still happens as scheduled. Then:

```bash
pukud cloud attach <session-id>
```

**Pass:** the session exists, carries `packs: [office, essentials]`, and
runs to completion on the stored credential.

### Managing them

```bash
pukud cloud schedule ls
pukud cloud schedule run <id>         # fire once, now
pukud cloud schedule disable <id>     # stop firing, keep the definition
pukud cloud schedule enable <id>      # re-anchors next_run_at to now, so a
                                      # long-disabled job does not fire for a
                                      # window that passed months ago
pukud cloud schedule rm <id>
```

**Delete your test schedules.** One left enabled fires every night forever.

## 6. The document workload

The end-to-end proof: an agent in a microVM, using skills pulled from the
registry, producing real files. Needs the `puku-agent-office` guest image.

```bash
pukud cloud run \
  --pack office --pack essentials \
  --disallowed-tools WebSearch,WebFetch \
  --max-turns 60 --max-budget-usd 6.00 \
  "Write two deliverables in /workspace from your own knowledge — do NOT use
   WebSearch or WebFetch. Topic: trade-offs between microVM and container
   isolation for running untrusted AI agent code. Produce summary.pptx FIRST (4 slides), then report.pdf
   (about 3 pages), keeping report.md and summary.md beside them. Follow your
   pptx, pdf and research-report skills. Page and slide counts are rough
   targets -- do NOT iterate on layout or styling to hit them exactly."
```

Budget **~$3 and 25–40 minutes**. Disabling WebSearch is not optional — see
the known issue below.

The deck comes first, and the "rough targets" clause matters. Asked for
"2-3 pages" without it, the agent spent about twenty turns shaving a PDF
from four pages to three and hit the turn ceiling before the deck existed —
observed on two separate runs. Whatever is last in the prompt is what gets
dropped.

Then pull the results out:

```bash
pukud cloud pull <session-id>                        # -> <id8>-workspace.tgz
pukud cloud pull <session-id> --out work.tgz
pukud cloud pull <session-id> --what home            # the transcript instead

tar -xzf <id8>-workspace.tgz -C out/ && file out/*
```

Packaging is asynchronous, so `pull` polls for up to two minutes rather than
making you re-run it.

**Pass:** `file` reports `PDF document … 3 pages` and `Microsoft OOXML`,
not "ASCII text". Reference run: `report.pdf` 12384 b / 3 pages,
`summary.pptx` 36303 b / 4 slides.

`skills/deployment-test/scripts/pull-workspace.sh <id> ./out` does the same
thing and prints the file inventory with types, if you want one command.

If it hits the turn ceiling (`error_max_turns`), do not re-run from scratch —
the session stays open with its finished work on the volume, so a re-run pays
for the report twice:

```bash
pukud cloud input <id> "Now create /workspace/summary.pptx from summary.md — 4 slides, using python-pptx per your pptx skill."
```

## 7. What to check when something looks wrong

| Symptom | What it means |
| --- | --- |
| `unknown command 'cloud'` | You are on the global 1.8.49. Build the branch, §0 |
| `not signed in` | `puku auth login`, or set `PUKU_CLOUD_API_KEY` |
| `no local session to push` | Not inside a session — pass `--session <id>` |
| `no transcript for session …` | You pushed from the wrong directory. Transcripts are per-cwd; `cd` back to where the session ran |
| Pushed session does not remember anything | The transcript did not cross. Test with the ORCHID sentinel in §3 before blaming the model |
| Session sits at `booting` | Worker cannot pull the guest image, or no KVM |
| Fails instantly, credential error | You stored no credential. Store a refresh token — see § above |
| Agent ignored the pdf/pptx skill | Packs did not resolve. Run `skills/deployment-test/scripts/health.sh` — usually `PUKU_SKILLS_TOKEN` ≠ the registry's `PUKU_SKILLS_OPERATOR_TOKEN` |
| Scheduled runs get no skills, interactive ones do | Same token mismatch. Interactive runs forward *your* bearer and mask it |
| Session died on `web_search_requests` | Known puku-cli bug, not the platform. Re-run with `--disallowed-tools WebSearch,WebFetch` |
| Frozen at a constant cost for minutes | Model-gateway 503 retries. It recovers. Do not cancel — you would pay twice |
| Permission mode narrower than you asked | The deployment's `PUKU_PERMISSION_CEILING` clamped it. Working as designed; the CLI tells you |

Server side, in order of usefulness:

```bash
curl -fsS "$PUKU_CLOUD_URL/health?deep=1" | jq
docker compose logs -f controld
journalctl -u puku-workerd -f
sudo msb list
```

And the dashboard at `$PUKU_CLOUD_URL/` shows running/stopped/failed tiles,
per-session detail with the resolved pack digests, and fleet drift.

## 8. A full verification pass — about $1, 15 minutes

Twelve commands that between them exercise every part of the platform. Each
has a pass condition you can check by eye; if one fails, the note says which
component it implicates, so you are not guessing.

Everything below was run against a live deployment. `skills/deployment-test/
scripts/feature-matrix.sh` automates the same ground and more (60 checks) if
you would rather not do it by hand.

```bash
export PUKU_CLOUD_URL=https://agent.api.puku.sh
puku auth login
```

**1. The platform is up**

```bash
curl -fsS "$PUKU_CLOUD_URL/health?deep=1"
```
`database: ok`, `object_storage_probe: "ok"`, `workers_connected >= 1`. A
missing `object_storage_probe` means artifacts and oversized events will
fail later, silently.

**2. A session runs, finishes, and is billed to you**

```bash
pukud cloud run --max-turns 5 \
  "Write the word ORCHID-7742 to /workspace/sentinel.txt, then stop."
pukud cloud ls
```
`completed`, non-zero cost, non-zero `in`/`out`. Cost with zero tokens is
the `modelUsage` bug; `stopped` after a long wait is the completion bug.

**3. The work actually exists**

```bash
pukud cloud pull <id> --out ./work.tgz && tar -xzf ./work.tgz && grep -r ORCHID-7742 .
```
Proves the artifact path end to end: collection, object storage, presigned
URL, download. A hang here is usually `PUKU_R2_ENDPOINT` being unreachable
*from your laptop* rather than a storage fault.

**4. The agent can see its skills**

```bash
pukud cloud run --pack office --pack essentials --max-turns 4 \
  "List the skills available to you, one per line, then stop."
```
`pptx`, `pdf`, `docx`, `xlsx` among them. Missing means the packs did not
resolve — check `PUKU_SKILLS_TOKEN` against the registry's
`PUKU_SKILLS_OPERATOR_TOKEN`.

**5. A permission prompt reaches you, and your answer changes the run**

```bash
pukud cloud run --detach --permission-mode default --max-turns 8 \
  "Use AskUserQuestion to ask which bucket to deploy to, offering 'staging'
   and 'prod'. Then tell me which I chose."
pukud cloud watch                       # shows it blocked
pukud cloud answer <id> "staging"
pukud cloud attach <id>
```
The agent's reply names *staging*. Reaching `waiting_input` only proves the
question got out; the reply proves the answer got back.

**6. Interrupt ends the turn, not the session**

```bash
pukud cloud run --detach --max-turns 20 "Count slowly from 1 to 400."
pukud cloud interrupt <id>
pukud cloud ls
```
`completed`, not `failed`. `failed` with `runner exited with code 130` is
the old SIGINT behaviour.

**7. Context survives a turn boundary**

```bash
pukud cloud run --max-turns 6 "Remember the codeword TANGERINE. Reply READY."
pukud cloud input <id> "What was the codeword? Reply with just that word."
pukud cloud attach <id>
```
It answers TANGERINE. This is also the resume path: the first turn finished
and released its VM, so the follow-up booted it again on the same volumes.

**8. Tool policy is enforced without waking anyone**

```bash
pukud cloud run --disallowed-tools WebSearch,WebFetch --max-turns 6 \
  "Use WebSearch to look up the weather. If you cannot, say BLOCKED."
```
`completed`, and it never reached `waiting_input`. A refused tool must not
become a human's problem.

**9. Structured output**

`output_schema` is a REST field with **no CLI flag yet**, so this one goes
through curl:

```bash
curl -sS -X POST -H "Authorization: Bearer $PUKU_CLOUD_API_KEY" \
  -H 'content-type: application/json' -d '{
    "prompt": "Judge whether 2+2=4. Answer with the required JSON only.",
    "max_turns": 4,
    "output_schema": {"type":"object","required":["verdict","score"],
      "properties":{"verdict":{"type":"string","enum":["pass","fail"]},
                    "score":{"type":"integer"}}}
  }' "$PUKU_CLOUD_URL/v1/sessions"
```

The final `result` is a **string containing JSON** — parse it, do not grep
it — and should match the schema, e.g. `{"score":1,"verdict":"pass"}`. Prose
instead means the schema never reached the agent. The agent's `init` message
should also list a `StructuredOutput` tool.

**10. Scheduled jobs — the path with no human in it**

```bash
pukud cloud credentials ls              # must show kind: refresh
pukud cloud schedule create --name nightly --cron "0 3 * * *" \
  --pack office --max-turns 8 "Write /workspace/brief.txt containing OK."
pukud cloud schedule run <schedule-id>  # fire it now
pukud cloud ls
```
The fired session completes and inherits the packs. This is the only check
that covers the stored credential — nothing else does, because every other
command carries your bearer.

**11. Your sessions are yours**

```bash
curl -s -o /dev/null -w '%{http_code}\n' "$PUKU_CLOUD_URL/v1/sessions"
```
`401`. Then confirm a teammate cannot see your ids: their `cloud ls` should
list only their own.

**12. The fleet is honest**

```bash
curl -fsS -H "Authorization: Bearer $PUKU_CLOUD_API_KEY" \
  "$PUKU_CLOUD_URL/v1/fleet" | jq '.workers[0].capacity_slots, .drift'
```
Capacity should reflect the host, not a number someone typed — on a 24-core,
62 GiB box, 11 rather than 8. `drift.orphaned` should stay flat across runs;
growth means sandboxes are leaking.

### The document workload

The heaviest thing the platform does, and the only check that exercises the
office skills for real. It has its own section — see
[6. The document workload](#6-the-document-workload) — and takes about $3
and 25–40 minutes.

Whatever you use to run it, check the output with `file`, not the
extension: a `.pptx` the agent wrote as text passes a name check and opens
in nothing.

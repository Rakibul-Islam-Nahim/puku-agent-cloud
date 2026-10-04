# puku-bot — Master Plan

> **Superseded.** puku-bot was built differently from what this document
> plans: puku-bot-svc runs its own agent (Pi) and uses puku-agent-cloud only
> for the bot's *computer*, through the `puku-cloud` sandbox provider and
> the Machines API. See [CLOUD-HYPERVISOR-PLAN.md](CLOUD-HYPERVISOR-PLAN.md)
> for the plan that was implemented and [MACHINES-API.md](MACHINES-API.md)
> for the contract. Kept for the history of the idea.

> A Grok-Bot-style chat app where every chat is an AI agent, built on the puku agent stack (puku-cli, puku-agent-cloud, puku-skills-service) and forked from OpenMausBot.

**Status:** planning document. No code yet.

**Audience:** anyone building puku-bot. Read top-to-bottom once, then use the section index to navigate.

---

## 1. Vision

**One sentence.** puku-bot is a Telegram-style chat app where every "bot" in the sidebar is a real, isolated puku-cli agent with its own personality, model, skills, and computer — running locally first, with an optional cloud mode for hosted sessions.

**Differentiators vs Grok Bot (closed) and OpenMausBot (generic):**

1. **Built on puku-cli, not Claude Code.** Every bot speaks the puku NDJSON event protocol out of the box. No NDJSON-to-Canonical re-encoding layer — the harness is puku-native.
2. **Skill registry is first-class.** Every bot has a `~/.puku-bot/skills/` mirror of `~/.puku-cli/skills/`, automatically synced from `puku-skills-service` at session start. The `office` and `essentials` packs become defaults.
3. **Cloud bots via puku-agent-cloud.** A bot can be `kind: "local"` (default — puku-cli on this box) or `kind: "cloud"` (puku-agent-cloud microVM session, streamed over the existing `attach` WebSocket).
4. **Open driver model preserved.** The `ProviderDriver` interface from OpenMausBot stays — we just ship one driver that wraps puku-cli (`puku-bot` engine), and skip the Claude/Codex/Grok driver sprawl unless we want them.
5. **No managed companion dependency.** mDNS/LAN companion is fine; the Cloudflare-tunnel "managed" path is opt-in infrastructure we can replicate or drop.

---

## 2. Section index

| # | Section |
|---|---|
| 3 | Codebase inventory & what we have to work with |
| 4 | puku-bot architecture (components + data flow) |
| 5 | Tech choices: what to keep, fork, drop, add |
| 6 | Repo layout proposal |
| 7 | Build & integration with puku-agent-cloud and puku-skills-service |
| 8 | Migration strategy from OpenMausBot |
| 9 | Phased milestones |
| 10 | Open questions / decisions to make |

---

## 3. Codebase inventory

Five codebases in `/Users/sagoresarker/Documents/workspace/personal/poridhi/puku/puku-bot/`:

| Path | Role for puku-bot | Borrow / fork / ignore |
|---|---|---|
| `OpenMausBot/` | App shell: chat UI + harness server + Electron. This is what we fork and rebrand. | **Fork wholesale**, then strip and replace. |
| `puku-code-cli/` | The agent. `puku-cli` ships the puku NDJSON protocol that bots speak. | **Use as-is.** Bots spawn `puku-cli -p ... --output-format stream-json --input-format stream-json --verbose`. |
| `puku-agent-cloud/` | Backend for cloud bots. Each cloud bot is a controld session, streamed over the existing attach WS. | **Integrate as backend.** Reuse `puku-cloud-proto` types; call `POST /v1/sessions` + `WS /v1/sessions/{id}/attach`. |
| `puku-skills-service/` | Skill registry. We don't run our own — we point at the existing `skills.puku.sh`. | **Use as-is.** Bots fetch `/v1/resolve?packs=...` at session start. |
| `opensession/` | Reference for self-hosted-agent UX patterns, installer scripts, deploy shapes. Worth skimming for installer ideas. | **Reference only.** Don't merge — different engine (Pi), different scope. |

The center of gravity is the fork of `OpenMausBot/`. The other four are upstream dependencies we plug into.

---

## 4. puku-bot architecture

### 4.1 Component diagram

```
┌──────────────────────────────────────────────────────────────────────┐
│                         puku-bot desktop app                          │
│                                                                       │
│  ┌─────────────────────────────────────────────────────────────────┐ │
│  │  React chat UI (fork of OpenMausBot src/)                        │ │
│  │  • Sidebar of bots                                                │ │
│  │  • Chat thread per bot (renders NDJSON events)                   │ │
│  │  • Composer + approval cards                                      │ │
│  │  • Settings (engines, models, skills, computers)                 │ │
│  └─────────────────────────────────────────────────────────────────┘ │
│                              │ SSE                                    │
│                              ▼                                        │
│  ┌─────────────────────────────────────────────────────────────────┐ │
│  │  puku-bot harness (Node, fork of OpenMausBot server/)             │ │
│  │  • Owns one puku-cli subprocess per local bot                    │ │
│  │  • Forwards user text → puku-cli stdin (stream-json)              │ │
│  │  • Forwards puku-cli stdout (NDJSON) → UI as SSE                 │ │
│  │  • Broker can_use_tool → approval UI → puku-cli stdin             │ │
│  │  • Persists transcripts to ~/.puku-bot/transcripts/              │ │
│  │  • SkillsClient → resolves packs from puku-skills-service         │ │
│  └─────────────────────────────────────────────────────────────────┘ │
│           │ subprocess                  │ HTTPS                       │
│           ▼                              ▼                            │
│  ┌──────────────────┐         ┌───────────────────────────────┐     │
│  │  puku-cli        │         │  puku-skills-service           │     │
│  │  (one per bot)   │         │  skills.puku.sh                │     │
│  │                  │         │  (skill resolution only)       │     │
│  └──────────────────┘         └───────────────────────────────┘     │
│                                                                       │
│  ┌─────────────────────────────────────────────────────────────────┐ │
│  │  Electron shell (fork of OpenMausBot electron/)                    │ │
│  │  • Forks harness via utilityProcess.fork                          │ │
│  │  • Hosts the React app in a BrowserWindow                         │ │
│  │  • Native: notifications, mic, screen capture, auto-update        │ │
│  └─────────────────────────────────────────────────────────────────┘ │
│                                                                       │
└──────────────────────────────────────────────────────────────────────┘

                  (optional) cloud bot path
                  ─────────────────────────
                          │
                          ▼
              ┌─────────────────────────────┐
              │  puku-agent-cloud           │
              │  Cloud bot = controld session│
              │  Same NDJSON protocol,      │
              │  relayed over WS attach.    │
              └─────────────────────────────┘
```

### 4.2 Bot lifecycle

1. **Bot created.** User clicks `+` in sidebar → harness writes a `BotRecord` (`~/.puku-bot/bots/<id>.json`). Record contains: `id`, `name`, `personality`, `model`, `kind: "local" | "cloud"`, `engine`, `cwd`, `skills[]`, `apps[]`, `computer`.
2. **Bot opened.** User clicks a bot → harness starts the agent.
   - `kind: "local"` → `spawn("puku-cli", [...headless flags...])`. New session id (or `--resume <prev>` if continuing). Working dir = bot's `cwd`. Skills resolved from `puku-skills-service` and staged into `~/.puku-cli/skills/` so puku-cli auto-discovers them.
   - `kind: "cloud"` → `POST {PUKU_AGENT_CLOUD_URL}/v1/sessions` with `spec.skills` from the resolver. Capture session id.
3. **User sends a message.** Composer text → harness → (local) write to puku-cli stdin as `{type:"user", message:{role:"user", content:"…"}}`, or (cloud) `POST /v1/sessions/{id}/input`.
4. **Agent emits events.** (local) puku-cli stdout NDJSON lines → harness normalizes to canonical events → SSE `event:` → React renderer. (cloud) `attach` WebSocket delivers the same shape (controld already normalizes).
5. **Permission prompts.** (local) `control_request/can_use_tool` over stdin → harness pauses puku-cli read loop → posts `approval_request` to UI → user clicks Allow → harness writes `control_response/success` back to stdin. (cloud) controld already does this via its `pending_question` state; harness just surfaces it.
6. **Turn ends.** `result/success` event → harness marks turn complete → transcript appended → UI shows turn-end marker.
7. **Bot closed / app quit.** SIGTERM to puku-cli, transcript finalized. On resume: spawn puku-cli with `--resume <sessionId>`.

### 4.3 Canonical event schema

Inherit OpenMausBot's canonical event shape (`server/schema.ts`, `server/thread-events.ts`) — but since puku-cli already emits a typed NDJSON stream, we mostly forward:

| Canonical event | Source (local) | Source (cloud) |
|---|---|---|
| `message` (assistant text/thinking) | `assistant.message.content[]` blocks | controld `Agent` event with same shape |
| `message` (user) | `replay-user-messages` echo | controld `User` event |
| `tool_use` | `assistant.message.content[type=tool_use]` | controld `Agent` tool_use |
| `tool_result` | `assistant.message.content[type=tool_result]` or tool_progress tail | controld `Agent` tool_result |
| `stream_event` (token deltas) | `stream_event` w/ `--include-partial-messages` | controld streamed partials (when proxy emits them) |
| `approval_request` | `control_request/can_use_tool` | controld `pending_question` |
| `approval_resolved` | `control_response/success|error` | controld answer POST |
| `turn_start` / `turn_end` | `system/subtype=session_state_changed` + `result` | controld `Session` state events |
| `compaction` | `system/subtype=compact_boundary` | controld same |
| `error` | `result/error_*` | controld `Failed` terminal state |

Canonical events are what the React renderer consumes. They are the same shape for local and cloud bots — the UI doesn't know the difference. That is the whole point of having the canonical event schema.

### 4.4 Skill resolution flow

```
Bot config → SkillsClient.resolve(bearer, org, ["office", "essentials"])
         ↓
GET https://skills.puku.sh/v1/resolve?packs=office,essentials
         ↓
[{name:"office", version:"1.2.0", digest:"sha256:…", url:"https://r2…/packs/abc.tgz", skills:[…]}]
         ↓
For each pack: download → sha256 verify → unpack into ~/.puku-cli/skills/
         ↓
Spawn puku-cli with HOME pointing at a writable mirror so it auto-discovers the skills.
```

For cloud bots, controld already does this — harness passes `SessionSpec.skills` to `POST /v1/sessions` and the worker unpacks. We don't need to reimplement.

**What we add:** a per-bot skill filter UI. Bot config gets `skills: ["office", "essentials@1", "research-report"]` — the same `PackRequest` parsing as `puku-skills-service`'s `resolve.rs`. Harness resolves, stages, and passes the resolved packs to either puku-cli (via `~/.puku-cli/skills/`) or controld (via `SessionSpec.skills`).

### 4.5 Cloud bot integration

We don't reimplement the agent cloud. puku-bot is a **client of puku-agent-cloud**.

```
Harness                         controld                    workerd
  │                                │                            │
  │ POST /v1/sessions (Bearer)     │                            │
  ├───────────────────────────────►│                            │
  │                                │ AssignSession { skills[] } │
  │                                ├───────────────────────────►│
  │                                │                            │ spawn msb microVM
  │                                │                            │ install packs (sha256 verified)
  │                                │                            │ puku-runner launches puku-cli
  │ 201 { session_id }             │                            │
  │◄───────────────────────────────┤                            │
  │                                                             │
  │ WS /v1/sessions/{id}/attach?api_key=…                       │
  ├═════════════════════════════════════════════════════════════►
  │                                                             │
  │ ServerMsg::Events (replay) → ServerMsg::Live → live events │
  │◄═════════════════════════════════════════════════════════════┤
  │                                                             │
  │ User sends message                                           │
  │ POST /v1/sessions/{id}/input                                 │
  ├───────────────────────────────►                             │
  │                                │ DeliverInput (stdin to puku-cli)
  │                                ├───────────────────────────►│
```

The harness reuses the **same canonical event normalization** for both. That's the integration point.

**Open question (decided below in §10):** do we wire auth through the user's puku platform bearer, or mint a per-bot `pkc_…` server-side API key? Recommendation: **use the user's bearer** for simplicity in v1, **add `pkc_` for unattended bots** in v2.

### 4.6 Skills shipping (defaults)

Ship `office` + a curated subset of `essentials` as defaults — exactly the packs already in `puku-skills-service/skills/`. Recommended bot defaults:

| Pack | Skills | Why |
|---|---|---|
| `office` | `pptx`, `docx`, `xlsx`, `pdf`, `scripts/extract-text` | Universal — every chat app needs these. |
| `essentials` (filtered) | `research-report`, `dataviz`, `code-review`, `debugging`, `data-analysis`, `web-research` | Generic utility skills a chat-app user would want. |
| Drop | `cloud-session` | puku-VM-specific; only useful inside puku-agent-cloud. |

When puku-bot adds its own bot personas, they ship as **per-bot skill overrides**, not as new packs.

---

## 5. Tech choices

### 5.1 What we keep (verbatim or with rebrand only)

- **OpenMausBot 3-process split.** Chat app + harness server + Electron shell. This is the right shape.
- **SSE transport between chat UI and harness.** One persistent `EventSource`. Already resumable via `id:` field. Don't replace with WebSocket — SSE is simpler for the read path.
- **`ProviderDriver` interface from `server/contracts.ts`.** Reusable engine abstraction. We just ship **one** driver (`puku-cli`) instead of N.
- **`ProviderRegistry` and `ShadowInstance` pattern.** Forward/backward compat for engine configs.
- **Canonical event schema (`server/schema.ts`).** Use as-is. Puku NDJSON maps onto it cleanly.
- **Bot profile model (`server/bot-profile.ts`).** Patch schema (name, title, description, notifications, avatar, voice).
- **Tool approval flow (`server/auto-approve.ts`, `PendingApproval.tsx`, `ApprovalCard.tsx`).** Broker pattern: harness pauses, posts event, gets resolution.
- **Per-bot computer abstraction (`server/computer-control.ts`, `container-computer.ts`, `local-vm.ts`).** Keep the `kind: "local" | "container" | "cloud"` enum. Local computer requires opt-in like upstream.
- **Bundled build pipeline (`package.json` scripts, `electron-builder.yml`).** DMG / EXE / DEB targets. Just rebrand artifact names and app ids.
- **Test files.** Pin behavior; rename in place only when underlying file is renamed.

### 5.2 What we fork and modify

- **Driver list (`server/drivers/builtIn.ts`)** — strip to one entry: `puku-cli.ts`. Remove `claude.ts`, `codex.ts`, `grok.ts`, `pi.ts`, `minimax.ts`, `boxagent.ts`, `agents-proxy.ts` (or keep them disabled by default; `ProviderRegistry` allows this).
- **`server/drivers/puku-cli.ts`** — new driver. Wraps `child_process.spawn("puku-cli", [...])`. Parses stream-json NDJSON. Maps onto canonical events. Implements `create(input)` returning a `ProviderInstance` with `adapter` that drives the per-bot lifecycle.
- **Engine setup UI (`src/components/EngineSetup.tsx`)** — strip the multi-engine picker. Default to `puku-cli`. Show model picker (puku-ai-2.7/2.8/opus/sonnet/haiku). Keep advanced: custom binary path (for sandbox builds).
- **Bot personas seed (`server/store.ts`)** — replace `chief-of-staff.ts` and `openmaus-status-capsule.ts` with puku-bot default bots:
  - **Puku** — general-purpose coding assistant.
  - **Researcher** — web research + report-writing specialist.
  - **Docsmith** — uses the `office` pack (pptx/docx/xlsx/pdf).
  - **Coder** — code-review/debugging focused, uses `essentials:code-review`, `essentials:debugging`.
- **Settings UI** — add a "Skills" tab where users can see resolved packs per bot, refresh, or pin versions.
- **Cloud bot UI** — add a bot `kind: "cloud"` selector that reveals extra fields: puku-agent-cloud URL, repo, branch, region.
- **Composer** — keep, but add a slash-command picker driven by discovered puku skills (already a feature in puku-cli's skill system).

### 5.3 What we drop (or mark opt-in)

- All non-puku engines by default — `claude.ts`, `codex.ts`, `grok.ts`, `pi.ts`, `minimax.ts`, `boxagent.ts`, `agents-proxy.ts`. (`OpenMausBot` ships them, but they're not our story.)
- Managed companion tunnel path — `electron/managed-companion-tunnel.mjs`, `cloudflare/control-plane/`. **Decision: leave them in `OpenMausBot/` but don't carry forward into `puku-bot/`.** LAN-only mDNS companion is enough for v1.
- 500+ Composio integrations in the default bot persona. **Decision: ship the broker code** (`cloudflare/composio-broker/`) as a pluggable connector — each bot's `apps[]` is a list of connector ids. But don't ship a default bot that uses Composio. Reduces vendor surface area.
- Host computer control on macOS/Ubuntu Xorg. **Decision: ship behind an explicit opt-in toggle**, same as upstream. The Linux Wayland caveat is upstream's problem; we inherit their `#345` stance.
- iOS app (`ios/`). **Decision: defer to v2.** Companion sidecar code stays so iOS could ship later, but no Swift app in v1.

### 5.4 What we add

- **`puku-cli` driver** (`server/drivers/puku-cli.ts`) — the single engine.
- **`SkillsClient` wrapper** in the harness — calls `puku-skills-service` `/v1/resolve`, downloads, sha256-verifies, unpacks into a per-bot `~/.puku-cli/skills/` mirror.
- **Cloud bot path** (`server/cloud-bot.ts`) — wraps puku-agent-cloud REST + WS attach. Speaks the canonical event schema on the other side.
- **Skill picker UI** in composer (`src/components/SkillPicker.tsx`) — lists discovered skills, inserts `/skill-name` command on click.
- **Pack refresh worker** — background job that re-resolves packs every N hours and surfaces "skill updates available" in the bot list.
- **Telemetry scaffolding** — match puku-agent-cloud's Sentry scrubbing pattern (`ALLOWED_FIELDS` whitelist). Don't ship without telemetry in v1.

---

## 6. Repo layout proposal

```
puku-bot/
├── package.json              # pnpm workspace root
├── pnpm-workspace.yaml
├── tsconfig.json
├── tsconfig.server.json
├── tsconfig.server.build.json
├── vite.config.ts
├── electron-builder.yml      # rebranded app id, artifact names
├── index.html
│
├── src/                       # React chat app (fork of OpenMausBot/src/)
│   ├── main.tsx
│   ├── App.tsx
│   ├── state/store.tsx        # default bots, default engine selection
│   ├── components/
│   │   ├── Sidebar.tsx
│   │   ├── ChatView.tsx
│   │   ├── Composer.tsx
│   │   ├── SkillPicker.tsx      # NEW
│   │   ├── CloudBotFields.tsx    # NEW (when bot.kind === "cloud")
│   │   ├── ApprovalCard.tsx
│   │   └── EnginesSettings.tsx
│   └── lib/live-events.ts
│
├── server/                    # Node harness (fork of OpenMausBot/server/)
│   ├── index.ts
│   ├── contracts.ts           # ProviderDriver / ProviderInstance — kept
│   ├── schema.ts              # canonical events — kept
│   ├── store.ts               # keep
│   ├── bot-profile.ts         # keep
│   ├── auto-approve.ts        # keep
│   ├── harness/
│   │   ├── registry.ts        # keep
│   │   └── bus.ts             # keep
│   ├── drivers/
│   │   ├── builtIn.ts         # → [puku-cli] only
│   │   └── puku-cli.ts        # NEW — the only engine
│   ├── cloud-bot.ts           # NEW — puku-agent-cloud client
│   ├── skills-client.ts       # NEW — puku-skills-service wrapper
│   ├── pack-installer.ts      # NEW — sha256 verify + unpack
│   └── bot-defaults/
│       ├── puku.md            # NEW
│       ├── researcher.md
│       ├── docsmith.md
│       └── coder.md
│
├── electron/                  # Desktop shell (fork of OpenMausBot/electron/)
│   ├── main.mjs               # rebrand bundle id, window title, IPC names
│   └── ...
│
├── companion/                 # mDNS/LAN companion (fork of OpenMausBot/companion/)
│   └── src/index.ts
│
├── shared/                    # canonical event + bot profile types (fork)
│
├── build/                     # rebranded icons, splash, mascot
│
├── scripts/                   # forked from OpenMausBot/scripts/ + new helpers
│   ├── vendor-puku-cli.sh     # NEW — bundle puku-cli binary into the app
│   ├── install.sh             # rebrand
│   └── ...
│
├── docs/
│   ├── ARCHITECTURE.md        # this plan, distilled
│   ├── SKILLS.md              # how puku-bot uses skills-service
│   ├── CLOUD-BOTS.md          # how puku-agent-cloud integration works
│   └── SECURITY.md
│
├── tests/
│   ├── puku-cli-driver.test.ts    # NEW
│   ├── skills-client.test.ts      # NEW
│   ├── cloud-bot.test.ts          # NEW
│   └── ...
│
├── AGENTS.md
├── README.md
├── LICENSE                    # Apache-2.0 (inherit from OpenMausBot, retain attribution)
└── NOTICE                     # preserve third-party attribution per Apache §4(d)
```

---

## 7. Build & integration details

### 7.1 Spawning puku-cli (the canonical headless invocation)

From `puku-code-cli/docs/REMOTE_SESSION_GUIDE.md`:

```sh
puku-cli \
  --print "" \
  --output-format stream-json \
  --input-format stream-json \
  --verbose \
  --include-partial-messages \
  --include-hook-events \
  --replay-user-messages \
  --enable-auth-status \
  --permission-prompt-tool stdio \
  --session-id "$(uuidgen)" \
  --cwd "$BOT_CWD" \
  --allowed-tools "$BOT_ALLOWED_TOOLS" \
  --disallowed-tools "$BOT_DISALLOWED_TOOLS" \
  --model "$BOT_MODEL" \
  --effort "$BOT_EFFORT"
```

**Stdin** — NDJSON messages:

```json
{ "type": "user", "message": { "role": "user", "content": "Hello" } }
{ "type": "control_response", "response": { "subtype": "success", "request_id": "…", "response": { "behavior": "allow", "updatedPermissions": [] } } }
{ "type": "control_request", "request": { "subtype": "interrupt" } }
```

**Stdout** — NDJSON events (every event from `coreSchemas.ts#SDKMessageSchema`). Harness normalizes to canonical events for the UI.

**Gotchas to handle in the driver (from the puku-code-cli exploration):**

- `--output-format=stream-json` requires `--verbose`. Driver sets both.
- The CLI heap-relaunches itself with `--max-old-space-size=8192`. Spawn should pass through stdio cleanly.
- NDJSON stdout guard diverts stray writes to stderr — driver must NOT mix `console.log` into puku-cli's stdout.
- Initial prompt must be supplied via stdin (since we set `--print ""`), or driver uses `--sdk-url` mode instead.
- `keep_alive` messages appear on stdin/stdout during idle — driver must ignore them.
- Stream events (`stream_event`) only emitted with `--include-partial-messages`. Driver sets this for token-by-token rendering.
- `--permission-prompt-tool stdio` is the magic flag that converts every tool call into a `control_request/can_use_tool` over stdin. Without it, the CLI silently auto-allows tools.

### 7.2 Skills resolver (per-bot, on session start)

```ts
// server/skills-client.ts (sketch)
export async function resolveAndStage(opts: {
  bearer: string,
  orgId: string,
  packs: string[], // e.g. ["office", "essentials@1"]
  cfgDir: string,   // e.g. ~/.puku-bot/skills/<botId>/
}): Promise<{ name, version, digest, skills[] }[]> {
  const url = `${PUKU_SKILLS_URL}/v1/resolve?packs=${packs.join(",")}`;
  const resolved = await fetch(url, { headers: { Authorization: `Bearer ${bearer}` } }).then(r => r.json());
  for (const pack of resolved.packs) {
    const bytes = await fetch(pack.url).then(r => r.arrayBuffer());
    const got = sha256Hex(bytes);
    if (got !== pack.digest) throw new Error(`sha256 mismatch for ${pack.name}@${pack.version}`);
    await untarInto(`${cfgDir}/`, bytes);
  }
  return resolved.packs;
}
```

Then spawn puku-cli with `PUKU_CONFIG_DIR=${cfgDir}` so it auto-discovers the staged skills.

**Auth:** forward the user's platform bearer; on outage, fail closed (503), never anonymously succeed. Same discipline as puku-skills-service.

**For cloud bots**, skip this — controld does it. Harness passes the resolved pack list as `SessionSpec.skills`:

```ts
const skills = await resolveAndStage({ bearer, orgId, packs: bot.skills });
await fetch(`${PUKU_AGENT_CLOUD_URL}/v1/sessions`, {
  method: "POST",
  headers: { Authorization: `Bearer ${bearer}` },
  body: JSON.stringify({ spec: { skills } }),
});
```

### 7.3 Cloud bot attach

After creating a session, attach via WebSocket:

```ts
// server/cloud-bot.ts (sketch)
const ws = new WebSocket(`${PUKU_AGENT_CLOUD_URL_WS}/v1/sessions/${sessionId}/attach?api_key=${pkcKey}`);
ws.on("open", () => ws.send(JSON.stringify({ type: "hello", after_seq: 0 })));
ws.on("message", (frame) => {
  const msg = JSON.parse(frame.toString());
  if (msg.type === "events") for (const event of msg.events) emitCanonical(event);
  if (msg.type === "live") /* switch to live mode */;
});
```

The controld message types (`ServerMsg::Events`, `ServerMsg::Live`, `ServerMsg::Events`) already line up with the canonical event model — minimal mapping required.

### 7.4 Auth model

| Action | Who | Credential |
|---|---|---|
| User signs in | User | Platform bearer (verified via `PUKU_API_URL/auth/verify`) |
| Local bot runs | puku-bot harness on user's box | Inherits the platform bearer via `PUKU_AI_API_KEY` env passed to puku-cli |
| Resolve skills | Harness | Bearer forwarded to `puku-skills-service` |
| Spawn cloud bot | Harness | Bearer or `pkc_…` key (server-minted for unattended bots) |
| Worker → controld | Internal | Per-worker token (out of scope for puku-bot) |

**v1 simplification:** all auth via the user's platform bearer. Mint `pkc_…` for unattended scheduled bots in v2.

### 7.5 Branding scrub checklist

When forking `OpenMausBot/`:

- [ ] `package.json` — name → `puku-bot`, repo URL, author, homepage, descriptions.
- [ ] `electron/main.mjs` — bundle id (`com.puku.bot.desktop`), window title, IPC names.
- [ ] `electron-builder.yml` — app id, artifact names, signing config.
- [ ] `index.html`, `mascot-preview.html`, all `public/` assets.
- [ ] String occurrences of `OpenMausBot`, `openmausbot`, `OpenMaus`, `MausBot`, `maus` in `src/`, `server/`, `electron/`, `companion/`, `cloudflare/`, `docs/`, `README.md`, `AGENTS.md`, `NOTICE`.
- [ ] `cloudflare/composio-broker/wrangler.jsonc` and `cloudflare/control-plane/wrangler.jsonc` — Worker names, route patterns, KV/D1 binding ids. (Or strip these workers and re-add when we have puku.sh hosting.)
- [ ] `LICENSE` + `NOTICE` — preserve Apache §4(d) attribution to OpenMausBot contributors and third parties.

---

## 8. Migration strategy from OpenMausBot

**Approach:** symlink-and-strip, not cp-and-rewrite. This keeps the upstream diff visible and makes rebases tractable.

1. **Day 0 — empty repo with placeholders.** Create the directory structure in §6 with stubs that import from `OpenMausBot/...`. Verify the build runs end-to-end (compiles, bundles, launches).
2. **Week 1 — rebrand pass.** Walk the rebranding checklist in §7.5. Keep all engines loaded; just remove the user-facing strings.
3. **Week 2 — strip engines.** Delete `claude.ts`, `codex.ts`, `grok.ts`, `pi.ts`, `minimax.ts`, `boxagent.ts`, `agents-proxy.ts`. Reduce `builtIn.ts` to `[puku-cli]`.
4. **Week 3 — `puku-cli` driver.** Write the new `server/drivers/puku-cli.ts`. Map puku NDJSON onto canonical events. Unit test against puku-cli in a fixture repo.
5. **Week 4 — skills integration.** Add `skills-client.ts` + `pack-installer.ts`. Wire into bot startup. Add skill picker UI.
6. **Week 5 — cloud bot.** Add `cloud-bot.ts`. Wire into bot `kind: "cloud"` path. Add cloud fields in bot config UI.
7. **Week 6 — default bots + personas.** Ship the 4 default bots (§5.2). Polish onboarding.
8. **Week 7 — installer.** Linux/macOS/Windows installers. Rebrand `install.sh`.
9. **Week 8 — telemetry, error handling, polish.** Sentry scrubbing per puku-observability pattern. Crash reports. Auto-update.

**Throughout:** keep `OpenMausBot/` as a sibling reference, not a submodule. Periodically diff to pick up bug fixes selectively (drivers we don't ship, harness internals, Electron shell improvements).

---

## 9. Phased milestones

### M0 — Repo scaffold + rebrand (1 week)

- [ ] Stand up `puku-bot/` with stub structure (§6).
- [ ] Branding scrub checklist (§7.5) — every checkbox.
- [ ] Bundles build and launch with empty harness + empty chat UI.
- [ ] LICENSE/NOTICE preservation reviewed.

**Exit:** `puku-bot` launches, shows "Add your first bot" placeholder, no crash.

### M1 — One local bot, end-to-end (2 weeks)

- [ ] `puku-cli` driver complete, maps NDJSON → canonical events.
- [ ] Harness spawns one puku-cli subprocess per bot.
- [ ] Composer → stdin. Stdout → SSE → React. Approval cards wired.
- [ ] Default bot "Puku" ships with a working system prompt.
- [ ] Resume works (`--resume <sessionId>` after app restart).
- [ ] Transcripts persist to `~/.puku-bot/transcripts/`.

**Exit:** user can chat with a single local bot, approve tool calls, restart the app, and pick up where they left off.

### M2 — Skills + multiple bots (2 weeks)

- [ ] `SkillsClient` resolves packs from `puku-skills-service`.
- [ ] Pack installer sha256-verifies and stages into `~/.puku-bot/skills/<botId>/`.
- [ ] Bot config gets `skills: ["office", "essentials@1"]`.
- [ ] Skill picker UI in composer (slash command list).
- [ ] 4 default bots ship: Puku, Researcher, Docsmith, Coder.
- [ ] Settings tab to manage pack subscriptions.

**Exit:** user can have multiple local bots with different skill sets; skills resolve automatically; chat works.

### M3 — Cloud bots (2 weeks)

- [ ] `cloud-bot.ts` wraps puku-agent-cloud REST + WS attach.
- [ ] Bot config gets `kind: "cloud"` with repo/branch/region fields.
- [ ] Cloud bots stream the same canonical events as local bots.
- [ ] Approval flow works against cloud sessions.
- [ ] Auth via user's platform bearer (no `pkc_` keys in v1).

**Exit:** user can create a cloud bot pointing at a GitHub repo, chat with it, and approve tool calls — same UX as local.

### M4 — Telemetry, polish, ship (2 weeks)

- [ ] Sentry scrubbing per puku-observability pattern.
- [ ] Crash reports wired.
- [ ] Auto-update channel.
- [ ] Installers for macOS / Windows / Ubuntu.
- [ ] CI green; release v0.1.0 tagged.

**Exit:** public release.

### M5 — v2 backlog (post-launch)

- iOS app via companion sidecar (already wired in OpenMausBot; port Swift).
- `pkc_…` API keys for unattended scheduled bots.
- Composio connectors as opt-in (drop the broker Worker code in).
- Memory snapshots for fast resume (like puku-agent-cloud's planned M5).
- WebSocket bridge for browser-based puku-bot (no Electron).
- More default bots from a community pack.

---

## 10. Open questions / decisions to make

| # | Question | Recommendation |
|---|---|---|
| Q1 | Where do bot transcripts live? | `~/.puku-bot/transcripts/<botId>/<sessionId>.jsonl` — mirrors puku-cli's own transcript path so we can `--resume` directly. |
| Q2 | How do we handle multi-bot puku-cli concurrency? | Each bot is its own subprocess. No in-process pooling needed. Default CPU/memory limits inherited from puku-cli defaults; expose `bot.memoryMb` later. |
| Q3 | Skill conflicts across bots? | Per-bot skill stage dir + `PUKU_CONFIG_DIR` per process. No global skills root. Means each bot has its own copy of `office`, but unpacked disk is cheap and isolates bot updates. |
| Q4 | Cloud bot region pinning? | Pass through to `SessionSpec`; expose region dropdown in bot config. |
| Q5 | Auth for unattended / scheduled cloud bots? | v1: bearer-only. v2: `pkc_…` server-minted, with refresh tokens and per-bot scopes. |
| Q6 | Do we run our own puku-skills-service instance? | No — point at the shared `skills.puku.sh`. If we ever ship org-private packs, deploy our own with the same schema. |
| Q7 | Companion sidecar — mDNS only, or also Tailscale? | mDNS only in v1. Tailscale in v2 if there's demand. |
| Q8 | Should `puku-cli` be vendored in the app bundle, or required as a system install? | Vendor in v0.1 (download on first run; checksum against published). Defer to system install in v0.2 once `puku-cli` has stable installers. |
| Q9 | Default model per bot — fixed or per-bot override? | Per-bot override; ships default = `puku-ai-2.8`. Model picker in bot config. |
| Q10 | Hard fork or git subtree against `OpenMausBot`? | Hard fork (copy + rebrand). Periodically cherry-pick bug fixes from upstream. Subtree adds rebasing pain. |

---

## Appendix A — File-by-file fork map

Top 25 files to touch in the first 8 weeks:

| Week | Path | Action |
|---|---|---|
| 0 | `package.json` | rebrand, drop unused deps |
| 0 | `electron/main.mjs` | rebrand bundle id, IPC names |
| 0 | `electron-builder.yml` | rebrand app id, artifact names |
| 0 | `index.html` | rebrand title, favicon |
| 0 | `src/main.tsx` + `App.tsx` | rebrand copy, default bots |
| 0 | `src/state/store.tsx` | rebrand copy, default bots |
| 0 | `README.md`, `AGENTS.md`, `NOTICE` | rebrand |
| 1 | `server/index.ts` | default port, telemetry hook, default persona |
| 2 | `server/drivers/builtIn.ts` | strip to `[puku-cli]` |
| 2 | `server/drivers/{claude,codex,grok,pi,minimax,boxagent,agents-proxy}.ts` | delete or keep-with-feature-flag |
| 3 | `server/drivers/puku-cli.ts` | NEW — implement |
| 3 | `server/schema.ts` | confirm canonical event coverage |
| 4 | `server/skills-client.ts` | NEW |
| 4 | `server/pack-installer.ts` | NEW |
| 4 | `server/index.ts` | call skills-client at bot start |
| 5 | `server/cloud-bot.ts` | NEW |
| 5 | `src/components/CloudBotFields.tsx` | NEW |
| 5 | `src/components/SkillPicker.tsx` | NEW |
| 6 | `server/bot-defaults/{puku,researcher,docsmith,coder}.md` | NEW — system prompts |
| 6 | `server/store.ts` | seed default bots |
| 7 | `scripts/install.sh` + platform scripts | rebrand |
| 7 | `electron-builder.yml` | signing config |
| 8 | `server/observability.ts` | NEW — Sentry scrubbing |
| 8 | `tests/*` | full coverage of driver + skills-client + cloud-bot |
| 8 | `docs/{ARCHITECTURE,SKILLS,CLOUD-BOTS,SECURITY}.md` | NEW |

## Appendix B — Glossary

- **puku-bot** — the chat app this document describes.
- **bot** — a persona in the sidebar backed by one puku-cli (local) or one puku-agent-cloud session (cloud). Has a `BotRecord` on disk.
- **engine** — the binary the harness spawns to run an agent. We ship one: `puku-cli`.
- **skill** — a `SKILL.md` instructions file. Lives in a pack. Resolved through puku-skills-service.
- **pack** — versioned, content-addressed tarball of skills. The unit of publish/resolution in puku-skills-service.
- **canonical event** — the normalized event shape the React renderer consumes. Source-agnostic (local or cloud).
- **driver** — TypeScript implementation of `ProviderDriver` that wraps one engine. We ship one: `puku-cli.ts`.
- **harness** — the Node server process that owns agents, brokers approvals, streams events.

# Puku Agent Cloud — Architecture & Build Plan

Run puku-cli sessions in the cloud — each session in its own hardware-isolated
microVM, streamed live back to the user's terminal, built on the microsandbox
runtime.

- Status: draft v1 · 2026-08-16
- Stack: puku-cli · microsandbox (libkrun/KVM) · agentd over vsock

---

## 1. The one decision everything hangs on

There are two ways to split an agent between control plane and sandbox:

- **A — Split brain:** the agent loop (LLM calls, tool dispatch) runs in your
  control plane; only tool executions (bash, file edits) are shipped into the
  sandbox.
- **B — Whole agent in the VM:** headless puku-cli runs *inside* the microVM.
  The control plane only manages VM lifecycle and relays event streams.

**Pick B.** It is what Claude Code cloud sessions and exe.dev do, and for good
reason:

- The agent and its tools share one filesystem and one process tree — no RPC
  boundary in the hot path of every tool call.
- The sandbox contains everything untrusted: both the code the agent writes
  *and* the agent's own tool use.
- The control plane stays a thin, boring lifecycle manager instead of a
  distributed agent runtime.
- puku-cli already *is* the agent — reuse it wholesale.

**Consequence:** the only genuinely new backend software is a control-plane
API, a session relay, and an LLM egress gateway. Everything inside the VM is
puku-cli you already have, plus microsandbox's `agentd` you already have.

## 2. System architecture

```
┌─────────────────────────── Client ────────────────────────────┐
│  puku-cli (local)                    Web dashboard (later)    │
│  puku cloud run / attach / ls        same event stream        │
└──────────────────────┬────────────────────────────────────────┘
                       │ HTTPS + WebSocket
┌────────────── Control plane (stateless, Postgres) ────────────┐
│  API server        Session relay          Scheduler           │
│  auth, session     WebSocket fan-in/out   places sessions,    │
│  CRUD, quotas,     of session events;     maintains warm pool │
│  audit, billing    attach / interrupt /   of pre-booted VMs   │
│                    answer; PTY pass-thru                      │
└──────────────────────┬────────────────────────────────────────┘
                       │ gRPC to workers / event stream back
┌────────────── Worker fleet (Linux hosts with KVM) ────────────┐
│  puku-workerd (Rust)                 LLM egress gateway       │
│  embeds microsandbox runtime         only route from a VM to  │
│  crates directly — no daemon.        a model provider. Swaps  │
│  Boots microVMs from puku-agent      session JWT for the real │
│  OCI image, wires vsock.             API key (never in VM),   │
│                                      meters tokens, enforces  │
│  ┌── microVM (one per session) ──┐   budgets. Also the egress │
│  │ agentd: exec, PTY, files      │   policy point: registry   │
│  │         over vsock            │   allowlist, deny metadata │
│  │ puku-cli --headless: the      │   IPs, deny inter-sandbox  │
│  │   agent loop; emits JSONL     │   traffic.                 │
│  │   events; scoped token only   │                            │
│  └───────────────────────────────┘                            │
└──────────────────────┬────────────────────────────────────────┘
                       │
┌───────────────── Storage & integrations ──────────────────────┐
│  Postgres          S3               OCI registry   GitHub App │
│  sessions, users,  transcripts,     puku-agent     short-lived│
│  quotas, audit     snapshots,       images,        repo-scoped│
│                    artifacts        cached layers  tokens     │
└───────────────────────────────────────────────────────────────┘
```

One session = one microVM. The control plane never executes agent code; the VM
never holds a real secret.

### Component notes

- **puku-workerd** — microsandbox is embeddable ("no long-running daemon"), so
  the worker agent is a small Rust service that links `crates/runtime` +
  agentd's client side directly. It owns the VM lifecycle on its host and
  streams agentd's vsock output up to the relay. Start with one binary per host
  and a static host list; the scheduler grows later.
- **Session relay** — sessions outlive connections. The relay persists every
  event to an append-only per-session log (Postgres table or Redis stream,
  archived to S3), so `puku cloud attach` replays history and then tails live.
  Multiple viewers can attach to one session.
- **LLM gateway** — microsandbox's "secrets that can't leak" idea applied to
  the platform: the VM authenticates with a short-lived JWT bound to
  `session_id`; the gateway swaps it for the real provider key outside the VM
  boundary. It is also the metering point — token counts per session feed
  billing directly.
- **Guest image** — one OCI image, `puku-agent`: puku-cli (headless), git,
  ripgrep, common toolchains (node, python, go), and a tiny init that reads a
  bootstrap manifest (repo, branch, task, resumed transcript) and starts the
  agent. Use microsandbox's bootstrap + snapshot support to keep cold-start
  work out of the boot path.

## 3. Session lifecycle

```
created → scheduled → booting → bootstrapping → running ⇄ waiting_input
running → idle → snapshotted → running (resumed) → completed/failed → reaped
```

- **bootstrapping** — clone repo with a GitHub App installation token minted
  for this session, restore transcript if resuming, write the bootstrap
  manifest, exec `puku --headless`.
- **waiting_input** — the agent asked the user a question or hit a permission
  gate. The event goes through the relay; the CLI (or a push notification)
  surfaces it; the answer flows back down the same channel. The VM keeps
  running or idles depending on timeout.
- **idle → snapshotted** — after N minutes without activity, snapshot the VM
  (microsandbox snapshots) and release the slot. Resume restores in place —
  this is what makes long-lived sessions affordable.
- **completed** — agent pushes its branch / opens a PR via the scoped token,
  transcript and artifacts land in S3, VM is destroyed. Nothing durable lives
  only inside a VM.

## 4. The session event protocol

Define this early — it is the contract between headless puku, the relay, the
CLI, and any future web UI. One JSONL stream per session, append-only,
replayable:

```jsonl
{"seq":1,  "type":"session.started",  "session_id":"ses_9f2","repo":"poridhi/app","branch":"puku/fix-auth"}
{"seq":2,  "type":"assistant.text",   "text":"Looking at the auth middleware first."}
{"seq":3,  "type":"tool.use",         "tool":"bash","input":"go test ./auth/..."}
{"seq":4,  "type":"tool.result",      "exit":1,"output_ref":"s3://.../ses_9f2/4.log"}
{"seq":9,  "type":"user.question",    "id":"q1","text":"Migration will drop a column. Proceed?"}
{"seq":10, "type":"user.answer",      "id":"q1","answer":"yes"}
{"seq":41, "type":"session.completed","result":"pr_opened","url":"https://github.com/..."}
```

Rules that save pain later:

- Monotonically increasing `seq` for resume-from-cursor.
- Large outputs go to object storage by reference, never inline.
- Every event is safe to show to the user — secrets are filtered at the
  source, inside the VM, before emission.

## 5. Security model

| Boundary  | Mechanism |
| --------- | --------- |
| Compute   | One microVM per session (libkrun/KVM). Agent-written code, agent tool use, and user code all stay inside. No shared kernel with the host or other tenants. |
| Network   | Default-deny egress. Allow: LLM gateway, package registries (pull-through cache), github.com. Deny: link-local + cloud metadata IPs, all inter-sandbox traffic, inbound except relay-brokered port-forward for dev-server previews. |
| Secrets   | No provider API keys in any VM, ever. Session JWT (short-lived, session-scoped) → LLM gateway injects real keys. Git access via per-session GitHub App installation tokens scoped to the target repo, expiring ≤ 1 h. |
| Resources | Per-session vCPU/mem/disk caps (microsandbox tuning), wall-clock cap, LLM token budget enforced at the gateway, per-org concurrency quota enforced at the API. |
| Audit     | Every session's full event log is retained; control-plane mutations write audit events (the fork's metrics/audit plumbing already points this way). |

> **Infrastructure constraint:** libkrun needs **KVM**, so workers must be
> bare-metal Linux (Hetzner, OVH, Latitude) or metal cloud instances (AWS
> `*.metal`) — ordinary VMs without nested virt won't run it. Local
> development on macOS still works via Hypervisor.framework, so the team can
> run the whole stack on laptops with a single-node compose of API + relay +
> workerd.

## 6. Build plan

### P0 — Headless puku

The prerequisite for everything. puku-cli must run non-interactively: take a
task, emit the §4 event stream on stdout, accept answers on stdin, exit with a
result. If puku already has an internal event model, this is mostly
serialization.

**Exit:** `puku --headless -p "task"` produces a valid replayable JSONL
transcript locally.

### P1 — Single-node MVP

One Linux/KVM box. Build the `puku-agent` OCI image; a minimal workerd that
boots a microVM per request and runs headless puku via agentd; a minimal API
(create/list/get session) and relay (stream events over WebSocket). CLI grows
`puku cloud run "task" --repo ...` and `puku cloud logs`.

**Exit:** a task submitted from a laptop runs to completion in a cloud microVM
and streams back live.

### P2 — Interactive sessions

Attach/detach with replay, question/answer relay, interrupts, PTY passthrough
(`puku cloud shell` into the session's VM via agentd), idle detection,
snapshot/pause/resume.

**Exit:** close the laptop mid-session, reattach from another machine, answer
a pending question, session finishes.

### P3 — Multi-tenant hardening

LLM egress gateway with key injection + token metering, default-deny network
policy, GitHub App for scoped git tokens, per-org quotas and budgets, audit
log, secret-redaction in the event stream.

**Exit:** a hostile prompt inside a session cannot exfiltrate a provider key
or reach another tenant.

### P4 — Scale & product surface

Multi-worker scheduler with warm pools (pre-booted VMs make session start feel
instant), autoscaling on pool depth, web dashboard on the same event stream,
port-forwarded dev-server previews, PR-triggered sessions.

**Exit:** N workers, zero-downtime deploys, p50 session start < 2 s from warm
pool.

## 7. Deliberate choices, stated once

- **Whole-agent-in-VM over split-brain** — §1. Revisit only if you need
  server-side agent logic the CLI can't ship.
- **Embed microsandbox, don't shell out to `msb`** — workerd links the runtime
  crates; you control the lifecycle API and avoid parsing CLI output. The
  fork's layout (runtime, agentd, protocol, vsock crates) is already shaped
  for this.
- **Event log as the source of truth** — the relay, CLI, dashboard, billing,
  and audit all read the same append-only stream. No second bookkeeping
  system.
- **Rust for workerd, anything sensible for the API** — workerd must be Rust
  (it links microsandbox). API/relay can be Go or Rust; pick one and keep the
  control plane to two services until P4 forces more.
- **Bare metal first** — a couple of Hetzner AX-line boxes carry hundreds of
  concurrent microVMs and cost less than one managed k8s cluster. Add
  orchestration when the fleet, not the ambition, demands it.

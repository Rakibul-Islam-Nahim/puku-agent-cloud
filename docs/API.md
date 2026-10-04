# puku-agent-cloud — control plane API

HTTP API of `puku-controld`. This is what `puku cloud`, the dashboard and
any integration talk to.

Base URL in dev: `http://103.174.50.75:7770`

- [Authentication](#authentication)
- [Errors](#errors)
- [Sessions](#sessions)
- [Live session control](#live-session-control)
- [Events and transcripts](#events-and-transcripts)
- [Artifacts](#artifacts)
- [Schedules](#schedules)
- [Credentials](#credentials)
- [Notifications and triggers](#notifications-and-triggers)
- [Fleet](#fleet)
- [Health and metrics](#health-and-metrics)
- [Session states](#session-states)

---

## Authentication

Every `/v1/*` route except `/v1/hooks/{token}` and `/v1/worker` requires a
credential:

```
Authorization: Bearer <key>
```

or, for the websocket where headers are awkward, `?api_key=<key>`.

Two schemes, chosen by prefix:

| Key | Meaning |
| --- | --- |
| `pkc_…` | A control-plane API key, looked up in `api_keys` by hash. Scoped to an org, optionally a user, with a scope list. For CI, scripts, the dashboard |
| anything else | A puku platform bearer, verified against `{PUKU_API_URL}/auth/verify`. Never decoded locally. This is what Puku Desktop and the web app hold |

If the platform is unreachable, verification returns **503, never 200** — a
platform outage must not silently downgrade to anonymous.

**A platform bearer is per-person identity.** On first use the subject is
provisioned into its own personal org with its own quota, and the bearer is
stored encrypted on the session so the microVM runs on *that user's*
credential. A `pkc_` key carries no bearer, so its sessions fall back to the
org's stored credential. On a shared deployment there is nothing after
that: operator-wide credentials are gated behind
`PUKU_ALLOW_OPERATOR_CREDENTIALS` (off by default), so a session with no
credential of its own is refused before a VM boots. Give people logins, not
`pkc_` keys.

Platform users are never `admin`: fleet operations stay on an
explicitly-scoped `pkc_` key.

When controld runs with `PUKU_AUTH=off`, the middleware injects a fixed dev
org and user and treats every caller as admin. **Only ever do that on a box
that is not reachable from the internet.**

### Ownership

A caller sees their own sessions. `?scope=org` widens a list to the whole
org. Requesting another org's resource returns **404, not 403** — existence
is not leaked.

## Errors

Uniform shape:

```json
{ "error": { "message": "no model credential for this session: …" } }
```

| Status | Means |
| --- | --- |
| 400 | Malformed body or query |
| 401 | Missing or invalid key |
| 403 | Authenticated but not permitted |
| 404 | Does not exist, or is not yours |
| 409 | Conflicts with current state (answering a question that already got one) |
| 410 | Gone for good — an event blob whose payload was on a reaped volume |
| 413 | Import transcript over the inline cap on a deployment with no object storage |
| 429 | Over the org's quota. Returned by session create and import, **before** a VM boots |
| 500 | Bug or unhandled failure |
| 501 | Feature not configured on this deployment (object storage, or `PUKU_SECRET_KEY` for credentials) |
| 502 | A dependency failed — e.g. object storage rejected the transcript upload |
| 503 | Platform auth unreachable |

Successes are equally specific: **202** for anything handed to the worker
asynchronously (input, interrupt, stop, resume, answer, artifact collect),
**204** for deletes and for a no-op, **302** for the blob and artifact
redirects.

---

## Sessions

### `POST /v1/sessions`

Create and dispatch a session.

```jsonc
{
  "prompt": "Summarise the changes on main and write /workspace/brief.pdf",
  "title": "nightly brief",          // optional; derived from prompt if absent
  "repo": "https://github.com/you/proj",
  "branch": "main",
  "model": "…",
  "max_budget_usd": 5.0,
  "max_turns": 30,
  "allowed_tools": [],               // [] takes PUKU_DEFAULT_ALLOWED_TOOLS
  "disallowed_tools": ["WebSearch"], // [] takes PUKU_DEFAULT_DISALLOWED_TOOLS
  "permission_mode": "acceptEdits",  // clamped, see below
  "connectors": true,                // attach the caller's MCP connectors
  "packs": ["office", "essentials"], // skill packs; [] = the org's defaults
  "output_schema": {                 // optional; final answer must satisfy it
    "type": "object",
    "properties": {"verdict": {"type": "string"}},
    "required": ["verdict"]
  },
  "idle_timeout_s": 900,
  "max_duration_s": 14400,
  "engine": "libkrun"                // or "cloud_hypervisor"; see below
}
```

Only `prompt` is required.

`engine` picks the hypervisor: `libkrun` (microsandbox) or `cloud_hypervisor`.
Absent takes the deployment's `PUKU_ENGINE_DEFAULT` (`libkrun` unless the
operator changed it), which is exactly what every request got before the
field existed. An engine outside `PUKU_ENGINES_ALLOWED` is a `400` naming what
the deployment offers. The session is only ever placed on a worker that
advertised its engine, and keeps it for life: a resume boots the same engine
on the same worker, because that is where its volumes are. If no connected
worker runs the engine yet, the session waits in `created` and its transcript
gets one `session.waiting_for_worker` event saying so. `POST /v1/sessions/import`
and `POST /v1/schedules` take the same field.

`permission_mode` is one of `default`, `plan`, `acceptEdits`, `dontAsk`,
`bypassPermissions`, `auto`, and is **clamped to the deployment's
`PUKU_PERMISSION_CEILING`**. The stored value is the effective one, so read
it back rather than assuming you got what you asked for.

`packs` absent or `[]` means "the org's defaults"; it is not a way to opt out
of skills.

`output_schema` is a JSON Schema the final answer must satisfy — for runs a
program consumes rather than a person reads, so a scheduled job can return a
verdict to branch on instead of prose to scrape. Requires the SDK runner
(`docs/SDK-MIGRATION-PLAN.md`); sessions on the bash runner ignore it.

Returns a [session object](#session-object). Fails with a credential error
**before** booting a VM if no model key resolves.

### `GET /v1/sessions`

| Query | Meaning |
| --- | --- |
| `state` | Filter, e.g. `running` |
| `limit` | Max rows. Default 50, capped at 200 |
| `scope` | `org` for everyone's, default just yours |
| `pr` | Filter by pull-request URL |

### `GET /v1/sessions/{id}`

One [session object](#session-object).

### `DELETE /v1/sessions/{id}`

Cancel and discard. The VM is destroyed and volumes are released.

### `POST /v1/sessions/import`

Teleport: continue a **local** puku-cli session in the cloud. Body limit is
64 MB on this route alone, because a transcript dwarfs a normal request.

```jsonc
{
  "puku_session_id": "…",     // the local session's id
  "transcript": "…",          // its .jsonl, verbatim
  "prompt": "Carry on in the cloud.",
  "repo": "https://github.com/you/proj",
  "branch": "main"
  // plus model, title, max_budget_usd, max_turns, allowed_tools,
  // disallowed_tools, permission_mode, connectors, idle_timeout_s,
  // max_duration_s -- as on POST /v1/sessions
}
```

Import takes no `packs` and no `output_schema`: a teleported session keeps
the skills and output contract of the local run it continues.

**Tool lists.** An empty `allowed_tools`/`disallowed_tools` is not "no
policy" — it takes the deployment's `PUKU_DEFAULT_ALLOWED_TOOLS` /
`PUKU_DEFAULT_DISALLOWED_TOOLS`, which are themselves empty unless an
operator set them. Naming any tool replaces that default outright, including
naming *fewer* restrictions than the deployment does: this is a default, not
a ceiling. Only `permission_mode` has a ceiling. Applied at dispatch, so
scheduled and webhook-fired sessions inherit it too — they carry no request
of their own to express a policy in.

An empty transcript is rejected: a session that claims to be imported but
remembers nothing is worse than an error.

### Session object

```jsonc
{
  "id": "uuid", "org_id": "uuid", "user_id": "uuid", "worker_id": "uuid",
  "title": "…", "prompt": "…", "repo": null, "branch": null, "model": null,
  "max_budget_usd": null,
  "allowed_tools": [], "disallowed_tools": ["WebSearch"],
  "permission_mode": "bypassPermissions",   // the EFFECTIVE, clamped value
  "max_turns": 40,
  "credential_kind": "api_key",             // which credential resolved
  "connectors": true,
  "packs": ["office", "essentials"],
  "branch_pushed": null, "pr_url": null,
  "imported_from": null,                    // set for teleported sessions
  "state": "running",
  "sandbox_name": "ses-0ff8a280bd1e",       // the microVM, for `msb exec`
  "volume_path": null,
  "puku_session_id": "uuid",                // the agent's own session id
  "last_seq": 173, "last_guest_line": 166,
  "pending_question": null,                 // set when state=waiting_input
  "cost_usd": 3.0166615,
  "tokens_in": 0, "tokens_out": 0,
  "cache_read_tokens": 0, "cache_write_tokens": 0,
  "idle_timeout_s": 900, "max_duration_s": 14400,
  "error": null,
  "created_at": "…", "started_at": "…", "ended_at": null
}
```

---

## Live session control

| Route | Does |
| --- | --- |
| `POST /v1/sessions/{id}/input` | Follow-up turn. Body `{"text": "…"}`. 202 |
| `POST /v1/sessions/{id}/interrupt` | Stop the current turn, keep the session |
| `POST /v1/sessions/{id}/stop` | Park: VM stops, volumes kept |
| `POST /v1/sessions/{id}/resume` | Boot again and continue |
| `POST /v1/sessions/{id}/answer` | Answer a permission prompt |
| `GET  /v1/sessions/{id}/attach` | WebSocket: replay then live events |

### `POST /v1/sessions/{id}/answer`

When a session is `waiting_input`, its `pending_question` holds the request.
**Answer with `pending_question.request_id`** — not the tool-use id, which is
a different value and yields 409.

```jsonc
{
  "question_id": "…",                       // = pending_question.request_id
  "answer": "yes, use the staging bucket",  // single-question form
  "answers": {"Which bucket?": "staging"},  // multi-question form, keyed by HEADER
  "decision": "deny",                       // or omit to allow
  "message": "not in production"            // reason, with deny
}
```

Sessions wait **indefinitely** — there is no reply timeout, so a session
blocked overnight is fine.

### `GET /v1/sessions/{id}/attach`

WebSocket. Authenticate with `?api_key=…`, since headers are awkward on a
socket.

The client speaks first. `Hello` carries the resume cursor — `0` replays
everything, `-1` skips replay and gives live only. The server then replays
persisted events with `seq > after_seq`, sends `Live`, and tails from
there. Events that arrive *during* replay are buffered and deduped by
`seq`, so the stream is gap-free and strictly ordered — a client can trust
`seq` as its only cursor.

Client → server, tagged by `type`:

```jsonc
{"type": "hello",     "after_seq": 0}
{"type": "input",     "text": "also update the changelog"}
{"type": "interrupt"}
{"type": "answer",    "question_id": "…",        // = pending_question.request_id
                      "answer": "…",             // or "answers": {"<header>": "…"}
                      "decision": "deny",        // omit to allow
                      "message": "not in production"}
```

Server → client:

```jsonc
{"type": "events", "events": [ /* Event objects, ordered by seq */ ]}
{"type": "live"}                                  // replay done; live from here
{"type": "state",  "state": "waiting_input", "error": null}
{"type": "error",  "message": "…"}
```

`state` is a convenience for clients that only render session state — the
same transition also arrives as a `session` event, so a client that renders
events already has it and can ignore this.

The socket is an alternative to `POST .../input`, `/interrupt` and
`/answer`, not a separate capability: both paths end in the same worker
frame.

---

## Events and transcripts

### `GET /v1/sessions/{id}/events`

| Query | Default |
| --- | --- |
| `after_seq` | 0 |
| `limit` | 500, capped at 2000 |

Returns an array of events ordered by `seq`. Polling this is the simple
alternative to the websocket, and is what a script should use — a dropped
socket should not read as a dead session.

```jsonc
{
  "session_id": "uuid",
  "seq": 174,                    // global per-session order, assigned by controld
  "ts": "2026-08-25T03:00:11Z",
  "kind": "agent",
  "payload": { "type": "assistant", "…": "…" },
  "guest_line": 167,             // only on guest-originated events
  "blob_ref": null               // set when the payload was truncated
}
```

| `kind` | Payload is |
| --- | --- |
| `agent` | A verbatim puku-cli stream-json line, never rewritten by the platform |
| `session` | Platform lifecycle, e.g. `{"type":"session.state","state":"running"}` |
| `exec` | In-guest process event, e.g. `{"type":"exec.exited","code":0}` |
| `user` | Input echoed back into the log so a replay shows the whole conversation |

`seq` is allocated by controld and is what clients replay with. `guest_line`
is the 1-based line in the guest's `/session/events.ndjson`; `(session_id,
guest_line)` is unique, which is what makes worker redelivery idempotent.
When `blob_ref` is set the payload in the row is truncated — fetch the full
one from `/blobs/{guest_line}`.

### `GET /v1/sessions/{id}/blobs/{line}`

Oversized event payloads are truncated in the transcript and spilled to
object storage. 302s to a presigned GET for the full payload.

---

## Artifacts

Two-step because packaging is asynchronous.

### `POST /v1/sessions/{id}/artifacts/{what}`

`what` is `workspace` (the deliverables) or `home` (the agent's own state).
Asks the worker to package it. Returns immediately:

```json
{"what":"workspace","status":"collecting","download":"/v1/sessions/…/artifacts/workspace"}
```

**409 once the session is reaped.** Packaging reads live volumes, so this
only works while the session still has them — the retention sweep deletes
them `PUKU_RETENTION_DAYS` after it ends. There is no way to collect an
artifact afterwards; the archived event log is all that survives.

`home` is the guest's `$HOME`, and inside it
`.puku-cli/projects/<slug>/<puku_session_id>.jsonl` is puku-cli's own
transcript — the file a local `puku-cli --resume` reads. That is what makes
teleport-down possible: fetch it, re-home it under the *local* working
directory's slug, and resume. `<slug>` is derived from the VM's cwd
(`-workspace-repo` when a repo was cloned, `-workspace` otherwise), so match
on the filename rather than the directory.

The tarball deliberately **excludes `.config/pukucode/session.json`**, which
holds a live access and refresh token for whoever started the session.
Artifacts created before that exclusion landed may still contain it, so a
client should skip `.config/` on extract regardless.

### `GET /v1/sessions/{id}/artifacts/{what}`

- **302** — to a presigned GET for the `.tgz`.
- **400** — `what` is neither `workspace` nor `home`.
- **404** — not ready yet. Keep polling.
- **501** — object storage is not configured on this deployment.

A 403 following the redirect is object storage rejecting the signature,
which almost always means wrong `PUKU_R2_ACCESS_KEY_ID`/`SECRET`. Confirm
with `GET /health?deep=1`.

---

## Schedules

Cron entries that create ordinary sessions at fire time.

### `POST /v1/schedules`

```jsonc
{
  "prompt": "Summarise yesterday and write /workspace/brief.pdf",
  "cron": "0 3 * * *",              // FIVE fields, UTC, no seconds
  "name": "nightly-brief",
  "repo": null, "branch": null, "model": null,
  "max_budget_usd": 5.0, "max_turns": 30,
  "allowed_tools": [], "disallowed_tools": ["WebSearch"],
  "permission_mode": "acceptEdits", // clamped at creation; stored value is effective
  "connectors": true,
  "packs": ["office", "essentials"],// [] = the org's defaults
  "idle_timeout_s": 180             // scheduled runs park sooner by default
}
```

| Route | Does |
| --- | --- |
| `GET /v1/schedules` | List |
| `DELETE /v1/schedules/{id}` | Delete |
| `POST /v1/schedules/{id}/enable` | Enable, re-anchoring `next_run_at` to now so a long-disabled job does not fire for a window that passed months ago |
| `POST /v1/schedules/{id}/disable` | Stop firing, keep the definition |
| `POST /v1/schedules/{id}/run` | Fire now. Returns `{"session_id":"uuid"}`. Does **not** disturb `next_run_at` |

Firing is safe with multiple controld instances (`FOR UPDATE SKIP LOCKED`).
A schedule over quota still advances `next_run_at` — a missed window is
skipped, not queued.

**A scheduled run has no caller to borrow credentials from.** Store a
`refresh` token with `POST /v1/credentials`; controld mints a short-lived
bearer from it at each dispatch, so the job keeps running on your identity
and bill after your login lapses. Without one the run fails at dispatch,
before a VM boots — a shared deployment has no operator-wide fallback.

---

## Credentials

The model credential unattended runs use.

| Route | Does |
| --- | --- |
| `POST /v1/credentials` | Store. `{"kind":"api_key","value":"…","expires_at":null}` |
| `GET /v1/credentials` | List — metadata only, **never values** |
| `DELETE /v1/credentials/{id}` | Remove |

`kind` is one of three, and anything else is a 400:

| `kind` | Lifetime |
| --- | --- |
| `refresh` | **Preferred** — the only kind that outlives a login. controld mints a short-lived bearer from it at each dispatch |
| `bearer` | Works until that login lapses, hours |
| `api_key` | No expiry, but only useful where the gateway accepts one. A puku `pk_live_` key is sent as a bearer, not as `x-api-key` |

Values are encrypted at rest with XChaCha20-Poly1305 under
`PUKU_SECRET_KEY`. A deployment with no `PUKU_SECRET_KEY` cannot store them
at all and returns **501** — it is not a silent plaintext fallback.

Resolution order per session: the caller's own bearer → a cached org bearer
that is still fresh → a bearer minted from the org's `refresh` token → the
org's `api_key`. An operator-wide credential is only consulted when
`PUKU_ALLOW_OPERATOR_CREDENTIALS` is on; otherwise the session is refused
with a message naming what to store.

---

## Notifications and triggers

### Notifications — outbound, when something happens

| Route | Does |
| --- | --- |
| `POST /v1/notifications` | `{"kind":"webhook","url":"…","events":["waiting_input","terminal"],"secret":"…"}` |
| `GET /v1/notifications` | List |
| `DELETE /v1/notifications/{id}` | Remove |

`kind` is `webhook`, `slack` or `platform`; `webhook` and `slack` need a
`url`.

`events` has exactly two members and an unknown name is a 400, not an
ignored no-op:

| Event | Fires when |
| --- | --- |
| `waiting_input` | The session blocked on a question and needs a human |
| `terminal` | The session reached a terminal state — completed, failed or canceled |

Omitting `events` (or sending `[]`) subscribes to **both**. `secret` is the
HMAC key for webhook signatures, stored encrypted — sending one to a
deployment with no `PUKU_SECRET_KEY` is a 400.

### Triggers — inbound, start a session from a webhook

| Route | Does |
| --- | --- |
| `POST /v1/triggers` | `{"prompt":"…","name":"…","repo":null,"branch":null,"model":null,"max_budget_usd":null}`. Returns a hook token |
| `GET /v1/triggers` | List |
| `DELETE /v1/triggers/{id}` | Remove |
| `POST /v1/hooks/{token}` | **Unauthenticated by design** — the token in the URL *is* the credential. Fires the trigger |

Treat a hook URL as a secret: anyone holding it can start sessions and spend
money.

---

## Machines

Generic VMs driven from outside -- exec, files, archives, guest ports and
capability links, with no puku-cli inside. What puku-bot's computers run on.
The full contract is in [MACHINES-API.md](MACHINES-API.md).

| Route | Does |
| --- | --- |
| `POST /v1/machines` | Create, or ensure running by `external_id` |
| `GET /v1/machines[/{id}]` | Read |
| `POST /v1/machines/{id}/start` · `/stop` · `/touch` | Lifecycle |
| `DELETE /v1/machines/{id}` | Destroy (VM and volume) |
| `POST /v1/machines/{id}/exec` | Run a command |
| `GET·PUT /v1/machines/{id}/files` | Read, list, write files |
| `GET·PUT /v1/machines/{id}/archive` | A directory as tar.gz, out or in |
| `ANY /v1/machines/{id}/ports/{port}/…` | HTTP/WebSocket proxy to an exposed guest port |
| `POST /v1/machines/{id}/links` | A short-lived capability URL for one port |

---

## Fleet

| Route | Does |
| --- | --- |
| `GET /v1/fleet` | Workers, their sessions and machines, and drift |
| `GET /v1/workers` | Worker rows alone |
| `POST /v1/workers/{id}/drain` | Stop assigning new sessions; existing ones finish |
| `POST /v1/workers/{id}/undrain` | Resume assignment |

```jsonc
{
  "workers": [{
    "id": "uuid", "name": "box-1",
    "connected": true, "status": "online",
    "capacity_slots": 2, "used_slots": 1,
    "last_heartbeat_at": "…",
    "engines": ["libkrun", "cloud_hypervisor"],
    "inventory": null,
    "sessions": [{"id":"uuid","sandbox":"ses-…","state":"running","engine":"libkrun","cost_usd":3.01, "…": "…"}],
    "machines": [{"id":"uuid","sandbox":"mch-…","state":"running","engine":"cloud_hypervisor","memory_mib":4096}]
  }],
  "drift": { "orphaned": [], "vanished": [] }
}
```

`engines` is what the worker advertised at registration: only engines whose
runtime checks passed on that host, never merely those switched on.
`GET /v1/workers` carries `engines` and `features` too.

**Drift** compares what controld believes against the worker's own VM
inventory, across all its engines: `orphaned` are VMs (`ses-…` or `mch-…`)
running that controld does not know about, `vanished` are sessions or
machines whose VM is gone. Non-zero after a worker restart is the normal
cause.

`GET /v1/worker` is the workers' own WebSocket. Workers authenticate with a
worker token inside the Register frame, not with a client API key, and they
**dial out** — a worker never needs an inbound port.

---

## Health and metrics

### `GET /health` — unauthenticated

```json
{"status":"ok","version":"0.1.0","database":"ok","object_storage":true,"workers_connected":1}
```

`object_storage` reports only that a bucket is **configured**. A wrong access
key stays `true` here.

### `GET /health?deep=1`

Adds `object_storage_probe`, which round-trips a real object and reports the
storage error verbatim:

```json
{"…":"…","object_storage_probe":"ok"}
{"…":"…","object_storage_probe":"object storage write probe failed …: InvalidAccessKeyId …"}
```

Off by default because the container healthcheck polls every 30s and should
not bill a write each time. Use it on a fresh deploy, not in a loop.

### `GET /metrics` — unauthenticated

Prometheus text exposition.

### `GET /` — the dashboard

Public shell; every API call it makes carries the user's key, so it renders
nothing without one.

`#/s/<session-id>` opens that session directly, which is what makes a
session linkable — `puku-cli --cloud` prints exactly this shape as its
`View:` line. The hash is client-side only; the server serves one static
page either way.

---

## Session states

`state` on the session object, and the `state` field of a `session` event.

| State | Means |
| --- | --- |
| `created` | Row exists, not yet assigned to a worker |
| `scheduled` | Assigned to a worker, which has been told to start it |
| `booting` | The worker is starting the microVM |
| `bootstrapping` | VM up; repo clone, skill unpack and connector wiring |
| `running` | The agent is working |
| `waiting_input` | Blocked on a human. `pending_question` holds the ask |
| `stopping` | Park requested; the VM is being shut down |
| `stopped` | Parked. VM gone, volumes kept — resumable |
| `completed` | Finished successfully |
| `failed` | Finished with an error. `error` says what |
| `canceled` | Ended by the caller |
| `reaped` | Volumes reclaimed. The row remains, the data does not |

Terminal states are `completed`, `failed`, `canceled` and `reaped`.

```text
created -> scheduled -> booting -> bootstrapping -> running <-> waiting_input
running/waiting_input -> stopping -> stopped        (resumable)
running/waiting_input -> completed | failed | canceled
stopped/completed/failed -> scheduled               (resume, or a follow-up turn)
any terminal, and stopped -> reaped
```

Two edges matter to a client. **`stopped` is not terminal** — it is a parked
session with its volumes intact, and `POST /resume` moves it back to
`scheduled`. And a successful result now *completes* a session rather than
leaving it running, so `completed -> scheduled` is the ordinary path for a
follow-up turn: `POST /input` on a finished session resumes it instead of
erroring.

Transitions are validated server-side; an out-of-order worker report is
rejected rather than applied, so a client can trust the order it sees.

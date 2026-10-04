# Session sequence flows

Five flows through puku-agent-cloud. The platform never reimplements the agent: it
runs stock `puku-cli` headless inside a microVM and turns its stdout into a durable,
replayable event stream. Everything below follows from that.

## Cast

| Participant | Where | Role |
| --- | --- | --- |
| `puku-cloud` | client | CLI + dashboard. REST, plus one WebSocket per attached session. |
| `puku-controld` | control plane | REST API, scheduler, client relay, worker link. Allocates every event's global `seq`. |
| Postgres | control plane | Source of truth: sessions, partitioned event log, quotas, audit, NOTIFY bus. |
| `puku-workerd` | worker host | One daemon per Linux/KVM box. One actor + one microVM per session. |
| `/session` | worker host | Host dir bind-mounted into the guest. `events.ndjson`, `stdin.fifo`, durable `HOME`. |
| microVM guest | guest | `puku-runner` supervising `puku-cli -p --output-format stream-json`. |

---

## 1. Cold start: create → boot → stream → complete

```mermaid
sequenceDiagram
  autonumber
  participant CLI as puku-cloud
  participant CTL as controld
  participant PG as Postgres
  participant WK as workerd
  participant VOL as /session volume
  participant GST as microVM guest

  CLI->>CTL: POST /v1/sessions {prompt, repo}
  CTL->>PG: check_quota(org), INSERT sessions (created)
  CTL->>CTL: dispatch_pending → pick least-loaded, non-draining worker
  CTL->>PG: assign_worker, transition → scheduled
  CTL->>WK: AssignSession {spec}
  Note over CTL,WK: one persistent WS, dialed OUT by the worker
  WK->>VOL: create session + workspace dirs, write manifest.json and spec.json
  Note over WK,VOL: manifest.json is the guest-visible subset —<br/>credentials excluded by construction
  WK->>CTL: SessionState {booting}
  WK->>GST: create_detached, bind /session and /workspace
  WK->>CTL: SessionState {bootstrapping}
  WK->>GST: exec setsid sh /session/runner-launch.sh
  GST->>GST: puku-runner: clone repo, shred git token, HOME=/session/home
  GST->>GST: mkfifo stdin.fifo, hold on fd 3, write the first user turn
  WK->>CTL: SessionState {running}
  GST->>VOL: puku-cli stdout → redact → cap_lines → append events.ndjson
  loop poll every 250 ms
    WK->>VOL: read from the last byte offset (host-side, no RPC)
    WK->>CTL: SessionEvents [{line, payload}] — 50 per frame
    CTL->>PG: lock session, drop line ≤ cursor, assign seq, INSERT
    CTL-->>CLI: Events {seq, payload}
  end
  GST->>VOL: {type: result, subtype: success, is_error: false}
  WK->>CTL: SessionUsage {cost, tokens, cache read + write}
  WK->>WK: the result ends the turn → Exited(0)
  WK->>GST: stop and remove the sandbox — host volumes survive
  WK->>CTL: SessionState {completed}
  CTL->>PG: record_usage
```

- The worker never asks the VM for output. `/session` is a bind mount, so the guest's
  append-only `events.ndjson` is an ordinary file on the host.
- **The `result` line is what ends the turn**, not process exit. puku-cli runs with
  `--input-format stream-json` and stays alive for a follow-up, so waiting for
  `exec.exited` meant waiting for the idle timeout: a session that had succeeded sat
  `running` for 15 minutes and then reported `stopped`. `exec.exited` still arrives when
  the runner does exit, and still ends the session — it is no longer the only way.
- A `result` carrying `is_error: true` fails the session with the agent's own message,
  so an operator sees `API Error: 429 quota_exceeded` rather than an exit code.
- Credentials are filtered out of the manifest before it is written; the runner scrubs
  known secret values out of the stream before it leaves the VM.

---

## 2. Attach: replay from a cursor, then tail live

```mermaid
sequenceDiagram
  autonumber
  participant CLI as puku-cloud attach
  participant CTL as controld A
  participant PG as Postgres
  participant CTL2 as controld B

  CLI->>CTL: WS /v1/sessions/{id}/attach?api_key=pkc_...
  CLI->>CTL: Hello {after_seq: 0}
  CTL->>CTL: hub.subscribe(session) — before any replay
  Note over CTL: subscribing first means nothing published<br/>during replay can be dropped
  loop until a batch comes back empty
    CTL->>PG: fetch_events(after = cursor, limit 200)
    CTL-->>CLI: Events {batch}, cursor = last.seq
  end
  CTL-->>CLI: Live
  CTL-->>CLI: State {running}
  CTL-->>CLI: Events — forwarded only where seq > max_sent
  Note over CTL,CLI: the seq guard dedupes the replay / live seam
  CTL2->>PG: pg_notify(puku_events, {instance, session, seq range})
  CTL->>CTL: LISTEN puku_events — ignore my own instance id
  CTL->>PG: refetch that seq range, only if I have local subscribers
  CTL-->>CLI: Events {batch}
  Note over CTL,CTL2: attach to any instance, receive everything
```

- `after_seq: 0` replays the whole transcript; `-1` skips to live. The cursor is the
  client's, so detaching costs nothing.
- Persistence always happens before publish, so a lagging subscriber reconciles against
  Postgres instead of losing events.
- Cross-instance fanout carries only a seq range, never a payload — the peer refetches,
  and only if someone is actually watching.

---

## 3. The agent asks a question

```mermaid
sequenceDiagram
  autonumber
  participant CLI as puku-cloud
  participant CTL as controld
  participant PG as Postgres
  participant WK as workerd
  participant VOL as /session volume
  participant GST as microVM guest

  GST->>VOL: assistant event with an AskUserQuestion tool_use
  WK->>VOL: tail picks up the line
  WK->>WK: detect_question — AskUserQuestion, ExitPlanMode, or control_request
  WK->>WK: pending_question = true → idle timer backs off to 4x
  WK->>CTL: PendingQuestion {question}
  CTL->>PG: store pending_question, transition → waiting_input
  CTL-->>CLI: session.question event, State {waiting_input}
  CLI->>CTL: Answer {question_id, answer}
  CTL->>CTL: accepts_input? only running or waiting_input qualify
  CTL->>WK: DeliverInput {stream_json user message}
  WK->>GST: exec sh -c cat >> /session/stdin.fifo
  Note over WK,GST: the fifo write happens inside the guest —<br/>host-side fifo writes are not portable over virtio-fs
  GST->>GST: puku-cli reads the turn from fd 3 and resumes
  CTL->>PG: append the answer as a user event (replay shows both sides)
  CTL->>PG: clear pending_question, transition → running
  CTL-->>CLI: State {running}
```

- Interrupt takes the same route but ends in a signal: `pkill -INT -f puku-cli` in-guest.
- The user's message is written back into the event log, so replay reproduces the whole
  conversation, not just the agent's half.
- A session waiting on a human idles 4× longer before parking.

---

## 4. Park and resume on the same volumes

```mermaid
sequenceDiagram
  autonumber
  participant CLI as puku-cloud
  participant CTL as controld
  participant PG as Postgres
  participant WK as workerd
  participant VOL as /session volume
  participant GST as microVM guest

  alt idle timeout fires on the worker
    WK->>WK: no outbox line for idle_timeout_s (x4 while blocked)
  else user runs stop
    CLI->>CTL: POST /v1/sessions/{id}/stop
    CTL->>PG: transition → stopping
    CTL->>WK: StopSession {mode: Park}
  end
  WK->>GST: stop and remove the sandbox
  WK->>VOL: delete spec.json so restart-reconcile skips this session
  Note over WK,VOL: session/ and workspace/ are untouched —<br/>transcript, HOME and the git checkout all survive
  WK->>CTL: SessionState {stopped}
  CTL->>PG: stamp ended_at, record_usage

  Note over CLI,GST: hours later

  CLI->>CTL: POST /v1/sessions/{id}/resume
  CTL->>PG: worker_id = NULL, transition stopped → scheduled
  CTL->>WK: AssignSession {resume: true, puku_session_id, events_cursor}
  WK->>GST: boot a fresh microVM on the same two volumes
  GST->>GST: runner sees resume — skips the clone, skips the first prompt
  GST->>GST: puku-cli --resume, reading its transcript from /session/home
  WK->>VOL: re-tail events.ndjson from events_cursor
  WK->>CTL: SessionEvents — appended to the same seq line, uninterrupted
```

- The event log is continuous across a park: resume appends to the same
  `events.ndjson` and the same `seq` sequence, so an attach still replays one story.
- `resume` is derived, not requested — a session that ever reported a
  `puku_session_id` is resumed rather than started.
- **Resume is host-pinned in practice.** The dispatcher clears `worker_id` and re-places
  the session, but the volumes only exist on the original box; cross-worker volume moves
  are not implemented.

---

## 5. The worker daemon dies mid-session

```mermaid
sequenceDiagram
  autonumber
  participant CTL as controld
  participant PG as Postgres
  participant WK as workerd
  participant VOL as /session volume
  participant GST as microVM guest

  WK--xCTL: process dies, WebSocket drops
  CTL->>PG: mark the worker offline
  GST->>VOL: the agent keeps running and keeps appending lines
  Note over GST,VOL: nothing in the guest depends on workerd being alive

  WK->>WK: restart, reconcile_from_disk
  WK->>VOL: every session dir holding a spec.json gets a recovery actor
  WK->>CTL: Register {running_sessions: [...]}
  CTL->>PG: resume_cursors — max persisted guest_line per session
  CTL->>WK: RegisterAck {resume_cursors}
  WK->>GST: Sandbox::get(name).connect()
  alt the sandbox is gone
    WK->>CTL: SessionState {failed, sandbox gone after restart}
  else still alive
    WK->>VOL: re-tail from line 0, deliberately re-sending old lines
    WK->>CTL: SessionEvents [{line: 1..n}]
    CTL->>PG: drop every line ≤ last_guest_line, insert only the rest
    Note over CTL,PG: redelivery is idempotent by construction,<br/>so over-sending is the safe default
  end
  CTL->>WK: StopSession {Kill} for sessions the platform already ended
  Note over CTL,WK: reconnect reaps only sessions already archived
```

- Worker registration doubles as the crash-recovery handshake: `Register` lists what the
  worker still holds, the ack answers with where to resume.
- The recovered actor ignores those cursors and re-tails from zero, leaning entirely on
  the DB-side drop. Correct, but `RegisterAck.resume_cursors` is currently unused on the
  worker (bound to `_resume_cursors` in `controlplane.rs`).
- Deleting `spec.json` is what ends a session's claim on reconcile.

---

## Why every flow is safe to interrupt anywhere

1. **Total order** — `seq` is allocated under `SELECT … FOR UPDATE` on the session row,
   incremented in the same transaction. One writer per session: no gaps, no ties.
2. **Identity** — a guest line number names the event forever, because `events.ndjson` is
   append-only and never rewritten. Anything at or below the persisted cursor is dropped,
   which is what makes redelivery free.
3. **Durability first** — the broadcast fanout only ever sees committed events, so any
   subscriber can reconcile against Postgres.
4. **No seam** — attach subscribes before replaying, then suppresses anything at or below
   what replay already sent.

Source: `crates/puku-cloud-proto/src/worker_proto.rs`, `crates/puku-controld/src/db/mod.rs`,
`crates/puku-controld/src/api/mod.rs`, `crates/puku-workerd/src/session_actor.rs`,
`images/puku-agent/runner/puku-runner.sh`

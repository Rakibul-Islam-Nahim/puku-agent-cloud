# Architecture

What the pieces are and how they fit. [SEQUENCE-FLOWS.md](SEQUENCE-FLOWS.md)
covers how a session moves through them over time; [DEPLOYMENT.md](DEPLOYMENT.md)
covers standing them up on a box.

- [The platform](#the-platform)
- [Engines and machines](#engines-and-machines)
- [Inside controld](#inside-controld)
- [Inside puku-skills-service](#inside-puku-skills-service)
- [Where the telemetry goes](#where-the-telemetry-goes)

---

## The platform

Two independent deployments — the control plane and the skills registry —
each with its own Postgres, sharing object storage.

```mermaid
flowchart TB
  subgraph clients["Clients"]
    cli["puku cli<br/><code>puku cloud run · attach · push</code>"]
    dash["Dashboard<br/>served at /"]
    hooks["Cron · webhooks"]
  end

  platform["chat.api.puku.sh<br/>identity · /auth/verify"]
  cli -. "signs in once" .-> platform

  subgraph cloud["puku-agent-cloud"]
    cd["controld :7770<br/>REST · attach WS · worker link"]
    pg1[("puku_cloud<br/>sessions · events (partitioned)<br/>credentials · schedules")]
    subgraph fleet["Worker fleet — systemd, needs /dev/kvm"]
      wk["workerd<br/>one actor per session or machine"]
      vm["microVM (libkrun or Cloud Hypervisor)<br/>puku-cli headless<br/>/workspace · /session"]
      wk --> vm
    end
    cd --> pg1
  end

  subgraph skills["puku-skills-service"]
    sk["skills :7870"]
    pg2[("puku_skills<br/>packs · versions · skills")]
    sk --> pg2
  end

  r2[("Object storage<br/>packs · blobs · artifacts")]

  clients --> cd
  cd -->|"verify, never decode"| platform
  cd -->|"AssignSession over one<br/>outbound WebSocket"| wk
  wk -->|"events · usage · questions<br/>heartbeat + capacity"| cd

  cd -->|"resolve packs :7870<br/>name@version + digest + presigned URL"| sk
  sk --> r2
  wk -->|"download pack, verify digest"| r2
  wk -->|"presigned PUT — blobs, artifacts"| r2
  cd --> r2

  vm -->|"model calls"| gw["api-cli.puku.sh<br/>model gateway"]
  vm -->|"MCP, brokered"| mcp["mcp.proxy.puku.sh"]

  classDef store fill:#eef4fa,stroke:#5b8db8
  class pg1,pg2,r2 store
```

**The worker never talks to the skills registry.** controld resolves packs at
dispatch and puts `name@version` plus a digest and a presigned URL on the
session spec; the worker downloads the object and verifies the digest against
what controld told it. The guest gets neither a URL nor a token — it sees only
unpacked files. (`README.md` drew a `workerd → skills` edge for a while; it was
never real.)

**The agent is puku-cli itself, headless inside the microVM.** The platform
never reimplements the agent loop — it boots the VM, relays the event stream,
and gets the work back out. That one rule is what keeps a cloud session and a
local one the same agent.

---

## Engines and machines

```mermaid
flowchart LR
  subgraph cp["controld"]
    api["/v1/sessions · /v1/machines"] --> pick["placement<br/>engine · feature · slots · volume pin"]
    links["/v1/links/{cap}/… (separate hostname)"]
    data["/v1/worker/data<br/>pooled data sockets"]
  end
  subgraph host["worker host"]
    wk["workerd"]
    seam["vm::VmBackend"]
    msb["libkrun<br/>microsandbox SDK · agentd"]
    ch["Cloud Hypervisor<br/>systemd unit · virtiofsd · TAP + nftables<br/>guest init: puku-guestd (vsock)"]
    wk --> seam --> msb
    seam --> ch
  end
  pick -- "AssignSession / AssignMachine<br/>(JSON control link)" --> wk
  wk -- "dials out" --> data
  bot["puku-bot-svc<br/>puku-cloud sandbox provider"] -- "pkc_ key" --> api
  browser["bot owner's browser<br/>noVNC iframe"] -- "capability URL" --> links
```

**Two engines, one seam.** Everything above `vm::VmBackend` in workerd --
the outbox, the stdin fifo, reconcile, idle parking, machines -- is
engine-agnostic. Each request names an engine (`libkrun` by default); a
worker advertises the engines whose runtime checks pass on its host; and
controld only places a VM on a worker that advertised its engine. A worker
from before engines existed advertises nothing and is treated as
libkrun-only, which is what makes the field safe to add.

**Machines are VMs without puku-cli.** A session is an agent in a VM; a
machine is only the VM, driven from outside: exec, files, archives, guest
ports. puku-bot's computers are machines. Contract: [MACHINES-API.md](MACHINES-API.md).

**Bytes never ride the control link.** It is one JSON socket carrying every
session's events with no backpressure. Exec, file, archive and port traffic
takes a socket from a small pool each worker keeps dialled out to
`/v1/worker/data`, one stream per socket. Workers still never accept a
connection.

**Volumes pin placement.** Once a session or machine has run, its volume is
on that worker's disk and nowhere else. A resume or a start goes back there;
if the worker is gone for more than 15 minutes, a session fails with that
reason and a machine is re-placed with a fresh volume (`resumed: false`, so
the caller restores its own copy).

---

## Inside controld

```mermaid
flowchart TB
  subgraph edge["Edge"]
    rest["REST + attach WebSocket"]
    link["Worker link<br/>/v1/worker"]
  end

  subgraph auth["Authentication"]
    who{"bearer or pkc_?"}
    bearer["platform bearer<br/>verified at /auth/verify<br/>→ own org, own bill"]
    apikey["pkc_ key<br/>org-scoped, no user<br/>→ CI and operators"]
    who -->|"pk_/oauth"| bearer
    who -->|"pkc_"| apikey
  end

  subgraph disp["Dispatcher"]
    claim["claim a worker slot<br/>capacity from the host"]
    cred["resolve credential<br/>session → org → refresh mint"]
    packs["resolve skill packs"]
    clamp["clamp permission mode<br/>to the deployment ceiling"]
    claim --> cred --> packs --> clamp
  end

  subgraph loops["Background — all supervised"]
    d3["dispatch retry · 3s"]
    sch["scheduler · 20s"]
    arch["archive · 6h"]
    rel["NOTIFY relay<br/>cross-instance fanout"]
  end

  pg[("Postgres")]
  rest --> who
  bearer --> disp
  apikey --> disp
  clamp -->|"AssignSession {spec}"| link
  link -->|"events · usage · state"| pg
  disp --> pg
  loops --> pg
  d3 --> disp
  sch --> disp

  classDef store fill:#eef4fa,stroke:#5b8db8
  class pg store
```

Two things in here are easy to miss and both are load-bearing:

- **Credential resolution never falls back to the operator.** Session
  credential, then the org's — minting a fresh bearer from a stored refresh
  token if that is what it holds — and then nothing. A session with none is
  refused before a VM boots, unless `PUKU_ALLOW_OPERATOR_CREDENTIALS` is on.
- **Capacity comes from the host, not a config value.** The worker advertises
  `used + what still fits` on every heartbeat, so a filling box stops being
  assigned work and recovers on its own.

---

## Inside puku-skills-service

A content-addressed registry. Postgres owns identity and versioning; object
storage owns the bytes.

```mermaid
flowchart TB
  subgraph pub["Publishing"]
    author["author<br/>tar.gz of a pack"]
    val["validate<br/>no symlinks · no ..<br/>32 MiB · 2000 entries"]
    idx["index skills<br/>SKILL.md frontmatter"]
    dig["sha256 → object key"]
    author --> val --> idx --> dig
  end

  subgraph res["Resolution — at dispatch"]
    req["packs=office,essentials@2.1"]
    match["highest non-yanked<br/>semver match"]
    sign["presign GET · 15 min"]
    req --> match --> sign
  end

  subgraph who["Callers"]
    op["operator token<br/>may publish builtins"]
    user["platform bearer<br/>own org only"]
  end

  pg[("packs · pack_versions<br/>skills · org_packs")]
  r2[("packs/{digest}.tgz")]

  who --> pub
  who --> res
  dig --> pg
  dig --> r2
  match --> pg
  sign --> r2

  classDef store fill:#eef4fa,stroke:#5b8db8
  class pg,r2 store
```

- **An org's pack shadows a builtin of the same name** — the lookup orders by
  `org_id IS NULL`, so publishing `office` privately overrides the shipped one
  without renaming anything.
- **The key is the digest**, so publishing identical bytes twice stores them
  once, and the worker can verify what it downloaded against what controld
  promised.
- **The guest never talks to this service** and never holds a token for it.

---

## Where the telemetry goes

```mermaid
flowchart TB
  subgraph reporting["Reports to Sentry"]
    cd["controld<br/>5xx · panics · dispatch failures<br/>HTTP transactions"]
    wk["workerd<br/>boot · reap · runner exit<br/>one hub per session actor"]
    sk["skills<br/>5xx · publish · resolve"]
  end

  subgraph silent["Deliberately not reporting"]
    vm["microVM guest<br/>puku-cli + agent output"]
  end

  wk -.->|"boots, never instruments"| vm
  sentry["Sentry<br/>tags: session_id · org_id · worker"]
  cd --> sentry
  wk --> sentry
  sk --> sentry

  prom["/metrics<br/>sessions · workers"] --> ext["external alerting<br/>catches OOM-kill"]
  cd --> prom
```

**The microVM is outside the boundary on purpose.** A DSN inside a sandbox
running untrusted agent code is an exfiltration channel, and agent output —
the customer's work — would flow to a third party. The same reasoning
`connectors.rs` applies to refresh tokens.

**Correlation is by tag, not by distributed trace.** The worker protocol
carries no trace id, and a session lives minutes to hours — far too long to be
one transaction. Every event in all three services carries the same
`session_id`, which makes one session's failures searchable across them.

**What Sentry cannot see stays on `/metrics`:** an OOM-kill or a stack
overflow never unwinds, so no panic hook fires. workerd boots microVMs, which
makes that a realistic way for it to die.

Field values are whitelisted before anything leaves the process — see
`crates/puku-observability/src/scrub.rs`. `SessionSpec` carries `prompt` and
`git_token`, so one careless `tracing::error!(?spec, …)` would ship a
customer's prompt and a GitHub token; a blacklist would fail open the first
time someone logged a field nobody had thought of.

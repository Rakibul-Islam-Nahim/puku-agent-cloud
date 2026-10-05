# puku-agent-cloud

Self-hosted agent cloud for **puku-cli**: each session runs headless inside a
hardware-isolated [microsandbox](https://github.com/superradcompany/microsandbox)
microVM on your own Linux/KVM servers and streams live back to the user's
terminal. Design doc: [`../AGENT-CLOUD-DESIGN.md`](../AGENT-CLOUD-DESIGN.md).

![alt text](image.png)
## Layout

| Path | What |
| --- | --- |
| `crates/puku-cloud-proto` | Shared wire types: event envelope, session state machine, worker frames, client WS protocol |
| `crates/puku-controld` | Control plane: REST API, client attach relay, worker WebSocket link, Postgres persistence |
| `crates/puku-workerd` | Per-host worker daemon; one microVM per session or machine, on libkrun (microsandbox SDK) and/or Cloud Hypervisor |
| `crates/puku-guestd` | Init and host agent inside Cloud Hypervisor guests (vsock: exec, ports, shutdown) |
| `crates/puku-leases` | Host liveness leases and the sweeper (suspect, dead, mass-loss guard) |
| `crates/puku-volume` | Volume backends: Ceph RBD (map, exclusive lock, fence by blocklist) and local |
| `crates/puku-fence` | Fencing with an audit trail (`fence_log`): Ceph blocklist, IPMI/Redfish hooks |
| `crates/puku-snapshot`, `puku-proxy`, `puku-rebuild` | Designed, not wired in yet: snapshot model and restore plan, reconnect proxy, environment rebuild. The snapshots and disk backups that run today are controld's own (`snapshots.rs`, `diskbackup.rs`) |
| `crates/puku-cloud-cli` | `puku-cloud` client: `run / ls / attach / answer / input / interrupt / stop / resume / cancel` |
| `migrations/` | sqlx migrations (applied automatically by controld at startup) |
| `images/puku-agent/` | Guest OCI image + `puku-runner` in-guest supervisor |
| `deploy/` | compose for dev deps, systemd units, provisioning + msb prestage scripts |

## Documentation

| Doc | For |
| --- | --- |
| [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md) | Standing the whole thing up on a bare-metal box, step by step, ending with a test sequence |
| [`docs/CLI-WALKTHROUGH.md`](docs/CLI-WALKTHROUGH.md) | Driving it from `puku cloud` — teleport, schedules, document runs |
| [`docs/API.md`](docs/API.md) | The control plane's HTTP API |
| [`docs/MACHINES-API.md`](docs/MACHINES-API.md) | Machines: generic VMs driven from outside (what puku-bot's computers run on) |
| [`docs/CLOUD-HYPERVISOR-PLAN.md`](docs/CLOUD-HYPERVISOR-PLAN.md) | The two-engine design (libkrun + Cloud Hypervisor), machines, and the puku-bot integration |
| [`../puku-skills-service/docs/API.md`](../puku-skills-service/docs/API.md) | The skill registry's HTTP API |
| [`docs/PUKU-CLI-CONTRACT.md`](docs/PUKU-CLI-CONTRACT.md) | The headless puku-cli contract, as measured against the real binary |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Component diagrams: the platform, controld, the skills registry, and where telemetry goes |
| [`docs/SEQUENCE-FLOWS.md`](docs/SEQUENCE-FLOWS.md) | Sequence diagrams for each session flow |
| [`skills/deployment-test/`](skills/deployment-test/) | An agent skill that runs the test sequence for you |
| [`docs/RELIABILITY-REBUILD.md`](docs/RELIABILITY-REBUILD.md) | The reliability design: leases, fencing, shared disks, snapshots, recovery |
| [`PLAN.md`](PLAN.md) | Reliability work tracker; section 4 is what is built and tested |
| [`docs/SDK-MIGRATION-PLAN.md`](docs/SDK-MIGRATION-PLAN.md) | Driving the guest agent with `puku-agent-sdk` — compatibility findings, what is built, and the gate before it becomes the default |

## Setup

Three ways to run it, from smallest to the real thing. Each builds on the
one before. The full bare-metal walkthrough, with secrets, tunnel, guest
images and a paid test sequence, is [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md);
this section is the map and the parts that guide does not cover.

| Goal | You need | Section |
| --- | --- | --- |
| Build, run every test | Linux or macOS, Rust, Docker (or a local Postgres) | [1](#1-build-and-test) |
| One box running real sessions | Linux with `/dev/kvm`, Postgres, the guest image | [2](#2-one-box-dev-or-single-host) |
| Sessions that survive a dead host | 2+ worker hosts, a Ceph cluster | [3](#3-reliability-surviving-dead-hosts-crashed-vms-and-a-lost-pool) |

### Prerequisites

| What | Why | Install |
| --- | --- | --- |
| Rust stable (edition 2021) | builds everything | `curl https://sh.rustup.rs -sSf \| sh` |
| `build-essential pkg-config libssl-dev libcap-ng-dev` (Linux) | workerd links the microsandbox SDK, which needs `libcap-ng` | `sudo apt-get install -y build-essential pkg-config libssl-dev libcap-ng-dev` |
| Postgres 16+ | all state; migrations run on controld startup | `docker compose -f deploy/compose.dev.yml up -d postgres`, or a local server |
| Linux + `/dev/kvm` | **workers only**: every session is a microVM | bare metal or nested virt; check with `ls -l /dev/kvm` |
| Docker | building the guest image; dev Postgres/MinIO | docker.com |
| Node 20 | only the runner test in CI | nodejs.org |
| `ceph-common` (`rbd`, `ceph`) | only for shared session disks (section 3) | `sudo apt-get install -y ceph-common` |

controld itself runs anywhere (it is also shipped as a container, see
`Dockerfile`). Only workerd needs KVM.

### 1. Build and test

```sh
git clone https://github.com/Rakibul-Islam-Nahim/puku-agent-cloud && cd puku-agent-cloud
cargo build --workspace

# unit tests: no database, no KVM
cargo test --workspace

# integration tests: a real controld + a fake worker over the real
# worker protocol, against Postgres. They SKIP without this variable.
docker compose -f deploy/compose.dev.yml up -d postgres
docker compose -f deploy/compose.dev.yml exec postgres createdb -U puku puku_test
PUKU_TEST_DATABASE_URL=postgres://puku:puku@127.0.0.1:5432/puku_test cargo test --workspace
```

The integration suite creates a schema per test, so many run in parallel
against one database; give that Postgres `max_connections` of a few hundred
if you see "too many clients". Use a database the suite may drop schemas in,
never a real one.

### 2. One box (dev or single host)

**Dependencies.** Postgres, plus MinIO if you want artifacts, archives and
machine snapshots (it stands in for Cloudflare R2 or any S3 API):

```sh
docker compose -f deploy/compose.dev.yml up -d postgres minio minio-init
```

**Guest image.** Build it, and load it where the worker's msb can see it
(see "Load them into msb" in `docs/DEPLOYMENT.md`, step 6):

```sh
docker build -t puku-agent images/puku-agent
```

**Control plane.** Defaults: listens on `127.0.0.1:7770`, database
`postgres://puku:puku@127.0.0.1:5432/puku_cloud`, auth off (every request is
the dev org).

```sh
export PUKU_AGENT_IMAGE=puku-agent:latest
export PUKU_AI_API_KEY=...                  # operator model key, dev only
export PUKU_ALLOW_OPERATOR_CREDENTIALS=true # allow that key to reach guests
export PUKU_SECRET_KEY=$(openssl rand -hex 32)   # encrypts credentials at rest
# optional object storage (MinIO from the compose file):
export PUKU_R2_ENDPOINT=http://127.0.0.1:9000 PUKU_R2_BUCKET=puku-cloud \
       PUKU_R2_REGION=us-east-1 PUKU_R2_ACCESS_KEY_ID=puku PUKU_R2_SECRET_ACCESS_KEY=puku-dev-secret
./target/debug/puku-controld
```

**Worker** (same box or another; it dials out to controld, nothing listens):

```sh
# one token per host, printed once:
./target/debug/puku-controld gen-worker-token --name box-1 > /tmp/worker-token

sudo PUKU_CONTROLD_URL=ws://127.0.0.1:7770/v1/worker \
     PUKU_WORKER_NAME=box-1 PUKU_WORKER_TOKEN_FILE=/tmp/worker-token \
     PUKU_STATE_DIR=/var/lib/puku \
     ./target/debug/puku-workerd
```

Production hosts use the systemd units and scripts instead:
`deploy/scripts/prestage-msb.sh` (stage the VM toolchain so nothing
downloads at runtime), `deploy/scripts/setup-worker.sh`,
`deploy/systemd/puku-workerd.service`, and `deploy/scripts/preflight.sh`
(refuses to start on a host that cannot run VMs).

**Run a session:**

```sh
./target/debug/puku-cloud run "fix the failing test in src/auth" --repo https://github.com/you/repo
./target/debug/puku-cloud ls
./target/debug/puku-cloud attach <session-id>
```

The client reads `PUKU_CLOUD_URL` (default `http://127.0.0.1:7770`) and
`PUKU_CLOUD_API_KEY`. With `PUKU_AUTH=required`, mint a key:
`./target/debug/puku-controld gen-key --org dev --name me`.

**Stub smoke test** (no puku-cli, no model, no money): proves
controld → worker → VM → events → client with a stock image and a fake
runner.

```sh
PUKU_AGENT_IMAGE=alpine ./target/debug/puku-controld &
PUKU_RUNNER_CMD='echo "{\"type\":\"result\",\"subtype\":\"success\",\"total_cost_usd\":0}" >> /session/events.ndjson' \
  ./target/debug/puku-workerd &
./target/debug/puku-cloud run "smoke"
```

### 3. Reliability: surviving dead hosts, crashed VMs and a lost pool

Design: `docs/RELIABILITY-REBUILD.md`; what is built and tested: `PLAN.md`
section 4. The pieces, and what each needs:

| Piece | What it does | Needs |
| --- | --- | --- |
| Host leases | notices a dead worker host in ~18 s and settles its work | nothing (on by default) |
| Shared disks | session and machine disks on Ceph RBD, so work can move hosts | Ceph + `PUKU_RBD_POOL` |
| Fencing | cuts a dead host off a disk before anyone else opens it (blocklist, then a 5 s wait until every storage daemon enforces it) | Ceph user with `osd blocklist` |
| VM watchdog | restarts a VM that died or hung on a healthy host | nothing (on by default) |
| Storage cleanup | deletes finished disks even when their host is down | shared disks |
| Off-cluster backups | hourly encrypted backups; rebuilds a disk the pool lost | shared disks + object storage + `PUKU_SECRET_KEY` |

**Host leases: on by default, nothing to configure.** Every worker sends
a small "alive" frame each second; controld keeps one lease per host in
Postgres:

| After the host goes silent | What controld does |
| --- | --- |
| 3 s | *suspected*: no new work goes there |
| 15 s more | *dead*: its work is settled (below) |
| more than 30 % of 3+ hosts silent at once | declares nobody dead and logs `MASS HOST LOSS` (likely the network, not the hosts) |

What "settled" means for a dead host's work:

| What it was running | With shared disks (Ceph) | Without |
| --- | --- | --- |
| Session mid-turn | continues on another host by itself, with a "continue where you left off" message, after the old host is fenced | stopped; a resume waits for the host |
| Session waiting for an answer | stopped; the answer resumes it on another host | stopped; a resume waits for the host |
| Machine | boots on another host with its own disk (volume and root disk), after the fence | restored from its latest snapshot, or stopped if it has none |
| Work it never started | requeued | requeued |

The worker never stops its own VMs when it loses controld, so a
control-plane outage is not a data-plane outage. Only one controld instance
sweeps at a time (Postgres advisory lock); run as many as you like.

**Shared session disks: opt-in, needs Ceph.** Without this, a session's
files live on the worker that ran it, and if that host dies the session can
only fail after a 15-minute grace. With it, each session has its own RBD
image, and a session whose host died continues on another host, after
controld has fenced the old host off its disk.

1. **On the Ceph cluster**, once: a pool, and a cephx user that may map RBD
   images and blocklist clients (the fence):

   ```sh
   ceph osd pool create puku-sessions 32
   ceph osd pool application enable puku-sessions rbd
   rbd pool init puku-sessions
   ceph auth get-or-create client.puku \
     mon 'profile rbd, allow command "osd blocklist"' \
     osd 'profile rbd pool=puku-sessions' \
     -o /etc/ceph/ceph.client.puku.keyring
   ```

2. **On every worker host:** `ceph-common`, `/etc/ceph/ceph.conf` and that
   keyring, the `rbd` kernel module (`deploy/scripts/prestage-rbd.sh`
   checks), then:

   ```sh
   PUKU_RBD_POOL=puku-sessions      # turns it on; same pool on every worker
   PUKU_CEPH_USER=puku              # default
   PUKU_CEPH_CONF=/etc/ceph/ceph.conf
   PUKU_RBD_SIZE_MIB=20480          # per session, thin-provisioned (default)
   PUKU_RBD_MACHINE_SIZE_MIB=40960  # per machine: volume + root disk (default)
   ```

   workerd must run as root (it maps, formats and mounts the images). A
   session's disk is mounted at `$PUKU_STATE_DIR/sessions/<id>/disk` only
   while it runs there, mapped `--exclusive` so no second host can open it,
   and released when it stops.

3. **On controld:** the same three variables (`PUKU_RBD_POOL`,
   `PUKU_CEPH_USER`, `PUKU_CEPH_CONF`), plus `ceph-common` and the keyring
   on the controld host: controld is what runs the fence.

How a resume chooses its host:

| The host the session last ran on | Result |
| --- | --- |
| connected | goes back there (no fence) |
| declared dead by its lease | old host fenced off the disk (`fence_log`), then any shared-disk worker |
| away, not yet declared dead | waits: fencing a live host would cut every disk it has open |
| fence fails | stays queued: never two writers |

Machines get the same treatment: with `PUKU_RBD_POOL` set, a machine's whole
state directory (its volume and its kept root disk, so installed packages
too) is one RBD image, `machine-<id>`, sized by `PUKU_RBD_MACHINE_SIZE_MIB`
(default 40960, thin). Only an explicit destroy deletes it; a worker cleaning
up after a machine moved away never does.

**Storage cleanup** keeps the pool from filling up, and it does not depend
on any worker being alive:

| Where | When | What it removes |
| --- | --- | --- |
| controld (one instance, advisory lock) | every `PUKU_STORAGE_GC_S` (default 600 s) | RBD images whose session is archived or gone, or whose machine is destroyed or gone, once they have looked that way for `PUKU_STORAGE_GC_GRACE_S` (default 1 h). Never an image someone has open, never a name it does not recognise. Every delete is audited (`storage.gc.delete`). `PUKU_STORAGE_GC_DRY_RUN=true` logs instead of deleting |
| workerd, at startup | when a host starts or comes back after being declared dead | disks it still has mapped but no longer runs (unmounted, force-unmapped: a fenced host's dead mappings), and local session folders whose disk lives in Ceph. It never reattaches a session whose disk is not mounted |
| controld, always | every report | a report or event from a worker that no longer owns the session is ignored, and that worker is told to kill its copy |

**VM watchdog.** Every 15 s the worker runs `true` inside each running VM. Three
missed answers in a row (about 45 s) mean the VM died or hung while its host
stayed up: it is torn down, its disk released, and controld starts it again
(a session that was mid-turn continues with a "your VM crashed, continue"
message; one waiting for an answer is only stopped). A third crash within 30
minutes stops the automatic restarts and the reason says so.

**Off-cluster disk backups.** Ceph's three copies cover a lost drive or host,
not a lost pool. With shared disks, object storage (`PUKU_R2_*`) and
`PUKU_SECRET_KEY` all set, one controld backs up every shared disk each
`PUKU_DISK_BACKUP_INTERVAL_S` (default 3600): an RBD snapshot, then a full
`rbd export` the first time (and after 24 diffs) or an `rbd export-diff` of
what changed since the last backup (skipped when nothing changed). Each
export is compressed and encrypted in 4 MiB frames with its own data key,
uploaded in parts, then read back and checked before it counts. If a shared
disk is ever missing when it is needed, controld rebuilds it from the latest
full plus every later diff before anything boots on it; with no backup the
session fails with that reason instead of starting on an empty disk.
`PUKU_DISK_BACKUP=false` turns this off; `PUKU_DISK_BACKUP_TMP` is where
exports are staged (needs room for the largest disk).

Operator note: a host that was fenced keeps a blocklisted Ceph client until
its dead mappings are dropped; the startup cleanup does that, but rebooting a
fenced host before it rejoins is still the safe default.

#### Reliability settings at a glance

| Setting | On | Default | What it does |
| --- | --- | --- | --- |
| `PUKU_RBD_POOL` | controld + every worker | unset (off) | the Ceph pool for shared disks; same on all |
| `PUKU_CEPH_USER`, `PUKU_CEPH_CONF` | controld + every worker | `puku`, tool default | Ceph credentials and config |
| `PUKU_RBD_SIZE_MIB` | worker | 20480 | size of a new session disk (thin) |
| `PUKU_RBD_MACHINE_SIZE_MIB` | worker | 40960 | size of a new machine disk (thin) |
| `PUKU_RBD_MAP_OPTIONS` | worker | empty | extra `rbd device map -o` options (`noshare` for tests) |
| `PUKU_STORAGE_GC_S` | controld | 600 | seconds between storage cleanup sweeps |
| `PUKU_STORAGE_GC_GRACE_S` | controld | 3600 | how long a disk must look finished before it is deleted |
| `PUKU_STORAGE_GC_DRY_RUN` | controld | false | log deletions instead of doing them |
| `PUKU_DISK_BACKUP` | controld | true | off-cluster backups (needs object storage + `PUKU_SECRET_KEY`) |
| `PUKU_DISK_BACKUP_INTERVAL_S` | controld | 3600 | seconds between backups of one disk |
| `PUKU_DISK_BACKUP_TMP` | controld | system temp dir | where exports are staged; needs room for the largest disk |

#### Hardware for the full reliability setup

The software runs on one machine for development and tests. To run (and
prove) it for real:

| What | How many | For | Minimum |
| --- | --- | --- | --- |
| Worker servers, bare metal | 2+ | the VMs; one can die and its work moves | VT-x / AMD-V, 16 cores, 64 GB RAM, 500 GB NVMe, IPMI or Redfish port |
| Ceph storage servers | 3 | shared disks; Ceph needs 3 to survive losing one | 8 cores, 32 GB RAM, 1–2 NVMe of 1 TB+ each |
| Control server (a VM is fine) | 1 | controld, Postgres, MinIO for backups | 4 cores, 16 GB RAM, backup disk ~2x the data on shared disks |
| Network | — | Ceph traffic between all of them | 10 Gbps, 25 Gbps recommended; a separate Ceph network is better |

All of it is open source on Ubuntu 24.04; no licences. On a tight budget the
three Ceph servers can also be the workers for a test, but then losing one
server loses a worker and a storage node at once.

Still to do, and why it needs that hardware: memory snapshots (so running
processes survive, not only files) wait on a speed comparison of VM engines
on real servers; production object storage is a MinIO or Ceph RGW install;
the final proof is unplugging a real server, and power-off fencing through
its IPMI/Redfish port.

### Running the tests that need real infrastructure

| Suite | Needs | Command |
| --- | --- | --- |
| Unit | nothing | `cargo test --workspace` |
| controld integration (~235 tests) | Postgres | `PUKU_TEST_DATABASE_URL=postgres://… cargo test --workspace` |
| RBD fencing on real Ceph | Ceph, root, user `client.puku`, pools `puku-base` (with protected `agent-base@v1`) and `puku-sessions` | `PUKU_TEST_CEPH=1 cargo test -p puku-volume --test real_ceph` |
| Session disks moving between hosts on real Ceph | Ceph, root, pool `puku-sessions` | `PUKU_TEST_CEPH=1 cargo test -p puku-workerd real_ceph -- --test-threads=1` |
| Disk backup and restore on real Ceph | Ceph, root, Postgres | `PUKU_TEST_CEPH=1 PUKU_TEST_DATABASE_URL=… cargo test -p puku-controld real_ceph` |

A single-node test Ceph works (MicroCeph: `snap install microceph`,
`microceph cluster bootstrap`, `microceph disk add loop,4G,3`). Use the
distribution's `/usr/bin/rbd` from `ceph-common`, not the snap's: the snap's
confinement blocks it from mapping devices. The Ceph tests run
two "hosts" on one machine with `noshare` mappings.

## Skills and connectors

Two different things, deliberately kept apart:

- **Connectors give reach.** Brokered through `mcp.proxy.puku.sh`, the same
  proxy Puku Desktop uses. The guest gets an MCP endpoint plus the user's
  puku JWT; the proxy swaps that for the vendor token server-side, so **no
  third-party OAuth token ever enters the microVM**. On by default;
  `--no-connectors` to opt out.
- **Skills give competence.** Resolved from
  [puku-skills-service](../puku-skills-service) at dispatch, downloaded and
  **digest-verified** by the worker, then unpacked into
  `$HOME/.puku-cli/skills` where puku-cli discovers them with no
  configuration. `--pack office` to name one; omit it for the org's
  defaults. Set `PUKU_SKILLS_URL` to enable; unset means no skills.

## Egress

Two modes, and the default is open:

| Mode | When | Effect |
| --- | --- | --- |
| Open | single-tenant (default), or `PUKU_EGRESS_UNRESTRICTED=1` | No network policy attached; every host reachable |
| Allowlisted | `PUKU_MULTI_TENANT=1` | Domain-suffix allowlist; everything else 403s |

The microVM is a hardware isolation boundary either way. What the allowlist
adds is an *exfiltration* boundary — with egress open, a prompt injection
from a fetched page can POST the workspace anywhere. That is a reasonable
trade on a box you own and a bad one when running other people's code,
which is why opening it is explicit.

A blocked request returns **403 with a reason**, not a DNS failure, so the
agent can tell policy from breakage instead of retrying forever.

## Using it from puku-cli

Cloud sessions are a first-class part of the CLI (`puku cloud …`, implemented
in `puku-code-cli/src/cloud/`). Authentication piggybacks on the login you
already have — no separate credential to manage:

```sh
puku auth login                    # once; the cloud verifies this same token
export PUKU_CLOUD_URL=https://cloud.puku.sh

puku cloud run "fix the failing test in src/auth" --repo https://github.com/you/repo
puku cloud ls
puku cloud attach <session-id>     # replay + follow live, answer questions inline
puku cloud input <session-id> "also update the changelog"
puku cloud stop|resume|cancel|interrupt <session-id>
puku cloud pull <session-id> --what workspace   # get the work out before reaping
```

`run` streams the session and returns when the turn finishes; the session
stays open for follow-ups (puku-cli stays interactive under stream-json, so
it does not go terminal on its own). Ctrl-C detaches without stopping the
cloud session. When the agent asks a question, `run` and `attach` render the
options and read your answer from the terminal — the platform holds the
question open indefinitely, so there is no timer.

Because the CLI presents *your* puku token, the session runs on your
credential and the cost lands on your account, not the operator's.

## Operating it

`GET /` serves the operator console: summary counts that double as filters,
the live session list, and a fleet panel showing each worker's sandboxes
plus anything that has drifted out of step. `GET /v1/fleet` is the same data
as JSON.

Tests: see [Running the tests that need real infrastructure](#running-the-tests-that-need-real-infrastructure).
CI (`.github/workflows/ci.yml`) installs `libcap-ng-dev`, runs
`cargo clippy --workspace --all-targets -- -D warnings`, runs every test
against a Postgres service (and fails if the integration tests silently
skip), runs the guest runner's test, and builds the controld image. The
real-Ceph tests are not in CI: they need a Ceph cluster and root.

## Architecture

```mermaid
flowchart TB
  subgraph clients["Clients"]
    cli["puku cli<br/><code>PUKU_CLOUD_URL=https://agent.api.puku.sh</code>"]
    dash["Dashboard<br/>served at /"]
    hooks["Cron · webhooks"]
  end

  platform["chat.api.puku.sh<br/>identity · /auth/verify"]
  cli -. "signs in once" .-> platform

  subgraph edge["Ingress — no inbound port is open on the box"]
    tunnel["cloudflared<br/>agent.api.puku.sh"]
  end
  clients --> tunnel

  subgraph host["One host · docker compose project puku-cloud"]
    subgraph cp["controld — control plane · 127.0.0.1:7770"]
      api["REST + attach WebSocket"]
      authm["auth: platform bearer | pkc_ key"]
      disp["dispatcher<br/>clamp policy · resolve credential,<br/>connectors, skills, memory"]
      api --> authm --> disp
    end
    mem["puku-memory-service :7970<br/>not published — compose network only"]
    pg[("Postgres<br/>puku_cloud · puku_memory")]
  end

  r2[("Object storage<br/>blobs · artifacts · transcripts")]

  tunnel --> api
  authm -->|"verify, never decode"| platform
  api <--> pg
  api --> r2
  mem <--> pg

  disp -->|"preamble on dispatch"| mem
  api -->|"transcript + this tenant's<br/>model credential, in headers"| mem
  mem -->|"extract · consolidate"| gw

  subgraph fleet["Worker fleet — systemd on the host, dials OUT"]
    wk["workerd<br/>one actor per session"]
    vm["microVM (msb)<br/>puku-cli headless<br/>/workspace · /session"]
    wk --> vm
  end

  disp -->|"AssignSession over one<br/>outbound WebSocket"| wk
  wk -->|"events · usage · questions<br/>· heartbeat + sandbox inventory"| api
  vm -->|"model calls"| gw["PUKU_AI_BASE_URL<br/>defaults to PUKU_API_URL"]
  vm -->|"MCP, Bearer &dollar;PUKU_API_KEY"| mcp["mcp.proxy.puku.sh<br/>connector broker"]
  disp -->|"resolve packs :7870"| skills["puku-skills-service"]
  wk -->|"download pack, verify digest<br/>presigned PUT/GET"| r2

  classDef store fill:#eef4fa,stroke:#5b8db8
  class pg,r2 store
```

Three things that are easy to get wrong from the diagram alone:

- **Nothing listens publicly.** controld binds `127.0.0.1:7770` and the memory
  service publishes no port at all; `agent.api.puku.sh` reaches them through the
  Cloudflare tunnel, and workerd dials *out*. A worker never needs an inbound
  port, and neither does the box.
- **`chat.api.puku.sh` is doing two jobs.** It verifies identity, and it is also
  the default model gateway, because `PUKU_AI_BASE_URL` falls back to
  `PUKU_API_URL` (`main.rs:503`). Set the two apart if your identity endpoint and
  your model gateway are not the same host.
- **The memory service spends the *caller's* credential**, resolved exactly as
  dispatch resolves it, which is why it must point at the same gateway the guest
  does. A credential is only valid where it was issued, and a mismatch surfaces
  as a 401 that reads like a revoked key.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the component diagrams —
controld's internals, the skills registry, and where telemetry goes.

**The agent is puku-cli itself, headless inside the microVM.** The platform
never reimplements the agent loop — it boots the VM, relays the event
stream, and gets the work back out. That single rule is what keeps a cloud
session and a local one the same agent.

## A session, end to end

```mermaid
sequenceDiagram
  participant U as User (puku cli)
  participant C as controld
  participant W as workerd
  participant V as microVM

  U->>C: POST /v1/sessions (bearer = the user's puku token)
  C->>C: clamp permission mode to the deployment ceiling
  C->>C: resolve credential → connectors → skill packs
  C->>W: AssignSession{spec}
  W->>V: boot, materialize skills, exec puku-runner
  V-->>W: stream-json on /session/events.ndjson
  W-->>C: SessionEvents (batched)
  C-->>U: attach WebSocket — replay from a cursor, then live

  Note over V: the agent asks something
  V-->>C: control_request (can_use_tool)
  C-->>U: state = waiting_input + pending_question
  U->>C: POST /answer
  C->>V: control_response — the frame puku-cli is blocked on
  Note over C,V: no timeout — it waits as long as the human does

  V-->>C: result · usage
  W->>W: push branch (host-side, token never in the VM)
  C->>C: open the pull request
```

The credential a session runs on is **the caller's own**, captured
encrypted at create and re-captured on resume — so cost lands on the user's
puku account, not the operator's.

## How a session flows

1. `POST /v1/sessions` → row in Postgres (`created`) → dispatcher assigns an
   online worker (`scheduled`) and sends the spec down the worker WebSocket.
2. workerd creates `/var/lib/puku/sessions/<id>/{session,workspace}` (on a
   shared-disk worker: inside the session's RBD image, mounted at
   `sessions/<id>/disk`), writes
   `manifest.json`, boots a microVM with both dirs bind-mounted
   (`booting → bootstrapping`), and execs `puku-runner` in the guest
   (`running`).
3. The runner clones the repo, then runs
   `puku-cli -p --output-format stream-json --input-format stream-json`
   with stdin from `/session/stdin.fifo` and stdout appended (line-capped) to
   `/session/events.ndjson` — the append-only outbox.
4. workerd tails the outbox on the host side and ships batches up; controld
   assigns each event a global `seq` under the session row lock (guest line
   numbers make redelivery idempotent), persists to the partitioned
   `session_events` table, and fans out to attached clients.
5. `puku-cloud attach <id>` replays from any cursor, then tails live; typed
   lines go back down as stream-json user messages; Ctrl-C interrupts.
6. Idle/stop parks the session: VM destroyed, volumes kept, `stopped`;
   resume cold-boots a fresh VM on the same volumes with `puku-cli --resume`.

## Milestone status

All four milestones are implemented and smoke-tested on Apple Silicon
(stub runner + alpine guest); final acceptance against real puku-cli runs on
the Linux/KVM box.

- **M1 — run + stream: done.** Boot ≈ 44 ms after image pull; events flow
  guest → DB → replay; cost captured from the `result` event.
- **M2 — interactive + resume: done.** `waiting_input` detection +
  `pending_question`, answer/input/interrupt over REST and the attach WS,
  idle auto-park, park/resume on persistent volumes (`--resume`), and
  workerd restart reconciliation: the runner is daemonized inside the VM
  (survives workerd death), sessions re-tail from the outbox with zero loss.
- **M3 — multi-tenant hardening: done.** API keys (`gen-key`, sha256 at
  rest, PUKU_AUTH=required), per-org concurrency + monthly budget quotas,
  usage records at terminal transition, audit log, `secret_env` key
  injection (placeholder in guest, real key only at the network boundary
  for the puku API hosts — `PUKU_SECRET_HOSTS`, default
  `api-cli.puku.sh`), `DeploymentProfile::MultiTenant` + egress
  domain allowlist flags, GitHub App installation tokens (falls back to
  static PAT), event archival to ndjson + reaping, secret redaction in the
  runner.
- **M6–M9 — one account, and a way in and out: done.**
  Sessions belong to a **puku account**: controld verifies platform bearers
  against `{PUKU_API_URL}/auth/verify` (never decoding them locally, never
  failing open) alongside the existing `pkc_` keys, provisions users on
  first sight, and scopes every session to its owner rather than the whole
  org. A session runs on **the caller's own credential** — captured
  encrypted at create, re-captured on resume so a parked session doesn't
  wake to an expired token — injected with the same env contract
  `puku-cowork`'s spawnerd uses. Connectors are brokered through the
  `mcp.proxy.puku.sh` the ecosystem already runs, so no vendor OAuth token
  ever enters a microVM. Blocked and finished sessions **reach the human**
  over signed webhooks or Slack. Work **leaves the VM**: workspace and
  transcript tarballs, and a pushed branch plus a pull request — pushed from
  the worker host, so the repo-writable token never touches the guest.
  Inbound webhook **triggers** start sessions from a templated prompt.
  Client contract: [`docs/API.md`](docs/API.md).

- **M5 — correctness: done.** The claims above that the code didn't keep
  are now kept. Tool policy is applied end to end (`allowed_tools`,
  `disallowed_tools`, `--permission-mode` clamped to a per-deployment
  ceiling — `--god-mode` is no longer hardcoded); the question protocol is
  the real one (`--permission-prompt-tool stdio` + `control_response`, see
  [`docs/PUKU-CLI-CONTRACT.md`](docs/PUKU-CLI-CONTRACT.md)); oversized event
  payloads are uploaded to R2 instead of dangling; sessions get titles;
  workers authenticate with per-worker tokens; the monthly budget is
  enforced mid-run, not only at create; `/health` and `/metrics` exist; and
  CI builds, lints and tests on every PR.

- **M4 — scale: done (first pass).** Least-loaded scheduler that skips
  draining workers, admin drain/undrain + fleet endpoints, cross-instance
  live fanout via Postgres LISTEN/NOTIFY (verified with two controld
  instances), image pre-pull on worker startup. Warm memory-snapshot pools
  remain future work (microsandbox snapshots are disk-only today).

- **Reliability rebuild: software done; hardware proof pending.** Host
  leases with a single-leader sweeper and mass-loss guard; a dead host's
  machines and sessions settled automatically; session and machine disks on
  Ceph RBD that move to another host after the old one is fenced, with
  mid-turn sessions and machines resumed there on their own; a VM watchdog
  for crashes on healthy hosts; storage cleanup that needs no host to be up;
  encrypted off-cluster disk backups with automatic rebuild. Tested against
  Postgres and a real (single-node) Ceph cluster. Memory snapshots,
  multi-host chaos runs and BMC fencing need the hardware above. Status:
  [`PLAN.md`](PLAN.md) section 4.

## Production notes

- Workers need Linux with `/dev/kvm` (bare metal or nested virt). Stage the
  msb toolchain with `deploy/scripts/prestage-msb.sh`; the systemd units set
  `MSB_HOME`/`MSB_PATH`/`MSB_LIBKRUNFW_PATH` so nothing downloads at runtime.
- `deploy/scripts/preflight.sh` gates workerd startup on `msb doctor`.
- Each worker host gets its own registration token:
  `puku-controld gen-worker-token --name box-1`, stored at
  `/etc/puku/worker-token` on that host. A token is bound to the first
  worker name that presents it, so a leaked one can't fan out across hosts
  and is revocable without rotating the fleet. The legacy shared secret is
  still accepted while `PUKU_ALLOW_SHARED_WORKER_TOKEN=1` (it logs a warning
  on every use) so a running fleet can migrate host by host.
- controld ships as a container (`Dockerfile`, published on tag by
  `.github/workflows/publish-image.yml`); `deploy/bm/` runs it behind a
  Cloudflare tunnel next to Postgres, the same shape as
  `puku-chat-compute-service`. workerd stays a systemd unit — it needs
  `/dev/kvm` and the msb toolchain on the host.
- Object storage (Cloudflare R2 or any S3 API) holds spilled event payloads
  and archived transcripts. **Credentials live only on controld**; workers
  request a short-lived presigned PUT over the control link, so no worker
  ever holds a bucket key.

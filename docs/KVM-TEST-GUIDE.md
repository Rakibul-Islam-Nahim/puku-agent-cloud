# Testing the two-engine platform on a KVM machine

A runbook for testing this branch on a Linux box with KVM. It covers libkrun
and Cloud Hypervisor side by side, the Machines API on both, and puku-bot's
`puku-cloud` computer backend on top. Each test says what to run and what
you should see. Fill in the checklist at the end and send it back with any
logs it asks for.

What was already tested before this guide, on macOS (Apple Silicon) where
there is no KVM:

- the full unit and integration suite (`cargo test --workspace` against Postgres);
- sessions and machines on **real libkrun microVMs**;
- puku-bot's live canary against a local agent-cloud.

What was **not** possible: booting a single Cloud Hypervisor VM. So the
Cloud Hypervisor sections (T3–T6) test that engine on real hardware for the
first time. Expect to find things there; that is the point of this pass.

- [0. What you need](#0-what-you-need)
- [1. Build and run the test suite](#1-build-and-run-the-test-suite)
- [2. Stage both engines](#2-stage-both-engines)
- [3. Run controld and workerd](#3-run-controld-and-workerd)
- [4. The tests](#4-the-tests)
- [5. Cleaning up](#5-cleaning-up)
- [6. Troubleshooting](#6-troubleshooting)
- [7. Results checklist](#7-results-checklist)

---

## 0. What you need

**A Linux host with KVM**, x86_64 or aarch64, either bare metal or a VM with
nested virtualization. Ubuntu 24.04 or Debian 12 is what the scripts assume.
You need root. Budget about 4 cores, 8 GiB RAM and 40 GiB of disk.

```bash
ls -l /dev/kvm                      # must exist and be rw for root
grep -cE 'vmx|svm' /proc/cpuinfo    # x86: > 0
sudo modprobe vhost_vsock tun
ls -l /dev/vhost-vsock /dev/net/tun
```

**Packages:**

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libcap-ng-dev git curl python3 jq \
  e2fsprogs nftables iproute2 virtiofsd \
  flex bison libelf-dev libssl-dev bc           # only if prestage-ch.sh builds the kernel
# Docker (for Postgres, and for turning images into disks)
curl -fsSL https://get.docker.com | sudo sh
# Rust
curl -fsSL https://sh.rustup.rs | sh -s -- -y && . "$HOME/.cargo/env"
```

**The code:**

```bash
git clone https://github.com/sagoresarker/puku-agent-cloud && cd puku-agent-cloud
git checkout feat/engines-cloud-hypervisor
```

For T8 (puku-bot) you also need Node 22 and pnpm, and puku-bot-svc on the
`feat/puku-cloud-sandbox` branch.

---

## 1. Build and run the test suite

```bash
docker compose -f deploy/compose.dev.yml up -d postgres
cargo build --release --workspace
PUKU_TEST_DATABASE_URL=postgres://puku:puku@127.0.0.1:5432/puku_cloud cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

**Expect:** every `test result: ok`. controld reports about 170 passed,
workerd 104, puku-guestd 12 (its Linux-only tests run here, unlike on
macOS), and the protocol crate 24. clippy prints nothing. The workerd
`gitpush` tests need `git` on the PATH and a git identity configured.

---

## 2. Stage both engines

Run everything in this section as root. Each script says what it did.

### 2.1 libkrun (unchanged from today)

```bash
sudo ./deploy/scripts/prestage-msb.sh            # msb + libkrunfw -> /opt/puku/msb
sudo MSB_HOME=/opt/puku/msb /opt/puku/msb/bin/msb pull alpine   # the smoke tests' image
```

> **Always pass `MSB_HOME=/opt/puku/msb` to `msb`.** workerd uses that
> store. Without the variable, `msb` uses `~/.microsandbox`, which for root is
> `/root/.microsandbox`, and an image loaded there is invisible to workerd.
> Its session or machine then fails trying to pull the tag from Docker Hub
> (`401 Not authorized`).

### 2.2 Cloud Hypervisor

First check the pins at the top of `deploy/scripts/prestage-ch.sh`
against the upstream release pages, and bump them if they are stale:

- `CH_VERSION`: <https://github.com/cloud-hypervisor/cloud-hypervisor/releases>
- `KERNEL_REF`: a `ch-*` branch of <https://github.com/cloud-hypervisor/linux>

A wrong pin fails loudly in the script.

```bash
sudo ./deploy/scripts/prestage-ch.sh
# or with a prebuilt kernel instead of building one (~10 min saved):
# sudo KERNEL_URL=https://…/vmlinux ./deploy/scripts/prestage-ch.sh
ls -l /opt/puku/ch /opt/puku/ch/bin
```

**Expect:** `/opt/puku/ch/bin/{cloud-hypervisor,virtiofsd,puku-guestd}` and
`/opt/puku/ch/vmlinux`. On aarch64 that file is an arm64 `Image`; the name
stays `vmlinux`. `cloud-hypervisor --version` prints the pinned version.

Turn images into boot disks, one per image you will run on Cloud Hypervisor:

```bash
sudo ./deploy/scripts/build-ch-rootfs.sh alpine
ls -l /var/lib/puku/images/alpine/
```

**Expect:** `rootfs.ext4` and `image-config.json`.

---

## 3. Run controld and workerd

Running both in the foreground, in two terminals, is the quickest loop.
workerd must run as **root**, because it creates TAP devices, nftables rules
and systemd units.

**Terminal 1 — controld:**

```bash
export PUKU_DATABASE_URL=postgres://puku:puku@127.0.0.1:5432/puku_cloud
export PUKU_AUTH=off PUKU_PLATFORM_AUTH=false
export PUKU_ALLOW_OPERATOR_CREDENTIALS=true PUKU_AI_API_KEY=test-dummy
export PUKU_SECRET_KEY=$(openssl rand -hex 32)
export PUKU_ENGINES_ALLOWED=libkrun,cloud_hypervisor
export PUKU_AGENT_IMAGE=alpine PUKU_MACHINE_IMAGE=alpine
./target/release/puku-controld
```

**Terminal 2 — workerd (root):**

```bash
sudo -E env \
  PUKU_CONTROLD_URL=ws://127.0.0.1:7770/v1/worker \
  PUKU_WORKER_NAME=kvm-1 PUKU_WORKER_TOKEN=dev-worker-token \
  PUKU_STATE_DIR=/var/lib/puku PUKU_EGRESS_UNRESTRICTED=true \
  PUKU_ENGINE_LIBKRUN=true PUKU_ENGINE_CLOUD_HYPERVISOR=true \
  MSB_HOME=/opt/puku/msb MSB_LIBKRUNFW_PATH=/opt/puku/msb/lib/libkrunfw.so \
  PUKU_RUNNER_CMD='echo "{\"type\":\"result\",\"subtype\":\"success\",\"total_cost_usd\":0}" >> /session/events.ndjson' \
  RUST_LOG=info ./target/release/puku-workerd
```

`PUKU_RUNNER_CMD` swaps puku-cli for a stub that finishes the turn at once,
so session tests need no model credential. T9 uses the real agent.

**Expect in the workerd log:**

```
engines enabled engines=libkrun=msb-0.6.9,cloud_hypervisor=cloud-hypervisor v…
registered with controld
```

If `cloud_hypervisor=` is missing, the line above it reads
`PUKU_ENGINE_CLOUD_HYPERVISOR is set but this host cannot run it`, followed
by the reason. Fix that and restart before going on (see §6).

```bash
curl -s localhost:7770/v1/fleet | jq '.workers[] | {name, engines}'
# {"name":"kvm-1","engines":["libkrun","cloud_hypervisor"]}
```

---

## 4. The tests

Commands assume `export PUKU_CLOUD_URL=http://127.0.0.1:7770` and the CLI at
`./target/release/puku-cloud`.

### T1 — Nothing changed for libkrun (backward compatibility)

```bash
./target/release/puku-cloud run "smoke" --detach           # no --engine
id=<the printed id>; sleep 5
curl -s localhost:7770/v1/sessions/$id | jq '{state, engine, volume_worker_id}'
```

**Expect:** `completed`, `"libkrun"`, and a non-null `volume_worker_id`.
`msb ls` shows no leftover `ses-*` VM.

### T2 — Engine routing

```bash
./target/release/puku-cloud run "x" --engine firecracker --detach
#   -> Error: 400 … it offers: libkrun, cloud_hypervisor
./target/release/puku-cloud run "on ch" --engine cloud_hypervisor --detach
```

**Expect:** the second session reaches `completed` with `engine:
cloud_hypervisor`, and the workerd log shows it ran on that engine (`sudo
systemctl list-units 'puku-vm-*'` while it runs). Then restart workerd with
`PUKU_ENGINE_CLOUD_HYPERVISOR=false` and create another
`--engine cloud_hypervisor` session:

- It stays `created`.
- `GET /v1/sessions/<id>/events` has one `session.waiting_for_worker` event.
- A libkrun session created after it still runs; the queue is not blocked.

Switch Cloud Hypervisor back on, and the waiting session starts.

### T3 — Cloud Hypervisor sessions, from the inside

Start a session that stays alive long enough to look at, by giving the
worker a runner that sleeps:

```bash
# restart workerd with:
#   PUKU_RUNNER_CMD='sleep 600; echo "{\"type\":\"result\",\"subtype\":\"success\"}" >> /session/events.ndjson'
./target/release/puku-cloud run "look inside" --engine cloud_hypervisor --detach
```

While it runs:

```bash
sudo systemctl status 'puku-vm-ses-*'               # one unit, active, in puku-vms.slice
ls /var/lib/puku/vms/ses-*/                          # launch.sh vm.json upper.ext4 console.log *.sock
sudo tail -30 /var/lib/puku/vms/ses-*/console.log   # kernel boot, then "puku-guestd: listening on vsock port 1024"
ip -br addr | grep pkt                               # pkt<N>  10.200.x.y/30
sudo nft list table inet puku | head -40
cat /var/lib/puku/vms/ses-*/launch.sh                # the exact VMM command line
```

**Expect:** the guest boots straight into `puku-guestd`, and `/session` and
`/workspace` are mounted in it. The session's `events.ndjson` appears under
`/var/lib/puku/sessions/<id>/session/`. `puku-cloud stop <id>` parks it: the
unit, the TAP and the VM directory go, and the session directory stays.
`puku-cloud resume <id>` boots it again **on the same worker**.

### T4 — Machines API, both engines (the smoke script)

```bash
ENGINE=libkrun          ./deploy/scripts/machines-smoke.sh
ENGINE=cloud_hypervisor ./deploy/scripts/machines-smoke.sh
```

Each run checks the following and prints PASS or FAIL per check:

- create, including `resumed: false` on the first boot;
- exec: default uid 1000, env and `secret_env`, stdin, running as root, and a timeout returning 124 without waiting on a backgrounded child;
- files: PUT, GET, 413, 404, 400 on traversal, and a recursive listing;
- archive GET, and archive PUT that merges rather than replaces;
- the guest-port proxy, 403 on an unexposed port, a capability link, and a forged link returning 404;
- on Cloud Hypervisor only: that metadata and host SSH are unreachable, and that a close-delimited response completes promptly;
- stop, then start with `resumed: true` and the file still there, and `external_id` idempotency;
- with `SNAPSHOTS=1` (see T11): a snapshot of the running machine, and a restore from it that brings back what was there and drops what came after;
- destroy.

**Expect:** `failed: 0` on both. On libkrun,
`close-delimited response took ~8s` is an INFO line, a known msb
port-forwarding quirk, not a failure. Keep a machine around to inspect with
`KEEP=1`.

After the Cloud Hypervisor run finishes (it destroys its machine):

```bash
sudo systemctl list-units 'puku-vm-*'     # none left
ip -br link | grep pkt                    # none left
ls /var/lib/puku/vms /var/lib/puku/machines   # empty
```

### T5 — Egress allowlist on Cloud Hypervisor

Restart workerd with the allowlist instead of open egress. Keep Alpine's
package mirror on it so the smoke script's server still installs:

```bash
#   PUKU_EGRESS_UNRESTRICTED unset
#   PUKU_EGRESS_ALLOW=github.com,alpinelinux.org
KEEP=1 ENGINE=cloud_hypervisor ./deploy/scripts/machines-smoke.sh
M=<machine id it prints>
curl -s -X POST localhost:7770/v1/machines/$M/exec -H 'content-type: application/json' -d '{"user":"root","timeout_ms":30000,"argv":["sh","-c",
 "nslookup example.com; echo rc=$?; wget -q -T 8 -O /dev/null http://github.com && echo github=ok; wget -q -T 8 -O /dev/null http://1.1.1.1 && echo raw-ip=LEAK || echo raw-ip=blocked"]}' | jq -r .stdout
sudo nft list set inet puku allow_<N>     # N from `ip -br addr | grep pkt`
curl -s -X DELETE localhost:7770/v1/machines/$M
```

**Expect:**

- `example.com` fails to resolve (NXDOMAIN).
- `github=ok`.
- `raw-ip=blocked`: an address nobody resolved through the allowlist goes nowhere.
- The nft set lists GitHub's addresses, each with a timeout.

### T6 — Restarting workerd does not kill VMs

Needs the systemd unit, because that is where `KillMode` matters:

```bash
sudo cp target/release/puku-workerd /opt/puku/bin/
sudo cp deploy/scripts/preflight.sh /opt/puku/bin/
sudo cp deploy/systemd/puku-workerd.service deploy/systemd/puku-vms.slice /etc/systemd/system/
sudo systemctl edit puku-workerd   # add the Environment= lines from §3 (engines, runner stub, egress)
sudo systemctl daemon-reload && sudo systemctl start puku-workerd
```

Create one libkrun machine and one Cloud Hypervisor machine (`KEEP=1` smoke
runs), plus a sleeping session (T3's runner) on each engine. Then:

```bash
sudo systemctl restart puku-workerd
sudo journalctl -u puku-workerd -n 50 | grep -E 'reattached|reconciling|did not survive'
```

**Expect:**

- Both machines log `reattached to a running machine`, and both sessions log `reconciling session from disk`.
- `msb ls` and `systemctl list-units 'puku-vm-*'` show the same VMs as before the restart.
- An exec against each machine still works.
- `/v1/fleet` has empty `vanished` and `orphaned`.

### T7 — A resume goes back to its volumes (needs a second worker)

Optional, if you have a second KVM host or a second workerd with its own
`PUKU_STATE_DIR` and `PUKU_WORKER_NAME`. Run a session to completion on
worker A, make A busier than B (or just leave it), then send
`puku-cloud input <id> "again"`.

**Expect:** the resume dispatches to **A** every time, never B. Stop A for
more than 15 minutes, send input again, and the session fails with an
error naming A and "volumes".

### T8 — puku-bot's computer on both engines

Build puku-bot's computer image and stage it for both engines:

```bash
cd ../puku-bot-svc && git checkout feat/puku-cloud-sandbox
docker build -t pukubot-computer:local infra/sandboxes/computer
sudo sh -c 'export MSB_HOME=/opt/puku/msb; M=/opt/puku/msb/bin/msb; $M image rm pukubot-computer:local 2>/dev/null; docker save pukubot-computer:local | $M load -t pukubot-computer:local'
sudo ../puku-agent-cloud/deploy/scripts/build-ch-rootfs.sh pukubot-computer:local
pnpm install
```

Run the live canary once per engine. It provisions a computer, runs a
command, writes a file, takes a screenshot through `control.py`, stops,
re-provisions with `fresh: false` and the file still there, and destroys:

```bash
for e in libkrun cloud_hypervisor; do
  VERIFY_PROVIDERS=1 PUKU_AGENT_CLOUD_URL=http://127.0.0.1:7770 PUKU_AGENT_CLOUD_API_KEY=test \
  PUKU_AGENT_CLOUD_IMAGE=pukubot-computer:local PUKU_AGENT_CLOUD_ENGINE=$e \
  pnpm exec vitest run packages/testkit/src/providers.canary.test.ts -t puku-cloud
done
```

**Expect:** `1 passed` for each engine.

**See the desktop.** Keep one computer running (`KEEP=1`-style: create it
with the body the adapter sends, or pause the canary before its destroy).
Then mint a link to its noVNC view and open it through an SSH tunnel:

```bash
curl -s -X POST localhost:7770/v1/machines/<id>/links -H 'content-type: application/json' \
  -d '{"port":6080,"path":"/embed.html","query":"view_only=1"}' | jq -r .url
# on your laptop:  ssh -L 7770:127.0.0.1:7770 <box>   then open the URL
```

**Expect:** a live desktop in the browser. Report whether it looks and
behaves the same on both engines.

### T9 — The real agent on Cloud Hypervisor (optional, costs model usage)

With a real guest image (`images/puku-agent`, loaded into msb and staged
with `build-ch-rootfs.sh`), a real model credential, and `PUKU_RUNNER_CMD`
unset, run the standard sequence from `skills/deployment-test` (DEPLOYMENT.md
§10) once with `--engine libkrun` and once with `--engine cloud_hypervisor`:
smoke, skills, a question and answer, interrupt, park and resume, and
artifacts.

**Expect:** the same behaviour on both engines.

### T10 — Fleet drift

With a Cloud Hypervisor machine running, kill its VM out from under
workerd (`sudo systemctl kill -s KILL puku-vm-mch-…`), then wait for a
heartbeat (10 s):

```bash
curl -s localhost:7770/v1/fleet | jq .drift
```

**Expect:** the machine in `vanished`. Start a stray VM by hand
(`sudo systemd-run --unit=puku-vm-mch-000000000000 sleep 600` plus a
`/var/lib/puku/vms/mch-000000000000/vm.json`), and it should show up in
`orphaned`. That second check is optional.

### T11 — Fail-fast placement and snapshots (MinIO)

**Fail fast.** Stop workerd (`sudo systemctl stop puku-workerd`) and ask for a
machine with a long wait:

```bash
time curl -s -XPOST localhost:7770/v1/machines -H 'content-type: application/json' \
  -d '{"wait_s":120}' | jq .error
```

**Expect:** an answer in well under a second, `"reason": "no_workers"`, and a
`Retry-After` header (add `-i` to see it). Start workerd again, then ask for
a machine bigger than the box (`"memory_mib": 999999` is clamped, so raise
`PUKU_MACHINE_MAX_MEMORY_MIB` first, or ask for more `cpus` than the host has
cores). **Expect:** `422 too_large`. Ask for a `cloud_hypervisor` machine
with an image you have not staged. **Expect:** `422 image_not_staged` naming
the `build-ch-rootfs.sh` command.

**Snapshots.** Bring up MinIO and its bucket:

```bash
docker compose -f deploy/compose.dev.yml up -d minio minio-init
```

Restart controld with object storage (keep the rest of its environment):

```bash
PUKU_R2_ENDPOINT=http://127.0.0.1:9000 PUKU_R2_BUCKET=puku-cloud \
PUKU_R2_REGION=us-east-1 PUKU_R2_ACCESS_KEY_ID=puku PUKU_R2_SECRET_ACCESS_KEY=puku-dev-secret \
PUKU_SNAPSHOTS=true ./target/release/puku-controld
```

It logs `machine snapshots on`, and `/v1/fleet` shows the worker's features
including `snapshots`. Then run both engines:

```bash
SNAPSHOTS=1 ENGINE=libkrun          ./deploy/scripts/machines-smoke.sh
SNAPSHOTS=1 ENGINE=cloud_hypervisor ./deploy/scripts/machines-smoke.sh
```

**Expect:** `failed: 0` on both; the snapshot INFO line shows the bytes
before and after compression. In MinIO's console (`:9001`, puku /
puku-dev-secret) the objects sit under `puku-cloud/machines/<id>/snapshots/`
while the machine exists, and are gone within a minute of the purge at the
end (the sweep runs every `PUKU_SNAPSHOT_SWEEP_S`).

**Relocation** (needs a second worker, as in T7). Create a Cloud Hypervisor
machine with `"volume": {"path": "/data"}, "snapshots": {"on_stop": true}`,
write a file into it, stop it (the stop snapshot becomes `ready`), then stop
the workerd that held it. `POST /v1/machines/<id>/start` answers
`503 volume_host_offline` with `detail.snapshot_available: true`;
`POST /v1/machines/<id>/start {"relocate": true, "wait_s": 300}` brings it up on
the other worker with `resumed: true`, the file present and `restored_from`
set. When the first worker comes back, its copy under
`/var/lib/puku/machines/<id>` is deleted.

**Optional, memory.** With `PUKU_CH_FREE_PAGE_REPORTING=true` on workerd, a
16 GiB machine that allocated and freed memory (for example
`python3 -c "b=bytearray(8<<30)"` in it) should hand it back: compare the
VM's `MemoryCurrent` (`systemctl show puku-vm-<name> -p MemoryCurrent`)
before and a minute after. If it stays high, leave the flag off and say so.

---

## 5. Cleaning up

```bash
for m in $(curl -s 'localhost:7770/v1/machines?limit=500' | jq -r '.[] | select(.state!="destroyed") | .id'); do
  curl -s -X DELETE localhost:7770/v1/machines/$m; done
sudo systemctl stop 'puku-vm-*' 2>/dev/null
sudo nft delete table inet puku 2>/dev/null
for t in $(ip -br link | awk '/^pkt/{print $1}'); do sudo ip link del "$t"; done
sudo rm -rf /var/lib/puku/vms /var/lib/puku/machines
docker compose -f deploy/compose.dev.yml down
```

---

## 6. Troubleshooting

| Symptom | Where to look / what it means |
|---|---|
| `cloud_hypervisor` not in `engines enabled` | The workerd log line right before it names the missing piece (`/dev/kvm`, `/dev/vhost-vsock`, a binary, `nft`/`ip`/`mkfs.ext4`). `sudo -E ./deploy/scripts/preflight.sh` prints the same checks. |
| `the guest agent did not come up within 60s` | The error ends with the last console lines. `/var/lib/puku/vms/<name>/console.log` is the guest console; `launch.log` has virtiofsd and the VMM's own errors. A kernel missing `VIRTIO_FS`, `VSOCKETS` or `OVERLAY_FS` stops here. |
| `image … is not staged for cloud_hypervisor` | Run `build-ch-rootfs.sh <exact image ref>`. The ref must match controld's `PUKU_AGENT_IMAGE` or the machine's `image` byte for byte. |
| Console shows `overlay root unavailable` | The guest runs on the read-only image. Check `puku.overlay=/dev/vdb` in `launch.sh`, and that the kernel has `OVERLAY_FS` and `EXT4_FS`. |
| Guest has no network | `ip -br addr | grep pkt` on the host; `sudo nft list table inet puku`. On a box running Docker, `iptables -nL DOCKER-USER` must show the two `pkt+` rules workerd adds. |
| `nft: …` errors at startup | An older nftables without `vmap` support, or a conflicting table. `sudo nft -f -` with the output of `net::base_ruleset` (in `vm/ch/net.rs`) reproduces it. |
| `systemd-run could not start` | `journalctl -u puku-vm-<name>`. Workerd needs root and a systemd host; without systemd it falls back to `setsid`. |
| A libkrun session dies on workerd restart | The unit is missing `KillMode=process`. |
| Proxy request hangs on libkrun for close-delimited responses | Known msb port-forwarding behaviour (see CLOUD-HYPERVISOR-PLAN.md, known limitations). |

Useful switches: `RUST_LOG=info,puku_workerd=debug` on workerd, and
`cat /var/lib/puku/vms/<name>/launch.sh` to rerun a VM by hand.

---

## 7. Results checklist

Copy this into your reply, mark each line, and attach the logs for any ✗.

| # | Test | libkrun | Cloud Hypervisor | Notes |
|---|---|---|---|---|
| 1 | `cargo test --workspace` + clippy | — | — | |
| 2 | prestage-ch.sh (pins used: …) | — | | |
| 3 | Worker advertises both engines | | | |
| T1 | Stub session, no `--engine` | | — | |
| T2 | Routing, 400, waiting_for_worker, queue not blocked | | | |
| T3 | CH session: unit, console, mounts, stop/resume | — | | |
| T4 | machines-smoke.sh `failed: 0` | | | |
| T5 | Allowlist: NXDOMAIN, allowed host ok, raw IP blocked | — | | |
| T6 | `systemctl restart puku-workerd`: all VMs reattach | | | |
| T7 | Resume pinned to its worker (optional) | | | |
| T8 | puku-bot canary; noVNC view in a browser | | | |
| T9 | Real agent sequence (optional) | | | |
| T10 | Fleet drift | | | |
| T11 | Fail fast (no_workers, too_large, image_not_staged); `SNAPSHOTS=1` smoke; relocation (optional); free page reporting (optional) | | | |

For any failure, send:

- `journalctl` (or the terminal output) for workerd and controld around the failure;
- for Cloud Hypervisor, `/var/lib/puku/vms/<name>/{console.log,launch.log,launch.sh}`;
- the output of `uname -a` and `cloud-hypervisor --version`.

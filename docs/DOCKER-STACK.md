# Running puku-agent-cloud with Docker Compose

Every puku-agent-cloud service in containers, started with one command:

```bash
docker compose up -d
```

from the repository root. This guide covers the first install, the endpoints,
the keys, and day-to-day operation.

- [What runs where](#what-runs-where)
- [Endpoints](#endpoints)
- [First install on the main server](#first-install-on-the-main-server)
- [A second server](#a-second-server)
- [Keys](#keys)
- [Guest images](#guest-images)
- [Shared disks on Ceph](#shared-disks-on-ceph)
- [Cloud Hypervisor workers](#cloud-hypervisor-workers)
- [Operating it](#operating-it)
- [Troubleshooting](#troubleshooting)

## What runs where

`COMPOSE_PROFILES` in `.env` picks the services for each host, so the
command never changes.

| Profile | Services | Where |
| --- | --- | --- |
| `control` | controld, Postgres, MinIO (and `minio-init`, which makes the bucket and controld's key, then exits) | the main server |
| `tunnel` | cloudflared, serving `agent.api.puku.sh` | the main server |
| `worker` | workerd with libkrun VMs | every server that runs VMs, the main one included |

Main server: `COMPOSE_PROFILES=control,tunnel,worker`. Any other server:
`COMPOSE_PROFILES=worker`.

Not in the compose file:

- **Ceph.** cephadm runs Ceph's daemons in containers of its own. See
  [Shared disks on Ceph](#shared-disks-on-ceph).
- **Cloud Hypervisor workers.** Each of their VMs is a transient systemd unit
  on the host, so that worker runs on the host. See
  [Cloud Hypervisor workers](#cloud-hypervisor-workers).
- **memory, skills and PukuBot.** Separate services with their own
  deployments; this stack calls them, or is called by them, at their
  hostnames.

## Endpoints

| Service | Hostname | Relation to this stack |
| --- | --- | --- |
| puku-agent-cloud (controld) | `https://agent.api.puku.sh` | served by this stack, through the tunnel |
| puku-memory-service | `https://memory.api.puku.sh` | controld calls it (`PUKU_MEMORY_URL`, `PUKU_MEMORY_SERVICE_KEY`) |
| puku-skills-service | `https://skill.api.puku.sh` | controld calls it (`PUKU_SKILLS_URL`, `PUKU_SKILLS_TOKEN`) |
| PukuBot | `https://app.bot.puku.sh` | calls controld (`PUKU_AGENT_CLOUD_URL`, `PUKU_AGENT_CLOUD_API_KEY` in puku-bot's settings) |

**The tunnel.** In Cloudflare Zero Trust, a tunnel with the public hostname
`agent.api.puku.sh` → service `http://controld:7770`. Its token goes in
`CLOUDFLARE_TUNNEL_TOKEN`. cloudflared runs on the stack's network, so
`controld` is the service name, not `localhost`.

> **Careful with the tunnel that serves `agent.api.puku.sh` today.** Running
> its token here adds this host as a second connector, and Cloudflare then
> splits live traffic between the old stack and this one. Use a new tunnel
> and move the hostname to it, or stop the old stack first.

**Machine links** (noVNC and other guest pages, `PUKU_LINKS_URL`): when
puku-bot runs on the same host, it joins the `puku-link` network and opens
them at `http://controld:7770`, the default. Otherwise give links a hostname
of their own on the same tunnel (for example `links.agent.api.puku.sh` →
`http://controld:7770`). Never use `agent.api.puku.sh` itself: guest pages
must not share the dashboard's origin.

## First install on the main server

Needs: Linux with Docker Engine and the compose plugin, `/dev/kvm` (for the
worker), and this repository.

```bash
git clone https://github.com/Rakibul-Islam-Nahim/puku-agent-cloud
cd puku-agent-cloud
./deploy/scripts/stack-init.sh
```

`stack-init.sh` copies `.env.example` to `.env`, fills every secret with a
fresh random value, fills this host's LAN address, and lists what is left:

| Setting | Value |
| --- | --- |
| `CLOUDFLARE_TUNNEL_TOKEN` | the tunnel's token |
| `PUKU_MEMORY_SERVICE_KEY` | the memory service's API key (its `PUKU_MEMORY_SERVICE_KEY`) |
| `PUKU_SKILLS_TOKEN` | the skills service's `PUKU_SKILLS_OPERATOR_TOKEN` |

Fill them, then:

```bash
docker compose up -d                 # builds the images the first time
docker compose exec controld puku-controld gen-worker-token --name bm1
```

Put the printed `pkw_…` token in `.env` as `PUKU_WORKER_TOKEN` (and
`PUKU_WORKER_NAME=bm1`), then once more:

```bash
docker compose up -d
curl -fsS "http://127.0.0.1:7770/health?deep=1"
```

Want: `"status":"ok"`, `"database":"ok"`, `"object_storage_probe":"ok"`,
`"workers_connected":1`, and `https://agent.api.puku.sh/health` answering the
same through the tunnel.

From then on, `docker compose up -d` is the whole procedure, after every
reboot or update.

**Keep `PUKU_SECRET_KEY` safe.** It encrypts stored credentials, snapshot
keys and disk-backup keys; losing it makes all of them unreadable. Keep an
offline copy of `.env`.

## A second server

On the main server, set `CONTROLD_BIND` to its LAN address (workers on other
servers dial in there), set `PUKU_CONTROLD_URL=ws://<that address>:7770/v1/worker`
for its own worker too, and run `docker compose up -d`. Mint a token:

```bash
docker compose exec controld puku-controld gen-worker-token --name bm2
```

On the second server: clone the repository, `cp .env.example .env`, and set

```ini
COMPOSE_PROFILES=worker
PUKU_CONTROLD_URL=ws://<main server's LAN IP>:7770/v1/worker
PUKU_WORKER_NAME=bm2
PUKU_WORKER_TOKEN=pkw_...
```

plus the Ceph settings if you use them. Then `docker compose up -d`.

## Keys

| Key | Where it comes from | Goes in |
| --- | --- | --- |
| Memory service key | the memory service's deployment | `PUKU_MEMORY_SERVICE_KEY` |
| Skills operator token | the skills service's deployment | `PUKU_SKILLS_TOKEN` |
| Tunnel token | Cloudflare Zero Trust | `CLOUDFLARE_TUNNEL_TOKEN` |
| Worker token, one per server | `docker compose exec controld puku-controld gen-worker-token --name <server>` | that server's `PUKU_WORKER_TOKEN` |
| puku-bot's API key | `docker compose exec controld puku-controld gen-key --org pukubot --name puku-bot` | puku-bot's `PUKU_AGENT_CLOUD_API_KEY` |
| Your admin key | `docker compose exec controld puku-controld gen-key --name ops --admin` | your shell, for the dashboard and `/v1/fleet` |
| `PUKU_SECRET_KEY`, Postgres and MinIO passwords | `stack-init.sh` | `.env` (already) |

puku-bot keeps one machine per user, and a new org may keep 20. Lift it for
the `pukubot` org once its key exists:

```bash
docker compose exec -T postgres psql -U puku puku_cloud -c \
  "UPDATE quotas SET max_concurrent_machines = 100000 WHERE org_id IN (SELECT id FROM orgs WHERE name = 'pukubot')"
```

## Guest images

Each worker keeps its own image store (msb), inside its state directory
(`/var/lib/puku/msb`). It cannot see Docker's images. Load a locally built
image into every worker:

```bash
docker build -t puku-agent:0.1.0 images/puku-agent
docker save puku-agent:0.1.0 | docker compose exec -T workerd msb load -t puku-agent:0.1.0
docker compose exec workerd msb image list
```

Set the same tag as `PUKU_AGENT_IMAGE` in `.env`. An image on a public
registry (`alpine`, `docker.io/...`) needs no loading: msb pulls it.

## Shared disks on Ceph

For production: sessions and machines keep their disks on Ceph, so work
moves to another server when one dies. Set up the cluster, pool and
`client.puku` user as in the README (section 3, "Shared disks on Ceph"), then
on every server:

- `/etc/ceph/ceph.conf` and `/etc/ceph/ceph.client.puku.keyring` as real
  files (the directory is mounted into the containers, so symlinks to paths
  outside it do not resolve there). `CEPH_CONF_DIR` points elsewhere if needed.
- On the main server, let controld (uid/gid 10001) read the keyring:
  `chgrp 10001 /etc/ceph/ceph.client.puku.keyring && chmod 640 /etc/ceph/ceph.client.puku.keyring`.
- `PUKU_RBD_POOL=puku-sessions` in `.env`, then `docker compose up -d`.

`/v1/workers` then lists `shared_volumes` among each worker's features.

## Cloud Hypervisor workers

The containerized worker runs libkrun only. Cloud Hypervisor (what puku-bot's
desktop computers use) starts each VM as a transient systemd unit on the
host, which a container cannot do safely, so a Cloud Hypervisor worker runs
on the host with `deploy/scripts/setup-worker.sh` (or the unit in
`deploy/systemd/`) and joins this same controld at `PUKU_CONTROLD_URL`. Then
add `cloud_hypervisor` to `PUKU_ENGINES_ALLOWED` in `.env`.

## Operating it

| Task | Command |
| --- | --- |
| Status | `docker compose ps` |
| Logs | `docker compose logs -f controld` (or `workerd`, `cloudflared`) |
| Update | `git pull && docker compose up -d --build` |
| Back up the database | `docker compose exec -T postgres pg_dump -U puku -Fc puku_cloud > puku_cloud-$(date +%F).dump` |
| Stop everything | `docker compose down` (volumes and data stay) |

**What a workerd restart does to running VMs.** The worker's VMs live in the
`workerd` container, so recreating or restarting that container (an update
that changes its image or settings, `docker compose restart workerd`, a
reboot) stops them. When the worker reconnects:

- a **machine** is marked `stopped` with its disk intact; the next start
  (puku-bot's create-or-ensure-running does one) boots it again;
- a **session** that was mid-task restarts on its own disk with a message
  telling the agent its VM was restarted; one waiting for an answer stays
  stopped until the answer comes. This counts towards the crash-loop guard
  (three in 30 minutes stops the automatic restarts).

The systemd worker keeps its VMs running across a restart; this is the price
of the container. Restarting controld, Postgres, MinIO or cloudflared does not
touch VMs, so to update only the control plane:
`docker compose up -d --build --no-deps controld`.

**Data.** Postgres, MinIO and controld's archive live in named volumes
(`docker volume ls`); the worker's state is the host directory
`/var/lib/puku`. `docker compose down -v` deletes the volumes: never on a
real deployment.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| `controld` waits forever | `minio-init` failed: `docker compose logs minio-init` (an empty MinIO password or key) |
| `object_storage_probe` not `"ok"` | `PUKU_R2_ENDPOINT` must be this host's LAN address, reachable from containers and workers, not `127.0.0.1` |
| `workers_connected: 0` | `PUKU_WORKER_TOKEN` missing or minted for another name, or `PUKU_CONTROLD_URL` cannot reach `CONTROLD_BIND` |
| workerd restarts over and over | `docker compose logs workerd`: preflight names what is missing (`/dev/kvm`, msb) |
| `agent.api.puku.sh` 502 | the tunnel's public hostname must point at `http://controld:7770` |
| Skills never resolve | `skill.api.puku.sh` must resolve and `PUKU_SKILLS_TOKEN` must match; controld logs the address it could not reach at startup |
| Memory off | `PUKU_MEMORY_SERVICE_KEY` empty, or behind Cloudflare Access without both `PUKU_MEMORY_ACCESS_*` values |

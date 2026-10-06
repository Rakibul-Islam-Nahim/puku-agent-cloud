# Deploying puku-agent-cloud on 3 machines, step by step

This guide takes you from three empty servers to a running puku-agent-cloud
that keeps working when one server dies. Every command says **which machine**
to run it on. Do the steps in order and do every **Check** before moving on:
if a check fails, stop and fix it (see [Troubleshooting](#troubleshooting))
instead of continuing.

Time: about half a day the first time, most of it waiting for builds.

> **Status.** Every part of this was tested on one machine (WSL with KVM and a
> single-node Ceph): the stack, a worker in a container, machines on Ceph
> disks. It has not yet been run end to end on three real servers. If a step
> does not behave as written, note what you saw and ask before improvising.

- [The plan](#the-plan)
- [What you need](#what-you-need)
- [Step 0: fill in your values](#step-0-fill-in-your-values)
- [Step 1: prepare all three machines](#step-1-prepare-all-three-machines)
- [Step 2: Ceph, the shared disks](#step-2-ceph-the-shared-disks)
- [Step 3: the code, on all three](#step-3-the-code-on-all-three)
- [Step 4: the Cloudflare tunnel](#step-4-the-cloudflare-tunnel)
- [Step 5: machine 1, the main server](#step-5-machine-1-the-main-server)
- [Step 6: machines 2 and 3, the workers](#step-6-machines-2-and-3-the-workers)
- [Step 7: guest images](#step-7-guest-images)
- [Step 8: connect PukuBot](#step-8-connect-pukubot)
- [Step 9: test everything](#step-9-test-everything)
- [Step 10: backups](#step-10-backups)
- [Daily operation](#daily-operation)
- [Troubleshooting](#troubleshooting)
- [Words used in this guide](#words-used-in-this-guide)

---

## The plan

```
                    Internet users, PukuBot (app.bot.puku.sh)
                                   |
                       https://agent.api.puku.sh
                                   |
                           Cloudflare tunnel
                                   |
 +---------------------------------+---------------------------------------+
 | MACHINE 1  (bm1, main)          | MACHINE 2 (bm2)    | MACHINE 3 (bm3)  |
 |   cloudflared  (tunnel)         |                    |                  |
 |   controld     (the brain)      |                    |                  |
 |   Postgres     (database)       |                    |                  |
 |   MinIO        (backups, files) |                    |                  |
 |   workerd      (runs VMs)       |   workerd          |   workerd        |
 |   Ceph: monitor + data disk     |   Ceph: mon + disk |   Ceph: mon+disk |
 +---------------------------------+--------------------+------------------+
          all three on one private network (LAN), e.g. 10.0.0.0/24
```

| | Machine 1 (bm1) | Machine 2 (bm2) | Machine 3 (bm3) |
| --- | --- | --- | --- |
| controld, Postgres, MinIO | yes | no | no |
| Cloudflare tunnel | yes | no | no |
| workerd (runs the VMs) | yes | yes | yes |
| Ceph monitor and data disk | yes | yes | yes |

Every machine runs the **same code** and the **same** `docker-compose.yml`.
Only the `.env` file is different on each.

**What survives what:**

| If this dies | What happens |
| --- | --- |
| bm2 or bm3 | After about 20 seconds controld cuts it off its disks and starts its work on the other machines. Nothing is lost. |
| bm1 | The API stops (no new work) until bm1 is back. VMs on bm2 and bm3 keep running, and Ceph keeps working (2 of 3 monitors left). Nothing is lost. |
| One disk | Ceph keeps 3 copies of everything; it copies the data again onto the others. |
| A VM crashes | It is restarted on its own disk. |

---

## What you need

**Three servers**, each with:

| Item | Minimum | Why |
| --- | --- | --- |
| OS | Ubuntu 24.04 LTS, fresh install | everything below assumes it |
| CPU | 16 cores, **virtualization on** (Intel VT-x / AMD-V in the BIOS) | every session is a small virtual machine |
| RAM | 64 GB | about 2.5 GB per running session, plus Ceph |
| Disk 1 | SSD, 200 GB or more, for the OS | OS, Docker images, database |
| Disk 2 | NVMe, 1 TB or more, **empty**, only for Ceph | Ceph wipes it and uses all of it |
| Network | all three on one private network, 10 Gbps if you can | Ceph copies every write to the other machines |
| Console | IPMI / iDRAC / iLO access | to fix a machine that lost its network |

**Accounts and keys** (ask the people who own them):

| What | From whom |
| --- | --- |
| GitHub access to `Rakibul-Islam-Nahim/puku-agent-cloud` (it is private) | the repo owner |
| Cloudflare dashboard access for the `puku.sh` zone | whoever manages Cloudflare |
| The memory service's API key (`PUKU_MEMORY_SERVICE_KEY` of puku-memory-service) | the memory service owner |
| The skills service's operator token (`PUKU_SKILLS_OPERATOR_TOKEN` of puku-skills-service) | the skills service owner |

**On your own computer:** an SSH client, and a password manager for the
secrets you will create.

---

## Step 0: fill in your values

Write these down before you start. This guide uses the example values; replace
them with yours everywhere.

| Value | Example used in this guide | Yours |
| --- | --- | --- |
| Machine 1 name and LAN IP | `bm1`, `10.0.0.11` | |
| Machine 2 name and LAN IP | `bm2`, `10.0.0.12` | |
| Machine 3 name and LAN IP | `bm3`, `10.0.0.13` | |
| LAN range | `10.0.0.0/24` | |
| Ceph data disk on each machine | `/dev/nvme1n1` | |
| Your own IP (for SSH) | `203.0.113.5` | |

To see a machine's LAN IP: `ip -4 addr` (look for the address on the private
network). To see its disks: `lsblk` (the Ceph disk is the empty one, with no
partitions and no mount point).

---

## Step 1: prepare all three machines

Do **all of Step 1 on bm1, then bm2, then bm3.** Log in as root (`sudo -i`).

**1.1 Name the machine** (use `bm2` on machine 2, `bm3` on machine 3):

```bash
hostnamectl set-hostname bm1
cat >> /etc/hosts <<'EOF'
10.0.0.11 bm1
10.0.0.12 bm2
10.0.0.13 bm3
EOF
```

**1.2 Check virtualization:**

```bash
ls -l /dev/kvm
```

**Check:** it prints a line with `/dev/kvm`. If it says "No such file", turn
on VT-x / AMD-V in the BIOS and reboot. Nothing below works without it.

**1.3 Install the tools:**

```bash
apt-get update
apt-get install -y curl git jq ca-certificates lvm2 chrony ceph-common cephadm
curl -fsSL https://get.docker.com | sh
systemctl enable --now docker chrony
modprobe rbd && echo rbd > /etc/modules-load.d/rbd.conf
```

- Docker from `get.docker.com` brings `docker compose` and BuildKit, which the
  build needs.
- `chrony` keeps the clocks in sync; Ceph refuses to work with clocks apart.
- `ceph-common` and `cephadm` are Ceph's tools.

**Check:**

```bash
docker compose version     # Docker Compose version v2.x or newer
ceph --version             # ceph version 19.x (squid)
lsmod | grep rbd           # one line starting with "rbd"
```

**1.4 Firewall.** The three machines trust each other on the LAN; from
outside, only SSH from your IP. The tunnel needs no open port (it connects
out to Cloudflare).

```bash
ufw default deny incoming
ufw default allow outgoing
ufw allow from 203.0.113.5 to any port 22 proto tcp
ufw allow from 10.0.0.0/24
ufw --force enable
```

**Check:** open a **second** SSH session to the machine before you close the
first one. If it does not connect, fix the SSH rule from the first session.

---

## Step 2: Ceph, the shared disks

Ceph stores every session's and machine's disk three times, once on each
machine, so any machine can take over another's work. **Run all of Step 2 on
bm1 only** unless it says otherwise.

**2.1 Start the cluster on bm1:**

```bash
cephadm bootstrap --mon-ip 10.0.0.11
```

It takes a few minutes and ends with a dashboard URL and a password. Save
them in your password manager.

**2.2 Let bm1 manage bm2 and bm3.** Copy Ceph's SSH key to them (it asks for
each machine's root password once):

```bash
ssh-copy-id -f -i /etc/ceph/ceph.pub root@bm2
ssh-copy-id -f -i /etc/ceph/ceph.pub root@bm3
ceph orch host add bm2 10.0.0.12
ceph orch host add bm3 10.0.0.13
ceph orch apply mon --placement="bm1 bm2 bm3"
ceph orch apply mgr --placement="bm1 bm2 bm3"
```

If `ssh-copy-id` refuses because root password login is off, copy the key by
hand: show it on bm1 with `cat /etc/ceph/ceph.pub`, and on bm2 and bm3 add
that line to `/root/.ssh/authorized_keys`.

**Check** (wait a minute or two first):

```bash
ceph orch host ls          # bm1, bm2, bm3
ceph mon stat              # 3 mons, quorum bm1,bm2,bm3
```

**2.3 Add the data disks.** **This erases them.** Make sure the device name
is the empty disk on each machine (`lsblk` on that machine).

```bash
ceph orch device ls        # each machine's empty disk shows "Yes" under AVAILABLE
ceph orch daemon add osd bm1:/dev/nvme1n1
ceph orch daemon add osd bm2:/dev/nvme1n1
ceph orch daemon add osd bm3:/dev/nvme1n1
```

**Check:**

```bash
ceph osd tree              # 3 hosts, one osd each, all "up"
ceph -s                    # health: HEALTH_OK
```

`HEALTH_OK` can take a few minutes. A warning about clock skew means chrony
is not running on one machine.

**2.4 The pool and the user puku-agent-cloud uses:**

```bash
ceph osd pool create puku-sessions 64
ceph osd pool application enable puku-sessions rbd
rbd pool init puku-sessions
ceph auth get-or-create client.puku \
  mon 'profile rbd, allow command "osd blocklist"' \
  osd 'profile rbd pool=puku-sessions' \
  -o /etc/ceph/ceph.client.puku.keyring
```

The pool keeps 3 copies by default (one per machine) and stays writable while
2 are healthy. `osd blocklist` is the permission controld needs to cut a dead
machine off its disks.

**2.5 Give every machine the config and the key:**

```bash
ceph config generate-minimal-conf > /etc/ceph/ceph.conf
for m in bm2 bm3; do
  ssh root@$m mkdir -p /etc/ceph
  scp /etc/ceph/ceph.conf /etc/ceph/ceph.client.puku.keyring root@$m:/etc/ceph/
done
chgrp 10001 /etc/ceph/ceph.client.puku.keyring
chmod 640 /etc/ceph/ceph.client.puku.keyring
```

The last two lines let controld (which runs as user 10001 in its container)
read the key on bm1.

**Check, on each of bm1, bm2 and bm3:**

```bash
rbd --id puku ls puku-sessions && echo "ceph ok"
```

It prints `ceph ok` (the pool is empty, so nothing else).

---

## Step 3: the code, on all three

On **bm1, bm2 and bm3**:

```bash
git clone https://github.com/Rakibul-Islam-Nahim/puku-agent-cloud /opt/puku-agent-cloud
cd /opt/puku-agent-cloud
git checkout mahi
```

The repository is private: when git asks, the username is your GitHub
username and the password is a GitHub **personal access token** (GitHub →
Settings → Developer settings → Personal access tokens), not your password.

**Check:** `ls /opt/puku-agent-cloud/docker-compose.yml` shows the file.

---

## Step 4: the Cloudflare tunnel

The tunnel is how the internet reaches controld as `https://agent.api.puku.sh`
without opening any port on bm1.

> **Read this first.** `agent.api.puku.sh` is served today by another
> deployment. If you attach it to your new tunnel while the old one still has
> it, users go to whichever Cloudflare picks. Do it in two phases: test with a
> new hostname first (`agent-new.puku.sh` below), and move `agent.api.puku.sh`
> only when Step 9 passes and the owner of the old deployment agrees.
> **Never** reuse the old tunnel's token: that would split live traffic
> between the two deployments.

In the Cloudflare dashboard:

1. **Zero Trust** → **Networks** → **Tunnels** → **Create a tunnel**.
2. Type **Cloudflared**, name `puku-agent-cloud`, **Save**.
3. On the install page, copy the long token (the text after `--token` in any
   of the commands). Do **not** run the install command; Docker runs
   cloudflared for you. Save the token in your password manager.
4. **Next** → **Public hostname**: subdomain `agent-new`, domain `puku.sh`,
   type `HTTP`, URL `controld:7770`. **Save**.

`controld` is the name of the container on the Docker network, so it really
is `controld:7770`, not `localhost`.

---

## Step 5: machine 1, the main server

Everything in Step 5 runs **on bm1**, in `/opt/puku-agent-cloud`.

**5.1 Make the settings file:**

```bash
cd /opt/puku-agent-cloud
./deploy/scripts/stack-init.sh
```

It creates `.env`, puts a fresh random value in every secret, fills in bm1's
LAN IP, and lists the lines still to fill.

**Check:** it printed `this host's LAN IP: 10.0.0.11`. If it shows another
address, you will fix it in the next step.

**5.2 Edit `.env`:** `nano .env` (save with Ctrl+O, Enter; exit with Ctrl+X).
Set these lines; leave everything else as it is:

```ini
COMPOSE_PROFILES=control,tunnel,worker

CLOUDFLARE_TUNNEL_TOKEN=<the token from Step 4>
PUKU_MEMORY_SERVICE_KEY=<the memory service API key>
PUKU_SKILLS_TOKEN=<the skills operator token>

MINIO_BIND=10.0.0.11
PUKU_R2_ENDPOINT=http://10.0.0.11:9000

CONTROLD_BIND=10.0.0.11
PUKU_CONTROLD_URL=ws://10.0.0.11:7770/v1/worker
PUKU_WORKER_NAME=bm1

PUKU_RBD_POOL=puku-sessions
```

`PUKU_RBD_POOL` is in the "Shared disks on Ceph" part, commented out with a
`#`: remove the `#`. What the main ones mean:

| Setting | Meaning |
| --- | --- |
| `COMPOSE_PROFILES` | which parts run here: all of them on bm1 |
| `CONTROLD_BIND` | the address workers use to reach controld: bm1's LAN IP |
| `PUKU_CONTROLD_URL` | where this machine's own worker finds controld |
| `MINIO_BIND`, `PUKU_R2_ENDPOINT` | where MinIO listens; workers upload there directly |
| `PUKU_RBD_POOL` | turns on shared disks on Ceph |
| `PUKU_SECRET_KEY` | already filled. **Copy it into your password manager now.** Losing it makes stored credentials and every backup unreadable. |

**Check:** run `./deploy/scripts/stack-init.sh` again. It should say
`.env is complete`.

**5.3 Start it:**

```bash
docker compose up -d
```

The first run builds the controld and workerd images from the source: 10 to
20 minutes. Later runs take seconds.

**Check:**

```bash
docker compose ps
```

`controld` shows `healthy`, `postgres` `healthy`, `minio` `running`,
`cloudflared` `running`, `minio-init` `exited (0)`. `workerd` keeps
restarting for now: it has no token yet. That is expected.

```bash
curl -s "http://10.0.0.11:7770/health?deep=1"
```

shows `"database":"ok"` and `"object_storage_probe":"ok"`.

**5.4 Make the worker tokens and your admin key.** Each is printed once:
copy it into your password manager straight away.

```bash
docker compose exec controld puku-controld gen-worker-token --name bm1
docker compose exec controld puku-controld gen-worker-token --name bm2
docker compose exec controld puku-controld gen-worker-token --name bm3
docker compose exec controld puku-controld gen-key --name ops --admin
```

The worker tokens start with `pkw_`, the admin key with `pkc_`. The name in
`--name` must be exactly the machine's `PUKU_WORKER_NAME`.

**5.5 Give bm1's worker its token:** in `.env` set
`PUKU_WORKER_TOKEN=<the pkw_ token for bm1>`, then:

```bash
docker compose up -d
```

**Check:**

```bash
curl -s "http://10.0.0.11:7770/health?deep=1"
```

now shows `"workers_connected":1`. And from your own computer, open
`https://agent-new.puku.sh/health`: it shows `"status":"ok"`.

---

## Step 6: machines 2 and 3, the workers

**On bm2**, in `/opt/puku-agent-cloud`, create `.env` with exactly this
(replace the token):

```bash
cd /opt/puku-agent-cloud
cat > .env <<'EOF'
COMPOSE_PROFILES=worker
PUKU_CONTROLD_URL=ws://10.0.0.11:7770/v1/worker
PUKU_WORKER_NAME=bm2
PUKU_WORKER_TOKEN=pkw_paste_the_bm2_token_here
PUKU_RBD_POOL=puku-sessions
PUKU_EGRESS_UNRESTRICTED=true
RUST_LOG=info
EOF
chmod 600 .env
docker compose up -d
```

**On bm3**, the same with `PUKU_WORKER_NAME=bm3` and bm3's token.

The first `up -d` builds the worker image (5 to 10 minutes per machine).

**Check, on bm1:**

```bash
curl -s "http://10.0.0.11:7770/health" ; echo
export K=pkc_paste_your_admin_key_here
curl -s -H "Authorization: Bearer $K" http://10.0.0.11:7770/v1/workers | jq '.[] | {name, connected, status, features}'
```

`workers_connected` is `3`, and bm1, bm2 and bm3 each show
`"connected": true`, `"status": "online"`, and `lease` and `shared_volumes`
among their features.

> **Faster builds (optional).** Instead of building the worker image on every
> machine, build it once on bm1 and copy it:
> `docker save puku-workerd:local | ssh root@bm2 docker load`, then
> `docker compose up -d --no-build` on bm2.

---

## Step 7: guest images

A session boots a VM from a "guest image". Each worker keeps its own copy, so
the image is loaded into all three.

**On bm1:**

```bash
cd /opt/puku-agent-cloud
docker build -t puku-agent:0.1.0 images/puku-agent
for m in bm1 bm2 bm3; do
  docker save puku-agent:0.1.0 | ssh root@$m \
    "cd /opt/puku-agent-cloud && docker compose exec -T workerd msb load -t puku-agent:0.1.0"
done
```

(`ssh root@bm1` from bm1 needs bm1's own key in `/root/.ssh/authorized_keys`;
or for bm1 run the part after `|` directly:
`docker save puku-agent:0.1.0 | docker compose exec -T workerd msb load -t puku-agent:0.1.0`.)

Then in bm1's `.env` set `PUKU_AGENT_IMAGE=puku-agent:0.1.0` (remove the `#`)
and run `docker compose up -d`.

**Check, on each machine:**

```bash
cd /opt/puku-agent-cloud && docker compose exec workerd msb image list
```

lists `puku-agent:0.1.0`.

When the image is rebuilt later, remove the old one first on each machine
(`docker compose exec workerd msb image rm puku-agent:0.1.0`), then load
again; loading onto an existing name keeps the old image.

---

## Step 8: connect PukuBot

PukuBot (`app.bot.puku.sh`) uses puku-agent-cloud for its computers.

**8.1 Its API key, on bm1:**

```bash
cd /opt/puku-agent-cloud
docker compose exec controld puku-controld gen-key --org pukubot --name puku-bot
docker compose exec -T postgres psql -U puku puku_cloud -c \
  "UPDATE quotas SET max_concurrent_machines = 100000 WHERE org_id IN (SELECT id FROM orgs WHERE name = 'pukubot')"
```

The second command lifts the default limit of 20 machines (PukuBot keeps one
per user). It should print `UPDATE 1`.

**8.2 In PukuBot's own settings** (its `.env`, on the machine that runs it),
give its owner:

```ini
PUKU_AGENT_CLOUD_URL=https://agent.api.puku.sh
PUKU_AGENT_CLOUD_API_KEY=<the pkc_ key from 8.1>
```

(If PukuBot runs on bm1 itself, it can use `http://controld:7770` over the
`puku-link` Docker network instead.)

**8.3 Engine.** The workers in this guide run the **libkrun** engine. PukuBot's
current deployment asks for **cloud_hypervisor**. Either set PukuBot to
`libkrun` (`PUKU_AGENT_CLOUD_ENGINE=libkrun` in its settings), or add a Cloud
Hypervisor worker on the host (`deploy/scripts/setup-worker.sh`). Whether
PukuBot's desktop image runs on libkrun has not been tested yet: test one
computer before switching users over.

**8.4 Machine links** (the noVNC screens PukuBot embeds). If PukuBot runs on
another machine, give links their own hostname: in the tunnel (Step 4) add a
public hostname such as `links.agent.api.puku.sh` → `HTTP` → `controld:7770`,
and in bm1's `.env` set `PUKU_LINKS_URL=https://links.agent.api.puku.sh`,
then `docker compose up -d`.

---

## Step 9: test everything

Run on **bm1**, with your admin key: `export K=pkc_...` and
`export U=http://10.0.0.11:7770`.

**9.1 Health:**

```bash
curl -s "$U/health?deep=1"; echo
```

All of: `"status":"ok"`, `"database":"ok"`, `"object_storage_probe":"ok"`,
`"workers_connected":3`.

**9.2 A test machine on a Ceph disk** (no AI credits needed):

```bash
M=$(curl -s -H "Authorization: Bearer $K" -H 'content-type: application/json' \
  -d '{"image":"alpine","engine":"libkrun","volume":{"path":"/data"},"wait_s":240}' \
  $U/v1/machines | jq -r .machine.id); echo "machine $M"
curl -s -H "Authorization: Bearer $K" -H 'content-type: application/json' \
  -d '{"argv":["sh","-c","date > /data/proof.txt; cat /data/proof.txt"]}' \
  $U/v1/machines/$M/exec | jq .
rbd --id puku ls puku-sessions
```

**Check:** the exec prints the date in `stdout`, and `rbd ls` shows
`machine-<the id>`.

**9.3 The failover drill.** Do this before real users arrive.

1. See which machine runs the test machine:

   ```bash
   W=$(curl -s -H "Authorization: Bearer $K" $U/v1/machines/$M | jq -r .worker_id)
   curl -s -H "Authorization: Bearer $K" $U/v1/workers | jq -r ".[] | select(.id==\"$W\") | .name"
   ```

   If it says `bm1`, pick that machine's own test anyway, but the drill is
   about bm2 or bm3: create another machine until one lands there.
2. Power that machine off hard, from its IPMI console (or pull its network
   cable). Not `shutdown`: a real failure does not shut down politely.
3. On bm1, watch: `docker compose logs -f controld`. Within about 20 seconds
   the dead machine is declared dead, then fenced, then the test machine
   starts on a healthy one.
4. Check:

   ```bash
   docker compose exec -T postgres psql -U puku puku_cloud -c \
     "SELECT action, outcome, ts FROM fence_log ORDER BY ts DESC LIMIT 5"
   curl -s -H "Authorization: Bearer $K" -H 'content-type: application/json' \
     -d '{"argv":["cat","/data/proof.txt"]}' $U/v1/machines/$M/exec | jq -r .stdout
   ```

   **Pass:** `fence_log` has `blocklist | ok`, and `proof.txt` still shows the
   same date.
5. Power the machine back on. After it boots, its worker container starts by
   itself and rejoins. `ceph -s` returns to `HEALTH_OK` once Ceph has caught
   up.

Delete the test machine when done:
`curl -s -X DELETE -H "Authorization: Bearer $K" $U/v1/machines/$M`.

**9.4 A real session** needs a user's puku login. Follow
[`DEPLOYMENT.md`](DEPLOYMENT.md), Steps 9 and 10, with
`PUKU_CLOUD_URL=https://agent-new.puku.sh`.

**9.5 Backups** start an hour after the first shared disk exists:

```bash
docker compose exec -T postgres psql -U puku puku_cloud -c \
  "SELECT kind, status, size_bytes, ts FROM disk_backups ORDER BY ts DESC LIMIT 5"
```

`Durable` means uploaded to MinIO, read back and checked.

**9.6 Go live.** When everything above passes and the old deployment's owner
agrees: in the Cloudflare dashboard remove `agent.api.puku.sh` from the old
tunnel, then add it to yours (`agent.api` / `puku.sh` / `HTTP` /
`controld:7770`). Check `https://agent.api.puku.sh/health` shows
`"workers_connected":3`.

---

## Step 10: backups

Ceph keeps 3 copies of every disk, and controld backs up every disk to MinIO
each hour. Two more things need backing up: the database, and the secrets.

**10.1 The database, every night at 02:30, kept 14 days, copied to bm2.** On
bm1:

```bash
mkdir -p /srv/backups && ssh root@bm2 mkdir -p /srv/backups
cat > /etc/cron.d/puku-db-backup <<'EOF'
30 2 * * * root cd /opt/puku-agent-cloud && docker compose exec -T postgres pg_dump -U puku -Fc puku_cloud > /srv/backups/puku_cloud-$(date +\%F).dump && scp -q /srv/backups/puku_cloud-$(date +\%F).dump root@bm2:/srv/backups/ && find /srv/backups -name '*.dump' -mtime +14 -delete
EOF
```

**Check** the next morning: `ls -lh /srv/backups` on bm1 and bm2.

**10.2 The secrets.** Keep these in your password manager or another safe
place off the servers:

- bm1's `/opt/puku-agent-cloud/.env` (it holds `PUKU_SECRET_KEY` and every
  password)
- `/etc/ceph/ceph.conf` and `/etc/ceph/ceph.client.puku.keyring`
- the Ceph dashboard password from Step 2.1

---

## Daily operation

All commands run in `/opt/puku-agent-cloud` on the machine named.

| Task | How |
| --- | --- |
| Is everything up? | bm1: `docker compose ps`, `curl -s http://10.0.0.11:7770/health`, `ceph -s` |
| Logs | `docker compose logs -f controld` (bm1), `docker compose logs -f workerd` (any) |
| Restart controld | bm1: `docker compose restart controld`. Running VMs are not touched. |
| Stop / start everything on one machine | `docker compose down` / `docker compose up -d` (data is kept) |

**Updating to a new version.** On bm1 first, then the workers one at a time:

```bash
git pull
docker compose up -d --build
```

Updating a worker restarts its container, and that **stops the VMs running on
it**: machines show `stopped` and start again on their next use; a session
that was mid-task restarts on its own disk. So update workers one at a time,
when they are quiet, and drain them first:

```bash
# on bm1: find the worker's id, drain it, update it, undrain it
curl -s -H "Authorization: Bearer $K" $U/v1/workers | jq -r '.[] | "\(.id) \(.name)"'
curl -s -X POST -H "Authorization: Bearer $K" $U/v1/workers/<id>/drain
# ... wait until nothing runs there, update that machine, then:
curl -s -X POST -H "Authorization: Bearer $K" $U/v1/workers/<id>/undrain
```

To update only controld on bm1 without touching its VMs:
`docker compose up -d --build --no-deps controld`.

**Rebooting a machine on purpose.** Drain it (above), then on bm1
`ceph osd set noout` (Ceph does not start copying data around for a planned
reboot), reboot, wait until `ceph -s` shows all osds up, `ceph osd unset noout`,
undrain. Everything starts by itself after the reboot.

**Never run** `docker compose down -v`: `-v` deletes the database and MinIO.

---

## Troubleshooting

| What you see | What to do |
| --- | --- |
| `ls /dev/kvm`: no such file | Turn on VT-x / AMD-V in the BIOS. |
| `docker compose up` fails: "port is already allocated" | Something else uses 7770 or 9000 on bm1 (an old deployment?): `ss -ltnp \| grep -E '7770\|9000'`. |
| `controld` never becomes healthy | `docker compose logs controld`. "password authentication failed" → the `.env` was edited after the first start: the database keeps the first password. |
| `minio-init` did not exit 0 | `docker compose logs minio-init`; usually an empty MinIO password in `.env` (rerun `stack-init.sh`). |
| `object_storage_probe` is not `"ok"` | `PUKU_R2_ENDPOINT` and `MINIO_BIND` must both be bm1's LAN IP. |
| `workers_connected` lower than expected | On that machine `docker compose logs workerd`. "auth failed" → wrong token, or the token was made for another `--name`. "connecting to controld" again and again → `PUKU_CONTROLD_URL` is wrong or the firewall blocks 7770. |
| workerd restarts and says `preflight FAILED` | It names what is missing; usually `/dev/kvm`. |
| A worker lacks `shared_volumes` | `PUKU_RBD_POOL` missing in that machine's `.env`, or `/etc/ceph` missing there (Step 2.5). |
| `ceph -s` shows `HEALTH_WARN` | `ceph health detail` explains. Clock skew → `systemctl restart chrony` on the named machine. |
| `https://agent-new.puku.sh` gives 502 / 1033 | `docker compose logs cloudflared`; the tunnel's public hostname must be `HTTP` → `controld:7770`. |
| A session fails with "Not authorized … index.docker.io" | The guest image is not loaded on that worker (Step 7). |
| Memory "off" in the controld log | `PUKU_MEMORY_SERVICE_KEY` empty or wrong. |
| Skills never load | `PUKU_SKILLS_TOKEN` wrong, or `skill.api.puku.sh` does not resolve yet (ask the skills owner to publish it). |

---

## Words used in this guide

| Word | Meaning |
| --- | --- |
| **controld** | The control server: the API, the dashboard, and the part that decides where work runs. |
| **workerd** | The worker: runs the VMs on one machine. |
| **Session** | One AI agent task, running in its own small VM. |
| **Machine** | A long-lived VM (PukuBot's computers). |
| **Guest image** | What a session's VM boots from. |
| **msb** | The tool workerd uses to run libkrun VMs; it keeps its own copy of guest images. |
| **Ceph** | Storage spread over the three machines, keeping 3 copies of every disk. |
| **OSD** | A Ceph data disk. **Monitor**: a Ceph process that keeps the cluster's map; more than half must be up. |
| **Fence** | Cutting a dead machine off its disks before another machine opens them. |
| **MinIO** | Our own file storage (S3-compatible): backups, snapshots, uploads. |
| **Tunnel** | cloudflared: connects out to Cloudflare so `agent.api.puku.sh` reaches controld with no open port. |
| **Worker token (`pkw_`)** | One machine's password for joining controld. |
| **API key (`pkc_`)** | A key for calling controld's API (yours, PukuBot's). |
| **`.env`** | The settings file of one machine. Secret: never share or commit it. |
| **Drain** | Tell controld to stop sending new work to a machine. |

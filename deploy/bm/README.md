# Deploying the control plane on a bare-metal box

Same pattern as `puku-chat-compute-service`: a git tag publishes the image to
Docker Hub, and the box runs `docker compose pull && docker compose up -d`
with only this directory copied over — no app source on the host.

```powershell
scp -r .\deploy\bm agent-N:~/puku-agent-cloud
# on the box:
#   cd ~/puku-agent-cloud
#   cp .env.example .env    # fill it in
#   docker compose pull
#   docker compose up -d
```

## What runs where

| Component | Where | Why |
| --- | --- | --- |
| `controld` + Postgres + `cloudflared` | this compose file | Plain network services; nothing needs the host |
| `workerd` | **systemd on the host**, not here | Needs `/dev/kvm` and the msb toolchain; see `../systemd/puku-workerd.service` and `../scripts/prestage-msb.sh` |

Workers dial **out** to controld over one WebSocket, so a worker box never
exposes an inbound port and can sit behind NAT.

## Ingress

controld binds to `127.0.0.1:7770`; the only public path is the Cloudflare
tunnel. Point one hostname per environment at `http://controld:7770`:

| Hostname | Environment |
| --- | --- |
| `cloud.puku.sh` | prod |
| `cloud.dev.puku.sh` | dev |

## First run

```sh
# A client API key (the dashboard and puku-cloud CLI use this).
docker compose exec controld puku-controld gen-key --org acme --admin

# One registration token per worker host. Put the output in
# /etc/puku/worker-token on that host, then start puku-workerd.
docker compose exec controld puku-controld gen-worker-token --name box-1
```

Then turn the legacy shared secret off (`PUKU_ALLOW_SHARED_WORKER_TOKEN=0`,
which is already the default here) so a leaked token can be revoked per host
instead of rotating the fleet.

## Checks

```sh
curl -fsS http://127.0.0.1:7770/health    # db reachable, workers connected
curl -fsS http://127.0.0.1:7770/metrics   # prometheus text
```

`/health` returns 503 when Postgres is unreachable, which is what the
container healthcheck and any uptime monitor should watch. A control plane
with zero workers is still healthy — sessions queue until a worker registers.

## Secrets

Everything sensitive is in `.env` (never committed): the Postgres password,
the Cloudflare tunnel token, the R2 keys, and the fallback model credential.
R2 credentials live **only** here — workers get short-lived presigned URLs
over the control link and never hold a bucket key.

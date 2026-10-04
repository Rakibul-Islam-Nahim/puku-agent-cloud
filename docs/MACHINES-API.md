# Machines API

A **machine** is a VM with nothing of ours inside it: you pick the image, the
platform boots it on a worker, and you drive it from outside — run commands,
move files, reach its ports. It is the resource puku-bot-svc's `puku-cloud`
sandbox provider sits on (one machine per bot computer), and it is
deliberately generic: no puku-cli, no event stream, no model credential.

Sessions (`/v1/sessions`) are unchanged by any of this.

- Base URL: the control plane (`https://agent.api.puku.sh` in production).
- Auth: `Authorization: Bearer <pkc_ key or platform bearer>`, exactly as for
  sessions. Machines are scoped to the key's org (and user, for platform
  bearers). Another org's machine is a 404, never a 403.
- Errors: `{"error": {"message": "..."}}`.
- An unknown machine id is `404` with a message containing
  `machine <id> not found`.

## The machine object

```json
{
  "id": "0b7c…",                  // uuid
  "external_id": "home-abc",      // caller's idempotency key, or null
  "name": "mch-0b7c1f2e9a41",     // VM name on the worker
  "state": "running",             // see below
  "engine": "cloud_hypervisor",   // libkrun | cloud_hypervisor
  "image": "pukubot-computer:latest",
  "cpus": 2,
  "memory_mib": 4096,
  "expose": [7070, 6080, 6081],   // guest ports reachable through /ports and links
  "volume": {"path": "/home/pukubot", "uid": 1000},  // or null
  "labels": {"spaceId": "s1"},
  "worker_id": "…",               // null when not placed
  "error": null,
  "reason": null,                 // why it is not running, as a word (see "When a machine cannot start")
  "idle_timeout_s": 0,
  "max_duration_s": 0,
  "created_at": "…", "started_at": "…", "stopped_at": null, "last_active_at": "…"
}
```

States: `scheduled → booting → running → stopping → stopped`, plus `failed`
and `destroyed`. `start` moves `stopped`/`failed` back to `scheduled`.
`destroyed` is terminal.

## Lifecycle

### `POST /v1/machines` — create, or ensure running

```json
{
  "external_id": "home-abc",          // optional; unique per org among live machines
  "image": "pukubot-computer:latest", // optional; default PUKU_MACHINE_IMAGE
  "engine": "cloud_hypervisor",       // optional; default PUKU_ENGINE_DEFAULT
  "cpus": 2,                          // optional; default 2, clamped to the deployment max
  "memory_mib": 4096,                 // optional; default 2048, clamped
  "expose": [7070, 6080, 6081],       // optional; default []
  "env": {"DISPLAY": ":1"},           // optional; given to the entrypoint and every exec
  "secret_env": {"TOKEN": "…"},       // optional; same, but stored encrypted and never echoed
  "entrypoint": {                     // optional; started after every boot
    "argv": ["/usr/local/bin/pukubot-computer"],
    "user": "1000"                    // optional; default the volume uid, else root
  },
  "volume": {"path": "/home/pukubot", "uid": 1000},  // optional; persistent across stop/start
  "idle_timeout_s": 0,                // optional; 0 = never auto-stop
  "max_duration_s": 0,                // optional; 0 = no cap
  "labels": {"spaceId": "s1"},        // optional; stored, returned, not interpreted
  "wait_s": 120,                      // optional; wait up to this long (max 300) for the boot to settle
  "queue": false,                     // optional; see "When a machine cannot start"
  "relocate": false,                  // optional; see "Recovery when a worker is lost"
  "persist_root": false,              // optional; cloud_hypervisor only; see "Snapshots"
  "snapshots": {"on_stop": true}      // optional; see "Snapshots"
}
```

Response `200`:

```json
{"machine": { … }, "resumed": true}
```

- With an `external_id` that already names a live (not destroyed) machine in
  the org, this is **ensure running**: the stored spec is replaced with this
  request's (it takes effect at the next boot), and a stopped or failed
  machine is started. A running one is returned as is.
- **`resumed`** is true when the machine's persistent volume already existed
  before this boot — the caller's files are still there. It is false for a
  first boot, and after the volume was lost (the worker holding it was gone
  past the grace period and the machine was re-placed). With `wait_s > 0`
  it is exactly what the worker found at boot; with `wait_s = 0` it is the
  best knowledge at request time.
- `400` for an invalid port or an `entrypoint.argv` that is empty.
- A machine the fleet cannot run right now is refused **at once**, whatever
  `wait_s` says, with a machine-readable reason: see
  [When a machine cannot start](#when-a-machine-cannot-start). A boot that a
  worker tries and fails comes back as soon as the worker reports it
  (`502 boot_failed`), not after `wait_s`.

### `POST /v1/machines/{id}/start`

Body `{"wait_s": 120, "queue": false}` (optional). Same response, and the same
refusals, as create. Idempotent.

### `POST /v1/machines/{id}/stop`

Body `{"wait_s": 60}` (optional). `202 {"machine": …}`. Stops the VM and keeps
the volume on its worker. Idempotent: stopping a stopped machine is `202`.

### `DELETE /v1/machines/{id}`

`204`. Stops the VM and deletes the volume. Idempotent while the row exists;
an unknown id is `404`.

### `POST /v1/machines/{id}/touch`

`204`. Marks the machine active, resetting its idle timer.

### `GET /v1/machines/{id}` / `GET /v1/machines?external_id=&state=&limit=`

The machine object, or a list of them (newest first, default limit 50).

### When a machine cannot start

Create and start never wait on a fleet that cannot run the machine. If no
connected worker could take it *right now*, the request returns at once,
whatever `wait_s` says, with a machine-readable reason:

```json
HTTP/1.1 503 Service Unavailable
Retry-After: 15

{"error": {"message": "no worker has 8 free slots right now (the most free is 3)",
           "reason": "capacity_full",
           "detail": {"requested_slots": 8, "best_free_slots": 3, "workers": 2},
           "retry_after_s": 15},
 "machine": { … }}   // only when the machine already existed
```

`error.message` is the field every other error has, so a client that reads
only that keeps working.

| `reason` | Status | Meaning | What to do |
|---|---|---|---|
| `no_workers` | 503 | No worker is connected. | Retry; alert if it persists. |
| `engine_unavailable` | 503 | No connected worker runs the requested engine. | Retry, or ask for another engine. |
| `all_draining` | 503 | Every worker that could run it is draining. | Retry. |
| `capacity_full` | 503 | It would fit, but no worker has room now. | Retry after `Retry-After`. |
| `insufficient_disk` | 503 | No worker has enough free disk. | Retry; an operator should free disk. |
| `volume_host_offline` | 503 | The worker holding the machine's volume is offline. `detail.grace_left_s` is how long until the machine is re-placed elsewhere with an empty volume. | Retry. |
| `volume_host_full` | 503 | The volume's worker is online but full. | Retry. |
| `worker_lost` | 503 | The worker it was placed on disconnected before booting it, and no other can take it. | Retry. |
| `machines_unsupported` | 422 | No connected worker runs machines. | Upgrade workerd. |
| `too_large` | 422 | Bigger (in slots or vCPUs) than any connected worker could ever hold. | Ask for less. |
| `image_not_staged` | 422 | A `cloud_hypervisor` machine whose image no suitable worker has staged. | Run `deploy/scripts/build-ch-rootfs.sh <image>` on a worker. |
| `quota_exceeded` | 429 | The org's `max_concurrent_machines` is reached. | Destroy a machine. |
| `engine_not_allowed` | 400 | The deployment does not offer that engine. | Ask for another. |
| `boot_failed` | 502 | A worker tried and the boot failed. `detail.worker_reason` (`image_not_staged`, `engine_unavailable`, `insufficient_disk`, `invalid_spec`, `boot_failed`) and `detail.worker_error` say why. | Depends on `worker_reason`. |
| `stopping` | 409 | Still stopping after 30 s. | Retry. |

- A **new** machine that is refused is not created: no quota is used and its
  `external_id` stays free.
- An **existing** machine that is refused stays `stopped` or `failed`; its
  `reason` and `error` fields say why.
- `"queue": true` restores the old behaviour: the machine stays `scheduled`
  and boots when a worker can take it, with `reason` saying why it waits.
- A slot is 2 vCPUs and 2 GiB: a machine takes
  `max(ceil(memory_mib / 2048), ceil(cpus / 2))` of them.

## Commands

### `POST /v1/machines/{id}/exec`

```json
{
  "argv": ["sh", "-c", "echo hi"],   // required, non-empty
  "cwd": "/home/pukubot",            // optional
  "env": {"A": "b"},                 // optional; merged over the machine env
  "user": "1000",                    // optional; uid or name; default the volume uid, else root
  "timeout_ms": 30000,               // optional; default 300000, max 3600000
  "stdin": "text",                   // optional; or "stdin_b64" for bytes
  "stdin_b64": null
}
```

Response `200`:

```json
{"code": 0, "stdout": "hi\n", "stderr": "", "timed_out": false, "truncated": false}
```

- Output is decoded as UTF-8 (lossily). Each stream is capped at 8 MiB;
  `truncated` says whether either was cut.
- A timeout kills the command and returns `code: 124`, `timed_out: true`,
  with `command timed out after <n> ms` appended to `stderr`.
- `409` when the machine is not `running`.

## Files

All paths are absolute guest paths. A path containing a `..` component is
refused with `400`. Operations run as the volume uid (else root).

### `GET /v1/machines/{id}/files?path=/abs/file&max_bytes=N`

`200 application/octet-stream` with the file's bytes; `404` if it does not
exist; `413` if it is larger than `max_bytes`; `400` if it is a directory.

### `GET /v1/machines/{id}/files?path=/abs/dir&list=1&recursive=1`

```json
[{"path": "/abs/dir/a.txt", "kind": "file", "size": 12, "executable": false},
 {"path": "/abs/dir/sub", "kind": "dir", "size": 0, "executable": false}]
```

Symlinks are skipped. `recursive=0` (the default) lists one level. `404` if
the directory does not exist.

### `PUT /v1/machines/{id}/files?path=/abs/file&mode=0644`

Body: the bytes. `204`. Parent directories are created. `mode` is octal,
default `0644`.

### `GET /v1/machines/{id}/archive?path=/abs/dir&exclude=GLOB`

`200 application/gzip`: a tar.gz of the directory's *contents* (entries are
relative, `./a.txt`). `exclude` is repeatable and uses tar's `--exclude`
matching. `404` if the directory does not exist.

### `PUT /v1/machines/{id}/archive?path=/abs/dir&replace=1`

Body: a tar.gz. `204`. Extracts into the directory, creating it;
`replace=1` empties it first. Keep bodies under your ingress's limit
(Cloudflare's is ~100 MB on most plans).

## Ports

### `ANY /v1/machines/{id}/ports/{port}/{path…}`

HTTP and WebSocket (upgrade) proxy to `127.0.0.1:{port}` inside the guest.
`{port}` must be in the machine's `expose` list, else `403`. The request path
after `/ports/{port}` is forwarded as is, query included.

- Your `Authorization` header, cookies and any `api_key=` query parameter
  are for the control plane and are **never** forwarded into the guest.
- A service in the guest that wants its own bearer (puku-bot's `control.py`)
  gets it from **`x-guest-authorization`**: that header is forwarded as the
  guest-side `Authorization`.
- `Set-Cookie` is stripped from responses.
- Nothing listening on the port is a `502`.

### `POST /v1/machines/{id}/links` — capability URLs for browsers

A browser iframe cannot send a bearer, so a port can be shared through a
short-lived capability URL instead:

```json
{"port": 6080, "path": "/embed.html", "query": "view_only=1", "ttl_s": 3600}
```

Response `200`:

```json
{"url": "https://links.agent.api.puku.sh/v1/links/<cap>/embed.html?view_only=1",
 "expires_at": "…"}
```

- `GET`/WebSocket `/v1/links/{cap}/{path…}` proxies to that one port of that
  one machine with no other authentication. The capability is in the
  **path**, so relative URLs inside the page (noVNC's `websockify`) resolve
  under it and inherit it.
- `port` must be in `expose`. `ttl_s` defaults to 3600, max 86400. An expired
  or forged capability is a `404`.
- Served from `PUKU_LINKS_URL` (a separate hostname in production, so guest
  HTML never shares an origin with the dashboard). `Set-Cookie` is stripped
  from proxied responses.
- A link is bound to one port: give a viewer the view port and a controller
  the control port, and the distinction is enforced by the platform.

## Snapshots

A snapshot is a machine's disks in object storage (MinIO in development,
Cloudflare R2 in production; any S3-compatible store), so the machine can be
restored on **any** worker, at the size it had, after the worker holding its
volume is gone.

- **Cold and disk-level.** The volume, as a tar of its host directory, and,
  for a `persist_root` machine, its root disk: what was installed. Never
  memory. A restored machine boots fresh, with its files.
- **Encrypted before it leaves the worker.** Each snapshot has its own
  256-bit key (XChaCha20-Poly1305, in independently authenticated 1 MiB
  chunks), sealed on the control plane with `PUKU_SECRET_KEY`. The bucket on
  its own reveals nothing.
- **Workers never hold bucket credentials.** They upload to presigned part
  URLs; only the control plane creates, completes and deletes objects.
- **Unchanged disks are not uploaded again.** A stop or periodic snapshot of
  a machine unchanged since its last one is `skipped`, and an unchanged root
  disk is reused from the last snapshot that has it.

Offered when the deployment has object storage and `PUKU_SECRET_KEY`
(`PUKU_SNAPSHOTS=auto`, the default). Otherwise the endpoints below answer
`409 snapshots_unavailable`.

### Policy: `snapshots` on create

```json
"snapshots": {
  "on_stop": true,          // snapshot every time the machine stops, idle stops included
  "interval_s": 900,        // and every 15 minutes while it runs (0 = never; minimum 300)
  "keep": 5,                // ready snapshots kept (default PUKU_SNAPSHOT_KEEP)
  "before_destroy": true,   // a last one before DELETE
  "exclude": [".cache"]     // paths under the volume never captured
}
```

Everything is off by default. `"persist_root": true` (Cloud Hypervisor only)
keeps the machine's root disk across stop/start, so what was installed
survives, and makes it part of every snapshot. A root disk belongs to the
machine's `image`: changing the image starts a fresh one.

### Consistency

- `clean`: taken with the VM stopped, so exactly what was on disk.
- `live`: taken while it ran, so each file as it was when read, like a
  backup of a running system. A database mid-write may need recovery on
  restore. A live snapshot never reads the root disk, which would be torn;
  it reuses the last clean snapshot's.

### `POST /v1/machines/{id}/snapshots`

Body `{"label": "before upgrade", "pinned": false, "wait_s": 60}` (all
optional). Live while the machine runs, clean while it is stopped. `202`
with the snapshot while it is still being taken; `200` once it is `ready`,
`skipped` or `failed`. Retention never deletes a `pinned` snapshot.

```json
{"id": "…", "machine_id": "…", "trigger": "manual", "state": "ready",
 "consistency": "live", "engine": "cloud_hypervisor", "image": "…",
 "cpus": 8, "memory_mib": 16384, "size_bytes": 5368709120, "stored_bytes": 1932735283,
 "layers": [{"layer": "volume", "plain_bytes": 5100000000, "stored_bytes": 1800000000, "reused": false},
            {"layer": "root", "plain_bytes": 268709120, "stored_bytes": 132735283, "reused": true}],
 "pinned": false, "label": null, "error": null, "created_at": "…", "completed_at": "…"}
```

States: `pending → uploading → ready | skipped | failed`, then `deleting →
deleted`. Triggers: `stop`, `manual`, `periodic`, `destroy`, `update`.

### `GET /v1/machines/{id}/snapshots` · `GET|DELETE /v1/machines/{id}/snapshots/{sid}`

List (newest first), fetch, delete. `DELETE` is `204` and idempotent; a
snapshot still being taken is `409`.

### `POST /v1/machines/{id}/restore`

```json
{"snapshot_id": "latest",           // or a snapshot id; default the latest ready one
 "worker_id": null,                 // land on this worker; default any that can take it
 "cpus": null, "memory_mib": null,  // default the machine's own size
 "stop": false,                     // stop a live machine first instead of answering 409
 "wait_s": 120}
```

Boots the machine from the snapshot on whichever worker has room (and, for
a root layer, the image staged), keeping its id, `external_id` and secrets.
The state goes `scheduled → restoring → booting → running`. The response is
create's, with `resumed: true` and `machine.restored_from` naming the
snapshot. A restore that cannot be placed gets the same fail-fast refusals as
a start.

### Recovery when a worker is lost

- `POST /start`, or create as ensure-running, with `"relocate": true`: when
  the worker holding the volume is offline, the latest snapshot is restored
  on another worker **now**.
- Without it, a start is refused with `volume_host_offline` until that
  worker has been gone 15 minutes. Its `detail.snapshot_available` says
  whether relocating is possible. After the 15 minutes the machine is
  restored from its latest snapshot automatically, or, with none, starts
  with an empty volume and `resumed: false`, as it always did.
- The worker whose copy a restore replaced is told to delete that copy once
  the restored boot runs, or when it next reconnects.

### Destroy

`DELETE /v1/machines/{id}?snapshot=true|false&purge=true`. `snapshot`
defaults to the machine's `before_destroy`; `purge` deletes every snapshot
of the machine now. Otherwise a destroyed machine's snapshots are kept for
`PUKU_SNAPSHOT_RETAIN_DESTROYED_DAYS` (7) days.

## Deployment settings (controld)

| Variable | Default | Meaning |
|---|---|---|
| `PUKU_MACHINE_IMAGE` | `pukubot-computer:latest` | Image for a machine that names none |
| `PUKU_MACHINE_MAX_CPUS` | `4` | Clamp for `cpus` |
| `PUKU_MACHINE_MAX_MEMORY_MIB` | `8192` | Clamp for `memory_mib` |
| `PUKU_LINKS_URL` | the listen address | Public base for capability URLs |
| `PUKU_LINKS_SECRET` | derived from `PUKU_SECRET_KEY` | HMAC key for capabilities |
| `PUKU_MACHINE_IDLE_SWEEP_S` | `30` | How often idle machines are checked |
| `PUKU_SNAPSHOTS` | `auto` | `auto` offers snapshots when object storage (`PUKU_R2_*`) and `PUKU_SECRET_KEY` are both set; `true` refuses to start without them; `false` never offers them |
| `PUKU_SNAPSHOT_PART_MIB` | `64` | Multipart part size. Every part but the last is exactly this big, which R2 requires |
| `PUKU_SNAPSHOT_KEEP` | `5` | Ready snapshots kept per machine that sets no `keep` of its own |
| `PUKU_SNAPSHOT_RETAIN_DESTROYED_DAYS` | `7` | How long a destroyed machine's snapshots are kept |
| `PUKU_SNAPSHOT_SWEEP_S` | `60` | How often periodic snapshots, retention and deletion run |

Workers need no new settings: a worker built with machine support advertises
the `machines` and `snapshots` features and runs machines on whichever
engines it has enabled. Two optional knobs: `PUKU_SNAPSHOT_CONCURRENCY`
(default `2`) is how many captures and restores run at once, and
`PUKU_SNAPSHOT_ZSTD_LEVEL` (default `3`) trades upload size for CPU.

## How it works (for operators)

- Placement uses the same scheduler as sessions: a machine goes to a worker
  that advertised its engine **and** the `machines` feature, and takes
  `max(ceil(memory_mib / 2048), ceil(cpus / 2))` slots. Once a machine has booted, its volume pins
  it to that worker; if the worker is gone for more than 15 minutes the
  machine is re-placed with a fresh volume (`resumed: false`).
- Workers still only dial out. Command, file and port traffic rides a small
  pool of extra WebSockets each worker keeps open to `/v1/worker/data`, never
  the JSON control link.
- The heartbeat inventory lists machine VMs (`mch-…`) next to sessions
  (`ses-…`), and `/v1/fleet` reports drift for both.

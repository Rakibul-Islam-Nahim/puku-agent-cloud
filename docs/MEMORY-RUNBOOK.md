# Memory deployment runbook — for an agent working on the box

You are working on the production host `poridhi`, from `/root/agent-cloud`.
Read this whole file before running anything.

Your job: find why an agent session does not *read* the memory it is given, fix
it, and prove the fix. Everything else in the chain already works — do not
re-litigate it.

---

## Guardrails, read first

- **Never print, echo, or commit a secret.** `deploy/bm/.env` holds live keys.
  Refer to variables by name. When you must show a value, truncate it.
- **Never rotate a key.** `PUKU_SECRET_KEY` encrypts stored credentials;
  changing it silently drops every stored credential.
- **Do not touch the `puku-skills` stack** (`puku-skills-*` containers). It is a
  separate compose project and unrelated to this work.
- **Ask before rebuilding or restarting `controld`.** It is serving live
  sessions through a Cloudflare tunnel. Rebuilding `puku-workerd` is expected
  and fine; restarting it kills any in-flight session, so check first:
  `docker exec puku-cloud-postgres-1 psql -U puku -d puku_cloud -Atc
   "SELECT count(*) FROM sessions WHERE state IN ('running','booting')"`
- **Report what you find, do not paper over it.** If a step fails, say so with
  the output. A wrong "it works" here costs more than a missing fix.

---

## The box, as verified

Do not rediscover these.

| thing | value |
|---|---|
| compose project | `puku-cloud`, file `/root/agent-cloud/puku-agent-cloud/deploy/bm/docker-compose.yml` |
| memory service | container `puku-cloud-memory-1`, profile `memory` — every `docker compose` command needs `--profile memory` |
| controld | container `puku-cloud-controld-1`, `127.0.0.1:7770`, public via Cloudflare tunnel at `agent.api.puku.sh` |
| databases | both in `puku-cloud-postgres-1`, user `puku`: `puku_cloud` and `puku_memory` |
| workerd | systemd unit `puku-workerd`, binary `/opt/puku/bin/puku-workerd`, built 2026-09-02 17:42:49 |
| workerd state | `/var/lib/puku/sessions/<session-uuid>/session/` |
| agent image | `puku-agent-office:0.1.0` — confirmed to contain the runner fix |
| memory profile | `612870cf-f599-4c63-9364-7e84e264a1ed`, `scope=org` |
| repos | `puku-agent-cloud` and `puku-memory-service`, both on branch `memory-integration` |

Compose commands must run from `/root/agent-cloud/puku-agent-cloud/deploy/bm`.

---

## What already works — confirmed, do not re-test

1. A CLI session runs through `agent.api.puku.sh` and succeeds.
2. controld ingests the transcript with the caller's own model credential.
3. The memory service extracts real claims with a real model call.
4. Consolidation ran and wrote a digest (120 bytes).
5. **controld attaches the preamble to the session row: 582 bytes**, containing
   `Tests in this repository must be run with cargo nextest; cargo test is not
   used.`
6. The agent image's `runner.mjs` contains the `memory_preamble` handling
   (`grep -c memory_preamble /opt/puku/runner.mjs` → 2).
7. No `PUKU_RUNNER_CMD` override exists, so the default
   `exec node /opt/puku/runner.mjs` is what runs.

## The one thing that does not work

Asked *"Do not run any commands. From what you already know about this
repository, which test runner is used and why?"*, the agent answered:

> I don't actually have prior knowledge of this repository.

So the preamble exists on the host and never reaches the model. The break is
somewhere in: session row → `SessionSpec` → `GuestManifest` → `manifest.json` →
`runner.mjs` → puku-cli.

---

## Step 1 — locate the break

Session `179fbfac-f291-440c-be1b-1c6c1a747e49` is the one that answered "no
prior knowledge". Its directory is still on disk.

```bash
S=/var/lib/puku/sessions/179fbfac-f291-440c-be1b-1c6c1a747e49/session
sudo ls -la "$S"

# (a) what workerd handed the guest
sudo python3 -c "
import json,sys
d=json.load(open(sys.argv[1]))
p=d.get('memory_preamble')
print('memory_preamble:', (str(len(p))+' bytes') if p else 'ABSENT')
print('keys:', sorted(d.keys()))
" "$S/manifest.json"

# (b) what the runner said — it logs a line when it writes the file
sudo grep -iE "memory preamble|append-system-prompt" "$S/runner.stderr" || echo "NO RUNNER LINE"

# (c) did the file get written
sudo ls -l "$S/memory.md" 2>&1

# (d) can the deployed binary even emit the field
sudo strings /opt/puku/bin/puku-workerd | grep -c memory_preamble
```

### Decision table

| (a) manifest | (d) strings | diagnosis | go to |
|---|---|---|---|
| ABSENT | `0` | workerd binary predates the feature | **Fix A** |
| ABSENT | `≥1` | binary can emit it but did not — controld sent a spec without it | **Fix B** |
| bytes present, (b) NO RUNNER LINE | any | the runner did not take the branch | **Fix C** |
| bytes present, (b) line present, (c) file exists | any | it reached puku-cli and was ignored there | **Fix D** |

---

## Fix A — rebuild workerd

`memory_preamble` lives on `GuestManifest` in `crates/puku-cloud-proto`
(`session.rs:418`, copied at `:458`). A workerd built before the
`memory-integration` checkout cannot serialise it.

```bash
cd /root/agent-cloud/puku-agent-cloud
git status --short && git log --oneline -1     # confirm memory-integration
cargo build --release -p puku-workerd
sudo strings target/release/puku-workerd | grep -c memory_preamble   # must be >= 1
sudo install -m755 target/release/puku-workerd /opt/puku/bin/puku-workerd
sudo systemctl restart puku-workerd
systemctl is-active puku-workerd
```

Go to **Step 2**.

## Fix B — controld is not sending it

The binary can emit the field but the spec arrived without it. Check where the
spec is built:

- `crates/puku-controld/src/memory/mod.rs` around line 255: the preamble is
  copied onto the spec only when `session.memory_preamble.is_some()`.
- `crates/puku-cloud-proto/src/session.rs:458`: `GuestManifest::from` copies it.

Confirm the running controld image is from this branch:

```bash
docker exec puku-cloud-controld-1 sh -c 'strings /usr/local/bin/puku-controld 2>/dev/null | grep -c memory_preamble' || true
```

`0` means the controld image is stale and must be rebuilt — **ask the user
first**, it is serving live traffic.

## Fix C — the runner did not take the branch

The manifest has the preamble but `runner.mjs` did not act on it. Check the
runner that actually executed, inside the image workerd boots:

```bash
docker run --rm --entrypoint sh puku-agent-office:0.1.0 -c \
  "sed -n '595,610p' /opt/puku/runner.mjs"
```

It must write `${SESSION}/memory.md` and set
`options.extraArgs['append-system-prompt-file']`. Also read the whole
`runner.stderr` — a crash earlier in the file would skip everything after it:

```bash
sudo tail -50 "$S/runner.stderr"
```

## Fix D — puku-cli ignored it

`memory.md` exists and the flag was passed, so agent-cloud has done its job.
Verify the flag actually reached the process and report to the user; the fix is
in puku-cli, not this repo.

```bash
sudo cat "$S/.runner-cmd.sh" 2>/dev/null
sudo head -40 "$S/memory.md"
```

---

## Step 2 — prove it

Server-side checks first:

```bash
cd /root/agent-cloud/puku-agent-cloud/deploy/bm
docker compose --profile memory ps
docker exec puku-cloud-postgres-1 psql -U puku -d puku_memory -c \
  "SELECT state, origin, left(content,60) FROM memory_items;"
docker exec puku-cloud-postgres-1 psql -U puku -d puku_cloud -c \
  "SELECT left(id::text,8), coalesce(length(memory_preamble),0) AS bytes
     FROM sessions ORDER BY created_at DESC LIMIT 3;"
```

**The final proof needs the user.** You cannot run it: a session needs a
platform bearer, and the server only has `pkc_` keys, which carry no model
credential by design. Ask the user to run this from their laptop:

```
pukud cloud run --max-turns 4 \
  "Do not run any commands. From what you already know about this repository, which test runner is used and why?"
```

- Answer names **cargo nextest**, ideally with the reason (`cargo test` silently
  skips the integration tests) → fixed. Say so plainly.
- Answer is still "I don't have prior knowledge" → not fixed. Re-run Step 1 on
  the **new** session id and report which row of the decision table you land on.

Then confirm the new session got a preamble:

```bash
docker exec puku-cloud-postgres-1 psql -U puku -d puku_cloud -c \
  "SELECT left(id::text,8), coalesce(length(memory_preamble),0) AS bytes
     FROM sessions ORDER BY created_at DESC LIMIT 1;"
```

---

## Known issues — do not treat as bugs to fix here

- **`scope=org`, not `scope=repo`.** The CLI sends an empty `repo`, so one
  notebook is shared by the whole org and a convention learned in one repository
  is served to every other. The fix belongs in puku-cli. Report it; do not work
  around it here.
- **`model gateway not configured` in the memory service log.** Expected. It
  refers to the *operator* fallback, which is deliberately off. Per-tenant
  credentials arrive at ingest time and are unaffected.
- **`PUKU_LLM_OPERATOR_TOKEN` must stay unset.** The available `pk_live_` key
  belongs to `api-cli.puku.sh`; this deployment's gateway is
  `chat.api.puku.sh`, which rejects it. Setting it creates a fallback that 401s
  on every extraction and hides the working path.

## When you are done

Report, in this order: which decision-table row you landed on, what you changed,
the command output that proves it, and anything you could not verify. If the
final CLI test was not run, say the fix is unproven rather than implying it
works.

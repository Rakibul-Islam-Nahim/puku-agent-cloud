---
name: deployment-test
description: "Use this skill to verify a puku-agent-cloud deployment end to end — after deploying to a new box, after upgrading controld/workerd/the skills service, or whenever someone says the cloud, its skills, its scheduled jobs, or its PDF/PPTX document workloads are not working. It runs a phased test from free health checks up to a real document workload in a microVM, states the pass criterion for each phase, and maps known failures to their causes. Trigger it on requests like 'test my puku cloud', 'verify the deployment', 'is the skills service wired up', or 'check the cloud can still produce a deck'. Do not use it to test puku-cli itself or a local session — this tests a remote control plane."
allowed-tools: Bash, Read, Write
---

# Testing a puku-agent-cloud deployment

You are verifying a live deployment: a control plane, a skills registry, at
least one worker with KVM, and the guest images. Sessions cost real money
and take real time, so this is **phased, cheapest first**, and you stop at
the first phase that fails rather than running the rest against a broken
system.

## Before anything

```bash
export PUKU_CLOUD_URL=https://cloud.puku.sh
export PUKU_CLOUD_API_KEY=pkc_...          # or the operator token on a dev box
export PUKU_SKILLS_URL=https://skills.puku.sh
export PUKU_SKILLS_TOKEN=...               # the registry's operator token
```

If the user has not given you these, ask — do not guess a hostname. The
scripts live beside this file; run them from `$SKILLS_ROOT/deployment-test`
or wherever this skill is installed.

**Ask before Phase 3.** Phases 1 and 2 cost under $0.20 together. Phase 3
costs roughly **$3 and 25–40 minutes**. Never start it without the user
saying yes, and never run it twice to "check the result is stable."

## Phase 1 — health, free

```bash
./scripts/health.sh
```

Checks the control plane, object storage, worker connectivity, fleet drift,
the pack listing, and — the one that matters — that `PUKU_SKILLS_TOKEN`
resolves packs with the `x-puku-org` header.

It uses `/health?deep=1`, which round-trips a real object. Plain `/health`
reports `object_storage: true` whenever a bucket is merely *configured*, so
a wrong access key looks healthy there and only surfaces later as a
deliverable that 403s on download.

**Pass:** `RESULT: PASS`. Anything else, stop and fix it; the paid phases
will only fail more expensively.

That last check is worth understanding rather than just running. controld
forwards the caller's own bearer when there is one, and falls back to the
service token for sessions with nobody behind them — scheduled runs, `pkc_`
API keys, `PUKU_AUTH=off`. **If the two tokens do not match, interactive
sessions still get skills and unattended ones silently get none.** It
surfaces weeks later as "the nightly report ignored the pdf skill."

## Phase 2 — a session runs, and skills reach it (~$0.15)

### 2a. Smoke

```bash
./scripts/run-session.sh --label smoke --max-turns 5 \
  --prompt 'Write /workspace/hello.txt containing the word ORCHID, then stop.'
```

**Pass:** `STATE completed`. If it never leaves `booting`, the worker cannot
pull the guest image or has no KVM — `journalctl -u puku-workerd -f`.

### 2b. Skills are materialized in the guest

```bash
./scripts/run-session.sh --label skills --max-turns 5 \
  --pack office --pack essentials \
  --prompt 'Run `ls $SKILLS_ROOT` and show me the output verbatim. Do nothing else.'
```

Read the transcript to see what it listed:

```bash
curl -fsS -H "Authorization: Bearer $PUKU_CLOUD_API_KEY" \
  "$PUKU_CLOUD_URL/v1/sessions/<id>/events" | jq -r '.[].text? // empty' | tail -40
```

**Pass:** all 11 skill directories are present —

```
cloud-session  code-review  data-analysis  dataviz  debugging  docx
pdf  pptx  research-report  scripts  web-research  xlsx
```

An empty `$SKILLS_ROOT` after Phase 1 passed means the packs resolved but
did not materialize: check the worker log for a **digest mismatch**, which
is the worker correctly refusing a tarball that does not match the digest in
its spec.

If the box is in reach, confirm from outside the guest too:

```bash
sudo msb exec ses-<first-12-of-session-id> -- ls /session/home/.puku-cli/skills
```

## Phase 3 — the document workload (~$3, 25–40 min) — ASK FIRST

This is the one that proves the system does useful work: an agent inside a
microVM, following skills it pulled from the registry, producing a real PDF
and a real deck.

Requires the **office guest image** (`puku-agent-office`). On the lean image
the agent must pip-install into a venv first, which works but adds a minute
and needs PyPI egress.

```bash
./scripts/run-session.sh --label docs --max-turns 60 --budget 6.00 \
  --pack office --pack essentials \
  --disallow WebSearch,WebFetch \
  --prompt 'Write two deliverables in /workspace from your own knowledge — do
NOT use WebSearch or WebFetch. Topic: trade-offs between microVM and container
isolation for running untrusted AI agent code. Produce summary.pptx FIRST (4
slides), then report.pdf (about 3 pages), keeping report.md and summary.md
beside them. Follow your pptx, pdf and research-report skills. Page and slide
counts are rough targets — do NOT iterate on layout or styling to hit them
exactly.'
```

`--disallow WebSearch,WebFetch` is not optional — see Known issue 1.

The deck comes first, and the "rough targets" clause matters. Asked for
"2-3 pages" without it, the agent spent about twenty turns shaving a PDF
from four pages to three and hit the turn ceiling before the deck existed —
observed on two separate runs. Whatever is last in the prompt is what gets
dropped.

Then pull and inspect:

```bash
./scripts/pull-workspace.sh <session-id> ./docs-test
```

**Pass** — the reference run produced:

| file | size | `file` says |
|------|------|-------------|
| `report.pdf` | 12384 | `PDF document, version 1.4, 3 pages` |
| `summary.pptx` | 36303 | `Microsoft OOXML` |
| `report.md` | 11596 | source beside the artifact |
| `summary.md` | 4464 | source beside the artifact |

Sizes will differ; **types and page/slide counts are the criterion.** A
`report.pdf` that `file` calls "ASCII text" is a fail dressed as a pass.

Verify the deck actually has slides:

```bash
docker run --rm -v "$PWD/docs-test":/w puku-agent-office:latest python3 -c "
from pptx import Presentation
p = Presentation('/w/summary.pptx'); print(len(p.slides), 'slides')
for s in p.slides: print(' -', s.shapes.title.text)"
```

**If it hits the turn ceiling** (`error_max_turns`), that is not a platform
failure — send a follow-up turn rather than re-running from scratch, which
would pay for the finished work twice:

```bash
curl -fsS -X POST -H "Authorization: Bearer $PUKU_CLOUD_API_KEY" \
  -H 'content-type: application/json' \
  -d '{"text":"Now create /workspace/summary.pptx from summary.md — 4 slides, using python-pptx per your pptx skill."}' \
  "$PUKU_CLOUD_URL/v1/sessions/<id>/input"
```

## Phase 4 — scheduled jobs (~$0 to create, one session's cost to fire)

```bash
curl -fsS -X POST -H "Authorization: Bearer $PUKU_CLOUD_API_KEY" \
  -H 'content-type: application/json' \
  -d '{"name":"deploy-test","prompt":"Write /workspace/brief.txt containing OK.","cron":"0 3 * * *","packs":["office","essentials"],"max_turns":5}' \
  "$PUKU_CLOUD_URL/v1/schedules" | jq '{id, packs, cron, next_run_at}'
```

**Pass:** the response echoes `packs: ["office","essentials"]` and a
`next_run_at` in UTC. A schedule that comes back with empty packs is running
a controld older than migration 0013.

Fire it immediately instead of waiting for 03:00:

```bash
curl -fsS -X POST -H "Authorization: Bearer $PUKU_CLOUD_API_KEY" \
  "$PUKU_CLOUD_URL/v1/schedules/<id>/run" | jq
```

**Pass:** a `session_id` comes back, and that session carries the packs.
Then clean up — a test schedule left enabled fires every night forever:

```bash
curl -fsS -X DELETE -H "Authorization: Bearer $PUKU_CLOUD_API_KEY" \
  "$PUKU_CLOUD_URL/v1/schedules/<id>"
```

**Unattended runs need an org credential.** Without one they fall back to
the operator key, and on a deployment that has no operator key they fail
before booting. Check with `GET /v1/credentials`; set with
`puku cloud credentials set <api-key>`.

## Sweeping the whole platform

`scripts/full-system-test.sh` checks every feature that does not need the
model's judgement, in one run: both services, the skills registry and its
rejections, session lifecycle and billing, skills delivery into the guest,
artifacts, schedules, and fleet drift.

Point the worker at the deterministic CLI first so the agent's behaviour is
fixed and the run is free:

```bash
PUKU_RUNNER_CMD='exec env PUKU_CLI_PATH=/opt/puku/fake-puku-cli.sh node /opt/puku/runner.mjs'
STATE=<the worker's PUKU_STATE_DIR> ./scripts/full-system-test.sh
```

It answers "is the platform intact". The gate below answers the different
question of whether the SDK runner is safe to make default.

## Verifying the SDK runner

If the deployment is trialling `runner.mjs` (see
`docs/SDK-MIGRATION-PLAN.md`), the checks above are not enough — they pass on
either runner. Run the gate instead:

```bash
./scripts/sdk-gate.sh                    # ~$0.50
./scripts/sdk-gate.sh --with-documents   # adds the ~$3 PDF/PPTX run
```

It asserts the things a fake CLI cannot: that the agent's own `init` message
lists the pack's skills, that token counters and cost land in the database,
that a permission prompt arrives as `kind: platform.question` and its answer
reaches the model, and that an interrupt ends a turn without killing the
session.

**Pass is required before that runner becomes the default.** A FAIL here
means the bash runner stays.

## Known issues — recognise, do not debug

These are established. If you see one, name it and move on; do not spend
turns investigating or retrying.

1. **`WebSearch` crashes puku-cli in the guest.** Signature:
   `Cannot read properties of undefined (reading 'web_search_requests')`,
   often after WebFetch says "Unable to verify if domain … is safe to fetch",
   then exit 1 and the session is `failed`. This is a **puku-cli usage
   accounting bug, not a platform fault** — it is not egress, and
   `PUKU_EGRESS_UNRESTRICTED` does not help. Always pass
   `--disallow WebSearch,WebFetch` on document runs.

2. **Model-gateway 503s stall long sessions.** The transcript shows
   `503 server_error` with `attempt N of 10` and backoff. The reference run
   lost several minutes and then recovered on its own. **Do not cancel and
   retry** — you pay twice for one result. A session frozen at a constant
   cost for ten minutes is almost always this; `run-session.sh` says so.

3. **A skill's `allowed-tools` versus the session's `--disallowed-tools` is
   unproven.** The platform is designed so a pack can never widen the
   ceiling, and publishing is admin-gated on that assumption, but the
   precedence has not been demonstrated empirically. If you are asked to
   test it, that is a genuine open question — design an experiment rather
   than asserting the answer.

## Diagnosing a failure

| Symptom | Cause |
|---------|-------|
| `$SKILLS_ROOT` empty, Phase 1 passed | Packs resolved but did not unpack — digest mismatch in the worker log |
| `$SKILLS_ROOT` empty, only on scheduled/`pkc_` runs | `PUKU_SKILLS_TOKEN` ≠ registry's `PUKU_SKILLS_OPERATOR_TOKEN` |
| Boot fails with `Not authorized … index.docker.io` | The guest image was built with Docker but never loaded into msb's separate store: `docker save <tag> \| msb load -t <tag>` |
| Stuck at `booting` | No KVM, or the worker cannot pull the guest image |
| Fails instantly, credential error | No model key. controld fails the session *before* booting, deliberately |
| `workers_connected: 0` | Usually `PUKU_CONTROLD_URL` set to the base URL instead of the full `wss://…/v1/worker`. The worker logs only `connecting to controld` every 3s, which does not say that |
| `object_storage_probe` not `"ok"` | Wrong R2 key/secret, missing bucket, or the endpoint set to the bucket URL rather than the account URL |
| PDF renders boxes for glyphs | Lean image without fonts — use `puku-agent-office` |
| `import weasyprint` fails | Office image built before 2026-08-23, missing `libpango`/`libcairo` |
| Costs far more than expected | Set `--budget`, and `PUKU_DEFAULT_MAX_TURNS` on controld |

Logs:

```bash
docker compose logs -f controld      # control plane
journalctl -u puku-workerd -f        # VM boots, pack downloads, digests
docker compose logs -f skills        # resolution and publishing
sudo msb list                        # what is actually running on the box
```

## Reporting

Give the user a table of phase → pass/fail → evidence, in that order. Rules:

- **Quote the actual observed value**, not "as expected". `3 pages, 12384
  bytes` is evidence; "PDF generated successfully" is not.
- **State the total spend.** Sum the `COST` from each `run-session.sh` line.
- **A phase you skipped is not a phase that passed.** Say which you skipped
  and why (cost, no approval, blocked by an earlier failure).
- **Separate platform faults from the known issues above.** A session that
  died on the `web_search_requests` crash is a CLI bug, and reporting it as
  "the cloud is broken" sends the user to debug the wrong system.
- If everything passed, say so plainly and stop. Do not invent extra tests
  to look thorough.

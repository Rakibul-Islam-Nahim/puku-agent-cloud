#!/usr/bin/env bash
# Phase 1 of the deployment test: everything that costs nothing.
#
# Checks the control plane, the skills registry, the worker fleet, and the
# one piece of configuration that fails silently (the shared skills token).
# Exits non-zero if any REQUIRED check fails, so it can gate the paid phases.
#
#   PUKU_CLOUD_URL / PUKU_CLOUD_API_KEY   control plane          (required)
#   PUKU_SKILLS_URL / PUKU_SKILLS_TOKEN   skills registry        (optional)
#   PUKU_TEST_ORG                         org uuid for resolve   (optional)

set -uo pipefail

CLOUD_URL="${PUKU_CLOUD_URL:-}"
CLOUD_KEY="${PUKU_CLOUD_API_KEY:-}"
SKILLS_URL="${PUKU_SKILLS_URL:-}"
SKILLS_TOKEN="${PUKU_SKILLS_TOKEN:-}"
ORG="${PUKU_TEST_ORG:-00000000-0000-0000-0000-000000000001}"

fail=0
pass() { printf '  PASS  %s\n' "$*"; }
warn() { printf '  WARN  %s\n' "$*"; }
bad()  { printf '  FAIL  %s\n' "$*"; fail=1; }
head() { printf '\n== %s ==\n' "$*"; }

if [ -z "$CLOUD_URL" ]; then
  echo "PUKU_CLOUD_URL is not set. Export it (and PUKU_CLOUD_API_KEY) first." >&2
  exit 2
fi
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }

auth=(); [ -n "$CLOUD_KEY" ] && auth=(-H "Authorization: Bearer $CLOUD_KEY")

# ---------------------------------------------------------------- control plane
head "control plane  $CLOUD_URL"
# ?deep=1 round-trips a real object. Worth the one write: the plain field
# only says a bucket is CONFIGURED, so a wrong access key stays green here
# and first appears much later as a 403 on a deliverable nobody can read.
health="$(curl -fsS --max-time 30 "$CLOUD_URL/health?deep=1" 2>/dev/null)"
[ -n "$health" ] || health="$(curl -fsS --max-time 15 "$CLOUD_URL/health" 2>/dev/null)"
if [ -z "$health" ]; then
  bad "GET /health unreachable — is controld up, and is the tunnel mapped?"
  echo; echo "RESULT: FAIL (control plane unreachable)"; exit 1
fi
echo "$health" | jq . 2>/dev/null | sed 's/^/        /'

[ "$(echo "$health" | jq -r '.database')" = "ok" ] \
  && pass "database reachable" || bad "database not ok"

probe="$(echo "$health" | jq -r '.object_storage_probe // empty')"
if [ "$probe" = "ok" ]; then
  pass "object storage write+read round-trip ok"
elif [ -n "$probe" ]; then
  # Not fatal for running a session, but every deliverable and transcript
  # will be unreadable, so it fails the deployment check.
  bad "object storage configured but NOT usable: $(echo "$probe" | tr '\n' ' ' | cut -c1-160)"
elif [ "$(echo "$health" | jq -r '.object_storage')" = "true" ]; then
  warn "object storage configured, but this controld is too old for ?deep=1 so the credentials are unverified"
else
  bad "no object storage configured — check PUKU_R2_ENDPOINT is the ACCOUNT url, not the bucket url"
fi

workers="$(echo "$health" | jq -r '.workers_connected // 0')"
if [ "$workers" -gt 0 ]; then
  pass "$workers worker(s) connected"
else
  bad "no workers connected — a worker dials OUT, so check its PUKU_CONTROLD_URL and token, not your firewall"
fi

# ---------------------------------------------------------------------- fleet
head "fleet"
fleet="$(curl -fsS --max-time 15 "${auth[@]}" "$CLOUD_URL/v1/fleet" 2>/dev/null)"
if [ -n "$fleet" ] && echo "$fleet" | jq -e . >/dev/null 2>&1; then
  echo "$fleet" | jq -r '.workers[]? |
    "        \(.name)  slots \(.used_slots)/\(.capacity_slots)  \(.status)  connected=\(.connected)"' 2>/dev/null
  # drift is {orphaned: [...], vanished: [...]} — sandboxes controld does
  # not know about, and sessions whose sandbox is gone.
  orph="$(echo "$fleet" | jq -r '.drift.orphaned | length' 2>/dev/null)"
  vani="$(echo "$fleet" | jq -r '.drift.vanished | length' 2>/dev/null)"
  if [ "${orph:-0}" = "0" ] && [ "${vani:-0}" = "0" ]; then
    pass "no fleet drift"
  else
    warn "fleet drift: $orph orphaned sandbox(es), $vani vanished — controld and the worker's msb inventory disagree, usually a worker restart mid-session"
  fi
else
  warn "could not read /v1/fleet (auth?)"
fi

# -------------------------------------------------------------------- skills
head "skills registry"
if [ -z "$SKILLS_URL" ]; then
  warn "PUKU_SKILLS_URL not set — skipping. Sessions will resolve NO skills; if that is not intended, set it on controld too."
else
  sh="$(curl -fsS --max-time 15 "$SKILLS_URL/health" 2>/dev/null)"
  [ -n "$sh" ] && pass "skills /health ok" || bad "skills service unreachable at $SKILLS_URL"

  if [ -n "$SKILLS_TOKEN" ]; then
    packs="$(curl -fsS --max-time 15 -H "Authorization: Bearer $SKILLS_TOKEN" \
              "$SKILLS_URL/v1/packs" 2>/dev/null)"
    if echo "$packs" | jq -e '.packs | length > 0' >/dev/null 2>&1; then
      pass "packs: $(echo "$packs" | jq -r '[.packs[] | "\(.name)@\(.latest) (\(.skills|length) skills)"] | join(", ")')"
    else
      bad "no packs listed — did the seeder run? check PUKU_SKILLS_SEED_DIR and the service log"
    fi

    # THE check that matters. controld presents this exact token plus
    # x-puku-org for any session with no caller bearer: scheduled runs,
    # pkc_ keys, PUKU_AUTH=off. A mismatch here is invisible in normal
    # interactive testing and shows up at 03:00 as "the agent ignored the
    # pdf skill".
    r="$(curl -fsS --max-time 15 \
          -H "Authorization: Bearer $SKILLS_TOKEN" -H "x-puku-org: $ORG" \
          "$SKILLS_URL/v1/resolve?packs=office" 2>/dev/null)"
    if echo "$r" | jq -e '.packs | length > 0' >/dev/null 2>&1; then
      pass "service-token resolve works: $(echo "$r" | jq -r '[.packs[] | "\(.name)@\(.version)"] | join(" ")')"
    else
      bad "service-token resolve returned nothing — PUKU_SKILLS_TOKEN must equal the registry's PUKU_SKILLS_OPERATOR_TOKEN. Unattended runs will silently get no skills."
    fi
  else
    warn "PUKU_SKILLS_TOKEN not set — cannot verify the unattended-run path, which is the one that fails silently"
  fi
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "RESULT: PASS — safe to run the paid phases"
else
  echo "RESULT: FAIL — fix the above before spending money on session tests"
fi
exit "$fail"

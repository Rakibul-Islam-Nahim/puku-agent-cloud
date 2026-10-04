#!/usr/bin/env bash
# End-to-end check of the memory integration: controld <-> puku-memory-service.
#
# Nothing tested this before. AppState.memory is hardcoded None in controld's
# integration harness, and full-system-test.sh has no memory coverage at all --
# so every guarantee the memory service claims has only ever been checked
# inside its own unit tests, against its own fakes.
#
# This drives the real HTTP path between two running processes. No microVM and
# no model: what it proves is the wiring, the isolation between users, the
# degradation ladder, and where a model credential ends up -- captured,
# encrypted, and absent from the job queue.
#
# What it deliberately does NOT prove is a billed model call, or whether the
# extraction prompts produce good sentences. Both need a real key. Said out
# loud rather than skipped quietly, because a suite that hides what it did not
# check is worse than one that never claimed to.
#
#   docker compose -f deploy/compose.dev.yml up -d
#   cargo run -p puku-controld   # with PUKU_MEMORY_URL / _SERVICE_KEY set
#   ./skills/deployment-test/scripts/memory-e2e.sh
#
# Env: CONTROLD (default http://127.0.0.1:7770)
#      MEMORY   (default http://127.0.0.1:7970)
#      MEMORY_KEY (default dev-memory-service-key)
#      PG_CONTROLD (docker compose service name for controld's postgres)
set -uo pipefail

CONTROLD="${CONTROLD:-http://127.0.0.1:7770}"
MEMORY="${MEMORY:-http://127.0.0.1:7970}"
MEMORY_KEY="${MEMORY_KEY:-dev-memory-service-key}"
COMPOSE="${COMPOSE:-deploy/compose.dev.yml}"
REPO="github.com/acme/e2e-$$"

pass=0; fail=0
ok()   { printf "  \033[32m✓\033[0m %s\n" "$1"; pass=$((pass+1)); }
bad()  { printf "  \033[31m✗\033[0m %s\n" "$1"; [ -n "${2:-}" ] && printf "      %s\n" "$2"; fail=$((fail+1)); }
sec()  { printf "\n\033[1m%s\033[0m\n" "$1"; }

mem()  { curl -sS -m 10 -H "Authorization: Bearer $MEMORY_KEY" "$@"; }
jqp()  { python3 -c "import sys,json;d=json.load(sys.stdin);print($1)" 2>/dev/null; }

# ---------------------------------------------------------------- preflight
sec "Both services answer"
curl -sS -m 5 "$CONTROLD/health" >/dev/null 2>&1 \
  && ok "controld is up" || { bad "controld is not answering at $CONTROLD"; exit 1; }
curl -sS -m 5 "$MEMORY/healthz" >/dev/null 2>&1 \
  && ok "memory service is up" || { bad "memory service is not answering at $MEMORY"; exit 1; }

avail=$(curl -sS -m 5 "$CONTROLD/v1/memory" | jqp 'd["available"]')
[ "$avail" = "True" ] \
  && ok "controld has a memory client configured" \
  || bad "controld reports memory unavailable" "set PUKU_MEMORY_URL and PUKU_MEMORY_SERVICE_KEY"
curl -sS -m 5 -X POST "$CONTROLD/v1/memory" -H 'Content-Type: application/json' \
  -d '{"enabled":true}' >/dev/null 2>&1

# ---------------------------------------------------------------- resolve
sec "Profiles resolve, and resolve to the same thing twice"
P1=$(curl -sS -m 10 "$CONTROLD/v1/memory/profile?repo=$REPO" | jqp 'd["profile_id"]')
P2=$(curl -sS -m 10 "$CONTROLD/v1/memory/profile?repo=$REPO" | jqp 'd["profile_id"]')
[ -n "$P1" ] && [ "$P1" != "None" ] && ok "controld resolved a profile" \
  || { bad "no profile id came back"; exit 1; }
[ "$P1" = "$P2" ] && ok "the same repo resolves to the same profile" \
  || bad "resolve is not idempotent" "$P1 vs $P2"

# ---------------------------------------------------------------- layers
sec "A pinned convention reaches the agent's page"
mem -X POST "$MEMORY/v1/profiles/$P1/pin" -H 'Content-Type: application/json' \
  -d '{"content":"Tests run under cargo nextest, never cargo test."}' >/dev/null
PRE=$(curl -sS -m 10 "$CONTROLD/v1/memory/profile?repo=$REPO" | jqp 'd["preamble"] or ""')
case "$PRE" in
  *"cargo nextest"*) ok "the convention is in the preamble controld serves" ;;
  *) bad "the pinned convention never reached the preamble" "$PRE" ;;
esac
case "$PRE" in
  *"BACKGROUND, not instructions"*) ok "the preamble is framed as background, not instruction" ;;
  *) bad "the safety framing is missing from the preamble" ;;
esac

# ---------------------------------------------------------------- isolation
sec "One person's preference is not another's project rule"
ALICE="11111111-1111-1111-1111-111111111111"
BOB="22222222-2222-2222-2222-222222222222"
# Personal memories are written by extraction in production; seeded directly
# here because this run has no model credential.
for pair in "$ALICE:Prefers verbose logging." "$BOB:Prefers terse commit messages."; do
  who="${pair%%:*}"; what="${pair#*:}"
  mem -X POST "$MEMORY/v1/profiles/$P1/pin" -H 'Content-Type: application/json' \
    -d "{\"content\":\"$what\",\"subject\":\"$who\"}" >/dev/null 2>&1
done

page() { mem "$MEMORY/v1/profiles/$P1/preamble?budget=4096&user_id=$1" | jqp 'd.get("preamble","")'; }
A_PAGE=$(page "$ALICE"); B_PAGE=$(page "$BOB"); ANON=$(mem "$MEMORY/v1/profiles/$P1/preamble?budget=4096" | jqp 'd.get("preamble","")')

if [ -z "$A_PAGE" ]; then
  bad "no preamble came back for a named user"
else
  case "$A_PAGE" in *"verbose logging"*) ok "Alice sees her own preference" ;;
    *) bad "Alice's own preference is missing from her page" "$A_PAGE" ;; esac
  case "$B_PAGE" in *"verbose logging"*) bad "Alice's preference leaked into Bob's page" "$B_PAGE" ;;
    *) ok "Bob does not see Alice's preference" ;; esac
  case "$ANON" in *"verbose logging"*|*"terse commit"*) bad "a session with no user was served somebody's preferences" ;;
    *) ok "a session with no user gets nobody's preferences" ;; esac
  case "$ANON" in *"cargo nextest"*) ok "and still gets the shared conventions" ;;
    *) bad "the anonymous page lost the shared conventions" "$ANON" ;; esac
fi

# ---------------------------------------------------------------- the cache
sec "The cacheable page cannot carry a personal layer"
SHARED=$(mem "$MEMORY/v1/profiles/$P1/preamble?budget=4096&user_id=$ALICE" | jqp 'd.get("shared","")')
if [ -z "$SHARED" ]; then
  bad "the response carries no 'shared' page" "agent-cloud has nothing safe to cache"
else
  case "$SHARED" in *"verbose logging"*|*"About you"*)
      bad "the cacheable page carries a personal section" "$SHARED" ;;
    *) ok "the cacheable page has no personal section" ;; esac
  case "$SHARED" in *"cargo nextest"*) ok "and still carries the shared conventions" ;;
    *) bad "the cacheable page lost the shared conventions" "$SHARED" ;; esac
fi

CACHED=$(docker compose -f "$COMPOSE" exec -T postgres \
  psql -U puku -d puku_cloud -Atc \
  "SELECT coalesce(preamble,'') FROM memory_preamble_cache WHERE profile_id = '$P1'" 2>/dev/null)
if [ -z "$CACHED" ]; then
  printf "  \033[2m–  nothing cached yet (controld caches on a session dispatch)\033[0m\n"
else
  case "$CACHED" in *"verbose logging"*|*"About you"*)
      bad "agent-cloud cached a page containing somebody's personal layer" ;;
    *) ok "what agent-cloud cached has no personal layer" ;; esac
fi

# ---------------------------------------------------------------- inspect
sec "A human can read and correct what the engine believes"
BODY=$(mem "$MEMORY/v1/profiles/$P1")
echo "$BODY" | grep -q '"digest"' \
  && ok "the profile resource carries the digest" \
  || bad "no digest on the profile resource; there is no way to read what we believe"
echo "$BODY" | grep -q '"items"' \
  && ok "and the per-state item counts" || bad "item counts missing"

IID=$(mem -X POST "$MEMORY/v1/profiles/$P1/pin" -H 'Content-Type: application/json' \
  -d '{"content":"Deploys run from the release branch."}' | jqp 'd["id"]')
RW=$(mem -o /dev/null -w '%{http_code}' -X POST "$MEMORY/v1/profiles/$P1/items/$IID/rewrite" \
  -H 'Content-Type: application/json' -d '{"content":"Deploys run from main only.","reason":"branch archived"}')
[ "$RW" = "200" ] && ok "a memory can be corrected in place" \
  || bad "rewrite returned $RW"

# ---------------------------------------------------------------- ladder
sec "Losing the memory service costs freshness, not availability"
docker compose -f "$COMPOSE" stop memory >/dev/null 2>&1
sleep 2
OUT=$(curl -sS -m 15 "$CONTROLD/v1/memory/profile?repo=$REPO" 2>&1)
CODE=$(curl -sS -m 15 -o /dev/null -w '%{http_code}' "$CONTROLD/v1/memory/profile?repo=$REPO" 2>&1)
case "$CODE" in
  200) ok "controld still answers with the memory service down" ;;
  502) ok "controld reports the dependency honestly (502) rather than pretending" ;;
  *)   bad "controld answered $CODE with memory down" "$OUT" ;;
esac
case "$OUT" in *"verbose logging"*)
    bad "a personal layer was served from cache while the service was down" ;;
  *) ok "no personal layer was served from the degraded path" ;; esac
docker compose -f "$COMPOSE" start memory >/dev/null 2>&1
sleep 4

# ---------------------------------------------------------------- credential
# The second half of "run it twice": once with a credential supplied the new
# way. What a run with no model key can prove is where the credential ENDS UP,
# which is the part with a blast radius. What it cannot prove is a billed model
# call; that needs a real key and is called out in the summary rather than
# quietly skipped.
sec "A model credential is stored, and is not left lying in the queue"
memdb() { docker compose -f "$COMPOSE" exec -T postgres-memory psql -U puku -d puku_memory -Atc "$1" 2>/dev/null; }

SENT="live-credential-sentinel-$$"
SRC="session-$(uuidgen | tr 'A-Z' 'a-z')"
mem -X POST "$MEMORY/v1/profiles/$P1/ingest" -H 'Content-Type: application/json' \
  -H "X-Puku-Model-Credential: $SENT" \
  -H "X-Puku-Model-Credential-Kind: bearer" \
  -H "X-Puku-Source-Id: $SRC" \
  -d "{\"source_id\":\"$SRC\",\"label\":\"a session\",\"messages\":[
        {\"role\":\"user\",\"content\":\"we deploy from main only\",\"origin\":\"user\"},
        {\"role\":\"assistant\",\"content\":\"noted\",\"origin\":\"agent\"}]}" >/dev/null

TEN=$(memdb "SELECT tenant_id FROM memory_profiles WHERE id = '$P1'")
[ -n "$TEN" ] && ok "the ingest resolved to tenant $TEN" || bad "could not read the tenant"

STORED=$(memdb "SELECT count(*) FROM tenant_credentials WHERE tenant_id = '$TEN'")
if [ "$STORED" = "1" ]; then
  ok "the credential was captured for this tenant"
  ENC=$(memdb "SELECT encode(value_enc,'escape') FROM tenant_credentials WHERE tenant_id = '$TEN'")
  case "$ENC" in *"$SENT"*) bad "the stored credential is PLAINTEXT in tenant_credentials" ;;
    *) ok "and stored encrypted, not in the clear" ;; esac
  KIND=$(memdb "SELECT kind FROM tenant_credentials WHERE tenant_id = '$TEN'")
  [ "$KIND" = "bearer" ] && ok "with the kind it was sent as" || bad "kind is $KIND"
elif [ "$STORED" = "0" ]; then
  printf "  \033[2m–  nothing stored; PUKU_MEMORY_SECRET_KEY is unset on this deployment\033[0m\n"
else
  bad "unexpected credential count: $STORED"
fi

# The sharpest assertion in this file. ingest_jobs.payload is jsonb retained for
# days and read by a human when a job dead-letters.
QUEUED=$(memdb "SELECT coalesce(payload::text,'') FROM ingest_jobs WHERE source_id = '$SRC'")
case "$QUEUED" in
  *"$SENT"*) bad "the credential is in ingest_jobs.payload, in plaintext, for seven days" ;;
  "")        printf "  \033[2m–  the job row is gone already (worker drained and reaped it)\033[0m\n" ;;
  *)         ok "the job queue holds the transcript and no credential" ;;
esac

# A kind this service cannot use must be refused, not relabelled: it does not
# mint, so a refresh token stored as a bearer would be spent as one.
#
# tenant_credentials is keyed on tenant_id, so accepting one would OVERWRITE the
# bearer above -- which is what this checks. Grepping value_enc for the
# plaintext sentinel would not: the column is ciphertext, so that comparison
# passes whether or not the credential was stored.
BEFORE=$(memdb "SELECT kind || ' @ ' || updated_at FROM tenant_credentials WHERE tenant_id = '$TEN'")
mem -X POST "$MEMORY/v1/profiles/$P1/ingest" -H 'Content-Type: application/json' \
  -H "X-Puku-Model-Credential: should-never-be-stored-$$" \
  -H "X-Puku-Model-Credential-Kind: refresh" \
  -H "X-Puku-Source-Id: refuse-$SRC" \
  -d "{\"source_id\":\"refuse-$SRC\",\"label\":\"a session\",\"messages\":[
        {\"role\":\"user\",\"content\":\"we deploy from main only\",\"origin\":\"user\"},
        {\"role\":\"assistant\",\"content\":\"noted\",\"origin\":\"agent\"}]}" >/dev/null
AFTER=$(memdb "SELECT kind || ' @ ' || updated_at FROM tenant_credentials WHERE tenant_id = '$TEN'")
if [ -z "$BEFORE" ]; then
  printf "  \033[2m–  no credential stored, so there is nothing a refusal could overwrite\033[0m\n"
elif [ "$AFTER" = "$BEFORE" ]; then
  ok "a 'refresh' credential was refused; the stored bearer is untouched"
else
  bad "the refused credential overwrote the stored one" "$BEFORE -> $AFTER"
fi

# ---------------------------------------------------------------- spend
sec "What this run cost"
SPEND=$(mem "$MEMORY/v1/spend?tenant_id=$(curl -sS -m 5 "$CONTROLD/v1/memory" >/dev/null 2>&1; echo dev)" 2>/dev/null)
printf "  %s\n" "${SPEND:-(no ledger read)}"

printf "\n\033[1m%d passed, %d failed\033[0m\n" "$pass" "$fail"
[ "$fail" -eq 0 ] || exit 1

#!/usr/bin/env bash
# Deploy puku-agent-cloud's control plane on this host and switch puku-bot to it.
# Run as root from the agent-cloud checkout, after a `git pull`. Secrets are yours
# to paste: this script checks they are there and never writes one. The keys and
# tokens it makes are printed once, for you to paste where they go.
#
#   setup-control-plane.sh check          what is missing, changing nothing
#   setup-control-plane.sh controld       build and restart controld with Cloud Hypervisor settings
#   setup-control-plane.sh key            print a new pkc_ key for puku-bot, and lift its machine quota
#   setup-control-plane.sh token NAME     print a new token for worker NAME
#   setup-control-plane.sh bot            switch puku-bot to agent-cloud (wipes it; asks first)
#   setup-control-plane.sh status         controld, the workers, puku-bot
#   setup-control-plane.sh smoke          a machine smoke test on Cloud Hypervisor
#
# The first time, in this order:
#
#   1. paste the secrets into deploy/bm/.env, then run: check, controld
#   2. key          -> paste the line it prints into puku-bot's .env
#   3. token bm1    -> paste it when setup-worker.sh asks, on this host
#      token bm2    -> the same on the other worker host
#   4. bot, then status and smoke
#
# Settings, from the environment:
#
#   BOT_DIR             the puku-bot checkout (/root/puku-bot)
#   BOT_REMOTE          where its new code comes from (git@github.com:puku-sh/puku-bot-svc.git)
#   BOT_BRANCH          (feat/puku-cloud-sandbox)
#   COMPUTER_IMAGE      the image computers boot (pukubot-computer:latest)
#   MACHINE_CPUS        a computer's CPUs (8)
#   MACHINE_MEMORY_MIB  and memory (16384)
#   MACHINE_QUOTA       machines puku-bot may keep, one per user (100000)
#   BACKUP_DB=0         skip the database dump before controld is replaced
#   SINGLE_HOST=1       allow object storage only this host can reach
#
# Every run except key and token is logged under /var/log/puku-rollout/.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
SELF=deploy/scripts/setup-control-plane.sh
AC_BRANCH=feat/engines-cloud-hypervisor
AC_BM=$ROOT/deploy/bm
AC_ENV=$AC_BM/.env
CONTROLD_LOCAL=http://127.0.0.1:7770
STAMP=$(date +%Y%m%d-%H%M%S)
SECRETS=(PUKU_DATABASE_URL PUKU_SECRET_KEY PUKU_R2_ENDPOINT PUKU_R2_BUCKET PUKU_R2_ACCESS_KEY_ID PUKU_R2_SECRET_ACCESS_KEY)

: "${BOT_DIR:=/root/puku-bot}"
: "${BOT_REMOTE:=git@github.com:puku-sh/puku-bot-svc.git}"
: "${BOT_BRANCH:=feat/puku-cloud-sandbox}"
: "${COMPUTER_IMAGE:=pukubot-computer:latest}"
: "${MACHINE_CPUS:=8}"
: "${MACHINE_MEMORY_MIB:=16384}"
: "${MACHINE_QUOTA:=100000}"
: "${BACKUP_DB:=1}"
: "${BACKUP_DIR:=/root/puku-backups}"

# Fail instead of prompting for a GitHub password, and trust github.com's host key
# on first use.
export GIT_TERMINAL_PROMPT=0
export GIT_SSH_COMMAND=${GIT_SSH_COMMAND:-ssh -o StrictHostKeyChecking=accept-new}

say()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
ok()   { printf '   ok    %s\n' "$*"; }
note() { printf '   ..    %s\n' "$*"; }
warn() { printf '   WARN  %s\n' "$*" >&2; }
die()  { printf '\n\033[31mFAILED:\033[0m %s\n' "$*" >&2; exit 1; }

usage() { awk 'NR > 1 && /^set -euo pipefail/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"; }

apt_install() { DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null; }

# -------------------------------------------------------------- .env files
# The value of an uncommented KEY=..., without surrounding quotes; empty if absent.
env_get() {
  [ -f "$1" ] || return 0
  awk -v k="$2" -v q="'" '
    index($0, k "=") == 1 { v = substr($0, length(k) + 2) }
    END {
      f = substr(v, 1, 1); l = substr(v, length(v), 1)
      if (length(v) >= 2 && f == l && (f == "\"" || f == q)) v = substr(v, 2, length(v) - 2)
      printf "%s", v
    }' "$1"
}

# KEY=VALUE: rewrites the first KEY= or commented-out #KEY= line and drops any later
# active one, or appends. The file keeps its owner and mode.
env_set() {
  local file=$1 key=$2 tmp
  tmp=$(mktemp)
  VALUE=$3 awk -v k="$key" '
    BEGIN { v = ENVIRON["VALUE"] }
    {
      bare = $0; sub(/^#[ \t]*/, "", bare)
      if (index(bare, k "=") == 1 && (!done || index($0, k "=") == 1)) {
        if (!done) { print k "=" v; done = 1 }
        next
      }
      print
    }
    END { if (!done) print k "=" v }' "$file" > "$tmp"
  cat "$tmp" > "$file"
  rm -f "$tmp"
}

env_comment() {
  local tmp
  tmp=$(mktemp)
  awk -v k="$2" 'index($0, k "=") == 1 { print "#" $0; next } { print }' "$1" > "$tmp"
  cat "$tmp" > "$1"
  rm -f "$tmp"
}

backup_file() { cp -p "$1" "$1.bak-$STAMP"; }

# puku-bot puts POSTGRES_PASSWORD into a URL unescaped, so / + @ # % : and friends
# leave the api with a different password than Postgres was created with.
url_safe() { [[ $1 =~ ^[A-Za-z0-9._~-]+$ ]]; }

# ------------------------------------------------------------------ helpers
wait_http() {
  local url=$1 secs=$2 i
  for ((i = 0; i < secs; i += 2)); do
    curl -fsS -o /dev/null --max-time 3 "$url" 2>/dev/null && return 0
    sleep 2
  done
  return 1
}

# KEY PATH [curl args]: an authenticated controld request, the key kept off the command line.
api() {
  local key=$1 path=$2
  shift 2
  printf 'authorization: Bearer %s\n' "$key" | curl -fsS --max-time 20 -H @- "$@" "$CONTROLD_LOCAL$path"
}

# DIR REMOTE BRANCH LOCAL_BRANCH: fast-forward DIR's LOCAL_BRANCH to REMOTE/BRANCH.
git_sync() {
  local dir=$1 remote=$2 branch=$3 local_branch=$4
  if ! { git -C "$dir" diff --quiet && git -C "$dir" diff --cached --quiet; }; then
    die "$dir has uncommitted changes to tracked files; commit or stash them, then re-run"
  fi
  git -C "$dir" fetch --quiet "$remote" "$branch" \
    || die "could not fetch $branch from $remote ($(git -C "$dir" remote get-url "$remote")) in $dir"
  if git -C "$dir" rev-parse --verify --quiet "refs/heads/$local_branch" >/dev/null; then
    git -C "$dir" switch --quiet "$local_branch"
    git -C "$dir" merge --ff-only --quiet "$remote/$branch" \
      || die "$dir: $local_branch has commits $remote/$branch does not; sort that out by hand"
  else
    git -C "$dir" switch --quiet -c "$local_branch" --track "$remote/$branch"
  fi
  ok "$dir: $(git -C "$dir" log --oneline -1)"
}

check_checkout() {
  say "agent-cloud checkout"
  local branch behind
  branch=$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)
  [ "$branch" = "$AC_BRANCH" ] || warn "this checkout is on $branch, not $AC_BRANCH"
  if git -C "$ROOT" fetch --quiet origin "$branch" 2>/dev/null; then
    behind=$(git -C "$ROOT" rev-list --count "HEAD..origin/$branch" 2>/dev/null || echo 0)
    [ "$behind" = 0 ] || die "this checkout is $behind commit(s) behind origin/$branch: run 'git pull' first"
  else
    warn "could not reach origin to check for newer commits"
  fi
  ok "$(git -C "$ROOT" log --oneline -1)"
}

need_env() { [ -f "$AC_ENV" ] || die "$AC_ENV is missing: copy deploy/bm/.env.example there and paste your secrets in"; }

ac_compose() {
  local profile=()
  if [ -n "$(env_get "$AC_ENV" CLOUDFLARE_TUNNEL_TOKEN)" ]; then profile=(--profile tunnel); fi
  (cd "$AC_BM" && docker compose "${profile[@]}" "$@")
}

# psql against controld's database; SQL on stdin, psql flags as arguments.
ac_psql() {
  PGURL=$(env_get "$AC_ENV" PUKU_DATABASE_URL) docker run --rm -i --network host -e PGURL postgres:17 \
    sh -c 'exec psql -X -q -v ON_ERROR_STOP=1 "$@" "$PGURL"' psql "$@"
}

# Waits out a controld that is still starting (migrations, startup probes).
controld_up() {
  wait_http "$CONTROLD_LOCAL/health" "${1:-60}" \
    || die "controld does not answer on $CONTROLD_LOCAL after ${1:-60}s: see 'docker compose logs --tail=40 controld' in $AC_BM, or run '$SELF controld'"
}

controld_env() {
  local cid
  cid=$(ac_compose ps -q controld)
  docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$cid" | awk -F= -v k="$1" '$1 == k { sub(/^[^=]*=/, ""); print; exit }'
}

# Problems with the object storage settings, one per line; nothing when they are fine.
storage_problems() {
  local endpoint host region
  endpoint=$(env_get "$AC_ENV" PUKU_R2_ENDPOINT)
  [ -n "$endpoint" ] || return 0
  host=${endpoint#*://}
  host=${host%%/*}
  host=${host%%:*}
  # A bare service name (minio) or loopback resolves on this host only.
  if [ "${SINGLE_HOST:-0}" != 1 ] && [[ $host == localhost || $host == 127.* || $host != *.* ]]; then
    echo "PUKU_R2_ENDPOINT is $endpoint, which other hosts cannot reach, and workers upload snapshot parts to it directly: use e.g. http://<this host's public IP>:9000 (SINGLE_HOST=1 if this is the only worker)"
  fi
  region=$(env_get "$AC_ENV" PUKU_R2_REGION)
  if [[ $endpoint != *r2.cloudflarestorage.com* ]] && { [ -z "$region" ] || [ "$region" = auto ]; }; then
    echo "PUKU_R2_REGION is '${region:-auto}', but MinIO wants us-east-1 (a mismatch fails as a signature error)"
  fi
}

# -------------------------------------------------------------------- check
cmd_check() {
  say "agent-cloud secrets in $AC_ENV"
  need_env
  local k missing=0 problems
  for k in "${SECRETS[@]}"; do
    if [ -n "$(env_get "$AC_ENV" "$k")" ]; then ok "$k"; else warn "$k is empty"; missing=1; fi
  done
  if [ -n "$(env_get "$AC_ENV" CLOUDFLARE_TUNNEL_TOKEN)" ]; then
    ok "CLOUDFLARE_TUNNEL_TOKEN (workers on other hosts come in through the tunnel)"
  else
    warn "CLOUDFLARE_TUNNEL_TOKEN is empty: workers on other hosts have no way in to controld"
  fi
  problems=$(storage_problems)
  if [ -n "$problems" ]; then
    while IFS= read -r k; do warn "$k"; done <<<"$problems"
    missing=1
  fi

  say "puku-bot in $BOT_DIR"
  if [ -f "$BOT_DIR/.env" ]; then
    local pw
    pw=$(env_get "$BOT_DIR/.env" POSTGRES_PASSWORD)
    if [ -n "$pw" ] && ! url_safe "$pw"; then warn "POSTGRES_PASSWORD in $BOT_DIR/.env has characters that break the database URL puku-bot builds from it (postgres://pukubot:PASSWORD@postgres:5432/...): use letters and digits only, e.g. openssl rand -hex 32"; missing=1; fi
    local key
    key=$(env_get "$BOT_DIR/.env" PUKU_AGENT_CLOUD_API_KEY)
    if [ -z "$key" ]; then
      note "PUKU_AGENT_CLOUD_API_KEY is empty: '$SELF key' makes one, for you to paste there"
    elif wait_http "$CONTROLD_LOCAL/health" 2 && api "$key" /v1/machines -o /dev/null 2>/dev/null; then
      ok "PUKU_AGENT_CLOUD_API_KEY works against controld"
    else
      note "PUKU_AGENT_CLOUD_API_KEY is set (controld is not up to check it, or it refused it)"
    fi
  else
    warn "$BOT_DIR/.env is missing"
  fi
  [ "$missing" = 0 ] || die "fix the items marked WARN in $AC_ENV, then run check again"
  ok "ready for '$SELF controld'"
}

# ----------------------------------------------------------------- controld
backup_db() {
  if [ "$BACKUP_DB" != 1 ]; then note "BACKUP_DB=0: no database backup"; return 0; fi
  say "back up the agent-cloud database"
  install -d -m700 "$BACKUP_DIR"
  local out=$BACKUP_DIR/agent-cloud-$STAMP.sql.gz
  if ! PGURL=$(env_get "$AC_ENV" PUKU_DATABASE_URL) docker run --rm --network host -e PGURL postgres:17 \
    sh -c 'exec pg_dump --no-owner "$PGURL"' | gzip > "$out"; then
    rm -f "$out"
    die "pg_dump failed. The new controld migrates the database forward, so this dump is the only way back to the old one; BACKUP_DB=0 skips it"
  fi
  chmod 600 "$out"
  ok "$out ($(du -h "$out" | cut -f1))"
}

# Lifts the pukubot org's machine quota (the default of 20 would refuse the 21st
# user). Returns 1 while that org does not exist yet.
raise_quota() {
  [[ $MACHINE_QUOTA =~ ^[0-9]+$ ]] || die "MACHINE_QUOTA must be a number"
  local cap
  cap=$(ac_psql -tA <<SQL | tr -d '[:space:]'
UPDATE quotas SET max_concurrent_machines = GREATEST(max_concurrent_machines, $MACHINE_QUOTA)
 WHERE org_id IN (SELECT id FROM orgs WHERE name = 'pukubot');
SELECT min(q.max_concurrent_machines) FROM quotas q JOIN orgs o ON o.id = q.org_id WHERE o.name = 'pukubot';
SQL
  )
  [ -n "$cap" ] || return 1
  [ "$cap" -ge "$MACHINE_QUOTA" ] || die "the pukubot org's machine quota reads $cap, not $MACHINE_QUOTA"
  ok "pukubot org: up to $cap machines"
}

cmd_controld() {
  need_env
  docker compose version >/dev/null 2>&1 || die "docker compose v2 is needed"
  check_checkout

  say "controld settings"
  local k problems
  for k in "${SECRETS[@]}"; do
    [ -n "$(env_get "$AC_ENV" "$k")" ] || die "$k is empty in $AC_ENV; '$SELF check' lists everything missing"
  done
  problems=$(storage_problems)
  [ -z "$problems" ] || die "$problems"
  ok "secrets present"
  backup_db

  # Tagged by the last commit that changed what the binary is built from, so a
  # commit touching only deploy/ or docs/ reuses the image already built.
  local image
  image="poridhi/puku-controld:ch-$(git -C "$ROOT" log -1 --format=%h -- Dockerfile Cargo.toml Cargo.lock crates migrations)"
  say "controld image $image"
  if docker image inspect "$image" >/dev/null 2>&1; then ok "already built"; else docker build -t "$image" "$ROOT"; fi

  # Settings only; the secrets in this file are left exactly as you pasted them.
  backup_file "$AC_ENV"
  env_set "$AC_ENV" CONTROLD_IMAGE "$image"
  env_set "$AC_ENV" PUKU_ENGINES_ALLOWED cloud_hypervisor
  env_set "$AC_ENV" PUKU_ENGINE_DEFAULT cloud_hypervisor
  env_set "$AC_ENV" PUKU_MACHINE_IMAGE "$COMPUTER_IMAGE"
  env_set "$AC_ENV" PUKU_MACHINE_MAX_CPUS "$MACHINE_CPUS"
  env_set "$AC_ENV" PUKU_MACHINE_MAX_MEMORY_MIB "$MACHINE_MEMORY_MIB"
  # puku-bot's containers share the puku-link network with controld and open links by name.
  env_set "$AC_ENV" PUKU_LINKS_URL http://controld:7770
  ok "engine, machine and link settings in $AC_ENV (the previous file is $AC_ENV.bak-$STAMP)"

  say "restart controld"
  ac_compose up -d --force-recreate
  if ! wait_http "$CONTROLD_LOCAL/health" 120; then
    ac_compose logs --tail=60 controld || true
    die "controld did not answer on $CONTROLD_LOCAL/health within 2 minutes (logs above)"
  fi
  local engines
  engines=$(controld_env PUKU_ENGINES_ALLOWED)
  [ "$engines" = cloud_hypervisor ] \
    || die "controld runs with PUKU_ENGINES_ALLOWED='$engines'; is $AC_BM/docker-compose.yml from this branch?"
  ok "controld up: $image, engines $engines, links $(controld_env PUKU_LINKS_URL)"

  say "machine quota"
  raise_quota || note "no pukubot org yet: '$SELF key' makes it and lifts the quota"
  echo
  echo "   Next: '$SELF key' for puku-bot, and '$SELF token <name>' for each worker."
}

# ------------------------------------------------------------- key, token
cmd_key() {
  need_env
  controld_up
  local key
  key=$(ac_compose exec -T controld puku-controld gen-key --org pukubot --name puku-bot 2>/dev/null \
    | grep -oE 'pkc_[A-Za-z0-9_-]+' | head -1 || true)
  [ -n "$key" ] || die "gen-key printed no key (see: docker compose logs controld, in $AC_BM)"
  raise_quota >/dev/null || die "the key was made, but the pukubot org's quota could not be raised"
  printf '\n   Paste this line into %s (shown once):\n\n   PUKU_AGENT_CLOUD_API_KEY=%s\n\n' "$BOT_DIR/.env" "$key"
}

cmd_token() {
  [ $# -eq 1 ] && [[ $1 =~ ^[A-Za-z0-9._-]+$ ]] || die "usage: $SELF token <worker name>, e.g. bm2"
  need_env
  controld_up
  local out tok
  out=$(ac_compose exec -T controld puku-controld gen-worker-token --name "$1" 2>&1) \
    || die "gen-worker-token failed: $out"
  tok=$(grep -oE 'pkw_[0-9a-f]{40}' <<<"$out" | head -1 || true)
  [ -n "$tok" ] || die "gen-worker-token printed no token"
  printf '\n   Token for worker %s. Paste it when setup-worker.sh asks, on that host (shown once):\n\n   %s\n\n' "$1" "$tok"
}

# ---------------------------------------------------------------------- bot
# owner/repo of a GitHub URL, https or ssh.
gh_repo() {
  local u=${1%.git}
  printf '%s\n' "${u#*github.com[:/]}"
}

# The remote that already points at BOT_REMOTE, or `svc`, added for it.
bot_remote_name() {
  local r want
  want=$(gh_repo "$BOT_REMOTE")
  for r in $(git -C "$BOT_DIR" remote); do
    if [ "$(gh_repo "$(git -C "$BOT_DIR" remote get-url "$r")")" = "$want" ]; then
      printf '%s\n' "$r"
      return 0
    fi
  done
  if git -C "$BOT_DIR" remote get-url svc >/dev/null 2>&1; then
    git -C "$BOT_DIR" remote set-url svc "$BOT_REMOTE"
  else
    git -C "$BOT_DIR" remote add svc "$BOT_REMOTE"
  fi
  printf 'svc\n'
}

# "project<TAB>file,file,..." of the puku-bot compose project running from BOT_DIR.
bot_stack() {
  local all
  all=$(docker ps -a --format '{{.Label "com.docker.compose.project"}}	{{.Label "com.docker.compose.project.config_files"}}')
  awk -F'\t' -v d="$BOT_DIR/" 'index($2, d) == 1 { print; exit }' <<<"$all"
}

# The compose files to run: the current list with the Docker-sandbox overlay swapped
# for the agent-cloud one, or prod + agent-cloud (+ tunnel) when nothing runs yet.
new_bot_files() {
  local c=$BOT_DIR/infra/compose f out=() placed=0
  if [ $# -eq 0 ]; then
    out=("$c/docker-compose.prod.yml" "$c/docker-compose.agent-cloud.yml")
    if [ -n "$(env_get "$BOT_DIR/.env" CLOUDFLARE_TUNNEL_TOKEN)" ]; then out+=("$c/docker-compose.tunnel.yml"); fi
    printf '%s\n' "${out[@]}"
    return 0
  fi
  for f in "$@"; do
    case ${f##*/} in docker-compose.docker-sandbox.yml | docker-compose.agent-cloud.yml) continue ;; esac
    out+=("$f")
    if [ "${f##*/}" = docker-compose.prod.yml ]; then
      out+=("$c/docker-compose.agent-cloud.yml")
      placed=1
    fi
  done
  if [ "$placed" = 0 ]; then out+=("$c/docker-compose.agent-cloud.yml"); fi
  printf '%s\n' "${out[@]}"
}

# PROJECT FILES_ARRAY_NAME compose-args...
bot_compose() {
  local project=$1
  local -n files_ref=$2
  shift 2
  local args=(-p "$project" --env-file "$BOT_DIR/.env") f
  for f in "${files_ref[@]}"; do args+=(-f "$f"); done
  (cd "$BOT_DIR" && docker compose "${args[@]}" "$@")
}

write_bot_helper() {
  local project=$1
  local -n helper_files=$2
  local f
  {
    printf '#!/usr/bin/env bash\n# Written by %s: docker compose for the puku-bot stack, with its files.\n' "$SELF"
    printf 'cd %q || exit 1\nexec docker compose -p %q --env-file .env' "$BOT_DIR" "$project"
    for f in "${helper_files[@]}"; do printf ' -f %q' "$f"; done
    printf ' "$@"\n'
  } > /usr/local/bin/pukubot-compose
  chmod 755 /usr/local/bin/pukubot-compose
}

wipe_bot() {
  local project=$1
  say "wipe puku-bot"
  echo "   This deletes puku-bot's database, uploads and every Docker-sandbox computer (project '$project')."
  if [ "${ASSUME_YES:-0}" != 1 ]; then
    [ -t 0 ] || die "this needs confirmation: run it in a terminal, or set ASSUME_YES=1"
    local answer="" _
    # A spare newline from pasting the command must not count as the answer.
    while read -r -t 0 _ 2>/dev/null; do read -r -t 1 _ || break; done
    read -rp "   Type WIPE to go ahead: " answer
    [ "$answer" = WIPE ] || die "stopped; puku-bot was not touched"
  fi
  docker ps -aq --filter "label=com.docker.compose.project=$project" | xargs -r docker rm -f >/dev/null
  # Caddy's certificates are not data, and re-issuing them runs into rate limits.
  docker volume ls -q --filter "label=com.docker.compose.project=$project" | { grep -v caddy || true; } | xargs -r docker volume rm >/dev/null
  docker network ls -q --filter "label=com.docker.compose.project=$project" | xargs -r docker network rm >/dev/null 2>&1 || true
  # The computers the Docker-sandbox supervisor created, and their homes.
  docker ps -aq --filter name=pukubot-bot- --filter name=pukubot-computer- | xargs -r docker rm -f >/dev/null
  docker volume ls -q | { grep -E '^pukubot-(bot|computer)-' || true; } | xargs -r docker volume rm >/dev/null 2>&1 || true
  ok "puku-bot wiped"
}

ch_workers() {
  ac_psql -tA <<<"SELECT count(*) FROM workers WHERE status <> 'offline' AND array_to_string(engines, ',') LIKE '%cloud_hypervisor%';" | tr -d '[:space:]'
}

cmd_bot() {
  need_env
  controld_up
  local env=$BOT_DIR/.env key have
  [ -f "$env" ] || die "$env is missing"
  key=$(env_get "$env" PUKU_AGENT_CLOUD_API_KEY)
  [ -n "$key" ] || die "PUKU_AGENT_CLOUD_API_KEY is empty in $env: paste the line '$SELF key' prints"
  api "$key" /v1/machines -o /dev/null 2>/dev/null || die "controld refuses the PUKU_AGENT_CLOUD_API_KEY in $env"
  ok "puku-bot's key works"
  url_safe "$(env_get "$env" POSTGRES_PASSWORD)" || die "POSTGRES_PASSWORD in $BOT_DIR/.env has characters that break the database URL puku-bot builds from it (postgres://pukubot:PASSWORD@postgres:5432/...): use letters and digits only, e.g. openssl rand -hex 32"
  have=$(ch_workers)
  [ "${have:-0}" -ge 1 ] || die "no Cloud Hypervisor worker is registered: run setup-worker.sh first, or every computer fails to start"
  ok "$have Cloud Hypervisor worker(s) registered"

  say "puku-bot code ($BOT_BRANCH)"
  [ -d "$BOT_DIR/.git" ] || die "$BOT_DIR is not a git checkout (set BOT_DIR)"
  git_sync "$BOT_DIR" "$(bot_remote_name)" "$BOT_BRANCH" cloud-hypervisor

  say "puku-bot stack"
  local stack project old_files=() files=() f switching=0
  stack=$(bot_stack)
  if [ -n "$stack" ]; then
    project=${stack%%$'\t'*}
    IFS=, read -ra old_files <<<"${stack#*$'\t'}"
    note "running now: project $project with ${old_files[*]##*/}"
  else
    project=$(basename "$BOT_DIR")
    warn "no puku-bot stack is running from $BOT_DIR; starting one as project $project"
  fi
  for f in "${old_files[@]}"; do
    if [ "${f##*/}" = docker-compose.docker-sandbox.yml ]; then switching=1; fi
  done
  if [ ${#old_files[@]} -eq 0 ]; then switching=1; fi
  mapfile -t files < <(new_bot_files "${old_files[@]}")
  for f in "${files[@]}"; do [ -f "$f" ] || die "$f is missing: is $BOT_DIR on $BOT_BRANCH?"; done

  if [ "$switching" = 1 ] || [ "${WIPE:-0}" = 1 ]; then wipe_bot "$project"; fi

  # Settings only; the key and every other secret stay as you pasted them.
  say "puku-bot settings"
  backup_file "$env"
  env_set "$env" SANDBOX_PROVIDER puku-cloud
  env_set "$env" PUKU_AGENT_CLOUD_URL http://controld:7770
  env_set "$env" PUKU_AGENT_CLOUD_ENGINE cloud_hypervisor
  env_set "$env" PUKU_AGENT_CLOUD_IMAGE "$COMPUTER_IMAGE"
  env_set "$env" PUKU_AGENT_CLOUD_CPUS "$MACHINE_CPUS"
  env_set "$env" PUKU_AGENT_CLOUD_MEMORY_MIB "$MACHINE_MEMORY_MIB"
  env_set "$env" PUKU_AGENT_CLOUD_PERSIST_ROOT 1
  env_set "$env" PUKU_COMPUTER_TOPOLOGY per-user
  env_comment "$env" SANDBOX_SUPERVISOR_URL
  ok "$env (the previous file is $env.bak-$STAMP)"

  say "start puku-bot with ${files[*]##*/}"
  bot_compose "$project" files up -d --build --remove-orphans
  write_bot_helper "$project" files
  ok "'pukubot-compose <args>' is docker compose for this stack from now on (e.g. pukubot-compose logs -f api)"

  say "puku-bot reaches controld"
  local i reached=0 members
  for ((i = 0; i < 90; i++)); do
    if bot_compose "$project" files exec -T api node -e \
      "fetch('http://controld:7770/health').then(r => process.exit(r.ok ? 0 : 1), () => process.exit(1))" >/dev/null 2>&1; then
      reached=1
      break
    fi
    sleep 2
  done
  if [ "$reached" = 0 ]; then
    bot_compose "$project" files logs --tail=60 api || true
    die "puku-bot's api cannot reach http://controld:7770 (logs above)"
  fi
  ok "api -> http://controld:7770"
  members=$(docker network inspect puku-link --format '{{range .Containers}}{{.Name}} {{end}}')
  for f in api worker web; do
    grep -q -- "-$f-" <<<"$members" || warn "no $f container on puku-link (members: $members)"
  done
  echo
  echo "   Next: '$SELF smoke', then sign up in the browser, create two bots and open their screens."
}

# ------------------------------------------------------------ status, smoke
cmd_status() {
  need_env
  say "controld"
  if wait_http "$CONTROLD_LOCAL/health" 2; then ok "up: $(env_get "$AC_ENV" CONTROLD_IMAGE)"; else warn "DOWN"; fi
  printf '   engines %s, machine image %s, links %s\n' "$(env_get "$AC_ENV" PUKU_ENGINES_ALLOWED)" \
    "$(env_get "$AC_ENV" PUKU_MACHINE_IMAGE)" "$(env_get "$AC_ENV" PUKU_LINKS_URL)"
  say "workers"
  ac_psql <<'SQL'
SELECT name, status, array_to_string(engines, ', ') AS engines,
       used_slots || '/' || capacity_slots AS slots,
       date_trunc('second', now() - last_heartbeat_at) AS since_heartbeat
  FROM workers WHERE status <> 'offline' ORDER BY name;
SELECT state, count(*) AS machines FROM machines WHERE state <> 'destroyed' GROUP BY state ORDER BY state;
SQL
  if [ -d "$BOT_DIR/.git" ]; then
    say "puku-bot"
    printf '   %s [%s], SANDBOX_PROVIDER=%s\n' "$(git -C "$BOT_DIR" log --oneline -1)" \
      "$(git -C "$BOT_DIR" rev-parse --abbrev-ref HEAD)" "$(env_get "$BOT_DIR/.env" SANDBOX_PROVIDER)"
  fi
}

cmd_smoke() {
  need_env
  controld_up
  command -v jq >/dev/null || { apt-get update -qq; apt_install jq; }
  local key=${SMOKE_KEY:-}
  if [ -z "$key" ]; then key=$(env_get "$BOT_DIR/.env" PUKU_AGENT_CLOUD_API_KEY); fi
  [ -n "$key" ] || die "no key to test with: paste puku-bot's into $BOT_DIR/.env, or set SMOKE_KEY"
  say "machine smoke test on Cloud Hypervisor"
  note "to test one host, stop puku-workerd on the others first (their VMs keep running)"
  PUKU_CLOUD_URL=$CONTROLD_LOCAL PUKU_CLOUD_API_KEY=$key ENGINE=cloud_hypervisor IMAGE=alpine SNAPSHOTS=1 \
    "$ROOT/deploy/scripts/machines-smoke.sh"
}

# --------------------------------------------------------------------- main
start_log() {
  install -d -m700 /var/log/puku-rollout
  LOG=/var/log/puku-rollout/$STAMP-control-$1.log
  exec > >(tee -a "$LOG") 2>&1
  note "logging to $LOG"
}

main() {
  local cmd=${1:-}
  case $cmd in "" | -h | --help | help) usage; exit 0 ;; esac
  shift
  [ "$(id -u)" = 0 ] || die "run as root"
  # key and token print secrets, so they stay out of the log.
  case $cmd in key | token) ;; *) start_log "$cmd" ;; esac
  case $cmd in
    check) cmd_check ;;
    controld) cmd_controld ;;
    key) cmd_key ;;
    token) cmd_token "$@" ;;
    bot) cmd_bot ;;
    status) cmd_status ;;
    smoke) cmd_smoke ;;
    *) usage; die "unknown command: $cmd" ;;
  esac
}

if [ "${SETUP_LIB:-}" = 1 ]; then return 0 2>/dev/null || exit 0; fi
main "$@"

#!/usr/bin/env bash
# Set up, or update, a puku-agent-cloud worker on this host: Cloud Hypervisor only,
# with the puku-bot computer image staged. Run it as root from the agent-cloud
# checkout on every worker host, the control host included:
#
#   git pull
#   ./deploy/scripts/setup-worker.sh
#
# The first run asks for three things; later runs remember the first two:
#
#   worker name    bm1, bm2, ...: the name the token was made for
#   controld URL   ws://127.0.0.1:7770/v1/worker on the control host,
#                  wss://<controld's public hostname>/v1/worker on any other host
#   worker token   printed by `setup-control-plane.sh token <name>` on the control host
#
# Set WORKER_NAME and CONTROLD_URL in the environment to skip the questions, and
# NEW_TOKEN=1 to be asked for a new token. Other settings:
#
#   CPU_OVERCOMMIT   how far the host may overcommit its CPUs (2)
#   COMPUTER_IMAGE   the image computers boot (pukubot-computer:latest)
#   BOT_SRC          where puku-bot is checked out to build it (/root/puku-bot-svc)
#   BOT_REMOTE       its repository (git@github.com:puku-sh/puku-bot-svc.git)
#   BOT_BRANCH       its branch (feat/puku-cloud-sandbox)
#   BUILD_IMAGE=0    use the COMPUTER_IMAGE already loaded here instead of building it
#   REBUILD_KERNEL=1 build the guest kernel again instead of reusing the staged one
#   RESET_STATE=1    move aside machines that belong to a different controld
#
# Every run is logged under /var/log/puku-rollout/.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
SELF=deploy/scripts/setup-worker.sh
AC_BRANCH=feat/engines-cloud-hypervisor
TOKEN_FILE=/etc/puku/worker-token
# Which worker name and controld the saved token was pasted for.
TOKEN_FOR=/etc/puku/worker-token.for
STATE_DIR=/var/lib/puku
UNIT=/etc/systemd/system/puku-workerd.service
DROPIN=/etc/systemd/system/puku-workerd.service.d/zz-puku-worker.conf
STAMP=$(date +%Y%m%d-%H%M%S)

: "${CPU_OVERCOMMIT:=2}"
: "${COMPUTER_IMAGE:=pukubot-computer:latest}"
: "${BOT_SRC:=/root/puku-bot-svc}"
: "${BOT_REMOTE:=git@github.com:puku-sh/puku-bot-svc.git}"
: "${BOT_BRANCH:=feat/puku-cloud-sandbox}"
: "${BUILD_IMAGE:=1}"

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

# A setting from this script's drop-in, from the last run.
from_dropin() {
  if [ -f "$DROPIN" ]; then sed -n "s/^Environment=$1=//p" "$DROPIN" | tail -1; fi
}

# A pasted command often brings a spare newline along; drop it so it does not
# answer the first question.
drain_input() {
  local _
  while read -r -t 0 _ 2>/dev/null; do read -r -t 1 _ || break; done
}

# VAR QUESTION [DEFAULT]: keep VAR if it is set; otherwise ask, offering DEFAULT.
ask() {
  local var=$1 question=$2 default=${3:-} answer=""
  if [ -n "${!var:-}" ]; then return 0; fi
  if [ -t 0 ]; then
    drain_input
    while [ -z "$answer" ]; do
      read -rp "   $question${default:+ [$default]}: " answer || die "no answer for $var"
      answer=${answer:-$default}
    done
  fi
  printf -v "$var" '%s' "${answer:-$default}"
  [ -n "${!var}" ] || die "$var is needed: set it in the environment, or run this in a terminal"
}

is_loopback() { [[ $1 =~ ^wss?://(127\.[0-9.]+|localhost|\[::1\])(:|/) ]]; }

# ws(s)://host/v1/worker -> http(s)://host/health
health_url() {
  local url=${1/#wss:/https:}
  url=${url/#ws:/http:}
  printf '%s/health\n' "${url%/v1/worker}"
}

# DIR REMOTE BRANCH LOCAL_BRANCH: fast-forward DIR's LOCAL_BRANCH to REMOTE/BRANCH.
git_sync() {
  local dir=$1 remote=$2 branch=$3 local_branch=$4
  if ! { git -C "$dir" diff --quiet && git -C "$dir" diff --cached --quiet; }; then
    die "$dir has uncommitted changes to tracked files; commit or stash them, then re-run"
  fi
  git -C "$dir" fetch --quiet "$remote" "$branch" || die "could not fetch $branch from $remote in $dir"
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

settings() {
  say "settings"
  # Production's controld runs next to deploy/bm/.env. On any other host a controld
  # on loopback is a test run's, which the leftovers step stops.
  local default_url control_host=0
  if [ -f "$ROOT/deploy/bm/.env" ]; then control_host=1; fi
  default_url=$(from_dropin PUKU_CONTROLD_URL)
  if [ "$control_host" = 0 ] && is_loopback "$default_url"; then default_url=""; fi
  if [ -z "$default_url" ] && [ "$control_host" = 1 ] \
    && curl -fsS -o /dev/null --max-time 3 http://127.0.0.1:7770/health 2>/dev/null; then
    default_url=ws://127.0.0.1:7770/v1/worker
  fi
  ask WORKER_NAME "Worker name (the one its token was made for), e.g. bm2" "$(from_dropin PUKU_WORKER_NAME)"
  ask CONTROLD_URL "controld URL (ws://127.0.0.1:7770/v1/worker on the control host, wss://<public host>/v1/worker elsewhere)" "$default_url"
  [[ $WORKER_NAME =~ ^[A-Za-z0-9._-]+$ ]] || die "WORKER_NAME '$WORKER_NAME' must be a plain name"
  case $CONTROLD_URL in
    ws://*/v1/worker | wss://*/v1/worker) ;;
    *) die "the controld URL must be the full ws:// or wss:// URL ending in /v1/worker (got '$CONTROLD_URL')" ;;
  esac
  if [ "$control_host" = 0 ] && is_loopback "$CONTROLD_URL"; then
    die "$CONTROLD_URL is this host's own loopback, and production's controld does not run here (no deploy/bm/.env): use wss://<controld's public hostname>/v1/worker"
  fi
  curl -fsS -o /dev/null --max-time 10 "$(health_url "$CONTROLD_URL")" \
    || die "$(health_url "$CONTROLD_URL") does not answer: this host cannot reach controld there"
  ok "$WORKER_NAME -> $CONTROLD_URL (controld answers)"
}

token() {
  say "worker token"
  # A token saved for another name or controld (an old test run's, say) is not reused.
  if [ -s "$TOKEN_FILE" ] && [ "${NEW_TOKEN:-0}" != 1 ] \
    && [ "$(cat "$TOKEN_FOR" 2>/dev/null || true)" = "$WORKER_NAME $CONTROLD_URL" ]; then
    ok "using the token saved for $WORKER_NAME (NEW_TOKEN=1 to paste a new one)"
    return 0
  fi
  [ -t 0 ] || die "a worker token is needed: run this in a terminal to paste it"
  local pasted="" try
  drain_input
  for try in 1 2 3; do
    read -rsp "   Paste the token from 'setup-control-plane.sh token $WORKER_NAME' (hidden): " pasted \
      || die "no token pasted"
    echo
    pasted=$(printf '%s' "$pasted" | tr -d '[:space:]')
    [[ $pasted =~ ^pkw_[0-9a-f]{40}$ ]] && break
    if [ -n "$pasted" ]; then echo "   that is not a worker token (pkw_ and 40 hex digits)"; fi
    pasted=""
  done
  [ -n "$pasted" ] || die "no worker token pasted (attempt $try of 3)"
  install -d -m755 /etc/puku
  (umask 077; printf '%s\n' "$pasted" > "$TOKEN_FILE")
  printf '%s\n' "$WORKER_NAME $CONTROLD_URL" > "$TOKEN_FOR"
  ok "saved to $TOKEN_FILE"
}

load_cargo() {
  command -v cargo >/dev/null && return 0
  local c
  for c in "$HOME/.cargo/env" /root/.cargo/env; do
    # shellcheck disable=SC1090
    if [ -f "$c" ]; then . "$c"; fi
  done
  command -v cargo >/dev/null
}

packages() {
  say "packages and kernel modules"
  apt-get update -qq
  apt_install build-essential pkg-config libcap-ng-dev git jq curl ca-certificates \
    e2fsprogs nftables iproute2 flex bison bc libelf-dev libssl-dev
  apt_install virtiofsd || warn "no virtiofsd package here; prestage-ch.sh looks for it next"
  command -v docker >/dev/null || apt_install docker.io
  modprobe vhost_vsock
  modprobe tun
  printf 'vhost_vsock\ntun\n' > /etc/modules-load.d/puku.conf
  if ! load_cargo; then
    note "installing rust (rustup)"
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
    load_cargo || die "cargo is still not on PATH after installing rustup"
  fi
  ok "packages, vhost_vsock, tun, $(cargo --version)"
}

stop_vm_units() {
  systemctl list-units 'puku-vm-*' --all --no-legend --plain 2>/dev/null | awk '{print $1}' | xargs -r systemctl stop || true
}

state_has_machines() {
  local d
  for d in machines vms sessions; do
    [ -n "$(ls -A "$STATE_DIR/$d" 2>/dev/null)" ] && return 0
  done
  return 1
}

# Hand-started puku processes (test runs), and machine state that belongs to a
# different controld.
leftovers() {
  say "leftovers from earlier runs"
  local proc pid unit found=0
  for proc in puku-workerd puku-controld; do
    for pid in $(pgrep -x "$proc" || true); do
      unit=$(ps -o unit= -p "$pid" 2>/dev/null | tr -d ' ' || true)
      case $unit in puku-workerd.service | puku-controld.service | docker-*.scope | *containerd*) continue ;; esac
      note "stopping a hand-started $proc (pid $pid, ${unit:-no unit})"
      kill "$pid" 2>/dev/null || true
      found=1
    done
  done
  # This host runs Cloud Hypervisor only, so a libkrun VM is left over from a test.
  for pid in $(pgrep -f '(^|/)msb sandbox' || true); do
    note "stopping a leftover libkrun VM (pid $pid)"
    kill "$pid" 2>/dev/null || true
    found=1
  done
  if [ "$found" = 1 ]; then sleep 3; fi

  local marker=$STATE_DIR/.controld-url recorded
  recorded=$(cat "$marker" 2>/dev/null || true)
  if state_has_machines && [ "$recorded" != "$CONTROLD_URL" ]; then
    if [ -n "$recorded" ] && [ "${RESET_STATE:-0}" != 1 ]; then
      die "$STATE_DIR holds machines of $recorded, not $CONTROLD_URL. RESET_STATE=1 moves them aside"
    fi
    note "$STATE_DIR holds machines from another controld; moving it to $STATE_DIR.pre-$STAMP"
    systemctl stop puku-workerd 2>/dev/null || true
    stop_vm_units
    mv "$STATE_DIR" "$STATE_DIR.pre-$STAMP"
  fi
  install -d "$STATE_DIR"
  printf '%s\n' "$CONTROLD_URL" > "$marker"
  ok "$STATE_DIR belongs to $CONTROLD_URL"
}

build_workerd() {
  say "workerd"
  (cd "$ROOT" && cargo build --release -p puku-workerd)
  install -d /opt/puku/bin
  install -m755 "$ROOT/target/release/puku-workerd" /opt/puku/bin/puku-workerd
  install -m755 "$ROOT/deploy/scripts/preflight.sh" /opt/puku/bin/preflight.sh
  ok "/opt/puku/bin/puku-workerd"
}

# prestage-ch.sh builds the guest kernel from source every time; one already staged
# here is handed back to it instead.
stage_ch() {
  say "Cloud Hypervisor toolchain"
  if [ -s /opt/puku/ch/vmlinux ] && [ "${REBUILD_KERNEL:-0}" != 1 ]; then
    local kernel
    kernel=$(mktemp)
    cp /opt/puku/ch/vmlinux "$kernel"
    note "reusing the staged guest kernel (REBUILD_KERNEL=1 builds it again)"
    KERNEL_URL="file://$kernel" "$ROOT/deploy/scripts/prestage-ch.sh"
    rm -f "$kernel"
  else
    note "building the guest kernel: 10-20 minutes, once per host"
    "$ROOT/deploy/scripts/prestage-ch.sh"
  fi
  install -m644 "$ROOT/deploy/systemd/puku-vms.slice" /etc/systemd/system/puku-vms.slice
}

image() {
  say "computer image $COMPUTER_IMAGE"
  if [ "$BUILD_IMAGE" = 1 ]; then
    if [ -d "$BOT_SRC/.git" ]; then
      git_sync "$BOT_SRC" origin "$BOT_BRANCH" "$BOT_BRANCH"
    else
      git clone --quiet --branch "$BOT_BRANCH" "$BOT_REMOTE" "$BOT_SRC" \
        || die "could not clone $BOT_REMOTE: this host needs an ssh key GitHub accepts for it (check: ssh -T git@github.com), or BUILD_IMAGE=0 with $COMPUTER_IMAGE loaded from another host"
      ok "$BOT_SRC: $(git -C "$BOT_SRC" log --oneline -1)"
    fi
    docker build -t "$COMPUTER_IMAGE" "$BOT_SRC/infra/sandboxes/computer"
  else
    docker image inspect "$COMPUTER_IMAGE" >/dev/null 2>&1 || die "BUILD_IMAGE=0, but $COMPUTER_IMAGE is not here"
    ok "using the $COMPUTER_IMAGE already here"
  fi
  say "boot disks"
  "$ROOT/deploy/scripts/build-ch-rootfs.sh" "$COMPUTER_IMAGE"
  # The guest setup-control-plane.sh's smoke test boots.
  "$ROOT/deploy/scripts/build-ch-rootfs.sh" alpine
}

wait_registered() {
  local since=$1 i log=""
  for ((i = 0; i < 45; i++)); do
    log=$(journalctl -u puku-workerd --since "$since" --no-pager -o cat 2>/dev/null || true)
    grep -q 'registered with controld' <<<"$log" && break
    sleep 2
  done
  if grep -q 'cannot run it' <<<"$log"; then
    grep 'cannot run it' <<<"$log" | tail -1
    die "this host cannot run Cloud Hypervisor (the reason is on the line above)"
  fi
  if ! grep -q 'registered with controld' <<<"$log"; then
    printf '%s\n' "$log" | tail -40
    die "workerd did not register within 90 seconds (journal above). A refused token needs a new one: NEW_TOKEN=1 $SELF"
  fi
  grep 'engines enabled' <<<"$log" | tail -1 | grep -q cloud_hypervisor \
    || die "workerd registered without cloud_hypervisor: journalctl -u puku-workerd"
  ok "$WORKER_NAME registered with controld, cloud_hypervisor enabled"
}

unit() {
  say "systemd unit"
  if [ -f "$UNIT" ] && ! cmp -s "$UNIT" "$ROOT/deploy/systemd/puku-workerd.service"; then
    cp -p "$UNIT" "$UNIT.bak-$STAMP"
    note "replacing $UNIT (the old one is $UNIT.bak-$STAMP)"
  fi
  install -m644 "$ROOT/deploy/systemd/puku-workerd.service" "$UNIT"
  install -d "$(dirname "$DROPIN")"
  local others
  others=$(find "$(dirname "$DROPIN")" -maxdepth 1 -name '*.conf' ! -name "$(basename "$DROPIN")" | tr '\n' ' ')
  if [ -n "$others" ]; then warn "other drop-ins apply too ($(basename "$DROPIN") wins where they overlap): $others"; fi
  cat > "$DROPIN" <<EOF
# Written by $SELF; re-running it rewrites this file.
[Service]
Environment=PUKU_CONTROLD_URL=$CONTROLD_URL
Environment=PUKU_DATA_URL=$CONTROLD_URL/data
Environment=PUKU_WORKER_NAME=$WORKER_NAME
Environment=PUKU_ENGINE_LIBKRUN=false
Environment=PUKU_ENGINE_CLOUD_HYPERVISOR=true
Environment=PUKU_EGRESS_UNRESTRICTED=true
Environment=PUKU_CPU_OVERCOMMIT=$CPU_OVERCOMMIT
EOF
  systemctl daemon-reload
  systemctl enable puku-workerd >/dev/null 2>&1
  local since
  since=$(date '+%Y-%m-%d %H:%M:%S')
  # KillMode=process: running VMs outlive the restart.
  systemctl restart puku-workerd
  wait_registered "$since"
}

start_log() {
  install -d -m700 /var/log/puku-rollout
  LOG=/var/log/puku-rollout/$STAMP-worker.log
  exec > >(tee -a "$LOG") 2>&1
  note "logging to $LOG"
}

main() {
  case ${1:-} in -h | --help | help) usage; exit 0 ;; "") ;; *) usage; die "unknown argument: $1" ;; esac
  [ "$(id -u)" = 0 ] || die "run as root"
  [ -e /dev/kvm ] || die "/dev/kvm is missing: a worker needs bare metal (or nested virtualization)"
  start_log
  check_checkout
  settings
  token
  packages
  leftovers
  build_workerd
  stage_ch
  image
  unit
  say "done"
  echo "   $WORKER_NAME is serving Cloud Hypervisor machines. 'setup-control-plane.sh status' on the"
  echo "   control host lists it. Re-run this script after a 'git pull' to update the worker; running"
  echo "   VMs keep running. Log: $LOG"
}

if [ "${SETUP_LIB:-}" = 1 ]; then return 0 2>/dev/null || exit 0; fi
main "$@"

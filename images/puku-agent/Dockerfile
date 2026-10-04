# puku-agent: the guest image every session microVM boots.
# Build:  docker build -t puku-agent:latest images/puku-agent
# The puku-cli install layer expects network at build time; pin PUKU_VERSION
# for reproducible images.
FROM ubuntu:24.04

ARG PUKU_VERSION=latest

ARG NODE_MAJOR=22

RUN apt-get update && apt-get install -y --no-install-recommends \
    git curl ca-certificates ripgrep jq openssh-client tini \
    build-essential pkg-config python3 python3-venv gnupg \
 && rm -rf /var/lib/apt/lists/*

# Ubuntu 24.04 ships Node 18, below puku-cli's `engines: node >=20` (and
# firecrawl wants >=22, better-sqlite3 pins an ABI range). npm only WARNs
# on an engine mismatch, so an apt-node image builds clean and then fails
# at runtime in the guest. Install a supported Node from NodeSource.
RUN curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && rm -rf /var/lib/apt/lists/* \
 && node --version

# puku-cli is distributed on npm under the @puku scope. Fail the build on an
# engine mismatch rather than shipping an image that breaks mid-session.
RUN npm install -g --engine-strict "@puku/puku-cli@${PUKU_VERSION}" \
 && puku-cli --version

COPY runner/puku-runner.sh /usr/local/bin/puku-runner
COPY runner/puku-egress-proxy.py /usr/local/bin/puku-egress-proxy
RUN chmod +x /usr/local/bin/puku-runner /usr/local/bin/puku-egress-proxy

# The SDK-driven runner. This is what a session runs by default --
# DEFAULT_RUNNER_CMD in crates/puku-workerd/src/session_actor.rs -- because
# it is the only one that routes AskUserQuestion on puku-cli 1.8.48. The bash
# runner above stays as the fallback:
#   PUKU_RUNNER_CMD='exec /usr/local/bin/puku-runner'
# selects it again with a restart, no rebuild.
#
# Staged by deploy/scripts/vendor-sdk.sh, not npm-installed at build time: a
# guest that fetches its own runtime at boot is a slower session and an
# outage waiting for npm to have a bad day. Same pattern as prestage-msb.sh.
#
# It is staged from a checkout rather than npm because the runner needs
# `spawnPukuCliProcess` (to tap CLI stderr and read its real exit code), and
# the published 3.0.0 silently ignores that option.
#
# manifest.json is NOT optional -- checkCompatibility() reads it, and without
# it the check degrades to harnessOk:false instead of verifying the harness
# schema.
COPY vendor/sdk/ /opt/puku/sdk/
RUN node -e "import('/opt/puku/sdk/sdk.mjs').then(m => { if (typeof m.query !== 'function') throw new Error('sdk.mjs has no query export'); console.log('sdk', m.SDK_VERSION, 'harness', m.HARNESS_SCHEMA); })"
COPY runner/runner.mjs /opt/puku/runner.mjs

# The deterministic stand-in for puku-cli, used by
# skills/deployment-test/scripts/full-system-test.sh. It has to live in the
# image because that sweep runs inside a real microVM on a real deployment,
# which is the whole point of it -- 28 checks against the actual platform, for
# nothing, with the agent's behaviour fixed so a diff means a regression.
#
# Build with --build-arg WITH_TEST_CLI=0 to leave it out of an image that
# should carry no test code. The sweep then cannot run on that box.
#
# Reaching it is never accidental: it requires PUKU_CLI_PATH, which only
# arrives via an explicit PUKU_RUNNER_CMD override. The runner also emits a
# `platform.warning` event whenever the agent binary is not the real one, so
# a stale override is visible in the session's own event stream rather than
# quietly returning canned output for ever.
ARG WITH_TEST_CLI=1
COPY runner/fake-puku-cli.sh /opt/puku/fake-puku-cli.sh
RUN if [ "$WITH_TEST_CLI" = "1" ]; then chmod +x /opt/puku/fake-puku-cli.sh; \
    else rm -f /opt/puku/fake-puku-cli.sh; fi

# Non-root agent user; uid 1000 matches the host session dirs workerd creates.
RUN userdel -r ubuntu 2>/dev/null || true; useradd -m -u 1000 agent
USER agent
WORKDIR /workspace

# The VM's init is microsandbox's agentd; workerd starts puku-runner via
# exec, so no ENTRYPOINT is required. Keep a sane default for manual runs.
CMD ["sleep", "infinity"]

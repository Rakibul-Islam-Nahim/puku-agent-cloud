#!/usr/bin/env node
// In-guest session supervisor, driven by puku-agent-sdk. The SDK spawns the
// same `puku-cli` with the same flags `puku-runner.sh` passes by hand; what
// changes is that we consume typed messages instead of scraping stdout.
//
// Contract with the host is UNCHANGED — see puku-runner.sh for the prose:
//   /session/manifest.json   read : session parameters (written by workerd)
//   /session/stdin.fifo      read : stream-json input lines arrive here
//   /session/events.ndjson   write: every message, one JSON object per line.
//                                   The line number is the event's identity,
//                                   so this is append-only, never rewritten.
//   /session/home            puku-cli state ($HOME/.puku-cli), survives resume
//   /workspace               working directory, survives resume
//
// The SDK's messages are byte-shape-identical to the CLI's own stream-json
// lines (verified key-for-key on system/assistant/result against 1.8.49), so
// workerd's inspect_line, the dashboard and `puku cloud attach` all keep
// parsing exactly what they parse today.

import { spawn, spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import {
  appendFileSync, closeSync, copyFileSync, existsSync,
  mkdirSync, openSync, readdirSync, readFileSync, readSync, writeFileSync,
} from 'node:fs';
import net from 'node:net';
import { createInterface } from 'node:readline';
import { PassThrough } from 'node:stream';
import { join } from 'node:path';

/** Overridable so the runner can be exercised outside a guest, the same way
 *  PUKU_SDK_PATH and PUKU_CLI_PATH already are. */
const SESSION = process.env.PUKU_SESSION_DIR || '/session';
const MANIFEST = `${SESSION}/manifest.json`;
const EVENTS = `${SESSION}/events.ndjson`;
const FIFO = `${SESSION}/stdin.fifo`;
const BLOBS = `${SESSION}/blobs`;
const STDERR_LOG = `${SESSION}/runner.stderr`;
const SDK = process.env.PUKU_SDK_PATH || '/opt/puku/sdk/sdk.mjs';
/** Overridable so the parity harness can point both runners at a fake CLI. */
const CLI = process.env.PUKU_CLI_PATH || '/usr/bin/puku-cli';

/** Same cap as the bash runner: keep Postgres rows bounded. */
const MAX_LINE_BYTES = 262144;

/** Exit 70 is reserved for runner-level failure, distinct from puku-cli's. */
function die(msg) {
  logStderr(`puku-runner: ${msg}`);
  process.exit(70);
}

function logStderr(line) {
  try {
    appendFileSync(STDERR_LOG, `${line}\n`);
  } catch {
    /* the outbox matters more than the log */
  }
}

// --------------------------------------------------------------- manifest

if (!existsSync(MANIFEST)) die('manifest missing');
let m;
try {
  m = JSON.parse(readFileSync(MANIFEST, 'utf8'));
} catch (e) {
  die(`manifest unreadable: ${e.message}`);
}
/** `jq -r '.x // empty'` returns "" for null/absent; match that. */
const str = (v) => (typeof v === 'string' ? v : v == null ? '' : String(v));
const arr = (v) => (Array.isArray(v) ? v : []);

const PROMPT = str(m.prompt);
const REPO = str(m.repo);
const BRANCH = str(m.branch);
const GIT_TOKEN = str(m.git_token);
const RESUME = m.resume === true;
const PUKU_SESSION_ID = str(m.puku_session_id);

// ------------------------------------------------------------ HOME, clone

mkdirSync(`${SESSION}/home`, { recursive: true });
mkdirSync(BLOBS, { recursive: true });
process.env.HOME = `${SESSION}/home`;
// Overridable for the same reason SESSION is: the runner can then be
// exercised outside a guest, which is how the memory handoff finally got a
// test.
const WORKSPACE = process.env.PUKU_WORKSPACE_DIR || '/workspace';
process.chdir(WORKSPACE);

function sh(cmd, args, opts = {}) {
  return spawnSync(cmd, args, { stdio: ['ignore', 'ignore', 'ignore'], ...opts }).status === 0;
}

// Clone on first run only; the workspace volume persists across resumes.
if (REPO && !existsSync('/workspace/.git') && !RESUME) {
  const cloneUrl = GIT_TOKEN
    ? REPO.replace('https://', `https://x-access-token:${GIT_TOKEN}@`)
    : REPO;
  const args = ['clone'];
  if (BRANCH) args.push('--branch', BRANCH);
  args.push(cloneUrl, '/workspace/repo');
  if (!sh('git', args)) die('git clone failed');
  // Keep the token out of .git/config and out of the manifest on disk.
  sh('git', ['remote', 'set-url', 'origin', REPO], { cwd: '/workspace/repo' });
  process.chdir('/workspace/repo');
}
if (existsSync('/workspace/repo')) process.chdir('/workspace/repo');

// ---------------------------------------------------------------- teleport

// A transcript lifted from a local puku-cli run, so `--resume` continues that
// conversation here instead of starting an empty one.
//
// puku-cli resolves a resumed transcript to
//   $HOME/.puku-cli/projects/<sanitize($PWD)>/<session-id>.jsonl
// and looks ONLY in the directory derived from the current working directory.
// sanitizePath is `replace(/[^a-zA-Z0-9]/g, '-')`.
//
// Deliberately NOT the SDK's encodeProjectDir()/resolveSessionRoot(): those
// compute `~/.local/share/puku-cli/projects` with a bare `/`->`-` replace,
// which is a different path than the CLI actually reads. Using them here
// would file the transcript where nothing looks for it.
//
// Runs after the clone, so $PWD is final.
const IMPORTED = `${SESSION}/import/transcript.jsonl`;
if (existsSync(IMPORTED) && PUKU_SESSION_ID) {
  const slug = process.cwd().replace(/[^a-zA-Z0-9]/g, '-');
  const projectDir = join(process.env.HOME, '.puku-cli', 'projects', slug);
  mkdirSync(projectDir, { recursive: true });
  const dest = join(projectDir, `${PUKU_SESSION_ID}.jsonl`);
  // Never clobber a transcript the session already accumulated: on a resume
  // after park, the volume's own copy is newer than the imported one.
  if (!existsSync(dest)) {
    copyFileSync(IMPORTED, dest);
    logStderr(`puku-runner: imported transcript -> ${dest}`);
  }
}

// Shred the token now that the clone is done.
if (GIT_TOKEN) {
  const { git_token: _drop, ...rest } = m;
  writeFileSync(MANIFEST, JSON.stringify(rest, null, 2));
}

// ------------------------------------------------------------ egress proxy

const EGRESS_ALLOW = arr(m.egress_allow).join(',');
if (EGRESS_ALLOW && existsSync('/usr/local/bin/puku-egress-proxy')) {
  const proxyLog = openSync(`${SESSION}/proxy.log`, 'a');
  spawn('/usr/local/bin/puku-egress-proxy', [], {
    env: { ...process.env, PUKU_EGRESS_ALLOW: EGRESS_ALLOW, PUKU_EGRESS_PROXY_PORT: '3128' },
    stdio: ['ignore', proxyLog, proxyLog],
    detached: true,
  }).unref();
  await waitForPort(3128, 10, 200);
  process.env.HTTP_PROXY = 'http://127.0.0.1:3128';
  process.env.HTTPS_PROXY = 'http://127.0.0.1:3128';
  process.env.http_proxy = process.env.HTTP_PROXY;
  process.env.https_proxy = process.env.HTTPS_PROXY;
  // Allowed hosts bypass the proxy entirely; localhost must never be proxied.
  process.env.NO_PROXY = `localhost,127.0.0.1,::1,${EGRESS_ALLOW}`;
  process.env.no_proxy = process.env.NO_PROXY;
}

async function waitForPort(port, tries, delayMs) {
  for (let i = 0; i < tries; i++) {
    const ok = await new Promise((res) => {
      const s = net.connect({ port, host: '127.0.0.1' }, () => { s.end(); res(true); });
      s.on('error', () => res(false));
    });
    if (ok) return true;
    await new Promise((r) => setTimeout(r, delayMs));
  }
  return false;
}

// ----------------------------------------------------------------- skills

// Skill packs are materialized into the guest HOME by workerd before this
// runs. puku-cli discovers $HOME/.puku-cli/skills on its own, but the skills
// reference shared files as $SKILLS_ROOT/... — the office pack's pptx skill
// calls $SKILLS_ROOT/scripts/extract-text — so the variable has to exist.
process.env.SKILLS_ROOT = join(process.env.HOME, '.puku-cli', 'skills');
if (existsSync(process.env.SKILLS_ROOT)) {
  const names = readdirSync(process.env.SKILLS_ROOT).join(' ');
  logStderr(`puku-runner: skills at ${process.env.SKILLS_ROOT}: ${names}`);
}

// -------------------------------------------------------------- redaction

// Belt-and-braces secret redaction, scrubbing the stream before it leaves the
// VM. Puku credentials are opaque with no distinguishing prefix, so shape
// regexes cannot find them; match the literal values readable at startup.
const literals = [];
function addSecret(v) {
  if (typeof v === 'string' && v.length >= 12) literals.push(v);
}
addSecret(process.env.PUKU_AI_API_KEY);
addSecret(process.env.PUKU_CLI_OAUTH_TOKEN);
// The bash runner missed these two; both are real credentials in the guest env.
addSecret(process.env.ANTHROPIC_AUTH_TOKEN);
addSecret(process.env.PUKU_API_KEY);
const sessionFile = join(process.env.HOME, '.config', 'pukucode', 'session.json');
if (existsSync(sessionFile)) {
  try {
    const s = JSON.parse(readFileSync(sessionFile, 'utf8'));
    addSecret(s.accessToken);
    addSecret(s.refreshToken);
  } catch { /* not fatal */ }
}
const SHAPES = [
  [/sk-ant-[A-Za-z0-9_-]{8,}/g, '[redacted-anthropic-key]'],
  [/(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}/g, '[redacted-github-token]'],
  [/github_pat_[A-Za-z0-9_]{20,}/g, '[redacted-github-token]'],
  [/AKIA[0-9A-Z]{16}/g, '[redacted-aws-key]'],
  [/xox[baprs]-[A-Za-z0-9-]{10,}/g, '[redacted-slack-token]'],
];
function redact(s) {
  for (const lit of literals) s = s.split(lit).join('[redacted-puku-credential]');
  for (const [re, to] of SHAPES) s = s.replace(re, to);
  return s;
}

// ------------------------------------------------------------------ outbox

// The blob name must be unique across the whole outbox, not per invocation.
// The bash runner counted from 1 each time it started, so a resumed session
// could overwrite a blob from the previous run before the uploader read it.
let lineNo = countLines(EVENTS);
function countLines(p) {
  // Chunked, not readFileSync: the outbox of a long-lived session is
  // unbounded (256 KB per line, no cap on lines), and slurping it whole into
  // a 2 GB guest just to count newlines is the wrong shape of risk on the one
  // path that runs at every resume.
  let fd;
  try {
    fd = openSync(p, 'r');
  } catch {
    return 0;
  }
  try {
    const buf = Buffer.allocUnsafe(64 * 1024);
    let n = 0;
    for (;;) {
      const read = readSync(fd, buf, 0, buf.length, null);
      if (read <= 0) break;
      for (let i = 0; i < read; i++) if (buf[i] === 0x0a) n++;
    }
    return n;
  } catch {
    return 0;
  } finally {
    try { closeSync(fd); } catch { /* already closed */ }
  }
}

function emit(obj) {
  let line;
  try {
    line = JSON.stringify(obj);
  } catch {
    line = JSON.stringify({ type: 'raw', text: String(obj) });
  }
  line = redact(line);
  lineNo += 1;
  if (Buffer.byteLength(line, 'utf8') > MAX_LINE_BYTES) {
    const name = `line-${lineNo}.json`;
    try {
      writeFileSync(join(BLOBS, name), line);
    } catch (e) {
      logStderr(`puku-runner: blob spill failed: ${e.message}`);
    }
    line = JSON.stringify({ type: 'truncated', blob: name, bytes: Buffer.byteLength(line, 'utf8') });
  }
  appendFileSync(EVENTS, `${line}\n`);
}

// ------------------------------------------------------------------ input

// stdin fifo: opened read-write so the reader never sees EOF between the
// host's one-shot `cat >>` writes.
if (!existsSync(FIFO)) sh('mkfifo', [FIFO]);
const fifoFd = openSync(FIFO, 'r+');

/** Runner-private control lines the host sends; never forwarded to the CLI. */
const PLATFORM_INTERRUPT = 'platform.interrupt';
const PLATFORM_ANSWER = 'platform.answer';

let onInterrupt = null;

/** request_id -> resolve(answerLine), for asks canUseTool is blocked on. */
const pendingAsks = new Map();

function resolveAsk(line) {
  const id = line?.request_id;
  const resolve = id && pendingAsks.get(id);
  if (!resolve) {
    logStderr(`puku-runner: answer for unknown question ${id}`);
    return;
  }
  pendingAsks.delete(id);
  resolve(line);
}
let fifoPump = null;
let fifoRl = null;

/**
 * Reading the FIFO is fussier than it looks, and two obvious approaches fail:
 *
 *   fs.createReadStream(fd) — reads fine, but keeps the process alive *through*
 *     `process.exit()`; the runner reaches the exit call and sits there.
 *   new net.Socket({fd})    — unrefs and exits cleanly, but never yields data
 *     for a FIFO, so follow-up turns are silently dropped.
 *
 * So: hand our already-open fd to `cat` as its stdin and read cat's pipe. A
 * child stdio pipe is a proper libuv stream (unref-able, killable), and
 * because our fd was opened read-write there is always a writer, so cat never
 * sees EOF between the host's one-shot `cat >>` deliveries.
 */
function shutdownInput() {
  try { fifoRl?.close(); } catch { /* already closed */ }
  try { fifoPump?.kill('SIGKILL'); } catch { /* already dead */ }
  try { closeSync(fifoFd); } catch { /* already gone */ }
}

async function* inputStream() {
  // With --input-format stream-json, puku-cli ignores positional prompts and
  // reads user turns from stdin — so the task goes in as the first message.
  //
  // Gated on the prompt alone, NOT on `resume`. A resumed session usually has
  // no prompt, but a teleported one does: it carries the follow-up turn that
  // motivated moving to the cloud. Skipping delivery there left the agent
  // parked on empty stdin forever, looking "running" while doing nothing.
  if (PROMPT) {
    yield { type: 'user', message: { role: 'user', content: [{ type: 'text', text: PROMPT }] } };
  }
  // Do NOT unref the pump or its pipe: unref'd, libuv stops scheduling reads
  // and follow-up turns are silently dropped. `shutdownInput` kills it instead,
  // which is what lets the process exit.
  fifoPump = spawn('cat', [], { stdio: [fifoFd, 'pipe', 'ignore'] });
  fifoRl = createInterface({ input: fifoPump.stdout, crlfDelay: Infinity });
  for await (const raw of fifoRl) {
    const line = raw.trim();
    if (!line) continue;
    let obj;
    try {
      obj = JSON.parse(line);
    } catch {
      logStderr(`puku-runner: dropping unparseable input line: ${line.slice(0, 200)}`);
      continue;
    }
    if (obj && obj.type === PLATFORM_ANSWER) {
      resolveAsk(obj);
      continue;
    }
    if (obj && obj.type === PLATFORM_INTERRUPT) {
      // Interrupt must go through the SDK, not a signal: the SDK owns the
      // child, and SIGINT-ing it behind the SDK's back tears down the
      // transport instead of ending one turn.
      if (onInterrupt) onInterrupt();
      continue;
    }
    yield obj;
  }
}

// ---------------------------------------------------------------- options

/**
 * The SDK validates permissionMode against four values, but the platform's
 * enum has six, and `bypassPermissions` — our default ceiling — is rejected
 * unless the caller also opts in explicitly. Verified against 1.8.49:
 * both spellings of the god-mode flag are accepted by the binary.
 */
function permissionOptions(mode) {
  if (!mode) {
    // No permission_mode means the platform did not express one; keep the
    // previous behaviour so an older controld works through one release.
    return { extraArgs: { 'god-mode': null } };
  }
  if (mode === 'bypassPermissions') {
    return { permissionMode: mode, allowDangerouslySkipPermissions: true };
  }
  if (mode === 'dontAsk' || mode === 'auto') {
    // Legal for puku-cli, rejected by the SDK's validator.
    return { extraArgs: { 'permission-mode': mode } };
  }
  return { permissionMode: mode };
}

function mcpServers(list) {
  const entries = arr(list);
  if (entries.length === 0) return undefined;
  const out = {};
  // The Authorization header holds the literal `${PUKU_API_KEY}`, which
  // puku-cli expands from the env — no credential is written to disk.
  for (const s of entries) out[s.name] = { type: s.type, url: s.url, headers: s.headers };
  return out;
}

const ALLOWED_TOOLS = arr(m.allowed_tools);
const DISALLOWED_TOOLS = arr(m.disallowed_tools);

/**
 * Second line of defence on the session's tool policy.
 *
 * The same lists go to the CLI as --allowed-tools/--disallowed-tools, so this
 * is belt-and-braces — but a published skill's frontmatter can carry its own
 * `allowed-tools`, and whether that can widen the CLI's view is still
 * unverified. Refusing here costs nothing and does not depend on the answer.
 *
 * Its reach is limited and worth stating plainly: canUseTool only runs when
 * the CLI decides to ask. A tool auto-approved by --allowed-tools, or every
 * tool under bypassPermissions, never reaches this function. So this narrows
 * the hole, it does not close it. Closing it needs the empirical answer about
 * skill frontmatter, on a real session.
 */
function policyDeny(toolName) {
  if (DISALLOWED_TOOLS.includes(toolName)) {
    return `${toolName} is disallowed for this session`;
  }
  if (ALLOWED_TOOLS.length > 0 && !ALLOWED_TOOLS.includes(toolName)) {
    return `${toolName} is not in this session's allowed tools`;
  }
  return null;
}

/**
 * Answer a permission ask by putting the question to the platform and waiting.
 *
 * This is what lets the control_request envelope disappear from the host: the
 * CLI's ask is answered here, in the process that actually holds the question
 * object, instead of being projected across two services and rebuilt blind.
 *
 * The wait is unbounded on purpose. A cloud session blocked on a human is the
 * feature, not a hang — puku-cli itself waits indefinitely, and the platform
 * quadruples the idle timeout while a question is outstanding.
 */
async function askPlatform(toolName, input, opts) {
  const refusal = policyDeny(toolName);
  if (refusal) {
    // Refused by policy, so do not wake a human to rubber-stamp it.
    emit({ type: 'platform.tool_denied', tool_name: toolName, reason: refusal });
    return { behavior: 'deny', message: refusal };
  }
  const requestId = opts?.requestId || randomUUID();
  const answer = new Promise((resolve) => pendingAsks.set(requestId, resolve));
  emit({
    type: 'platform.question',
    request_id: requestId,
    tool_name: toolName,
    tool_use_id: opts?.toolUseID ?? null,
    input: input ?? {},
  });
  const a = await answer;

  if (a.decision === 'deny') {
    return { behavior: 'deny', message: a.message || 'declined by the user' };
  }

  // AskUserQuestion-shaped asks carry questions whose answers are keyed by
  // each question's `header`, falling back to Q1, Q2, … — the CLI renders
  // them back to the model as "<header>"="<answer>". A bare permission ask
  // ("may I run Bash?") has no questions and is satisfied by allow alone.
  const updatedInput = { ...(input ?? {}) };
  const questions = Array.isArray(input?.questions) ? input.questions : null;
  if (questions) {
    const answers = {};
    questions.forEach((q, i) => {
      const header = (typeof q?.header === 'string' && q.header) || `Q${i + 1}`;
      const value = a.answers?.[header] ?? a.answer;
      if (value != null) answers[header] = String(value);
    });
    if (Object.keys(answers).length === 0) {
      // Allowing with no answers makes the CLI tell the model "User has
      // answered your questions: ." and the model invents a value.
      return { behavior: 'deny', message: 'answer required: this question expects one' };
    }
    updatedInput.answers = answers;
  }
  return { behavior: 'allow', updatedInput };
}

let child = null;
/**
 * Resolves with puku-cli's real exit code.
 *
 * The SDK suppresses the exit error once it has seen a terminal `result`, so
 * a CLI that reports a result and *then* fails looks like a clean run. The
 * bash runner used `${PIPESTATUS[0]}` and did not have that blind spot, and
 * the host maps a non-zero code to `failed` — so swallowing it would silently
 * turn failures into completions.
 */
let childExited = Promise.resolve(null);

/**
 * Spawn the CLI ourselves, for two things the SDK will not do.
 *
 * 1. stderr. `Options.stderr` is declared but not wired, and losing it would
 *    leave /session/runner.stderr empty -- the only in-guest debugging
 *    surface there is.
 *
 * 2. Tolerating a non-JSON line on stdout. The SDK's NDJSON parser throws on
 *    the first one and takes the whole iterator with it: verified, one
 *    `WARNING: ...` line turned a healthy session into
 *    "failed, NDJSON parse error at line 4" with no terminal result. The
 *    bash runner piped bytes and never cared. A banner, a deprecation
 *    notice or a stray progress line from any future puku-cli would
 *    therefore kill every session, so filter before the parser sees it.
 *
 *    Unparseable lines are preserved as `{"type":"raw","text":...}` rather
 *    than dropped -- the same shape workerd already produces for a line it
 *    cannot parse, so nothing downstream needs to learn a new one, and the
 *    evidence survives for whoever debugs it.
 */
function spawnFiltered(binaryPath, args, opts) {
  const c = spawn(binaryPath, args, opts);
  child = c;
  childExited = new Promise((res) => c.once('close', (code) => res(code)));

  if (c.stdout) {
    const clean = new PassThrough();
    const rl = createInterface({ input: c.stdout, crlfDelay: Infinity });
    rl.on('line', (line) => {
      const t = line.trim();
      if (!t) return;
      try {
        JSON.parse(t);
        clean.write(`${line}\n`);
      } catch {
        logStderr(`puku-runner: non-JSON stdout line kept as raw: ${t.slice(0, 200)}`);
        clean.write(`${JSON.stringify({ type: 'raw', text: t })}\n`);
      }
    });
    rl.on('close', () => clean.end());
    // The SDK reads `child.stdout`; hand it the filtered stream instead.
    Object.defineProperty(c, 'stdout', { value: clean, configurable: true });
  }

  if (c.stderr) {
    c.stderr.setEncoding('utf8');
    c.stderr.on('data', (d) => {
      // The SDK warns on every query that 3.0.6 is not in its manifest's
      // testedWrapperVersions. It is unsuppressible and says nothing useful.
      const kept = String(d)
        .split('\n')
        .filter((l) => l && !l.startsWith('[compat] WARN:'))
        .join('\n');
      if (kept) logStderr(kept);
    });
  }
  return c;
}

const perm = permissionOptions(str(m.permission_mode));
const options = {
  pathToPukuCliExecutable: CLI,
  cwd: process.cwd(),
  includePartialMessages: true,
  strictMcpConfig: true,
  spawnPukuCliProcess: spawnFiltered,
  ...perm,
  // --permission-prompt-tool stdio is added by the SDK itself, because
  // canUseTool is always registered.
  extraArgs: { ...(perm.extraArgs ?? {}) },
  // NO settingSources: it emits `--setting-source`, which 1.8.49 does not
  // have, and the CLI already discovers ~/.puku-cli/skills without it.
};
if (str(m.model)) options.model = str(m.model);
if (m.max_budget_usd != null) options.maxBudgetUsd = Number(m.max_budget_usd);
if (m.max_turns != null) options.maxTurns = Number(m.max_turns);
if (arr(m.allowed_tools).length) options.allowedTools = arr(m.allowed_tools);
if (arr(m.disallowed_tools).length) options.disallowedTools = arr(m.disallowed_tools);
const servers = mcpServers(m.mcp_servers);
if (servers) options.mcpServers = servers;

// Structured output, for runs a program consumes rather than a person reads.
//
// Deliberately --json-schema via extraArgs, NOT the SDK's `outputFormat`:
// that option also appends `--output-format json`, and the CLI rejects the
// pair outright — "--input-format=stream-json requires
// output-format=stream-json" — which would take the entire event stream with
// it. Verified against 1.8.49 both ways.
// Memory. The preamble the host resolved for this repository, delivered as
// PROJECT CONTEXT rather than as a system prompt.
//
// This runner did not implement it at all to begin with: workerd writes
// `memory_preamble` into the manifest and this one -- the DEFAULT -- silently
// dropped it, so every memory the platform learned reached the guest and was
// thrown away here. Fixing that got the page as far as
// --append-system-prompt-file, and it still never reached the model, because
// the gateway discards the caller's system prompt and substitutes its own.
// Measured, with nothing of ours in the path:
//
//   system: 47,526 bytes  ->  input_tokens: 796
//   system: absent        ->  input_tokens: 796
//
// Identical, so the system prompt is not in the bill and therefore not in the
// prompt the model sees. The same text in the user turn is obeyed exactly. So
// --append-system-prompt[-file], --system-prompt and custom agent prompts are
// all inert on that gateway; we were posting into the one envelope it throws
// away.
//
// Project context survives, and this fork names that file PUKU.md -- proven by
// token accounting rather than by reading the model's mood: a ~20k-token
// PUKU.md moves input+cache_read from 15,915 to 35,978, while CLAUDE.md and
// AGENTS.md move it not at all. It is also what everyone else does; ChatGPT and
// Claude Code both deliver memory as conversation context, never as a system
// prompt.
//
// If the gateway is ever fixed to merge the caller's system prompt, revert to
// `options.extraArgs['append-system-prompt-file'] = memoryFile` and stop
// writing PUKU.md -- but do ONE of them, because a page delivered twice is
// worse than a page delivered once.
// DIAGNOSTIC (Wave 1 / Task 1.1, removed at 1.7): the failure mode in
// MEMORY-RUNBOOK.md is "stderr says NNN bytes, host workspace is empty" —
// which is exactly what we see today, but the field that distinguishes EACCES
// from ENOENT from EISDIR is missing. This is the cheapest way to put it
// there. Safe to leave in until the end-to-end test passes.
process.stderr.write(
  `runner: pre-memory cwd=${process.cwd()} workspace=${WORKSPACE} session=${SESSION}\n`);

const memoryPreamble = str(m.memory_preamble);
if (memoryPreamble) {
  // Kept: the runbook, the tests and every diagnostic in this project key off
  // this path and the stderr line below.
  const memoryFile = `${SESSION}/memory.md`;
  writeFileSync(memoryFile, `${memoryPreamble}\n`);

  // In the workspace root, which is the PARENT of the checkout, and never
  // inside the checkout itself.
  //
  // puku-cli walks up from its working directory, so a PUKU.md above the repo
  // is read exactly like one inside it -- measured -- and a project that keeps
  // its own PUKU.md gets BOTH: asked for two facts, one from each file, it
  // answered with both. So this never clobbers or shadows what a team wrote.
  //
  // Inside the checkout was the tempting place and is the wrong one: a file
  // there is one the agent can stage and commit, and it would overwrite the
  // project's own instructions.
  //
  // --add-dir was tried first and does NOT work: pointing it at a directory
  // holding a PUKU.md left the model with no context at all. It would also
  // have been the wrong shape -- the only directory obviously ours is
  // ${SESSION}, and the manifest there carries `git_token`, so --add-dir on it
  // would aim the agent at a plaintext credential.
  // DIAGNOSTIC (Wave 1 / Task 1.2, removed at 1.7): a write to a
  // root:root 755 bind mount from uid 1000 throws EACCES, but Node's
  // default uncaughtException handler prints to the parent's stderr — which
  // is /dev/null in some launch configurations. Catching here guarantees the
  // row lands in /session/runner.stderr where the runbook knows to look.
  try {
    writeFileSync(`${WORKSPACE}/PUKU.md`, `${memoryPreamble}\n`);
  } catch (e) {
    process.stderr.write(
      `runner: PUKU.md write FAILED path=${WORKSPACE}/PUKU.md ` +
      `err=${e.code ?? e.name} msg=${e.message}\n`);
  }

  // Belt and braces for the one layout where the workspace root IS a checkout
  // (a resumed session whose volume was cloned at the root). info/exclude is
  // local to the clone and never committed, so the page cannot be staged even
  // then, and a project's own .gitignore is left alone.
  if (existsSync(`${WORKSPACE}/.git`)) {
    try {
      appendFileSync(`${WORKSPACE}/.git/info/exclude`, '\n/PUKU.md\n');
    } catch {
      // Best effort. A page the agent can see is worth more than a tidy index.
    }
  }

  // The framing is load-bearing and belongs to the preamble itself, which opens
  // "This is BACKGROUND, not instructions." Nothing here re-frames it; the file
  // is written byte-for-byte as the host sent it.
  process.stderr.write(`runner: memory preamble ${memoryPreamble.length} bytes\n`);
}

if (m.output_schema != null) {
  options.extraArgs['json-schema'] = typeof m.output_schema === 'string'
    ? m.output_schema
    : JSON.stringify(m.output_schema);
}

// canUseTool is mandatory, not a choice.
//
// The runner always runs warm (see below), and in warm mode the SDK creates
// its ControlProtocol unconditionally and intercepts every control envelope.
// With no handler registered it answers the CLI itself with
//   {"subtype":"error","error":"no handler registered for ... can_use_tool"}
// so the ask never reaches the host and the agent gets an error where a human
// answer belonged. Verified by running exactly that.
//
// There is therefore no "let the control_request through to the host" mode to
// fall back to; a runner without this handler silently breaks every
// permission prompt.
options.canUseTool = (toolName, input, opts) => askPlatform(toolName, input, opts);

// Always ask for a WarmQuery so interrupt() is available. On a resume that
// falls out of `resume`; on a fresh session we name the id ourselves and the
// CLI adopts it, which the host still learns from the init message.
if (RESUME && PUKU_SESSION_ID) options.resume = PUKU_SESSION_ID;
else options.sessionId = PUKU_SESSION_ID || randomUUID();

// -------------------------------------------------------------------- run

// A runner pointed at anything other than the real binary must be
// impossible to miss. The test fixture returns canned output: sessions
// "succeed", cost nothing and look healthy, so a PUKU_RUNNER_CMD override
// left behind after a test run is silent and indefinite. Say so in the
// outbox, where the dashboard and `puku cloud attach` will show it, not just
// in a log file nobody opens.
if (CLI !== '/usr/bin/puku-cli') {
  const warning = `agent binary is ${CLI}, not /usr/bin/puku-cli — this session is NOT running the real agent`;
  logStderr(`puku-runner: WARNING: ${warning}`);
  emit({ type: 'platform.warning', warning, cli_path: CLI });
}

const { query } = await import(SDK);

let exitCode = 0;
try {
  const q = query({ prompt: inputStream(), options });
  onInterrupt = () => {
    Promise.resolve(q.interrupt?.()).catch((e) =>
      logStderr(`puku-runner: interrupt failed: ${e?.message ?? e}`),
    );
  };
  for await (const msg of q) emit(msg);
} catch (e) {
  // A non-zero exit with no terminal result surfaces here. The host's own
  // completion signal is the exec.exited marker the launch wrapper appends,
  // so the job here is only to carry puku-cli's code out.
  logStderr(`puku-runner: ${e?.message ?? e}`);
  if (e?.stderr) logStderr(String(e.stderr).slice(0, 4096));
  exitCode = typeof e?.exitCode === 'number' ? e.exitCode : 1;
} finally {
  shutdownInput();
}

// Always prefer the child's own code over "the iterator ended without
// throwing". Bounded, because a wedged child must not wedge the runner.
const observed = await Promise.race([
  childExited,
  new Promise((res) => setTimeout(() => res(null), 5000)),
]);
if (typeof observed === 'number' && observed !== 0) exitCode = observed;

process.exit(exitCode);

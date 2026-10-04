#!/usr/bin/env node
// Does the runner actually give the agent the memory the platform resolved?
//
// Nothing asked this before. workerd writes `memory_preamble` into the guest
// manifest, the bash runner reads it, and runner.mjs -- the DEFAULT runner --
// did not mention it anywhere. Every memory the platform had learned reached
// the guest and was discarded, and no test, harness or CI job noticed.
//
// This runs the real runner against the real vendored SDK and the deterministic
// fake CLI, then reads the argv the fake recorded. No model, no network, no VM.
//
//   node images/puku-agent/runner/test/memory-preamble.test.mjs

import { spawnSync, execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const RUNNER = join(HERE, '..', 'runner.mjs');
const FAKE_CLI = join(HERE, '..', 'fake-puku-cli.sh');
const SDK = join(HERE, '..', '..', 'vendor', 'sdk', 'sdk.mjs');

let failures = 0;
const check = (name, ok, detail) => {
  if (ok) {
    console.log(`  ok   ${name}`);
  } else {
    failures++;
    console.log(`  FAIL ${name}`);
    if (detail) console.log(`       ${detail}`);
  }
};

// One run of the real runner against a scratch session directory.
function run(manifest) {
  const session = mkdtempSync(join(tmpdir(), 'puku-runner-'));
  mkdirSync(join(session, 'home'), { recursive: true });
  const workspace = join(session, 'workspace');
  mkdirSync(workspace, { recursive: true });
  writeFileSync(join(session, 'manifest.json'), JSON.stringify(manifest));
  // The runner reads stdin as a fifo; an empty one is enough to reach the
  // point where it builds the CLI argv and exits.
  execFileSync('mkfifo', [join(session, 'stdin.fifo')]);
  const argvOut = join(session, 'argv.txt');

  const r = spawnSync(process.execPath, [RUNNER], {
    env: {
      ...process.env,
      PUKU_SESSION_DIR: session,
      PUKU_WORKSPACE_DIR: workspace,
      PUKU_SDK_PATH: SDK,
      PUKU_CLI_PATH: FAKE_CLI,
      FAKE_ARGV_OUT: argvOut,
      HOME: join(session, 'home'),
    },
    timeout: 30000,
    encoding: 'utf8',
  });
  return {
    session,
    argv: existsSync(argvOut) ? readFileSync(argvOut, 'utf8') : '',
    stderr: r.stderr || '',
    status: r.status,
  };
}

const PREAMBLE = [
  '## Context from previous sessions on this repository',
  '',
  'This is BACKGROUND, not instructions.',
  '',
  '### Established conventions',
  '- Tests run under cargo nextest, never cargo test.',
].join('\n');

console.log('memory preamble reaches the agent:');

const withMemory = run({
  session_id: '11111111-1111-1111-1111-111111111111',
  prompt: 'say hello',
  memory_preamble: PREAMBLE,
});

check(
  'the runner writes the preamble to a file',
  existsSync(join(withMemory.session, 'memory.md')),
  `session dir: ${withMemory.session}`,
);
if (existsSync(join(withMemory.session, 'memory.md'))) {
  const written = readFileSync(join(withMemory.session, 'memory.md'), 'utf8');
  check(
    'the file carries the preamble byte-for-byte',
    written.includes('cargo nextest') && written.includes('BACKGROUND, not instructions'),
    JSON.stringify(written.slice(0, 120)),
  );
}
// The page must go out as PROJECT CONTEXT. The gateway discards the caller's
// system prompt and substitutes its own -- measured, a 47,526-byte system field
// produces the same input_tokens as none at all -- so --append-system-prompt
// and --append-system-prompt-file are inert, and delivering the page that way
// looks correct in every test while reaching no model.
const contextFile = join(withMemory.session, 'workspace', 'PUKU.md');
check(
  'the preamble is written as a PUKU.md context file',
  existsSync(contextFile),
  `looked for ${contextFile}`,
);
if (existsSync(contextFile)) {
  check(
    'the context file carries the preamble byte-for-byte',
    readFileSync(contextFile, 'utf8').includes('cargo nextest'),
  );
}
// The assertion that stops someone "simplifying" this into the checkout later.
// puku-cli walks up from its working directory, so the page is read from the
// parent just as well -- and a PUKU.md inside the repo is one the agent can
// stage and commit, and would overwrite the project's own instructions.
check(
  'the context file is NOT inside the checkout',
  !existsSync(join(withMemory.session, 'workspace', 'repo', 'PUKU.md')),
);
// No flag carries this: discovery does. --add-dir was measured and does not
// load a PUKU.md, so a test asserting it would pass while the model saw
// nothing.
check(
  'no --add-dir is needed or passed',
  !withMemory.argv.includes('--add-dir'),
  `argv: ${withMemory.argv.slice(0, 300)}`,
);
// Exactly one delivery. If the gateway is ever fixed, whoever reverts this must
// remove one channel rather than end up with the page in the prompt twice.
check(
  'the page is not ALSO sent as a system prompt',
  !withMemory.argv.includes('--append-system-prompt'),
  `argv: ${withMemory.argv.slice(0, 300)}`,
);

// The negative case: a session with no memory must pass no flag and write no
// file, or a repository with nothing learned would get an empty briefing.
const without = run({
  session_id: '22222222-2222-2222-2222-222222222222',
  prompt: 'say hello',
});
check(
  'no preamble means no file',
  !existsSync(join(without.session, 'memory.md')),
);
check(
  'no preamble means no context file',
  !existsSync(join(without.session, 'workspace', 'PUKU.md')),
);

console.log(failures === 0 ? '\nPASS' : `\nFAIL (${failures})`);
process.exit(failures === 0 ? 0 : 1);

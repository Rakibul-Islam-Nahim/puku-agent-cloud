# Vendored third-party code

Committed on purpose, the way Go's `vendor/` is.

## Why this is in the repo rather than fetched at build time

`puku-agent-sdk` **3.0.6** is what the guest runner is built and tested
against, and it is not obtainable from a package manager: npm publishes
3.0.0, which silently ignores `spawnPukuCliProcess`. The runner uses that
option for two things it cannot do without — capturing the CLI's stderr into
`/session/runner.stderr`, and reading the CLI's real exit code, without which
a failing session is recorded as completed.

So the choice was: make every deployment clone a second repository before it
can build a guest image, or commit ~450 KB of a pinned, tested artefact.
Committing it wins. The bytes that ship are the bytes that were tested, a
fresh clone can build the image, and there is no build-time network
dependency.

## What is here

    sdk/sdk.mjs         the SDK itself, dependency-free
    sdk/manifest.json   read by checkCompatibility(); without it the harness
                        check degrades to harnessOk:false instead of
                        verifying the schema
    sdk/package.json    so the SDK can report its own version rather than
                        logging 0.0.0-unknown in every compat warning
    sdk/VERSION         provenance, for humans

`sdk.d.ts` is deliberately absent: it is types only, the guest never reads
it, and it doubled the size of this directory.

## Updating

    ./deploy/scripts/vendor-sdk.sh --local ../../puku-cli-sdk   # or --npm <version>
    git add images/puku-agent/vendor && git commit

Then re-run the full-system sweep and, if the permission or exit-code paths
are touched, `sdk-gate.sh`. The runner leans on SDK behaviour that its own
README documents incorrectly in at least three places, so a version bump is
a testing event, not a bump.

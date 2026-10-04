-- Layered memory (puku-memory-service).
--
-- The memory state itself lives in the memory service's own database. What
-- agent-cloud keeps is the minimum needed to (a) opt in, (b) pin the preamble
-- to a session, and (c) keep serving a preamble when the memory service is
-- unreachable.

-- Opt-in per org: enabling this sends distilled transcript text to another
-- service, and (behind it) to Cloudflare. Default off, deliberately.
ALTER TABLE orgs ADD COLUMN IF NOT EXISTS memory_enabled boolean NOT NULL DEFAULT false;

-- Resolved once at first dispatch and pinned. A parked session resumes with
-- `--resume` plus a freshly built --append-system-prompt-file; if the profile
-- changed while it slept, the agent's own history and its system prompt would
-- disagree about what it knows. Storing the text removes that entire class of
-- unreproducible behaviour.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS memory_profile_id  text;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS memory_preamble    text;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS memory_ingested_at timestamptz;

-- Per-session and per-schedule escape hatch: a run that handles secrets, or
-- reads untrusted input, should not be indexed.
ALTER TABLE sessions  ADD COLUMN IF NOT EXISTS memory_opt_out boolean NOT NULL DEFAULT false;
ALTER TABLE schedules ADD COLUMN IF NOT EXISTS memory_opt_out boolean NOT NULL DEFAULT false;

-- The last preamble we successfully fetched, per profile.
--
-- This is what keeps the degradation ladder intact across the network hop to
-- the memory service: when it is slow or down, dispatch serves the cached
-- preamble instead of nothing. Losing the memory service costs freshness, not
-- availability.
CREATE TABLE IF NOT EXISTS memory_preamble_cache (
    profile_id text PRIMARY KEY,
    preamble   text NOT NULL,
    etag       text,
    fetched_at timestamptz NOT NULL DEFAULT now()
);

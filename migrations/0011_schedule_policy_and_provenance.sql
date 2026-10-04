-- P1/P2: teleported sessions, and schedules that carry the same policy an
-- interactive session does.

-- Provenance for a session lifted out of a local puku-cli run, so the UI
-- can say "continued from your laptop" instead of showing a resumed
-- session with no visible history.
ALTER TABLE sessions ADD COLUMN imported_from text;
-- Where the imported transcript lives, matching the two ImportRef carriers:
-- an object-storage reference, or the bytes themselves on a deployment with
-- no object storage configured (capped at MAX_INLINE_IMPORT_BYTES).
ALTER TABLE sessions ADD COLUMN import_ref    text;
ALTER TABLE sessions ADD COLUMN import_inline text;

-- A scheduled run used to be a prompt and nothing else: no tool policy, no
-- connectors, no turn budget. That made cron strictly less capable (and
-- less constrainable) than the same task run interactively.
ALTER TABLE schedules ADD COLUMN allowed_tools    text[] NOT NULL DEFAULT '{}';
ALTER TABLE schedules ADD COLUMN disallowed_tools text[] NOT NULL DEFAULT '{}';
ALTER TABLE schedules ADD COLUMN permission_mode  text
    CHECK (permission_mode IN ('default','plan','acceptEdits','dontAsk','auto','bypassPermissions'));
ALTER TABLE schedules ADD COLUMN max_turns        int;
ALTER TABLE schedules ADD COLUMN connectors       boolean NOT NULL DEFAULT true;
-- Unattended runs park quickly by default; make it configurable per schedule.
ALTER TABLE schedules ADD COLUMN idle_timeout_s   int;

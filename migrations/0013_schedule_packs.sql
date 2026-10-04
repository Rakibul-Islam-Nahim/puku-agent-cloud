-- A scheduled run could not name skill packs: `fire()` passed an empty list
-- and the org defaults applied. That makes cron strictly less capable than
-- the equivalent `puku cloud run --pack office`, which is backwards for the
-- unattended case a nightly report generator actually needs.
--
-- Empty array keeps the old behaviour (fall through to org defaults), so
-- existing schedules are unaffected.
ALTER TABLE schedules ADD COLUMN IF NOT EXISTS packs text[] NOT NULL DEFAULT '{}';

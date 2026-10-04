-- A JSON Schema the session's final answer must satisfy.
--
-- For runs whose output is consumed by a program rather than read by a
-- person: a nightly job should be able to return {"verdict":"pass"} that the
-- caller can branch on, instead of prose someone has to scrape.
--
-- Nullable, so every existing session and schedule is unaffected.
ALTER TABLE sessions  ADD COLUMN IF NOT EXISTS output_schema jsonb;
ALTER TABLE schedules ADD COLUMN IF NOT EXISTS output_schema jsonb;

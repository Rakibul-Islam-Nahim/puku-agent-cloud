-- Which skill packs a session runs with.
--
-- Stored rather than resolved once, so a resume re-resolves against the
-- registry: a parked session that wakes up a week later should get the
-- pack version that is current then, not a presigned URL that expired.
ALTER TABLE sessions ADD COLUMN packs text[] NOT NULL DEFAULT '{}';

-- Raise the per-org concurrent-session quota from 5 to 100.
ALTER TABLE quotas ALTER COLUMN max_concurrent_sessions SET DEFAULT 100;

-- Bump existing orgs still on the old default; leave custom values alone.
UPDATE quotas SET max_concurrent_sessions = 100 WHERE max_concurrent_sessions = 5;

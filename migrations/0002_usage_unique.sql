-- One usage record per session (written at terminal transition); the unique
-- index makes the insert idempotent under worker frame redelivery.
CREATE UNIQUE INDEX usage_records_session_idx ON usage_records (session_id);

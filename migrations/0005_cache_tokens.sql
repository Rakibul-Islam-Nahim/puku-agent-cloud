-- puku-cli reports prompt-cache traffic separately from fresh input tokens.
-- Only input_tokens/output_tokens were captured, so every cached read — the
-- bulk of a long coding session — went unrecorded.
ALTER TABLE sessions
    ADD COLUMN cache_read_tokens  bigint NOT NULL DEFAULT 0,
    ADD COLUMN cache_write_tokens bigint NOT NULL DEFAULT 0;

ALTER TABLE usage_records
    ADD COLUMN cache_read_tokens  bigint NOT NULL DEFAULT 0,
    ADD COLUMN cache_write_tokens bigint NOT NULL DEFAULT 0;

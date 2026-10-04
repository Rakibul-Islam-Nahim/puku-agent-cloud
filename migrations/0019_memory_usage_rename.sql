-- Rename the memory_usage columns to say what they count.
--
-- They were named `recalls` / `recall_failures` / `recall_ms` and counted
-- PREAMBLE FETCHES: two indexed Postgres reads on the memory service, no
-- Cloudflare, no model, no retrieval of any kind. The name dated from when
-- serving really did recall; that path was removed, and the column names
-- followed nothing.
--
-- It reached operators. `puku cloud memory status` printed "today: N recalls",
-- which invited reading a Cloudflare bill into a number that has nothing to do
-- with one.
ALTER TABLE memory_usage RENAME COLUMN recalls         TO preamble_fetches;
ALTER TABLE memory_usage RENAME COLUMN recall_failures TO preamble_failures;
ALTER TABLE memory_usage RENAME COLUMN recall_ms       TO preamble_ms;

-- Stop a resumed session from under-reporting what it spent.
--
-- puku-cli reports a *cumulative* total for its session on every `result`
-- line, so `SET cost_usd = <reported>` is right while one sandbox is running:
-- each report restates the running total and the last one wins.
--
-- A resume breaks that. It boots a fresh sandbox with a fresh puku-cli
-- process, whose counter starts over for the resumed transcript -- so its
-- first report overwrites everything the previous run spent. Measured on a
-- document session: $1.722 in the first run, $1.510 reported by the second,
-- recorded as $1.510. Roughly half the real spend, silently discarded.
--
-- Naive accumulation (`cost_usd = cost_usd + <reported>`) is worse: it
-- double-counts every report inside a single sandbox.
--
-- So each dispatch snapshots the totals so far, and usage is written as
-- baseline + whatever this sandbox reports. Correct in both directions, and
-- the worker stays dumb -- it keeps sending the only number it has.
--
-- Existing rows default to 0, which reproduces today's behaviour for them
-- rather than retroactively restating anyone's bill.

ALTER TABLE sessions
    ADD COLUMN cost_baseline_usd    numeric NOT NULL DEFAULT 0,
    ADD COLUMN tokens_in_baseline   bigint  NOT NULL DEFAULT 0,
    ADD COLUMN tokens_out_baseline  bigint  NOT NULL DEFAULT 0,
    ADD COLUMN cache_read_baseline  bigint  NOT NULL DEFAULT 0,
    ADD COLUMN cache_write_baseline bigint  NOT NULL DEFAULT 0;

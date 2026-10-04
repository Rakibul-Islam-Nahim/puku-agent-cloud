#!/usr/bin/env bash
# Repair the cost of sessions that were resumed before migration 0016.
#
# puku-cli restates its session's *cumulative* cost on every `result`, so
# `SET cost_usd = <reported>` was right while one sandbox ran. A resume boots
# a fresh sandbox whose counter starts over, and its first report overwrote
# everything the previous run had spent. 0016 fixed that going forward with a
# per-dispatch baseline, and deliberately left history alone rather than
# restating anyone's bill as a side effect of a schema change. This is the
# opt-in restatement.
#
# Reconstruction: every `session.usage` platform event carries the
# cumulative-per-sandbox total, and controld persisted all of them. Partition
# a session's events wherever the value *decreases* -- a counter reset is a
# new sandbox -- take the last value of each partition, and sum. That is the
# true total.
#
# Only sessions whose events are still in Postgres can be repaired. Events are
# deleted at archival (PUKU_RETENTION_DAYS, default 14); the ndjson survives in
# object storage, but reading it back is not what this script does. It reports
# what it had to skip rather than quietly under-repairing.
#
# Usage:
#   backfill-resumed-cost.sh              # dry run: print the table, change nothing
#   backfill-resumed-cost.sh --apply      # write it
#
# Env: PGURL, or the PG* variables psql already understands.
set -euo pipefail

APPLY=0
[ "${1:-}" = "--apply" ] && APPLY=1

PGURL="${PGURL:-postgres://puku:puku@127.0.0.1:5432/puku_cloud}"
psql_q() { psql "$PGURL" -v ON_ERROR_STOP=1 "$@"; }

# Migration 0016 must be in place first. It runs at controld startup, so a box
# that has not been redeployed since the fix still has the old schema -- and
# the apply step would otherwise fail halfway with a raw "column does not
# exist", after printing a table that suggests it was about to work.
if ! psql_q -tAc "SELECT 1 FROM information_schema.columns \
      WHERE table_name = 'sessions' AND column_name = 'cost_baseline_usd'" | grep -q 1; then
  echo "sessions.cost_baseline_usd is missing: this database has not run migration" >&2
  echo "0016_usage_baseline. Deploy controld first (migrations run at startup), then" >&2
  echo "re-run this. Repairing the history before the forward fix is in place would" >&2
  echo "leave the next resume of these same sessions wrong again." >&2
  exit 1
fi

# One CTE, used for both the report and the update, so the two cannot drift.
#
# `gen` counts how many resets precede each row, which labels the sandbox
# generation. lag() over the ordered events is the reset detector: a drop in a
# cumulative counter cannot happen within one sandbox.
read -r -d '' RECONSTRUCT <<'SQL' || true
WITH usage_events AS (
    SELECT session_id,
           seq,
           (payload->>'cost_usd')::numeric          AS cost,
           (payload->>'tokens_in')::bigint          AS tin,
           (payload->>'tokens_out')::bigint         AS tout,
           COALESCE((payload->>'cache_read_tokens')::bigint, 0)  AS cread,
           COALESCE((payload->>'cache_write_tokens')::bigint, 0) AS cwrite
    FROM session_events
    WHERE kind = 'session' AND payload->>'type' = 'session.usage'
),
marked AS (
    SELECT *,
           CASE WHEN cost < LAG(cost) OVER (PARTITION BY session_id ORDER BY seq)
                THEN 1 ELSE 0 END AS reset
    FROM usage_events
),
generations AS (
    SELECT *, SUM(reset) OVER (PARTITION BY session_id ORDER BY seq) AS gen
    FROM marked
),
per_generation AS (
    SELECT DISTINCT ON (session_id, gen)
           session_id, gen, cost, tin, tout, cread, cwrite
    FROM generations
    ORDER BY session_id, gen, seq DESC
),
totals AS (
    SELECT session_id,
           COUNT(*)      AS generations,
           SUM(cost)     AS true_cost,
           SUM(tin)      AS true_tin,
           SUM(tout)     AS true_tout,
           SUM(cread)    AS true_cread,
           SUM(cwrite)   AS true_cwrite
    FROM per_generation
    GROUP BY session_id
    HAVING COUNT(*) > 1          -- a single generation was never wrong
)
SQL

echo "== sessions whose recorded cost is short =="
psql_q -P pager=off -c "
${RECONSTRUCT}
SELECT t.session_id, t.generations,
       s.cost_usd AS recorded, t.true_cost AS actual,
       (t.true_cost - s.cost_usd) AS shortfall, s.title
FROM totals t JOIN sessions s ON s.id = t.session_id
WHERE s.cost_usd IS DISTINCT FROM t.true_cost
ORDER BY (t.true_cost - s.cost_usd) DESC;
"

echo
echo "== resumed sessions this cannot repair (events already archived) =="
psql_q -P pager=off -c "
SELECT s.id, s.cost_usd, s.archived_at, s.title
FROM sessions s
WHERE s.archived_at IS NOT NULL
  AND EXISTS (SELECT 1 FROM sessions x WHERE x.id = s.id AND x.puku_session_id IS NOT NULL)
  AND NOT EXISTS (SELECT 1 FROM session_events e WHERE e.session_id = s.id)
ORDER BY s.ended_at;
"

if [ "$APPLY" -ne 1 ]; then
  echo
  echo "dry run — nothing written. Re-run with --apply to restate the rows above."
  exit 0
fi

echo
echo "== applying =="
psql_q -c "
${RECONSTRUCT}
, upd AS (
    UPDATE sessions s
    SET cost_usd           = f.true_cost,
        tokens_in          = f.true_tin,
        tokens_out         = f.true_tout,
        cache_read_tokens  = f.true_cread,
        cache_write_tokens = f.true_cwrite,
        -- Leave the baseline consistent with the new total, so a further
        -- resume of one of these sessions adds to the corrected figure
        -- instead of re-introducing the bug this repairs.
        cost_baseline_usd    = f.true_cost,
        tokens_in_baseline   = f.true_tin,
        tokens_out_baseline  = f.true_tout,
        cache_read_baseline  = f.true_cread,
        cache_write_baseline = f.true_cwrite
    FROM totals f
    WHERE s.id = f.session_id AND s.cost_usd IS DISTINCT FROM f.true_cost
    RETURNING s.id, s.org_id, s.cost_usd, s.tokens_in, s.tokens_out,
              s.cache_read_tokens, s.cache_write_tokens, s.ended_at, s.started_at
)
-- usage_records is a pure copy of the session row, so it has to follow.
INSERT INTO usage_records (id, org_id, session_id, period, tokens_in, tokens_out,
                           cache_read_tokens, cache_write_tokens, cost_usd, vm_seconds)
SELECT gen_random_uuid(), org_id, id, now()::date, tokens_in, tokens_out,
       cache_read_tokens, cache_write_tokens, cost_usd,
       COALESCE(EXTRACT(EPOCH FROM (ended_at - started_at))::bigint, 0)
FROM upd
ON CONFLICT (session_id) DO UPDATE SET
    tokens_in = EXCLUDED.tokens_in, tokens_out = EXCLUDED.tokens_out,
    cache_read_tokens = EXCLUDED.cache_read_tokens,
    cache_write_tokens = EXCLUDED.cache_write_tokens,
    cost_usd = EXCLUDED.cost_usd, vm_seconds = EXCLUDED.vm_seconds;
"

echo
echo "== after =="
psql_q -P pager=off -c "
${RECONSTRUCT}
SELECT t.session_id, t.generations, s.cost_usd AS now_recorded, t.true_cost AS actual
FROM totals t JOIN sessions s ON s.id = t.session_id
ORDER BY t.true_cost DESC;
"
echo "done. Re-running is safe: repaired rows no longer differ, so they are skipped."

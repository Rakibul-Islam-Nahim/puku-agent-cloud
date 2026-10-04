-- Puku reliability rebuild: state machine + recovery columns.
-- Per docs/RELIABILITY-REBUILD.md §3.1.

-- New session states: recovering, hibernated, cold_archived, archived, deleted
-- The existing state constraint is replaced with the superset.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'sessions_state_check') THEN
        ALTER TABLE sessions DROP CONSTRAINT sessions_state_check;
    END IF;
END $$;
ALTER TABLE sessions ADD CONSTRAINT sessions_state_check CHECK (state IN (
  'created','scheduled','booting','bootstrapping','running','waiting_input',
  'stopping','stopped','completed','failed','canceled','reaped',
  'recovering','hibernated','cold_archived','archived','deleted'
));

-- Desired vs observed. desired=stopped *before* shutdown -> expected, do nothing.
-- desired=running but observed=missing -> crash, recover.
ALTER TABLE sessions ADD COLUMN desired_state text NOT NULL DEFAULT 'running'
  CHECK (desired_state IN ('running','stopped'));

-- Snapshot metadata, populated by puku-snapshot.
ALTER TABLE sessions ADD COLUMN last_snapshot_id text;
ALTER TABLE sessions ADD COLUMN snapshot_taken_at timestamptz;
ALTER TABLE sessions ADD COLUMN snapshot_host_id uuid;
ALTER TABLE sessions ADD COLUMN snapshot_cpu_flags text[] NOT NULL DEFAULT '{}';
ALTER TABLE sessions ADD COLUMN snapshot_hypervisor text;

-- One tier concept lives here. The RPO is derived from sla_tier by a constant
-- in puku-snapshot::triggers -- there is no separate rpo_target_s column to
-- drift out of sync. NULL rpo means cold_only sessions; their RAM RPO is
-- undefined by design.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM information_schema.columns
                   WHERE table_name = 'sessions' AND column_name = 'sla_tier') THEN
        ALTER TABLE sessions ADD COLUMN sla_tier text NOT NULL DEFAULT 'standard'
            CHECK (sla_tier IN ('standard','premium','best_effort'));
    END IF;
    IF NOT EXISTS (SELECT 1 FROM information_schema.columns
                   WHERE table_name = 'quotas' AND column_name = 'sla_tier') THEN
        ALTER TABLE quotas ADD COLUMN sla_tier text NOT NULL DEFAULT 'standard'
            CHECK (sla_tier IN ('standard','premium','best_effort'));
    END IF;
END $$;

-- Recovery mode.
-- cold_only = zero disk loss, full RAM loss on any recovery.
-- warm_allowed = may roll disk back to the last paired snapshot to preserve RAM.
-- Per-org ceiling in quotas.recovery_mode; per-session can be lower (never higher).
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM information_schema.columns
                   WHERE table_name = 'sessions' AND column_name = 'recovery_mode') THEN
        ALTER TABLE sessions ADD COLUMN recovery_mode text NOT NULL DEFAULT 'warm_allowed'
            CHECK (recovery_mode IN ('cold_only','warm_allowed'));
    END IF;
    IF NOT EXISTS (SELECT 1 FROM information_schema.columns
                   WHERE table_name = 'quotas' AND column_name = 'recovery_mode') THEN
        ALTER TABLE quotas ADD COLUMN recovery_mode text NOT NULL DEFAULT 'cold_only'
            CHECK (recovery_mode IN ('cold_only','warm_allowed'));
    END IF;
END $$;

-- best_effort is always cold_only at the session level. Enforce it in the
-- schema so the API cannot store an inconsistent row. The constraint is
-- added idempotently because a re-run of this migration would otherwise fail.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'besteffort_is_cold') THEN
        ALTER TABLE sessions ADD CONSTRAINT besteffort_is_cold
          CHECK (NOT (sla_tier = 'best_effort' AND recovery_mode = 'warm_allowed'));
    END IF;
END $$;

-- Tier-1 crash counter (resets on successful warm resume).
ALTER TABLE sessions ADD COLUMN crash_count int NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN last_crash_at timestamptz;

-- Set true when a snapshot was skipped or stayed Pending past rpo_seconds.
-- Cleared at the next LocalDurable snapshot. Paged for premium, ticketed for
-- standard (see docs/RELIABILITY-REBUILD.md §8.3).
ALTER TABLE sessions ADD COLUMN rpo_at_risk boolean NOT NULL DEFAULT false;

-- Idempotent index creation: drop-and-recreate so a re-run is safe.
DROP INDEX IF EXISTS sessions_recovering_idx;
CREATE INDEX sessions_recovering_idx ON sessions (state, last_crash_at)
  WHERE state = 'recovering';
DROP INDEX IF EXISTS sessions_hibernated_idx;
CREATE INDEX sessions_hibernated_idx ON sessions (state, snapshot_taken_at)
  WHERE state = 'hibernated';
DROP INDEX IF EXISTS sessions_archived_idx;
CREATE INDEX sessions_archived_idx ON sessions (state, snapshot_taken_at)
  WHERE state IN ('cold_archived','archived');

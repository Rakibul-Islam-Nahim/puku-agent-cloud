-- Puku reliability rebuild: paired-snapshot manifest table.
-- Per docs/RELIABILITY-REBUILD.md §3.3.
--
-- Three-stage status (RSD §4.4.1):
--   Pending       -> visibility-only, NEVER restorable
--   LocalDurable  -> fsynced on origin NVMe, local restore only
--   Durable       -> verified off-host copy (RADOS hot pool, or R2 when off)
--   Corrupt       -> sha256 mismatch, never restorable
--
-- The diff chain is enforced by parent_manifest_id FK with ON DELETE
-- RESTRICT: no manifest can be removed while another depends on it, so
-- retention must delete children first (see RSD §4.4.4).

CREATE TABLE IF NOT EXISTS snapshots (
    id                  uuid PRIMARY KEY,
    session_id          uuid NOT NULL REFERENCES sessions(id),
    -- Pair: same instant, atomic.
    disk_snap_id        text NOT NULL,            -- RBD snapshot name
    -- Memory: ref is empty for a base (full snapshot), points at a RADOS/R2
    -- key for a diff. Bytes must be on the same host for Pending (Phase 2),
    -- then uploaded to R2 / RADOS for Durable.
    mem_snap_ref        text NOT NULL,
    mem_snap_size_bytes bigint NOT NULL DEFAULT 0,
    is_full             boolean NOT NULL,         -- true if base, false if diff
    parent_manifest_id  uuid REFERENCES snapshots(id) ON DELETE RESTRICT,
    -- Manifest integrity.
    manifest_sha256     text NOT NULL,
    ts                  timestamptz NOT NULL DEFAULT now(),
    origin_host_id      uuid NOT NULL,
    cpu_flags           text[] NOT NULL,
    hypervisor          text NOT NULL,
    -- Durability state.
    status              text NOT NULL DEFAULT 'Pending'
                        CHECK (status IN ('Pending','LocalDurable','Durable','Corrupt')),
    hot_copy_at         timestamptz,              -- verified copy in RADOS hot pool
    cold_copied_at      timestamptz,              -- verified copy in R2
    r2_sha256           text,                     -- set when Durable
    -- Verification (background test-restore job).
    last_verified_at    timestamptz,
    last_verify_ok      boolean,
    -- A diff must have a parent, and a full must not. Enforced in the schema
    -- so a re-parent bug cannot land a bad row.
    CONSTRAINT parent_requires_diff CHECK (
        (parent_manifest_id IS NULL AND is_full) OR
        (parent_manifest_id IS NOT NULL AND NOT is_full)
    )
);

CREATE INDEX IF NOT EXISTS snapshots_session_ts_idx ON snapshots (session_id, ts DESC);
CREATE INDEX IF NOT EXISTS snapshots_session_durable_idx
    ON snapshots (session_id, ts DESC)
    WHERE status = 'Durable';
CREATE INDEX IF NOT EXISTS snapshots_unverified_idx ON snapshots (last_verified_at)
    WHERE last_verified_at IS NULL OR last_verify_ok = false;

-- Retention enforced by puku-snapshot::retention_job. Rule: keep the current
-- base + every diff after it; collapse old diffs into a new base via a
-- background merge (RSD §4.4.4). No manifest is deleted while another
-- depends on it -- enforced by parent_manifest_id FK.
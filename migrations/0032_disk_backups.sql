-- Puku reliability rebuild: off-cluster disk backup.
-- Per docs/RELIABILITY-REBUILD.md §3.6 and §4.7.
--
-- Ceph (3 replicas) protects against disk and host loss but not a lost pool.
-- The disk-backup job keeps an independent copy in R2. The restore ladder
-- (RSD §1.1 row 3) uses this table when the RBD volume itself is gone.

CREATE TABLE IF NOT EXISTS disk_backups (
    id           uuid PRIMARY KEY,
    session_id   uuid NOT NULL REFERENCES sessions(id),
    kind         text NOT NULL CHECK (kind IN ('full','diff')),
    from_snap    text,                 -- NULL for full
    to_snap      text NOT NULL,        -- RBD snapshot name this export ends at
    r2_key       text NOT NULL,        -- backups/<session>/<ts>.{full|diff}.zst
    size_bytes   bigint NOT NULL,
    sha256       text NOT NULL,
    status       text NOT NULL DEFAULT 'Pending'
                 CHECK (status IN ('Pending','Durable','Corrupt')),
    ts           timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT diff_has_from CHECK ((kind = 'full' AND from_snap IS NULL) OR
                                    (kind = 'diff' AND from_snap IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS disk_backups_session_ts_idx ON disk_backups (session_id, ts DESC);

-- Per-session "last successful backup" cursor.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM information_schema.columns
                   WHERE table_name = 'sessions' AND column_name = 'last_disk_backup_at') THEN
        ALTER TABLE sessions ADD COLUMN last_disk_backup_at timestamptz;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM information_schema.columns
                   WHERE table_name = 'quotas' AND column_name = 'disk_backup_interval_s') THEN
        ALTER TABLE quotas ADD COLUMN disk_backup_interval_s int NOT NULL DEFAULT 3600;
    END IF;
END $$;
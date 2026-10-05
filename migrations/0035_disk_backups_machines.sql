-- Disk backups for machines too, and the per-backup data key.
-- The cursor columns are (re)added here with IF NOT EXISTS: 0032 checked
-- information_schema without a schema filter, which can skip them in a
-- database holding more than one schema.
ALTER TABLE disk_backups ALTER COLUMN session_id DROP NOT NULL;
ALTER TABLE disk_backups ADD COLUMN IF NOT EXISTS machine_id uuid REFERENCES machines(id) ON DELETE CASCADE;
-- The backup's data key, sealed under PUKU_SECRET_KEY.
ALTER TABLE disk_backups ADD COLUMN IF NOT EXISTS dek_sealed bytea;
CREATE INDEX IF NOT EXISTS disk_backups_machine_ts_idx ON disk_backups (machine_id, ts DESC);
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                   WHERE conname = 'disk_backups_one_subject' AND conrelid = 'disk_backups'::regclass) THEN
        ALTER TABLE disk_backups ADD CONSTRAINT disk_backups_one_subject
            CHECK ((session_id IS NULL) <> (machine_id IS NULL));
    END IF;
END $$;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS last_disk_backup_at timestamptz;
ALTER TABLE machines ADD COLUMN IF NOT EXISTS last_disk_backup_at timestamptz;

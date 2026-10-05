-- Sessions whose disk is an RBD image on the shared Ceph cluster rather
-- than a directory on one worker's disk. Such a session can resume on any
-- worker advertising `shared_volumes`, after the old host is fenced; a
-- host-local one can only ever go back to `volume_worker_id`.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS volume_shared boolean NOT NULL DEFAULT false;

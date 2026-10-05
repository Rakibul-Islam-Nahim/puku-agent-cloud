-- Machines whose state directory (volume + kept root disk) is an RBD image
-- on the shared Ceph cluster. Such a machine boots on any worker advertising
-- `shared_volumes` once its old host is fenced -- no snapshot needed.
ALTER TABLE machines ADD COLUMN IF NOT EXISTS volume_shared boolean NOT NULL DEFAULT false;

-- Two hypervisors side by side: libkrun (microsandbox) and Cloud Hypervisor.
--
-- Every row that predates this column ran on libkrun, and every request that
-- does not name an engine still gets it, so the default is the migration.

ALTER TABLE sessions ADD COLUMN engine text NOT NULL DEFAULT 'libkrun'
    CHECK (engine IN ('libkrun', 'cloud_hypervisor'));
ALTER TABLE schedules ADD COLUMN engine text NOT NULL DEFAULT 'libkrun'
    CHECK (engine IN ('libkrun', 'cloud_hypervisor'));

-- The worker holding a session's volumes.
--
-- `worker_id` cannot say this: resume clears it so the dispatcher re-places
-- the session, and nothing then remembered which host had the workspace. On a
-- fleet of more than one worker a resume could land on a box with no volumes
-- and boot `--resume` against an empty disk. Set the first time a worker
-- reports on the session -- by then it has created the directories -- and
-- never cleared.
ALTER TABLE sessions ADD COLUMN volume_worker_id uuid REFERENCES workers(id);
-- Sessions currently placed already have their volumes where they run.
UPDATE sessions SET volume_worker_id = worker_id WHERE worker_id IS NOT NULL;

-- What each worker said it can do at its last registration. Informational
-- for the fleet view; placement uses the live registry.
ALTER TABLE workers ADD COLUMN engines  text[] NOT NULL DEFAULT '{libkrun}';
ALTER TABLE workers ADD COLUMN features text[] NOT NULL DEFAULT '{}';

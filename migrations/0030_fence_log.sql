-- Puku reliability rebuild: fence audit log.
-- Per docs/RELIABILITY-REBUILD.md §3.4.
--
-- Every fencing action (Ceph blocklist, BMC power-cycle) is recorded so the
-- runbook can prove F6: no cross-host relocate happened without a preceding
-- blocklist on the old host.

CREATE TABLE IF NOT EXISTS fence_log (
    id           bigserial PRIMARY KEY,
    host_id      uuid NOT NULL,
    session_id   uuid,                       -- the session being protected
    action       text NOT NULL CHECK (action IN ('blocklist','bmc_poweroff','bmc_powercycle','bmc_status')),
    outcome      text NOT NULL CHECK (outcome IN ('ok','failed','timeout')),
    detail       jsonb NOT NULL DEFAULT '{}',
    ts           timestamptz NOT NULL DEFAULT now(),
    requested_by text NOT NULL               -- controld instance id
);

CREATE INDEX IF NOT EXISTS fence_log_host_ts_idx ON fence_log (host_id, ts DESC);
CREATE INDEX IF NOT EXISTS fence_log_session_ts_idx ON fence_log (session_id, ts DESC)
  WHERE session_id IS NOT NULL;
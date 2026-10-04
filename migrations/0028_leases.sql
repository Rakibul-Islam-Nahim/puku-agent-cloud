-- Puku reliability rebuild: lease table.
-- Per docs/RELIABILITY-REBUILD.md §3.2.
--
-- Each worker holds a lease by renewing every 1 s. The sweeper marks leases
-- `suspected` when expires_at < now(); once BMC unreachable confirms death
-- they are released and a new instance can take over.

CREATE TABLE IF NOT EXISTS leases (
    host_id           uuid PRIMARY KEY,
    owner_instance    text NOT NULL,            -- controld instance id (split-brain on leases themselves)
    generation     bigint NOT NULL,               -- bumped on every takeover
    acquired_at    timestamptz NOT NULL,
    expires_at     timestamptz NOT NULL,        -- now() + 3s at each heartbeat
    last_renewed_at timestamptz NOT NULL,
    state          text NOT NULL DEFAULT 'held'
                   CHECK (state IN ('held','suspected','released')),
    suspected_at   timestamptz,
    confirmed_dead_at timestamptz,
    bmc_hostname   text,
    bmc_kind       text CHECK (bmc_kind IN ('ipmi','redfish') OR bmc_kind IS NULL)
);

CREATE INDEX IF NOT EXISTS leases_expires_idx ON leases (expires_at)
  WHERE state = 'held';
CREATE INDEX IF NOT EXISTS leases_suspected_idx ON leases (suspected_at)
  WHERE state = 'suspected';
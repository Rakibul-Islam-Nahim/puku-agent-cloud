-- Machines: generic VMs driven from outside (docs/MACHINES-API.md).

CREATE TABLE machines (
    id               uuid PRIMARY KEY,
    org_id           uuid NOT NULL REFERENCES orgs(id),
    user_id          uuid REFERENCES users(id),
    -- The caller's idempotency key: POST with the same one ensures the same
    -- machine is running instead of creating a second.
    external_id      text,
    name             text NOT NULL UNIQUE,
    engine           text NOT NULL DEFAULT 'libkrun'
                     CHECK (engine IN ('libkrun', 'cloud_hypervisor')),
    image            text NOT NULL,
    cpus             int NOT NULL,
    memory_mib       int NOT NULL,
    expose           int[] NOT NULL DEFAULT '{}',
    env              jsonb NOT NULL DEFAULT '{}',
    -- secret_env, sealed with PUKU_SECRET_KEY. Never returned by the API.
    secret_env_enc   bytea,
    entrypoint       jsonb,
    volume           jsonb,
    labels           jsonb NOT NULL DEFAULT '{}',
    idle_timeout_s   int NOT NULL DEFAULT 0,
    max_duration_s   int NOT NULL DEFAULT 0,
    state            text NOT NULL DEFAULT 'scheduled'
                     CHECK (state IN ('scheduled','booting','running','stopping',
                                      'stopped','failed','destroyed')),
    -- Bumped on every start; worker frames from an older boot are ignored.
    generation       bigint NOT NULL DEFAULT 1,
    worker_id        uuid REFERENCES workers(id),
    -- Where the volume lives once a boot has created it. Starts go there.
    volume_worker_id uuid REFERENCES workers(id),
    -- What the worker found at the last boot: was the volume already there?
    volume_existed   boolean NOT NULL DEFAULT false,
    error            text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    started_at       timestamptz,
    stopped_at       timestamptz,
    last_active_at   timestamptz NOT NULL DEFAULT now(),
    destroyed_at     timestamptz
);

-- One live machine per (org, external_id). Destroyed rows keep their key so
-- history stays readable, and a new machine may reuse it.
CREATE UNIQUE INDEX machines_external_idx ON machines (org_id, external_id)
    WHERE external_id IS NOT NULL AND state <> 'destroyed';
CREATE INDEX machines_dispatch_idx ON machines (created_at)
    WHERE state = 'scheduled' AND worker_id IS NULL;
CREATE INDEX machines_worker_idx ON machines (worker_id) WHERE worker_id IS NOT NULL;
CREATE INDEX machines_org_idx ON machines (org_id, created_at DESC);

-- One row per boot, closed when the VM goes away: VM-seconds for metering.
CREATE TABLE machine_runs (
    id          uuid PRIMARY KEY,
    machine_id  uuid NOT NULL REFERENCES machines(id) ON DELETE CASCADE,
    org_id      uuid NOT NULL REFERENCES orgs(id),
    worker_id   uuid REFERENCES workers(id),
    engine      text NOT NULL,
    generation  bigint NOT NULL,
    cpus        int NOT NULL,
    memory_mib  int NOT NULL,
    started_at  timestamptz NOT NULL DEFAULT now(),
    ended_at    timestamptz,
    UNIQUE (machine_id, generation)
);
CREATE INDEX machine_runs_org_idx ON machine_runs (org_id, started_at);

ALTER TABLE quotas ADD COLUMN max_concurrent_machines int NOT NULL DEFAULT 20;

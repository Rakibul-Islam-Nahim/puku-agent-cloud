-- Machine snapshots: a machine's disks in object storage, restorable on any
-- worker at the size they were taken at (docs/MACHINES-API.md, "Snapshots").

ALTER TABLE machines DROP CONSTRAINT machines_state_check;
ALTER TABLE machines ADD CONSTRAINT machines_state_check
    CHECK (state IN ('scheduled','booting','restoring','running','stopping',
                     'stopped','failed','destroyed'));

ALTER TABLE machines
    -- Keep the root disk across stop/start (Cloud Hypervisor), and snapshot it.
    ADD COLUMN persist_root        boolean NOT NULL DEFAULT false,
    -- {on_stop, interval_s, keep, before_destroy, exclude}; '{}' is all off.
    ADD COLUMN snapshot_policy     jsonb NOT NULL DEFAULT '{}',
    -- The newest ready snapshot: what a relocation restores from.
    ADD COLUMN latest_snapshot_id  uuid,
    -- The snapshot this boot restores (or restored) from. Cleared by the
    -- next plain start.
    ADD COLUMN restore_snapshot_id uuid,
    -- A worker still holding the copy a restore replaced. Told to drop it
    -- once the restored boot runs, or when it next reconnects.
    ADD COLUMN stale_worker_id     uuid;

CREATE TABLE machine_snapshots (
    id            uuid PRIMARY KEY,
    machine_id    uuid NOT NULL REFERENCES machines(id),
    org_id        uuid NOT NULL,
    trigger       text NOT NULL
                  CHECK (trigger IN ('stop','manual','periodic','destroy','update')),
    state         text NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending','uploading','ready','skipped','failed',
                                   'deleting','deleted')),
    consistency   text CHECK (consistency IN ('clean','live')),
    -- The worker the capture was ordered from; only it may upload.
    worker_id     uuid,
    generation    bigint NOT NULL,
    -- What it was taken of, so a restore boots it at exactly this size.
    engine        text NOT NULL,
    image         text NOT NULL,
    cpus          int NOT NULL,
    memory_mib    int NOT NULL,
    manifest      jsonb,
    size_bytes    bigint,
    stored_bytes  bigint,
    -- The data key, sealed with PUKU_SECRET_KEY: the objects are unreadable
    -- without it, so a bucket leak alone leaks nothing.
    dek_enc       bytea NOT NULL,
    pinned        boolean NOT NULL DEFAULT false,
    label         text,
    error         text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    completed_at  timestamptz,
    deleted_at    timestamptz
);
CREATE INDEX machine_snapshots_by_machine ON machine_snapshots (machine_id, created_at DESC);
CREATE INDEX machine_snapshots_open ON machine_snapshots (state)
    WHERE state IN ('pending', 'uploading', 'deleting');

-- One row per layer. A reused layer names an earlier snapshot's object, so an
-- object is deleted only once no live row names its key any more.
CREATE TABLE machine_snapshot_objects (
    snapshot_id   uuid NOT NULL REFERENCES machine_snapshots(id),
    layer         text NOT NULL CHECK (layer IN ('volume','root')),
    key           text NOT NULL,
    upload_id     text,
    part_bytes    bigint NOT NULL,
    parts         int,
    plain_bytes   bigint,
    stored_bytes  bigint,
    sha256        text,
    state         text NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending','uploading','complete','reused','failed','deleted')),
    PRIMARY KEY (snapshot_id, layer)
);
CREATE INDEX machine_snapshot_objects_key ON machine_snapshot_objects (key);

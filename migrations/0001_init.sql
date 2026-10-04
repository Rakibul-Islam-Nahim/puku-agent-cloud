-- puku-agent-cloud initial schema.
-- Conventions: uuid PKs generated app-side, timestamptz everywhere,
-- state machines enforced in application code with CHECK constraints as a backstop.

CREATE TABLE orgs (
    id          uuid PRIMARY KEY,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id          uuid PRIMARY KEY,
    org_id      uuid NOT NULL REFERENCES orgs(id),
    email       text NOT NULL UNIQUE,
    role        text NOT NULL DEFAULT 'member' CHECK (role IN ('owner','admin','member')),
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE api_keys (
    id            uuid PRIMARY KEY,
    org_id        uuid NOT NULL REFERENCES orgs(id),
    user_id       uuid REFERENCES users(id),
    -- sha256 of the full key; the plaintext is shown once at creation.
    key_hash      bytea NOT NULL UNIQUE,
    -- first 12 chars ("pkc_ab12cd34"), for display and O(1) lookup.
    prefix        text NOT NULL,
    name          text NOT NULL DEFAULT '',
    scopes        text[] NOT NULL DEFAULT '{}',
    last_used_at  timestamptz,
    revoked_at    timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX api_keys_prefix_idx ON api_keys (prefix) WHERE revoked_at IS NULL;

CREATE TABLE workers (
    id                uuid PRIMARY KEY,
    name              text NOT NULL UNIQUE,
    host_fingerprint  text,
    status            text NOT NULL DEFAULT 'offline'
                      CHECK (status IN ('online','draining','offline')),
    capacity_slots    int NOT NULL DEFAULT 0,
    used_slots        int NOT NULL DEFAULT 0,
    labels            jsonb NOT NULL DEFAULT '{}',
    msb_version       text,
    last_heartbeat_at timestamptz,
    registered_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    id                uuid PRIMARY KEY,
    org_id            uuid NOT NULL REFERENCES orgs(id),
    user_id           uuid REFERENCES users(id),
    worker_id         uuid REFERENCES workers(id),
    title             text NOT NULL DEFAULT '',
    prompt            text NOT NULL,
    repo              text,
    branch            text,
    model             text,
    max_budget_usd    numeric,
    allowed_tools     text[] NOT NULL DEFAULT '{}',
    disallowed_tools  text[] NOT NULL DEFAULT '{}',
    state             text NOT NULL DEFAULT 'created' CHECK (state IN (
                        'created','scheduled','booting','bootstrapping',
                        'running','waiting_input','stopping','stopped',
                        'completed','failed','canceled','reaped')),
    -- msb sandbox name: ses-<first 12 hex of id>
    sandbox_name      text NOT NULL,
    -- host directory holding /session and /workspace volumes; lives on the worker.
    volume_path       text,
    -- puku-cli's own session id, needed for --resume on cold-resume.
    puku_session_id   text,
    -- high-water mark of the global event seq (allocated at insert).
    last_seq          bigint NOT NULL DEFAULT 0,
    -- high-water mark of persisted guest_line (worker resume cursor).
    last_guest_line   bigint NOT NULL DEFAULT 0,
    -- denormalized current blocker; null when nothing is pending.
    pending_question  jsonb,
    cost_usd          numeric NOT NULL DEFAULT 0,
    tokens_in         bigint NOT NULL DEFAULT 0,
    tokens_out        bigint NOT NULL DEFAULT 0,
    idle_timeout_s    int NOT NULL DEFAULT 900,
    max_duration_s    int NOT NULL DEFAULT 14400,
    error             text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    started_at        timestamptz,
    ended_at          timestamptz,
    archived_at       timestamptz
);
CREATE INDEX sessions_org_state_idx ON sessions (org_id, state);
CREATE INDEX sessions_worker_idx ON sessions (worker_id)
    WHERE state IN ('scheduled','booting','bootstrapping','running','waiting_input','stopping');

-- Append-only event log: the source of truth for every session transcript.
-- Partitioned by month on ts; seq is allocated by controld under the
-- sessions.last_seq row lock, guest_line dedups worker redelivery.
CREATE TABLE session_events (
    session_id  uuid NOT NULL,
    seq         bigint NOT NULL,
    ts          timestamptz NOT NULL DEFAULT now(),
    kind        text NOT NULL CHECK (kind IN ('agent','session','exec','user')),
    payload     jsonb NOT NULL,
    guest_line  bigint,
    blob_ref    text,
    PRIMARY KEY (session_id, seq, ts)
) PARTITION BY RANGE (ts);

-- Guest-line idempotency is transactional, not index-backed: every insert
-- for a session goes through one controld code path that holds the sessions
-- row lock and skips events with guest_line <= sessions.last_guest_line.
-- (A unique index can't express this on a partitioned table without
-- including ts, which would defeat it.)
CREATE INDEX session_events_guest_line_idx
    ON session_events (session_id, guest_line) WHERE guest_line IS NOT NULL;

-- Partitions: one catch-all for the past, rolling monthlies created by the
-- (M3) maintenance job. Seed the first few so the MVP never hits a gap.
CREATE TABLE session_events_p2026_08 PARTITION OF session_events
    FOR VALUES FROM ('2026-08-01') TO ('2026-09-01');
CREATE TABLE session_events_p2026_09 PARTITION OF session_events
    FOR VALUES FROM ('2026-09-01') TO ('2026-10-01');
CREATE TABLE session_events_p2026_10 PARTITION OF session_events
    FOR VALUES FROM ('2026-10-01') TO ('2026-11-01');
CREATE TABLE session_events_default PARTITION OF session_events DEFAULT;

CREATE TABLE quotas (
    org_id                  uuid PRIMARY KEY REFERENCES orgs(id),
    max_concurrent_sessions int NOT NULL DEFAULT 5,
    max_monthly_usd         numeric NOT NULL DEFAULT 100,
    max_session_duration_s  int NOT NULL DEFAULT 14400
);

CREATE TABLE usage_records (
    id          uuid PRIMARY KEY,
    org_id      uuid NOT NULL REFERENCES orgs(id),
    session_id  uuid NOT NULL,
    period      date NOT NULL,
    tokens_in   bigint NOT NULL DEFAULT 0,
    tokens_out  bigint NOT NULL DEFAULT 0,
    cost_usd    numeric NOT NULL DEFAULT 0,
    vm_seconds  bigint NOT NULL DEFAULT 0
);
CREATE INDEX usage_records_org_period_idx ON usage_records (org_id, period);

CREATE TABLE audit_log (
    id             bigserial PRIMARY KEY,
    org_id         uuid,
    actor_user_id  uuid,
    action         text NOT NULL,
    subject        text NOT NULL,
    detail         jsonb NOT NULL DEFAULT '{}',
    ts             timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audit_log_org_ts_idx ON audit_log (org_id, ts);

-- Dev seed: a default org + user so the M1 loop works before auth lands (M3).
INSERT INTO orgs (id, name) VALUES ('00000000-0000-0000-0000-000000000001', 'dev');
INSERT INTO users (id, org_id, email, role)
    VALUES ('00000000-0000-0000-0000-000000000002',
            '00000000-0000-0000-0000-000000000001', 'dev@localhost', 'owner');
INSERT INTO quotas (org_id) VALUES ('00000000-0000-0000-0000-000000000001');

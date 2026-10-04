-- Scheduled jobs: a cron entry that creates a normal session at fire time.
-- The session machinery (VM, events, quotas, billing) is unchanged; a
-- schedule is just another creator.
CREATE TABLE schedules (
    id               uuid PRIMARY KEY,
    org_id           uuid NOT NULL REFERENCES orgs(id),
    user_id          uuid REFERENCES users(id),
    name             text NOT NULL DEFAULT '',
    prompt           text NOT NULL,
    repo             text,
    branch           text,
    model            text,
    max_budget_usd   numeric,
    -- five-field cron (minute hour day month weekday), evaluated in UTC
    cron             text NOT NULL,
    enabled          boolean NOT NULL DEFAULT true,
    next_run_at      timestamptz NOT NULL,
    last_run_at      timestamptz,
    last_session_id  uuid,
    created_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX schedules_due_idx ON schedules (next_run_at) WHERE enabled;

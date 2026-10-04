-- M7: let a cloud session reach the user's SaaS accounts, and let a blocked
-- session reach the user.

-- Connectors are brokered by mcp.proxy.puku.sh, which puku-cowork already
-- uses for local VM sessions: the guest gets an MCP endpoint plus the user's
-- puku JWT, and the proxy swaps that for the vendor token server-side. No
-- third-party OAuth token ever enters the microVM.
ALTER TABLE sessions ADD COLUMN connectors boolean NOT NULL DEFAULT true;

-- Where to reach a human when a session blocks. Without this the cron
-- scheduler is decorative: a session that hits waiting_input at 03:00 waits
-- until someone happens to open the dashboard.
CREATE TABLE notification_targets (
    id         uuid PRIMARY KEY,
    org_id     uuid NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    user_id    uuid REFERENCES users(id) ON DELETE CASCADE,
    kind       text NOT NULL CHECK (kind IN ('webhook', 'slack', 'platform')),
    -- webhook/slack: {"url": "..."}; platform: {} (routed by user_id).
    config     jsonb NOT NULL DEFAULT '{}',
    -- Which session events to deliver. Defaults to the two that need a
    -- human: the agent is blocked, or the run is over.
    events     text[] NOT NULL DEFAULT '{waiting_input,terminal}',
    -- HMAC key for webhook signatures, so a receiver can verify the sender.
    secret_enc bytea,
    enabled    boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX notification_targets_org_idx ON notification_targets (org_id) WHERE enabled;

-- One row per delivered notification, so a retry or a duplicate state
-- transition doesn't page the user twice for the same event.
CREATE TABLE notification_deliveries (
    target_id  uuid NOT NULL REFERENCES notification_targets(id) ON DELETE CASCADE,
    session_id uuid NOT NULL,
    event      text NOT NULL,
    -- Distinguishes two separate questions in one session.
    dedup_key  text NOT NULL,
    sent_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (target_id, session_id, event, dedup_key)
);

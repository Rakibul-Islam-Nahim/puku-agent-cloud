-- M9: something other than cron can start a session.
--
-- The scheduler already proved the "something creates a session" seam;
-- this adds an inbound producer on the same seam. A trigger is a schedule
-- without a clock: a token in a URL, a prompt template, and the same
-- session-creation path.

CREATE TABLE triggers (
    id          uuid PRIMARY KEY,
    org_id      uuid NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    user_id     uuid REFERENCES users(id) ON DELETE CASCADE,
    name        text NOT NULL DEFAULT '',
    kind        text NOT NULL DEFAULT 'webhook' CHECK (kind IN ('webhook')),
    -- sha256 of the URL token; the plaintext is shown once at creation,
    -- same discipline as api_keys and worker_tokens.
    token_hash  bytea NOT NULL UNIQUE,
    prefix      text NOT NULL,
    -- Prompt with {{payload}} / {{payload.path.to.field}} placeholders.
    prompt      text NOT NULL,
    repo        text,
    branch      text,
    model       text,
    max_budget_usd numeric,
    enabled     boolean NOT NULL DEFAULT true,
    last_fired_at   timestamptz,
    last_session_id uuid,
    created_at  timestamptz NOT NULL DEFAULT now()
);

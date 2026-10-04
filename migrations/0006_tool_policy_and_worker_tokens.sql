-- M5: make the tool policy real, and give every worker its own credential.

-- Tool policy. allowed_tools/disallowed_tools already existed and were never
-- populated; permission_mode is the third leg and the one that replaces the
-- unconditional --god-mode the runner used to hardcode. NULL means "use the
-- deployment default", which controld resolves at dispatch.
ALTER TABLE sessions ADD COLUMN permission_mode text
    CHECK (permission_mode IN ('default','plan','acceptEdits','dontAsk','auto','bypassPermissions'));
-- Unattended sessions need a turn budget; puku-cli otherwise yields after a
-- single model response.
ALTER TABLE sessions ADD COLUMN max_turns int;

-- Per-worker tokens. Until now every worker presented one shared secret
-- (/etc/puku/worker-token), so any host holding it could register and be
-- handed another org's sessions.
CREATE TABLE worker_tokens (
    id           uuid PRIMARY KEY,
    name         text NOT NULL,
    -- sha256 of the full token; the plaintext is shown once at creation,
    -- same discipline as api_keys.
    token_hash   bytea NOT NULL UNIQUE,
    -- first 12 chars ("pkw_ab12cd34"), for display in the fleet view.
    prefix       text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz,
    revoked_at   timestamptz
);

-- Which token a worker registered with. A token is bound to the first
-- worker name that uses it; a second name presenting the same token is
-- rejected, so a leaked token cannot silently fan out across hosts.
ALTER TABLE workers ADD COLUMN token_id uuid REFERENCES worker_tokens(id);

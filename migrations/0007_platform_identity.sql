-- M6: stop being a second identity provider, and stop spending one
-- operator credential on behalf of every tenant.

-- Platform identity. `sub` from chat.api.puku.sh/auth/verify is the real
-- user identity; a row here is a local projection of it, provisioned on
-- first sight. email stops being the key (and stops being required: the
-- verify response does not always carry one).
ALTER TABLE users ADD COLUMN external_id text UNIQUE;
ALTER TABLE users ADD COLUMN display_name text;
ALTER TABLE users ALTER COLUMN email DROP NOT NULL;
-- The old UNIQUE on email would collide for two platform users who both
-- arrive without one.
ALTER TABLE users DROP CONSTRAINT IF EXISTS users_email_key;
CREATE UNIQUE INDEX users_email_idx ON users (email) WHERE email IS NOT NULL;

-- The platform has no org concept — only `sub` — so one org per user is
-- synthesized on provision. The column keeps team orgs possible later
-- without another migration.
ALTER TABLE orgs ADD COLUMN owner_user_id uuid REFERENCES users(id);
ALTER TABLE orgs ADD COLUMN kind text NOT NULL DEFAULT 'team'
    CHECK (kind IN ('team', 'personal'));

-- Credentials for unattended runs (cron), when there is no live caller
-- whose bearer we can borrow. Encrypted with PUKU_SECRET_KEY; the column
-- never holds plaintext.
CREATE TABLE org_credentials (
    id         uuid PRIMARY KEY,
    org_id     uuid NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    user_id    uuid REFERENCES users(id) ON DELETE CASCADE,
    -- 'api_key'   — a puku platform key, no expiry (preferred).
    -- 'bearer'    — a platform JWT; expires, so only useful briefly.
    kind       text NOT NULL CHECK (kind IN ('api_key', 'bearer')),
    value_enc  bytea NOT NULL,
    expires_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (org_id, user_id, kind)
);

-- The caller's own credential, captured at create/resume so the dispatcher
-- (which runs long after the request) can inject it. Encrypted at rest.
-- NULL means "fall back to the operator's global credential", which is what
-- every session did before this migration.
ALTER TABLE sessions ADD COLUMN credential_enc bytea;
ALTER TABLE sessions ADD COLUMN credential_kind text
    CHECK (credential_kind IN ('api_key', 'bearer'));

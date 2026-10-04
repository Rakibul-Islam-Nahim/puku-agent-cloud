-- Let an org store an OAuth refresh token.
--
-- Unattended runs are the reason. A scheduled job fires when nobody is
-- holding a request, so its credential has to be stored in advance. A
-- bearer dies with its login, silently, and every schedule in the org then
-- fails with a 401 that reads like a revoked key. An api_key means asking
-- each user to mint and rotate one by hand -- and on a puku-gateway
-- deployment it is not even accepted.
--
-- A refresh token is durable and yields a fresh bearer on demand. It stays
-- in the control plane; the guest only ever receives the short-lived
-- bearer minted from it.

ALTER TABLE org_credentials DROP CONSTRAINT IF EXISTS org_credentials_kind_check;
ALTER TABLE org_credentials
    ADD CONSTRAINT org_credentials_kind_check
    CHECK (kind IN ('api_key', 'bearer', 'refresh'));

-- Grant zeroship_auth the privileges the password-reset gateway-session
-- teardown (security finding H1) needs.
--
-- A password reset must durably terminate the GATEWAY app-session tier, not
-- just the IdP login session: the gateway's stateless `__Host-zeroship_app_session`
-- cookie is gated ONLY by the per-app family marker in
-- `zeroship.token_revocations`, and its 30-day `__Host-zeroship_app_anchor`
-- re-mints fresh cookies via `?mint=1` while the `app_session_anchors` row is
-- live. `password_reset::complete` now, in the reset transaction:
--   - UPSERTs a `(client_id, pairwise_sub)` family marker per app the user
--     holds an identity with (rejects live cookies), and
--   - sets `revoked_at = NOW()` on every `app_session_anchors` row for the user
--     (makes `read_live` → None so `?mint=1` fails closed).
--
-- The auth service connects as the BYPASSRLS `zeroship_auth` role, so RLS is
-- not a barrier for the cross-app writes; but 0025 granted it NO privilege on
-- either table. This adds exactly the minimum:
--   - token_revocations: SELECT + INSERT (the family-marker INSERT … ON CONFLICT
--     DO UPDATE needs INSERT + UPDATE; SELECT covers the JOIN read).
--   - app_session_anchors: SELECT + UPDATE (the revoke UPDATE filters on
--     global_user_id / revoked_at, which a WHERE read needs SELECT for; no
--     INSERT/DELETE — auth only ever flips revoked_at).

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_auth') THEN
        EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.token_revocations  TO zeroship_auth';
        EXECUTE 'GRANT SELECT, UPDATE         ON zeroship.app_session_anchors TO zeroship_auth';
    END IF;
END $$;

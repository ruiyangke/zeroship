-- zeroship.gateway_sessions.auth_time + amr — the authentication-event recency
-- (auth_time) and the authentication-methods array (amr) carried on the
-- per-origin cookie session (auth-bff-session-redesign §2.2 step 5b / §5.3).
--
-- The BFF redesign holds NO JWT in the browser: the SPA learns identity via
-- the { user, expires_at, auth_time, amr } projection from
-- GET /__zs/auth/session, and the step-up gate needs a fresh auth_time. Both
-- live on zeroship.users/zeroship.idp_sessions today (0002_auth.sql) but the gateway
-- cookie path never reads those tables — it reads zeroship.gateway_sessions. So
-- the two values must live on the row the gateway path actually loads.
--
-- Populated by gateway::sessions::create from the validated id_token claims
-- (auth_time/amr are standard OIDC claims Hydra issues); validate() returns
-- them so the per-request projection reads them off the same row it loads.
--
-- amr defaults '{}' and auth_time is NULLABLE so existing-shape inserts and
-- the validate() idle-slide are unaffected beyond reading/writing the new
-- columns.

ALTER TABLE zeroship.gateway_sessions ADD COLUMN auth_time TIMESTAMPTZ;
ALTER TABLE zeroship.gateway_sessions ADD COLUMN amr TEXT[] NOT NULL DEFAULT '{}';

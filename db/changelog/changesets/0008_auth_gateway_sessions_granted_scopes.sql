--liquibase formatted sql

-- zeroship.gateway_sessions.granted_scopes — the granted-scope set carried on the
-- per-origin cookie session (auth-sdk Slice 3, spec §1.4 / §5.3 / §8.1).
--
-- The cookie-session path has NO access token in the browser to decode, so the
-- gateway emits WorkerUser.scopes for that path from this column instead of a
-- cross-schema zeroship.oauth_grants join on the hot path. gateway::sessions
-- ::create writes it from the consent grant at session-create; validate()
-- returns it so the per-request cookie path reads scopes off the same row it
-- already loads (no extra query).
--
-- NOT NULL DEFAULT '{}' so existing-shape inserts and the validate() idle-slide
-- are unaffected beyond reading/writing the new column.

--changeset zeroship:auth-gateway-sessions-granted-scopes splitStatements:true
ALTER TABLE zeroship.gateway_sessions ADD COLUMN granted_scopes TEXT[] NOT NULL DEFAULT '{}';
--rollback ALTER TABLE zeroship.gateway_sessions DROP COLUMN granted_scopes;

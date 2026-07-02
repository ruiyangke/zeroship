-- Native OP device-grant bindings.
--
-- V0061 created the shared pending device grant table for the control-owned
-- platform device flow. The auth OP's native RFC 8628 arm also needs to bind a
-- pending user code to its OAuth client and, after approval, to the approving
-- IdP session (`sid`) so the token poll can mint one-use OP tokens directly.

ALTER TABLE zeroship.device_grants
    ADD COLUMN client_id TEXT REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE,
    ADD COLUMN sid TEXT,
    ADD COLUMN poll_interval_secs INTEGER NOT NULL DEFAULT 5
        CHECK (poll_interval_secs > 0);

CREATE INDEX device_grants_client_id_idx
    ON zeroship.device_grants (client_id)
    WHERE client_id IS NOT NULL;

CREATE INDEX device_grants_provider_pending_user_code_idx
    ON zeroship.device_grants (provider, user_code)
    WHERE status = 'pending';

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_auth') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.device_grants TO zeroship_auth';
  END IF;
END $g$;

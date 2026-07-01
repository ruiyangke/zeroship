DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_auth') THEN
    EXECUTE 'REVOKE SELECT, INSERT, UPDATE, DELETE ON zeroship.device_grants FROM zeroship_auth';
  END IF;
END $g$;

DROP INDEX IF EXISTS zeroship.device_grants_provider_pending_user_code_idx;
DROP INDEX IF EXISTS zeroship.device_grants_client_id_idx;

ALTER TABLE zeroship.device_grants
    DROP COLUMN IF EXISTS poll_interval_secs,
    DROP COLUMN IF EXISTS sid,
    DROP COLUMN IF EXISTS client_id;

DO $rb$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'REVOKE ALL ON zeroship.identity_links FROM zeroship_control';
    EXECUTE 'REVOKE ALL ON zeroship.principal_grants FROM zeroship_control';
  END IF;
END $rb$;

DROP TABLE zeroship.principal_grants;
DROP INDEX IF EXISTS zeroship.identity_links_principal_id_idx;
DROP TABLE zeroship.identity_links;

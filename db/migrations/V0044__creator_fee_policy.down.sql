DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.creator_fee_policy FROM zeroship_control'; END IF; END $rb$;
ALTER TABLE zeroship.creator_accounts DROP COLUMN details_submitted, DROP COLUMN payouts_enabled, DROP COLUMN charges_enabled;
DROP TABLE zeroship.creator_fee_policy;

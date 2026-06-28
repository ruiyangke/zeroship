DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.pending_disputes FROM zeroship_control'; END IF; END $rb$;
DROP INDEX IF EXISTS zeroship.pending_disputes_charge_idx;
DROP INDEX IF EXISTS zeroship.pending_disputes_payment_intent_idx;
DROP TABLE zeroship.pending_disputes;

DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.stripe_events_seen FROM zeroship_control'; END IF; END $rb$;
DROP TABLE zeroship.stripe_events_seen;
DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.creator_billing_status FROM zeroship_control'; EXECUTE 'REVOKE ALL ON zeroship.creator_billing_status_history FROM zeroship_control'; END IF; END $rb$;
DROP INDEX IF EXISTS zeroship.idx_creator_billing_status_past_due;
DROP INDEX IF EXISTS zeroship.idx_creator_billing_status_history_creator_at;
DROP TABLE zeroship.creator_billing_status_history;
DROP TABLE zeroship.creator_billing_status;

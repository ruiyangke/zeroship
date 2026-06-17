DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.billing_notifications FROM zeroship_control'; END IF; END $rb$;
DROP INDEX IF EXISTS zeroship.idx_billing_notifications_pending;
DROP TABLE zeroship.billing_notifications;
DROP DOMAIN IF EXISTS zeroship.notification_status;
DROP DOMAIN IF EXISTS zeroship.billing_notification_kind;

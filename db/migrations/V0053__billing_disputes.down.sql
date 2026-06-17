DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.billing_disputes FROM zeroship_control'; END IF; END $rb$;
DROP TRIGGER IF EXISTS billing_disputes_controlled_update_trg ON zeroship.billing_disputes;
DROP FUNCTION IF EXISTS zeroship.billing_disputes_controlled_update();
DROP INDEX IF EXISTS zeroship.invoice_payments_dispute_provider_ref_key;
DROP INDEX IF EXISTS zeroship.billing_disputes_invoice_idx;
DROP TABLE zeroship.billing_disputes;
DROP DOMAIN IF EXISTS zeroship.dispute_status;

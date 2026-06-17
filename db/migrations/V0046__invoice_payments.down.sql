DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.invoice_payments FROM zeroship_control'; END IF; END $rb$;
DROP TRIGGER IF EXISTS invoice_payments_immutable_trg ON zeroship.invoice_payments;
DROP FUNCTION IF EXISTS zeroship.invoice_payments_immutable();
DROP INDEX IF EXISTS zeroship.invoice_payments_charge_provider_ref_key;
DROP INDEX IF EXISTS zeroship.invoice_payments_invoice_idx;
DROP TABLE zeroship.invoice_payments;
DROP DOMAIN IF EXISTS zeroship.invoice_payment_kind;

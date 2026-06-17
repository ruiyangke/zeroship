DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.credit_ledger FROM zeroship_control'; END IF; END $rb$;
DROP TRIGGER IF EXISTS credit_ledger_immutable_trg ON zeroship.credit_ledger;
DROP FUNCTION IF EXISTS zeroship.credit_ledger_immutable();
DROP INDEX IF EXISTS zeroship.credit_ledger_refund_to_credit_note_idx;
DROP INDEX IF EXISTS zeroship.credit_ledger_idempotency_key_idx;
DROP INDEX IF EXISTS zeroship.credit_ledger_creator_created_idx;
DROP TABLE zeroship.credit_ledger;
DROP DOMAIN IF EXISTS zeroship.credit_entry_kind;

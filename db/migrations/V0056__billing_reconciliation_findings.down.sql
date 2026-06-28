DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN EXECUTE 'REVOKE ALL ON zeroship.billing_reconciliation_findings FROM zeroship_control'; END IF; END $rb$;
DROP TRIGGER IF EXISTS billing_reconciliation_findings_controlled_update_trg ON zeroship.billing_reconciliation_findings;
DROP FUNCTION IF EXISTS zeroship.billing_reconciliation_findings_controlled_update();
DROP INDEX IF EXISTS zeroship.billing_reconciliation_findings_open_idx;
DROP INDEX IF EXISTS zeroship.billing_reconciliation_findings_kind_idx;
DROP TABLE zeroship.billing_reconciliation_findings;
DROP DOMAIN IF EXISTS zeroship.reconciliation_finding_severity;
DROP DOMAIN IF EXISTS zeroship.reconciliation_finding_kind;

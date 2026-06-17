DROP INDEX zeroship.auth_users_deletion_due_idx;
ALTER TABLE zeroship.users DROP COLUMN anonymized_at;
ALTER TABLE zeroship.users DROP COLUMN deletion_scheduled_for;
ALTER TABLE zeroship.users DROP COLUMN deletion_requested_at;

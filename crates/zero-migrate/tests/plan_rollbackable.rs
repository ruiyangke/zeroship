//! What `AppliedPlan::rollbackable` actually answers.
//!
//! The field is computed as "every step has a defined `down`"
//! (`render/plan.rs` `compute_rollbackable`), which is a claim about the
//! EXISTENCE of reversing SQL, not about whether the original state can be
//! restored. These arms pin both readings so a future change that narrows the
//! field has to break a test rather than a host.

use zero_migrate::model::migration::Migration;
use zero_migrate::model::migration::{Checksum, ChecksumInput, MigrationFlags, MigrationId};
use zero_migrate::AppliedPlan;

fn mig(up: &str, down: Option<&str>) -> Migration {
    let flags = MigrationFlags::default();
    Migration {
        version: MigrationId::generate(),
        name: "m".to_string(),
        up: up.to_string(),
        down: down.map(str::to_string),
        checksum: Checksum::of(&ChecksumInput {
            up,
            down,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

// A dropped column is structurally reversible and NOT recoverable: the `down`
// re-creates the column, and every value that was in it is gone. The field
// reports `true` here, which is the gap - a host reading `rollbackable` as "this
// can be undone" is told yes about a migration that destroys data permanently.
#[test]
fn a_dropped_column_reports_rollbackable_even_though_its_values_are_gone() {
    let plan = AppliedPlan::single_step(mig(
        "ALTER TABLE t DROP COLUMN email",
        Some("ALTER TABLE t ADD COLUMN email text"),
    ));
    assert!(
        plan.rollbackable,
        "rollbackable answers 'a down exists', so a lossy drop still reports true"
    );
}

// The control: the field is not vacuously true. A migration with no `down` at
// all reports false, which is the reading the name does support.
#[test]
fn a_migration_with_no_down_reports_not_rollbackable() {
    let plan = AppliedPlan::single_step(mig("CREATE TABLE t (id int)", None));
    assert!(
        !plan.rollbackable,
        "a migration carrying no down must not report rollbackable"
    );
}

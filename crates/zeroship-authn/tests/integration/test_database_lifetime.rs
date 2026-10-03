//! The lifetime measurements the authn shared database fixture owes: a
//! container is gone after the owning process exits, and after that process is
//! killed while the container is still starting or migrating.

use zeroship_testkit::lifetime;

/// The child test, by its full path in this binary.
const CHILD_TEST: &str = "integration::test_database_lifetime::the_authn_test_database_reports_its_container";

#[test]
fn the_authn_test_database_reports_its_container() {
    lifetime::report_owner();
    let id = zeroship_testkit::postgres::server_container_id();
    assert!(!id.is_empty(), "the fixture reports the container it started");
    lifetime::report_container(&id);
}

#[test]
fn the_authn_test_database_is_removed_when_its_process_ends() {
    lifetime::assert_removed_after_the_child_exits(CHILD_TEST);
}

#[test]
fn the_authn_test_database_is_removed_when_its_process_is_killed_while_starting() {
    lifetime::assert_removed_after_a_kill_during_startup(CHILD_TEST);
}

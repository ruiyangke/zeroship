//! This binary's migrated Postgres server is removed once the process that started
//! it has ended - after a normal exit, and after a SIGKILL while it is still
//! starting or migrating.
//!
//! The server lives in a `static`, which libtest never drops, so nothing but the
//! shared reaper can remove it. Both measurements run
//! [`the_control_test_database_reports_its_container`] alone in a child process; see
//! `container_reaper::lifetime`.

use crate::common;
use crate::common::test_database::container_reaper::lifetime;

/// The child test, by its full path in this binary.
const CHILD_TEST: &str = "test_database_lifetime::the_control_test_database_reports_its_container";

#[test]
fn the_control_test_database_reports_its_container() {
    lifetime::report_owner();
    let url = common::require_control_db();
    assert!(
        url.ends_with("/control_tests"),
        "the fixture hands out its migrated database: {url}"
    );
    lifetime::report_container(&common::test_database::container_id());
}

#[test]
fn the_control_test_database_is_removed_when_its_process_ends() {
    lifetime::assert_removed_after_the_child_exits(CHILD_TEST);
}

#[test]
fn the_control_test_database_is_removed_when_its_process_is_killed_while_starting() {
    lifetime::assert_removed_after_a_kill_during_startup(CHILD_TEST);
}

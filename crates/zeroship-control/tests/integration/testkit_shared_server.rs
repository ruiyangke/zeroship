//! The control fixtures reach the one migrated platform server the testkit
//! shares. An ordinary case works in that server's working database, scoped to
//! the rows it mints; a database-global case owns a clone of its template. This
//! binds both halves: a fixture that cloned for every case would accumulate one
//! database per case on the shared server, and one that reached a different
//! server would leave a per-process PostgreSQL behind.

use url::Url;

#[compio::test]
async fn an_ordinary_control_case_shares_the_working_database() {
    let url = Url::parse(&crate::support::require_control_db()).expect("fixture database URL");
    let server = zeroship_testkit::postgres::platform().admin_url();
    assert_eq!(
        url.host_str(),
        server.host_str(),
        "the case's database is not on the shared server's host: {url} vs {server}"
    );
    assert_eq!(
        url.port(),
        server.port(),
        "the case's database is not on the shared server's port: {url} vs {server}"
    );
    assert_eq!(
        url.path(),
        server.path(),
        "an ordinary case must work in the shared database, not a clone of it: {url}"
    );
}

#[compio::test]
async fn a_database_global_case_owns_a_clone_of_the_template() {
    let fresh = crate::support::fresh_control_db();
    let url = fresh.admin_url();
    let server = zeroship_testkit::postgres::platform().admin_url();
    assert_eq!(
        url.host_str(),
        server.host_str(),
        "the clone is not on the shared server's host: {url} vs {server}"
    );
    assert_eq!(
        url.port(),
        server.port(),
        "the clone is not on the shared server's port: {url} vs {server}"
    );
    assert_ne!(
        url.path(),
        server.path(),
        "a database-global case must own a clone, not the shared database: {url}"
    );
}

//! Where a URL's userinfo ends.
//!
//! `parse_credentials` scanned the WHOLE remaining string for `@`, so any
//! later `@` -- most easily in a query value -- was read as the userinfo
//! separator. The result was not a parse error but a REDIRECTED CONNECTION:
//!
//! ```text
//! postgres://127.0.0.1:5432/postgres?user=postgres&application_name=c@d
//!   libpq: connects to 127.0.0.1, application_name is "c@d"
//!   was:   host "d", user "127.0.0.1",
//!          password "5432/postgres?user=postgres&application_name=c"
//! ```
//!
//! A connection string carrying `password=` in its query therefore sent that
//! password to a host named by whatever followed the `@`.
//!
//! THE BOUND IS `/` AND NOT `?`, which is measured rather than assumed. With
//! no path at all libpq really does scan past the query:
//! `postgres://127.0.0.1:5432?...&application_name=a@b` fails there with
//! `could not translate host name "b"`. Stopping at `?` as well would be
//! unfaithful in the other direction, so both halves are pinned below.

use compio_postgres::Config;

#[test]
fn userinfo_ends_at_the_path() {
    // The defect. Everything after the path is a query value, not userinfo.
    let query_at = "postgres://127.0.0.1:5432/postgres?user=postgres&application_name=c@d"
        .parse::<Config>()
        .expect("an @ in a query value must parse");
    assert_eq!(
        format!("{:?}", query_at.get_hosts()),
        r#"[Tcp("127.0.0.1")]"#,
        "an @ in a query value redirected the connection"
    );
    assert_eq!(query_at.get_user(), Some("postgres"));
    assert_eq!(
        query_at.get_password(),
        None,
        "no password was in this string"
    );
    assert_eq!(query_at.get_application_name(), Some("c@d"));

    // THE OTHER HALF. No path, so libpq scans past the query and the last @
    // really is the separator. Matching it here is what stops the fix from
    // being "stop at the first ? too", which would be a different divergence.
    let no_path = "postgres://127.0.0.1:5432?user=postgres&application_name=a@b"
        .parse::<Config>()
        .expect("a pathless URL must parse");
    assert_eq!(
        format!("{:?}", no_path.get_hosts()),
        r#"[Tcp("b")]"#,
        "with no path, libpq treats the last @ as the userinfo separator"
    );

    // One-variable partners: ordinary userinfo must still work, or "stop
    // scanning" is satisfied by never finding userinfo at all.
    let full = "postgres://u:p@h:5432/db"
        .parse::<Config>()
        .expect("userinfo");
    assert_eq!(full.get_user(), Some("u"));
    assert_eq!(
        full.get_password()
            .map(|p| String::from_utf8_lossy(p).into_owned()),
        Some("p".to_string())
    );
    assert_eq!(full.get_dbname(), Some("db"));

    let user_only = "postgres://u@h/db".parse::<Config>().expect("user only");
    assert_eq!(user_only.get_user(), Some("u"));
    assert_eq!(user_only.get_password(), None);

    let none = "postgres://h/db".parse::<Config>().expect("no userinfo");
    assert_eq!(none.get_user(), None, "a bare host must not become a user");
    assert_eq!(none.get_dbname(), Some("db"));
}

//! `DROP EXTENSION` is decided at the extension NAME, against the same
//! `code.extension` allowlist entry that decides `CREATE EXTENSION`.
//!
//! The knob's value is a set of names, not a boolean, so "holds the capability" and
//! "may name THIS extension" are different questions. The drop side used to ask only
//! the first one - it routed to a capability predicate that takes no object and
//! answers "is the allowlist non-empty" - so a charter allowlisting one name admitted
//! a drop of every other name in the database.
//!
//! Each arm below changes only the allowlist and the name in the statement. The
//! create arms are the control: they must keep behaving exactly as they did, and a
//! granted name must still be droppable, so a build that simply refused every drop
//! could not pass this file.

mod support;

use zeroship_migrate_backend::guard::{GuardConfig, GuardError};
use zeroship_migrate_postgres::guard::denylist::rule;
use zeroship_migrate_postgres::guard::SqlGuard;
use zeroship_migrate_postgres::DIALECT as POSTGRES;

/// A charter owning the `app` schema whose `code.extension` allowlist is exactly
/// `names`. An empty `names` authors no grant at all, which is the deny-by-default
/// posture (the knob has no "empty set" spelling - an absent grant IS the empty set).
fn guard_allowing(names: &[&str]) -> SqlGuard {
    let extension_grant = if names.is_empty() {
        String::new()
    } else {
        let names = names
            .iter()
            .map(|name| format!("{name:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"
[[grant]]
key = "code.extension"
value = [{names}]
scope = "all"
"#
        )
    };
    let charter = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = {{ include = ["app"] }}

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = ["app"] }}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
{extension_grant}"#
    );
    SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(&charter),
        POSTGRES,
    ))
}

#[track_caller]
fn assert_admitted(guard: &SqlGuard, sql: &str) {
    if let Err(err) = guard.check(sql) {
        panic!("expected the charter's own allowlist to admit `{sql}`, got {err:?}");
    }
}

#[track_caller]
fn assert_denied_by(guard: &SqlGuard, sql: &str, want: &str) {
    match guard.check(sql) {
        Err(GuardError::Denied { rule, .. }) => assert_eq!(rule, want, "denial rule for `{sql}`"),
        other => panic!("expected `{sql}` to be denied by '{want}', got {other:?}"),
    }
}

// -- the defect ---------------------------------------------------------------------

/// A charter that may create exactly `postgis` must not be able to drop `pgcrypto`.
#[test]
fn a_charter_allowlisting_one_extension_cannot_drop_another() {
    let guard = guard_allowing(&["postgis"]);

    assert_denied_by(
        &guard,
        "DROP EXTENSION pgcrypto",
        rule::UNRECOGNIZED_DANGEROUS,
    );
    assert_denied_by(
        &guard,
        "DROP EXTENSION IF EXISTS pgcrypto",
        rule::UNRECOGNIZED_DANGEROUS,
    );
}

/// The two directions of ONE grant. Both name-checked, both against the same entry.
#[test]
fn the_grant_means_the_same_in_both_directions() {
    let guard = guard_allowing(&["postgis"]);

    // Control: the granted name still works in both directions. Without this a
    // build that refused every DROP EXTENSION would pass the case above.
    assert_admitted(&guard, "CREATE EXTENSION postgis");
    assert_admitted(&guard, "DROP EXTENSION postgis");
    assert_admitted(&guard, "DROP EXTENSION IF EXISTS postgis");

    // Control: the create side is unchanged - an ungranted name was always refused
    // there, and by its own rule.
    assert_denied_by(
        &guard,
        "CREATE EXTENSION pgcrypto",
        rule::EXTENSION_NOT_ALLOWLISTED,
    );
}

/// The empty allowlist is deny-all, in both directions.
#[test]
fn no_extension_grant_denies_both_directions() {
    let guard = guard_allowing(&[]);

    assert_denied_by(
        &guard,
        "CREATE EXTENSION postgis",
        rule::EXTENSION_NOT_ALLOWLISTED,
    );
    assert_denied_by(
        &guard,
        "DROP EXTENSION postgis",
        rule::UNRECOGNIZED_DANGEROUS,
    );
}

/// `FORBIDDEN_EXTENSIONS` is not grantable, and a charter naming one of them buys
/// nothing in either direction: the create side already hard-denied it, and the drop
/// side must not hand back the authority the create side refuses.
#[test]
fn the_hard_deny_is_not_grantable_in_either_direction() {
    let guard = guard_allowing(&["dblink", "postgis"]);

    assert_denied_by(&guard, "CREATE EXTENSION dblink", rule::FORBIDDEN_EXTENSION);
    assert_denied_by(
        &guard,
        "DROP EXTENSION dblink",
        rule::UNRECOGNIZED_DANGEROUS,
    );
    // The rest of the same charter's allowlist is untouched by the hard deny.
    assert_admitted(&guard, "CREATE EXTENSION postgis");
    assert_admitted(&guard, "DROP EXTENSION postgis");
}

/// A multi-object `DROP` is decided target by target, the way `DROP SCHEMA a, b`
/// already is: every name has to be granted, so one ungranted name refuses the
/// whole statement.
#[test]
fn every_named_extension_must_be_granted() {
    let guard = guard_allowing(&["postgis", "citext"]);

    assert_admitted(&guard, "DROP EXTENSION postgis, citext");
    assert_denied_by(
        &guard,
        "DROP EXTENSION postgis, pgcrypto",
        rule::UNRECOGNIZED_DANGEROUS,
    );
}

/// The name in the statement folds the way the create side folds it: an unquoted
/// identifier downcases, so one allowlist entry covers both spellings on both sides.
#[test]
fn the_name_folds_the_same_way_on_both_sides() {
    let guard = guard_allowing(&["postgis"]);

    assert_admitted(&guard, "CREATE EXTENSION PostGIS");
    assert_admitted(&guard, "DROP EXTENSION PostGIS");
    assert_denied_by(
        &guard,
        "DROP EXTENSION PgCrypto",
        rule::UNRECOGNIZED_DANGEROUS,
    );
}

/// A hyphenated quoted name is a real extension name (`uuid-ossp`), and it has to
/// resolve rather than fall into the unresolvable-name refusal.
#[test]
fn a_quoted_hyphenated_name_resolves() {
    let guard = guard_allowing(&["uuid-ossp"]);

    assert_admitted(&guard, "CREATE EXTENSION \"uuid-ossp\"");
    assert_admitted(&guard, "DROP EXTENSION IF EXISTS \"uuid-ossp\"");
    assert_denied_by(
        &guard,
        "DROP EXTENSION pgcrypto",
        rule::UNRECOGNIZED_DANGEROUS,
    );
}

//! **A live PostgreSQL oracle for the field-def emitter's `DEFAULT` and range `CHECK`.**
//!
//! `schema::query::def_to_constraints_for_dialect` turns one SDK field def
//! (`{ "type": …, "default": …, "min": …, "max": … }`) into the trailing column
//! constraints of a `CREATE TABLE`. Its `DEFAULT` block matched six type tokens
//! (`string`, `number`, `boolean`, `json`, `object`, `array`) and dropped the
//! declared default for every other token on the floor via `_ => {}`, and its
//! range block gated on `Some("number")` alone, so an integer column's `min`/`max`
//! produced no `CHECK` either.
//!
//! Both losses are SILENT: the emitted SQL is valid, the table is created, and the
//! declaration simply is not in it. The fold's `FieldDef` map carries the `default`
//! and the recovered `min`/`max` correctly - the emitter is the half that drops
//! them - so an instrument that compared the emitter against the fold's own answer
//! could never have found this. It would have been measuring agreement between two
//! halves of the same producer. This file feeds the emitter a field-def map DIRECTLY
//! and adjudicates it with a server that had no part in producing it.
//! (`sqlite_rebuild_field_defs_live.rs` is the companion that starts from a real
//! fold; it asserts the map's own claim before the server's.)
//!
//! # Why this file asks a server rather than a string
//!
//! An assertion over the emitter's own output string is the emitter grading its own
//! homework: it pins the spelling that was emitted, not that PostgreSQL stored a
//! default. So every claim here is read back out of `pg_catalog` AFTER the emitted
//! DDL has been executed by a real server, and the `INSERT` at the end proves the
//! default is the value PostgreSQL actually substitutes and the `CHECK` is a
//! predicate PostgreSQL actually enforces.
//!
//! # Which catalog relation, and why
//!
//! * DEFAULTs come from `pg_catalog.pg_attrdef`, rendered with
//!   `pg_get_expr(adbin, adrelid)`. `information_schema.columns.column_default`
//!   reports the same expression, but `information_schema` is a set of VIEWS
//!   filtered by the current role's privileges, so a permission difference between
//!   the connecting role and the table owner turns a missing default and an
//!   invisible row into the same answer. `pg_attrdef` IS the storage.
//! * CHECKs come from `pg_catalog.pg_constraint` (`contype = 'c'`), joined to
//!   `unnest(conkey)` so the constraint is attributed to the exact column it
//!   constrains. `information_schema.check_constraints` is the wrong source twice
//!   over: it carries no column at all (the association lives in a separate
//!   `constraint_column_usage` view), and PostgreSQL materialises every `NOT NULL`
//!   as a synthetic `IS NOT NULL` row in it, so a count taken there answers a
//!   different question than the one asked.
//!
//! # What this file does NOT prove
//!
//! `def_to_constraints_for_dialect` is dialect-parameterised and this file drives
//! its PostgreSQL arm on a real PostgreSQL server. The only PRODUCTION caller that
//! reaches it today is the SQLite 12-step table rebuild
//! (`render/declarative.rs`'s `desired.sqlite_schemas` arm, `SqlDialect::Sqlite`
//! hard-coded); `sqlite_rebuild_field_defs_live.rs` is the live oracle for that
//! path. So this file measures the emitter, not a shipped PostgreSQL code path.

use crate::support;

use serde_json::json;
use zero_migrate::schema::query::{
    build_create_table_with_fks_for_dialect_scoped_statements, FkEmission, SqliteEmitScope,
};
use zero_migrate::SqlDialect;

/// A schema name nobody else in the suite will collide with.
fn test_schema() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "field_def_facets_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

/// The columns under test, as ONE SDK field-def map.
///
/// Every entry is a type token `def_to_pg_type` already knows how to spell, paired
/// with the facet the emitter was dropping. `bigint_col`'s default is deliberately
/// `9007199254740993` - one past the largest integer an IEEE-754 double represents
/// exactly - so a renderer that routes the value through `f64` writes
/// `9007199254740992` and the server reports the difference.
fn field_defs() -> serde_json::Value {
    json!({
        "int_col":      { "type": "int",      "default": 7 },
        "integer_col":  { "type": "integer",  "default": 11 },
        "small_col":    { "type": "smallInt", "default": 3 },
        "bigint_col":   { "type": "bigInt",   "default": 9_007_199_254_740_993_i64 },
        "real_col":     { "type": "real",     "default": 1.5 },
        "char_col":     { "type": "char",     "default": "ab", "charLen": 2 },
        "inet_col":     { "type": "inet",     "default": "10.0.0.1" },
        "ranged_int":   { "type": "int",      "min": 1, "max": 9 },
        "ranged_big":   { "type": "bigInt",   "min": 2 },
        // The CONTROLS. These two tokens were already handled, so they must read
        // back unchanged - a fix that widened the match by relaxing the whole block
        // would move them too.
        "text_col":     { "type": "string",   "default": "dark" },
        "number_col":   { "type": "number",   "default": 2.5, "min": 0, "max": 10 },
    })
}

/// Render the `CREATE TABLE` for `field_defs()` through the real emitter.
fn create_table_sql(schema: &str) -> Vec<String> {
    build_create_table_with_fks_for_dialect_scoped_statements(
        schema,
        "facets",
        &field_defs(),
        &FkEmission::Inline,
        SqlDialect::Postgres,
        SqliteEmitScope::AttachAlias,
        &support::no_inject(schema),
    )
    .expect("the field-def map renders a CREATE TABLE")
}

/// `column -> pg_get_expr(adbin, adrelid)` for every column of `schema.facets`
/// that has a stored default. Read from `pg_attrdef`, the catalog that HOLDS the
/// default expression.
fn server_defaults(
    client: &mut postgres::Client,
    schema: &str,
) -> std::collections::BTreeMap<String, String> {
    client
        .query(
            "SELECT a.attname::text, pg_get_expr(d.adbin, d.adrelid)
             FROM pg_catalog.pg_attribute a
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_catalog.pg_attrdef d
               ON d.adrelid = a.attrelid AND d.adnum = a.attnum
             WHERE n.nspname = $1 AND c.relname = 'facets' AND a.attnum > 0
               AND NOT a.attisdropped",
            &[&schema],
        )
        .expect("read pg_attrdef")
        .into_iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect()
}

/// `column -> pg_get_constraintdef(oid)` for every single-column `CHECK` on
/// `schema.facets`. Read from `pg_constraint` with `conkey` unnested, so each
/// predicate is attributed to the column it actually constrains.
fn server_checks(
    client: &mut postgres::Client,
    schema: &str,
) -> std::collections::BTreeMap<String, String> {
    client
        .query(
            "SELECT a.attname::text, pg_get_constraintdef(k.oid)
             FROM pg_catalog.pg_constraint k
             JOIN pg_catalog.pg_class c ON c.oid = k.conrelid
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             JOIN LATERAL unnest(k.conkey) AS key(attnum) ON true
             JOIN pg_catalog.pg_attribute a
               ON a.attrelid = k.conrelid AND a.attnum = key.attnum
             WHERE n.nspname = $1 AND c.relname = 'facets' AND k.contype = 'c'",
            &[&schema],
        )
        .expect("read pg_constraint")
        .into_iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect()
}

/// **The oracle.** Emit the `CREATE TABLE` for a field-def map, run it on a real
/// PostgreSQL server, and read every `DEFAULT` and range `CHECK` back out of the
/// catalog.
#[test]
fn a_declared_default_and_range_reach_the_postgres_catalog() {
    let url = skip_if_no_pg!();
    let mut client =
        postgres::Client::connect(&url, postgres::NoTls).expect("connect to the live server");
    let schema = test_schema();

    client
        .batch_execute(&format!("CREATE SCHEMA \"{schema}\""))
        .expect("create the throwaway schema");

    let run = || -> Result<(), postgres::Error> {
        let mut client =
            postgres::Client::connect(&url, postgres::NoTls).expect("connect to the live server");
        for statement in create_table_sql(&schema) {
            client.batch_execute(&statement)?;
        }

        let defaults = server_defaults(&mut client, &schema);
        let checks = server_checks(&mut client, &schema);

        // ---- the controls, first: the two tokens that already worked -------------
        assert_eq!(
            defaults.get("text_col").map(String::as_str),
            Some("'dark'::text"),
            "a `string` default already reached the server and must still; \
             server defaults were {defaults:?}"
        );
        assert_eq!(
            defaults.get("number_col").map(String::as_str),
            Some("2.5"),
            "a `number` default already reached the server and must still"
        );
        assert_eq!(
            checks.get("number_col").map(String::as_str),
            Some("CHECK (((number_col >= (0)::double precision) AND (number_col <= (10)::double precision)))"),
            "a `number` range already reached the server and must still; \
             server checks were {checks:?}"
        );

        // ---- the losses -----------------------------------------------------------
        assert_eq!(
            defaults.get("int_col").map(String::as_str),
            Some("7"),
            "PostgreSQL must hold the `int` column's declared DEFAULT; \
             server defaults were {defaults:?}"
        );
        assert_eq!(
            defaults.get("integer_col").map(String::as_str),
            Some("11"),
            "and the `integer` spelling of the same token"
        );
        assert_eq!(
            defaults.get("small_col").map(String::as_str),
            Some("3"),
            "and a `smallInt` default"
        );
        assert_eq!(
            defaults.get("bigint_col").map(String::as_str),
            Some("'9007199254740993'::bigint"),
            "and a `bigInt` default BEYOND f64's exact range, digit for digit - \
             a renderer that goes through a double writes ...992 here"
        );
        assert_eq!(
            defaults.get("real_col").map(String::as_str),
            Some("1.5"),
            "and a `real` default"
        );
        assert_eq!(
            defaults.get("char_col").map(String::as_str),
            // `bpchar`, not `text`: the PostgreSQL renderer spells a `char` column
            // `character(2)`, and PostgreSQL records the default already coerced to
            // the column's own type.
            Some("'ab'::bpchar"),
            "and a `char` default"
        );
        assert_eq!(
            defaults.get("inet_col").map(String::as_str),
            Some("'10.0.0.1'::inet"),
            "and an `inet` default"
        );
        assert_eq!(
            checks.get("ranged_int").map(String::as_str),
            Some("CHECK (((ranged_int >= 1) AND (ranged_int <= 9)))"),
            "an `int` column's min/max must be a CHECK PostgreSQL enforces; \
             server checks were {checks:?}"
        );
        assert_eq!(
            checks.get("ranged_big").map(String::as_str),
            Some("CHECK ((ranged_big >= 2))"),
            "and a min-only bound on `bigInt`"
        );

        // ---- and the server BEHAVES that way, not merely records it ---------------
        client.batch_execute(&format!(
            "INSERT INTO \"{schema}\".facets (ranged_int, ranged_big) VALUES (5, 5)"
        ))?;
        let row = client.query_one(
            &format!(
                "SELECT int_col, integer_col, small_col, bigint_col, real_col, \
                 char_col, inet_col::text, text_col, number_col \
                 FROM \"{schema}\".facets"
            ),
            &[],
        )?;
        assert_eq!(
            row.get::<_, i32>(0),
            7,
            "the row PostgreSQL defaulted `int`"
        );
        assert_eq!(row.get::<_, i32>(1), 11, "and `integer`");
        assert_eq!(row.get::<_, i16>(2), 3, "and `smallInt`");
        assert_eq!(
            row.get::<_, i64>(3),
            9_007_199_254_740_993_i64,
            "and the exact 64-bit `bigInt`"
        );
        assert!(
            (row.get::<_, f32>(4) - 1.5_f32).abs() < f32::EPSILON,
            "and `real`"
        );
        assert_eq!(row.get::<_, String>(5), "ab", "and `char`");
        // `10.0.0.1/32`, not `10.0.0.1`: PostgreSQL 18's canonical text form for an
        // `inet` carries the prefix length. The stored value is the declared one.
        assert_eq!(row.get::<_, String>(6), "10.0.0.1/32", "and `inet`");
        assert_eq!(row.get::<_, String>(7), "dark", "and the string control");
        assert!(
            (row.get::<_, f64>(8) - 2.5_f64).abs() < f64::EPSILON,
            "and the number control"
        );

        // The range CHECKs REJECT, which no catalog row can prove on its own.
        assert!(
            client
                .batch_execute(&format!(
                    "INSERT INTO \"{schema}\".facets (ranged_int, ranged_big) VALUES (99, 5)"
                ))
                .is_err(),
            "PostgreSQL must refuse a value above the declared `int` max"
        );
        assert!(
            client
                .batch_execute(&format!(
                    "INSERT INTO \"{schema}\".facets (ranged_int, ranged_big) VALUES (5, 1)"
                ))
                .is_err(),
            "and one below the declared `bigInt` min"
        );
        Ok(())
    };

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    let _ = client.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("live PostgreSQL rejected a statement: {error}"),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

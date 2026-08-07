//! Namespace-authority conservative-deny suite: raw-SQL create/rename
//! classification, creation gating, and injected-shape immutability.
//!
//! Each rule the policy redesign introduces is a fail-closed decision with the
//! design's named error code. These tests drive the guard's PUBLIC API
//! (`SqlGuard::check`) over a caller-composed [`EffectivePolicy`] carrying inject
//! rules + scoped creation/raw-SQL grants (via `GuardConfig::from_policy`),
//! so both the DENY cases and their ACCEPT counterparts are exercised on the real
//! parse-time path.

mod support;

use zero_migrate_guard::guard::{namespace_rule, GuardConfig, GuardError, SqlGuard};
use zero_migrate_ir::dialect::SqlDialect;

/// A confined guard over the composed policy, pinned to project schema `app`.
fn guard_with(charter_toml: &str) -> SqlGuard {
    SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter_toml),
        SqlDialect::Postgres,
    ))
}

fn assert_namespace_denied(guard: &SqlGuard, sql: &str, want_rule: &str) {
    match guard.check(sql) {
        Err(GuardError::NamespacePolicy { rule, .. }) => assert_eq!(
            rule, want_rule,
            "expected {want_rule} for `{sql}`, got {rule}"
        ),
        other => panic!("expected NamespacePolicy({want_rule}) for `{sql}`, got {other:?}"),
    }
}

// ── a charter that mandatorily injects over the whole app-schema universe ───────
// `schema.create_table` / `schema.rename` are granted only inside `app` (creatable ⊑
// injected). `deleted_at` is an injected column; the PK is pinned to `id`.
const INJECT_APP_CHARTER: &str = r#"policy_version = 1
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.rename"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.alter_injected"
value = false
scope = { include = ["app"] }
[[inject]]
scope = { include = ["app"] }
mandatory = true
primary_key = ["id"]
columns = [
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
  { name = "deleted_at", type = "timestamptz", nullable = true  },
]
indexes = [ { name = "ix_deleted_at", columns = ["deleted_at"] } ]
"#;

// ════════════════════════════════════════════════════════════════════════════════
// 1. Raw CREATE TABLE inside an inject scope → RawCreateInInjectScope (DENY)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn raw_create_table_in_inject_scope_is_denied() {
    // `app.t` is covered by the mandatory inject rule; injection cannot rewrite raw
    // text, so the structured DSL is the only way to make an injected table.
    let g = guard_with(INJECT_APP_CHARTER);
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t (id text)",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

#[test]
fn raw_ctas_and_select_into_in_inject_scope_are_denied() {
    let g = guard_with(INJECT_APP_CHARTER);
    // CTAS spelling.
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t AS SELECT 1 AS id",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
    // SELECT … INTO spelling — same target, same denial.
    assert_namespace_denied(
        &g,
        "SELECT 1 AS id INTO app.t",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

// ================================================================================
// 1b. Raw CREATE TABLE that CONFORMS to the covering injects is ADMITTED
// ================================================================================
//
// The rule is a conformance check, not a blanket deny: the structured resolver
// renders the injected columns into the CREATE TABLE text it hands the guard, so a
// create whose column list already carries every injected column and whose declared
// PK equals the pinned one is exactly what the inject asked for.

#[test]
fn raw_create_carrying_the_full_injected_shape_is_admitted() {
    let g = guard_with(INJECT_APP_CHARTER);
    // Every injected column (`id`, `created_at`, `deleted_at`) is present and the
    // declared PK is the pinned `(id)`, spelled as a column-level marker. Author
    // columns beyond the injected set are free.
    g.check(
        "CREATE TABLE app.t (id text PRIMARY KEY NOT NULL, \
         created_at timestamptz NOT NULL, deleted_at timestamptz, title text)",
    )
    .expect("a create carrying the full injected shape conforms and is admitted");
}

#[test]
fn conforming_create_accepts_the_table_level_primary_key_spelling() {
    let g = guard_with(INJECT_APP_CHARTER);
    // `PRIMARY KEY (id)` as a table constraint is the same pinned key as the
    // column-level marker.
    g.check(
        "CREATE TABLE app.t (id text NOT NULL, created_at timestamptz NOT NULL, \
         deleted_at timestamptz, PRIMARY KEY (id))",
    )
    .expect("a table-level PRIMARY KEY (id) matches the pinned key");
}

#[test]
fn conformance_folds_column_names_like_the_immutability_rules() {
    let g = guard_with(INJECT_APP_CHARTER);
    // Unquoted identifiers fold to lowercase, so `Created_At` is `created_at`: the
    // same fold the injected-shape immutability check uses.
    g.check(
        "CREATE TABLE app.t (Id text PRIMARY KEY NOT NULL, \
         Created_At timestamptz NOT NULL, DELETED_AT timestamptz)",
    )
    .expect("a case-varying but folding-equal column list conforms");
}

#[test]
fn create_short_one_injected_column_is_denied() {
    let g = guard_with(INJECT_APP_CHARTER);
    // `deleted_at` is missing, so the create does not carry the injected shape, and
    // raw text cannot be rewritten to add it.
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t (id text PRIMARY KEY NOT NULL, created_at timestamptz NOT NULL)",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

#[test]
fn conforming_create_outside_the_create_table_grant_is_still_denied() {
    // Conformance does not replace the creation grant: a create that carries the
    // whole injected shape in a schema the create grant does not cover is denied by
    // `schema.create_table`, not waved through.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app", "staging"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[inject]]
scope = { include = ["staging"] }
columns = [ { name = "id", type = "text", nullable = false } ]
"#;
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    assert_namespace_denied(
        &g,
        "CREATE TABLE staging.t (id text NOT NULL)",
        namespace_rule::CREATE_TABLE_NOT_GRANTED,
    );
}

#[test]
fn create_with_a_wrong_or_absent_primary_key_is_denied() {
    let g = guard_with(INJECT_APP_CHARTER);
    let full = "id text NOT NULL, created_at timestamptz NOT NULL, deleted_at timestamptz";
    // A different PK column: a real key-shape change, not the pinned `(id)`.
    assert_namespace_denied(
        &g,
        &format!("CREATE TABLE app.t ({full}, PRIMARY KEY (created_at))"),
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
    // An EXTRA PK column: `(id, created_at)` is a different key than `(id)`, so
    // containment is not enough.
    assert_namespace_denied(
        &g,
        &format!("CREATE TABLE app.t ({full}, PRIMARY KEY (id, created_at))"),
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
    // No declared PK at all against a pinned PK.
    assert_namespace_denied(
        &g,
        &format!("CREATE TABLE app.t ({full})"),
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

#[test]
fn create_whose_columns_the_parse_cannot_enumerate_is_denied() {
    let g = guard_with(INJECT_APP_CHARTER);
    let full = "id text PRIMARY KEY NOT NULL, created_at timestamptz NOT NULL, \
                deleted_at timestamptz";
    // `LIKE` copies columns the parse cannot see, so the column list is short by
    // construction and conformance is unprovable.
    assert_namespace_denied(
        &g,
        &format!("CREATE TABLE app.t ({full}, LIKE app.src)"),
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
    // `INHERITS` likewise brings in parent columns.
    assert_namespace_denied(
        &g,
        &format!("CREATE TABLE app.t ({full}) INHERITS (app.base)"),
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
    // `PARTITION OF` takes its whole shape from the parent.
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t PARTITION OF app.p FOR VALUES IN ('x')",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
    // `OF <type>` takes its whole shape from the composite type.
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t OF app.shape",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

#[test]
fn a_quoted_column_name_does_not_satisfy_an_unquoted_injected_column() {
    let g = guard_with(INJECT_APP_CHARTER);
    // The parser hands the guard the exact PostgreSQL identifier: an unquoted
    // `Created_At` arrives already folded to `created_at`, a quoted `"Created_At"`
    // arrives verbatim. A quoted spelling therefore names a DIFFERENT column than
    // the injected `created_at`, and a create that omits the injected column must
    // not be admitted just because a case-insensitive comparison collides.
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t (\"Id\" text PRIMARY KEY NOT NULL, \
         \"Created_At\" timestamptz NOT NULL, \"Deleted_At\" timestamptz)",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

#[test]
fn a_trailing_space_in_a_quoted_column_name_does_not_satisfy_the_injected_column() {
    let g = guard_with(INJECT_APP_CHARTER);
    // `"id "` is a genuinely distinct PostgreSQL column from `id`; trimming the
    // declared name would silently merge the two.
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t (\"id \" text PRIMARY KEY NOT NULL, \
         \"created_at \" timestamptz NOT NULL, \"deleted_at \" timestamptz)",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

#[test]
fn a_quoted_primary_key_column_does_not_satisfy_the_pinned_key() {
    let g = guard_with(INJECT_APP_CHARTER);
    // `PRIMARY KEY ("ID")` keys the table on a column named `ID`, which the create
    // does not even declare; it is not the pinned `(id)`.
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t (id text NOT NULL, created_at timestamptz NOT NULL, \
         deleted_at timestamptz, PRIMARY KEY (\"ID\"))",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

#[test]
fn an_inject_that_constrains_no_column_and_no_key_cannot_admit_a_raw_create() {
    // `columns` is optional, so an `[[inject]]` carrying only `indexes` is a legal
    // charter. It obliges the create to nothing the statement text can prove, so
    // the create stays denied rather than being waved through by a vacuous check.
    let indexes_only = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[inject]]
scope = { include = ["app"] }
mandatory = true
indexes = [ { name = "ix_tenant", columns = ["tenant_id"] } ]
"#;
    assert_namespace_denied(
        &guard_with(indexes_only),
        "CREATE TABLE app.t (title text)",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );

    // The same holds for an inject block that carries nothing at all.
    let empty_inject = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[inject]]
scope = { include = ["app"] }
mandatory = true
"#;
    assert_namespace_denied(
        &guard_with(empty_inject),
        "CREATE TABLE app.t (title text)",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

#[test]
fn a_pinned_composite_key_is_matched_in_order() {
    // `(a, b)` and `(b, a)` are different indexes with different leading columns, so
    // a pinned key is an ordered list, not a set.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[inject]]
scope = { include = ["app"] }
mandatory = true
primary_key = ["a", "b"]
columns = [
  { name = "a", type = "text", nullable = false },
  { name = "b", type = "text", nullable = false },
]
"#;
    let g = guard_with(charter);
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t (a text NOT NULL, b text NOT NULL, PRIMARY KEY (b, a))",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
    g.check("CREATE TABLE app.t (a text NOT NULL, b text NOT NULL, PRIMARY KEY (a, b))")
        .expect("the pinned key in the pinned order conforms");
}

#[test]
fn two_primary_key_declarations_are_unreadable_and_denied() {
    let g = guard_with(INJECT_APP_CHARTER);
    // Postgres itself rejects a table with two primary keys, so there is no single
    // declared key to compare against the pinned one.
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t (id text PRIMARY KEY NOT NULL, created_at timestamptz NOT NULL, \
         deleted_at timestamptz, PRIMARY KEY (id))",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

#[test]
fn two_covering_injects_pinning_different_keys_admit_nothing() {
    // Every covering inject must be satisfied, and no single create can declare two
    // different primary keys, so a charter with contradictory pins denies outright.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[inject]]
scope = { include = ["app"] }
primary_key = ["a"]
columns = [ { name = "a", type = "text", nullable = false } ]
[[inject]]
scope = { include = ["app"] }
primary_key = ["b"]
columns = [ { name = "b", type = "text", nullable = false } ]
"#;
    let g = guard_with(charter);
    for pk in ["PRIMARY KEY (a)", "PRIMARY KEY (b)", "PRIMARY KEY (a, b)"] {
        assert_namespace_denied(
            &g,
            &format!("CREATE TABLE app.t (a text NOT NULL, b text NOT NULL, {pk})"),
            namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
        );
    }
}

#[test]
fn an_inject_without_a_pinned_key_does_not_require_one() {
    // Conformance demands only what the inject rule states: an inject that
    // contributes columns but pins no key leaves the primary key to the author,
    // including having none at all.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[inject]]
scope = { include = ["app"] }
mandatory = true
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#;
    let g = guard_with(charter);
    g.check("CREATE TABLE app.t (title text, created_at timestamptz NOT NULL)")
        .expect("no pinned key means no declared key is required");
    g.check("CREATE TABLE app.t (id text PRIMARY KEY, created_at timestamptz NOT NULL)")
        .expect("no pinned key means an author key is free");
}

// -- an injected column whose name contains a dot -------------------------------
// A dotted name is one identifier, not a schema-qualified pair, so the create must
// declare a column literally named `a.b` and nothing else counts.
const INJECT_DOTTED_COLUMN_CHARTER: &str = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[inject]]
scope = { include = ["app"] }
mandatory = true
columns = [ { name = "a.b", type = "text", nullable = false } ]
"#;

#[test]
fn a_case_varying_quoted_dotted_column_does_not_satisfy_the_injected_column() {
    let g = guard_with(INJECT_DOTTED_COLUMN_CHARTER);
    // `"A.B"` is a column literally named `A.B`; the injected column is `a.b`, so
    // the create omits it.
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t (\"A.B\" text NOT NULL)",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
    // The exact injected name still conforms, so the rule is a name match and not a
    // blanket deny on dotted names.
    g.check("CREATE TABLE app.t (\"a.b\" text NOT NULL)")
        .expect("a create declaring the injected dotted column conforms");
}

// ════════════════════════════════════════════════════════════════════════════════
// 2. Structured/raw create OUTSIDE the schema.create_table grant → denied
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn create_outside_the_create_table_grant_is_denied() {
    // A charter that grants create only in `app`, injects nothing (so no
    // RawCreateInInjectScope) — a create in `staging` is not granted.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["staging"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
"#;
    // `staging` is in cross-schema scope (so `staging.t` is not a CrossSchema
    // violation) but NOT in the create grant (`app`-only) → CreateTableNotGranted.
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    assert_namespace_denied(
        &g,
        "CREATE TABLE staging.t (id text)",
        namespace_rule::CREATE_TABLE_NOT_GRANTED,
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// 3. ALTER TABLE … DROP COLUMN <injected> → InjectedShapeImmutable (DENY)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn drop_injected_column_is_immutable() {
    let g = guard_with(INJECT_APP_CHARTER);
    // `deleted_at` (a covering-inject column) cannot be dropped without an explicit
    // schema.alter_injected grant (the charter grants it `false`).
    assert_namespace_denied(
        &g,
        "ALTER TABLE app.t DROP COLUMN deleted_at",
        namespace_rule::INJECTED_SHAPE_IMMUTABLE,
    );
}

#[test]
fn drop_non_injected_column_is_allowed() {
    let g = guard_with(INJECT_APP_CHARTER);
    // A plain author column is not injected → the immutability rule does not fire.
    g.check("ALTER TABLE app.t DROP COLUMN title")
        .expect("dropping a non-injected column is allowed");
}

#[test]
fn drop_pinned_pk_constraint_is_immutable() {
    let g = guard_with(INJECT_APP_CHARTER);
    // The covering inject rule pins the PK → any DROP CONSTRAINT is fail-closed
    // immutable (the guard cannot always tell which constraint is the PK by name).
    assert_namespace_denied(
        &g,
        "ALTER TABLE app.t DROP CONSTRAINT t_pkey",
        namespace_rule::INJECTED_PRIMARY_KEY_IMMUTABLE,
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// 4. SET search_path / CREATE FUNCTION under a Scoped sql.raw grant → DENY
// ════════════════════════════════════════════════════════════════════════════════

// A charter with `sql.raw` granted only in `app` (Scoped, non-⊤) + a ⊤ create
// grant so ordinary DDL is not incidentally denied by creation-gating.
const SCOPED_RAW_SQL_CHARTER: &str = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
[[grant]]
key = "schema.rename"
value = true
scope = "all"
"#;

#[test]
fn set_search_path_under_scoped_raw_sql_is_denied() {
    let g = guard_with(SCOPED_RAW_SQL_CHARTER);
    assert_namespace_denied(
        &g,
        "SET search_path TO app, public",
        namespace_rule::SEARCH_PATH_UNDER_SCOPED_RAW_SQL,
    );
}

#[test]
fn create_function_under_scoped_raw_sql_is_denied() {
    let g = guard_with(SCOPED_RAW_SQL_CHARTER);
    assert_namespace_denied(
        &g,
        "CREATE FUNCTION app.f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        namespace_rule::OPAQUE_BODY_UNDER_SCOPED_RAW_SQL,
    );
}

#[test]
fn unqualified_reference_under_scoped_raw_sql_is_denied() {
    let g = guard_with(SCOPED_RAW_SQL_CHARTER);
    // An unqualified relation reference cannot be attributed (no live search_path to
    // resolve it) → matches only a ⊤-scoped grant → deny.
    assert_namespace_denied(
        &g,
        "INSERT INTO widgets (id) VALUES ('x')",
        namespace_rule::UNQUALIFIED_NAME_UNDER_SCOPED_RAW_SQL,
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// 5. Raw rename INTO an inject scope with a nonconforming shape → denied
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn rename_into_scope_without_rename_grant_is_denied() {
    // A same-schema `RENAME TO` that crosses an inject/rename sub-boundary within the
    // pinned schema (so cross-schema confinement does NOT fire). `schema.rename`
    // is granted only over `app.keep_*`; renaming `app.t` → `app.other` lands outside
    // the rename grant and (no inject covers `app.other`) is denied for lack of it.
    let charter = r#"policy_version = 1
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
[[grant]]
key = "schema.rename"
value = true
scope = { include = ["app.keep_*"] }
"#;
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    assert_namespace_denied(
        &g,
        "ALTER TABLE app.t RENAME TO other",
        namespace_rule::RENAME_INTO_NOT_GRANTED,
    );
}

#[test]
fn rename_into_inject_scope_is_denied_raw() {
    // A same-schema `RENAME TO` where the NEW name lands in a mandatory-inject
    // sub-scope (`app.injected_*`): raw text cannot carry the injection, so the move
    // is RawRenameIntoInjectScope. `app.t` (source) is injection-free.
    let charter = r#"policy_version = 1
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
[[grant]]
key = "schema.rename"
value = true
scope = "all"
[[inject]]
scope = { include = ["app.injected_*"] }
columns = [ { name = "id", type = "text", nullable = false } ]
"#;
    // The inject need not be mandatory: RawRenameIntoInjectScope denies a raw move
    // into ANY inject scope, because raw text cannot carry the injection.
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    assert_namespace_denied(
        &g,
        "ALTER TABLE app.t RENAME TO injected_t",
        namespace_rule::RAW_RENAME_INTO_INJECT_SCOPE,
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// ACCEPT cases — the enforcement is not over-broad
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn granted_create_outside_any_inject_scope_is_allowed() {
    // create in `app` where create_table is granted AND no inject covers it.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
"#;
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    g.check("CREATE TABLE app.plain (id text)")
        .expect("a granted create outside any inject scope is allowed");
}

#[test]
fn dsl_path_injected_create_renders_and_passes() {
    // The DSL/structured path renders the injected columns into the CREATE TABLE
    // fragment, and the guard admits that fragment because it conforms (see
    // `raw_create_carrying_the_full_injected_shape_is_admitted`). This ACCEPT case
    // covers the neighbouring path: a create in `app` under a create grant with NO
    // inject covering it, where conformance never comes up at all.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[inject]]
scope = { include = ["other"] }
columns = [ { name = "id", type = "text", nullable = false } ]
"#;
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    // `app.t` is granted + no inject covers `app.*` (the inject is scoped to `other`).
    g.check("CREATE TABLE app.t (id text)")
        .expect("granted create in a scope no inject covers is allowed");
}

#[test]
fn top_raw_sql_admits_search_path_and_opaque_body() {
    // A ⊤-scoped sql.raw grant is the fully-trusted raw posture: SET search_path
    // and CREATE FUNCTION are admitted (the deny-list still runs, so use a benign
    // trusted-language body + a benign GUC).
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"
[[grant]]
key = "sql.raw"
value = true
scope = "all"
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
"#;
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    // SET search_path is NOT refused under a ⊤ grant.
    match g.check("SET search_path TO app, public") {
        Ok(_) => {}
        Err(GuardError::NamespacePolicy { rule, .. }) => {
            panic!("⊤ raw_sql must admit SET search_path, got {rule}")
        }
        // The deny-list may still classify SET search_path (FORBIDDEN_SET) — that is a
        // separate Denied belt, not a NamespacePolicy refusal. Accept either non-namespace
        // outcome; the point is the SCOPED-raw-SQL namespace rule does not fire.
        Err(_) => {}
    }
    // A trusted-language CREATE FUNCTION is not refused as an opaque body under ⊤.
    if let Err(GuardError::NamespacePolicy { rule, .. }) =
        g.check("CREATE FUNCTION app.f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$")
    {
        panic!("⊤ raw_sql must not refuse an opaque body, got {rule}");
    }
}

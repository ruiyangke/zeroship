//! Live PostgreSQL regressions for drift-comparable ID metadata and references.
//!
//! The suite is gated by `ZERO_MIGRATE_TEST_PG_URL`. Every mutation runs in a
//! transaction that is rolled back after introspection, so all assertions share
//! one authoritative folded snapshot without mutation order coupling.

mod support;

use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{
    diff_snapshots, fold_ops, snapshot_schema, IrAuthor, LiveSchema, SchemaSnapshot, SqlDialect,
    StructuralDrift,
};

const OWNER: &str = "app_drift_id_facets";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "drift_id_facets_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn fixture(schema: &str) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "drift_id_facets_pg",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createSequence",
                "name": "counter_a",
                "schema": schema,
                "as": "bigInt"
            },
            {
                "op": "createSequence",
                "name": "counter_b",
                "schema": schema,
                "as": "bigInt"
            },
            {
                "op": "createTable",
                "name": "identity_facets",
                "columns": [
                    {
                        "name": "always_id",
                        "type": "bigInt",
                        "nullable": false,
                        "identity": { "always": true }
                    },
                    {
                        "name": "default_id",
                        "type": "bigInt",
                        "nullable": false,
                        "identity": { "always": false }
                    },
                    {
                        "name": "plain_id",
                        "type": "bigInt",
                        "nullable": false
                    }
                ],
                "primaryKey": null,
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "uuid_defaults",
                "columns": [
                    {
                        "name": "v4_id",
                        "type": "uuid",
                        "nullable": false,
                        "default": { "expr": { "node": "uuidV4" } }
                    },
                    {
                        "name": "v7_id",
                        "type": "uuid",
                        "nullable": false,
                        "default": { "expr": { "node": "uuidV7" } }
                    },
                    {
                        "name": "manual_id",
                        "type": "uuid",
                        "nullable": false
                    },
                    {
                        "name": "literal_id",
                        "type": "uuid",
                        "nullable": false,
                        "default": {
                            "expr": {
                                "node": "cast",
                                "operand": {
                                "node": "literal",
                                "value": "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA"
                                },
                                "target": "uuid"
                            }
                        }
                    }
                ],
                "primaryKey": null,
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "sequence_defaults",
                "columns": [
                    {
                        "name": "counter",
                        "type": "bigInt",
                        "nullable": false,
                        "default": {
                            "nextval": { "name": "counter_a", "schema": schema }
                        }
                    },
                    {
                        "name": "unqualified_counter",
                        "type": "bigInt",
                        "nullable": false,
                        "default": {
                            "nextval": { "name": "counter_a" }
                        }
                    }
                ],
                "primaryKey": null,
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "generated_id_facets",
                "columns": [
                    {
                        "name": "source_uuid",
                        "type": "uuid",
                        "nullable": false
                    },
                    {
                        "name": "derived_uuid",
                        "type": "uuid",
                        "nullable": false,
                        "generated": {
                            "expr": { "node": "colRef", "name": "source_uuid" },
                            "stored": true
                        }
                    },
                    {
                        "name": "source_type_id",
                        "type": "text",
                        "nullable": false
                    },
                    {
                        "name": "derived_type_id",
                        "type": "text",
                        "nullable": false,
                        "valueFormat": { "typeId": { "prefix": "account" } },
                        "generated": {
                            "expr": {
                                "node": "fnCall",
                                "fn": "lower",
                                "args": [{ "node": "colRef", "name": "source_type_id" }]
                            },
                            "stored": true
                        }
                    }
                ],
                "primaryKey": null,
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "type_keys",
                "columns": [{
                    "name": "id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": { "typeId": { "prefix": "account" } },
                    "default": {
                        "expr": {
                            "node": "cast",
                            "operand": {
                                "node": "fnCall",
                                "fn": "lower",
                                "args": [{
                                    "node": "literal",
                                    "value": "ACCOUNT_00000000000000000000000000"
                                }]
                            },
                            "target": "text"
                        }
                    }
                }],
                "primaryKey": ["id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "ulid_keys",
                "columns": [{
                    "name": "id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": "ulid",
                    "default": {
                        "literal": { "value": "00000000000000000000000000" }
                    }
                }],
                "primaryKey": ["id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "case_default_keys",
                "columns": [{
                    "name": "id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": { "typeId": { "prefix": "account" } },
                    "default": {
                        "expr": {
                            "node": "case",
                            "branches": [{
                                "when": { "node": "literal", "value": true },
                                "then": {
                                    "node": "literal",
                                    "value": "account_00000000000000000000000000"
                                }
                            }]
                        }
                    }
                }],
                "primaryKey": ["id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "trim_default_keys",
                "columns": [{
                    "name": "id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": { "typeId": { "prefix": "account" } },
                    "default": {
                        "expr": {
                            "node": "fnCall",
                            "fn": "trim",
                            "args": [{
                                "node": "literal",
                                "value": " account_00000000000000000000000000 "
                            }]
                        }
                    }
                }],
                "primaryKey": ["id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "single_parent",
                "columns": [{
                    "name": "id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": { "typeId": { "prefix": "account" } }
                }],
                "primaryKey": ["id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "single_parent_alt",
                "columns": [{
                    "name": "id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": { "typeId": { "prefix": "account" } }
                }],
                "primaryKey": ["id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "single_child",
                "columns": [{
                    "name": "parent_id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": { "typeId": { "prefix": "account" } },
                    "default": {
                        "expr": {
                            "node": "fnCall",
                            "fn": "lower",
                            "args": [{
                                "node": "literal",
                                "value": "ACCOUNT_00000000000000000000000000"
                            }]
                        }
                    },
                    "references": {
                        "table": "single_parent",
                        "column": "id",
                        "onDelete": "cascade",
                        "onUpdate": "restrict"
                    }
                }],
                "primaryKey": null,
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "composite_parent",
                "columns": [
                    { "name": "left_id", "type": "bigInt", "nullable": false },
                    { "name": "right_id", "type": "bigInt", "nullable": false }
                ],
                "primaryKey": ["left_id", "right_id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "composite_parent_alt",
                "columns": [
                    { "name": "left_id", "type": "bigInt", "nullable": false },
                    { "name": "right_id", "type": "bigInt", "nullable": false }
                ],
                "primaryKey": ["left_id", "right_id"],
                "constraints": [],
                "indexes": []
            },
            {
                "op": "createTable",
                "name": "composite_child",
                "columns": [
                    { "name": "parent_left", "type": "bigInt", "nullable": true },
                    { "name": "parent_right", "type": "bigInt", "nullable": true }
                ],
                "primaryKey": null,
                "constraints": [{
                    "name": "composite_child_parent_fk",
                    "kind": {
                        "kind": "fk",
                        "columns": ["parent_left", "parent_right"],
                        "referencesTable": "composite_parent",
                        "referencesColumns": ["left_id", "right_id"],
                        "onDelete": "setNull",
                        "onUpdate": "cascade",
                        "deferrable": true,
                        "initiallyDeferred": true
                    }
                }],
                "indexes": []
            }
        ]
    }))
    .expect("PostgreSQL drift fixture must deserialize")
}

async fn snapshot_after_mutation(
    session: &support::PgDevSession,
    schema: &str,
    mutation: &str,
) -> Result<SchemaSnapshot, String> {
    session
        .batch("BEGIN")
        .await
        .map_err(|error| format!("begin drift mutation: {error}"))?;
    if let Err(error) = session.batch(mutation).await {
        let _ = session.batch("ROLLBACK").await;
        return Err(format!("apply drift mutation `{mutation}`: {error}"));
    }
    let snapshot = snapshot_schema(session, schema)
        .await
        .map_err(|error| format!("snapshot after `{mutation}`: {error}"));
    let rollback = session
        .batch("ROLLBACK")
        .await
        .map_err(|error| format!("rollback `{mutation}`: {error}"));
    match (snapshot, rollback) {
        (Ok(snapshot), Ok(())) => Ok(snapshot),
        (Err(snapshot), Ok(())) => Err(snapshot),
        (Ok(_), Err(rollback)) => Err(rollback),
        (Err(snapshot), Err(rollback)) => Err(format!("{snapshot}; {rollback}")),
    }
}

async fn format_constraint_name(
    session: &support::PgDevSession,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<String, String> {
    let rows = session
        .query(
            "SELECT con.conname \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_attribute a \
               ON a.attrelid = c.oid AND a.attnum = con.conkey[1] \
             WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3 \
               AND con.contype = 'c' AND array_length(con.conkey, 1) = 1 \
             ORDER BY con.conname",
            &[schema.into(), table.into(), column.into()],
        )
        .await
        .map_err(|error| format!("query {table}.{column} format constraint: {error}"))?;
    if rows.len() != 1 {
        return Err(format!(
            "expected one format CHECK for {table}.{column}, got {}",
            rows.len()
        ));
    }
    rows[0]
        .try_get("conname")
        .map_err(|error| format!("decode {table}.{column} format constraint: {error}"))
}

fn foreign_key_name(expected: &SchemaSnapshot, table: &str) -> Result<String, String> {
    let foreign_keys = expected
        .tables
        .get(table)
        .ok_or_else(|| format!("missing expected table {table}"))?
        .constraints
        .iter()
        .filter(|constraint| constraint.kind == "FOREIGN KEY")
        .collect::<Vec<_>>();
    if foreign_keys.len() != 1 {
        return Err(format!(
            "expected one foreign key on {table}, got {}",
            foreign_keys.len()
        ));
    }
    Ok(foreign_keys[0].name.clone())
}

fn require_altered(
    drift: &StructuralDrift,
    table: &str,
    object: &str,
    field: &str,
    actual_contains: &str,
) -> Result<(), String> {
    if drift.altered_objects.iter().any(|altered| {
        altered.table == table
            && altered.object == object
            && altered.field == field
            && altered.actual.contains(actual_contains)
    }) {
        Ok(())
    } else {
        Err(format!(
            "missing {table}.{object} {field} drift containing {actual_contains:?}: {drift:#?}"
        ))
    }
}

fn require_missing(drift: &StructuralDrift, object: &str) -> Result<(), String> {
    if drift
        .missing_objects
        .iter()
        .any(|missing| missing == object)
    {
        Ok(())
    } else {
        Err(format!("missing object drift {object:?}: {drift:#?}"))
    }
}

#[compio::test]
async fn live_postgres_introspects_identity_default_format_and_reference_drift() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = token();
    let quoted_schema = quote_ident(&schema);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated drift schema");

    let result: Result<(), String> = async {
        let ir = fixture(&schema);
        let expected = fold_ops(&ir.ops, SqlDialect::Postgres, &schema)
            .map_err(|error| format!("fold PostgreSQL drift fixture: {error}"))?;
        let migrations = IrAuthor::new(&schema, OWNER, SqlDialect::Postgres)
            .lower(&ir, &LiveSchema::default())
            .map_err(|error| format!("lower PostgreSQL drift fixture: {error}"))?;
        // The unqualified nextval fixture intentionally exercises normal
        // same-schema name resolution at DDL creation time.
        session
            .batch(&format!("SET search_path TO {quoted_schema}, public"))
            .await
            .map_err(|error| format!("set fixture search_path: {error}"))?;
        for migration in &migrations {
            session
                .batch(&migration.up)
                .await
                .map_err(|error| format!("apply {}: {error}", migration.name))?;
        }

        // Make every same-schema target visible. A pg_get_constraintdef-based FK
        // snapshot would now omit the target schema; the structured catalog path
        // must remain byte-identical to the folded, qualified reference contract.
        let clean = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect clean PostgreSQL fixture: {error}"))?;
        let clean_drift = diff_snapshots(&expected, &clean);
        if !clean_drift.is_clean() {
            return Err(format!(
                "clean PostgreSQL ID/reference fixture drifted: {clean_drift:#?}"
            ));
        }

        // PostgreSQL qualifies pinned pg_catalog objects when search_path would
        // otherwise resolve the authored spelling to a user object. Those
        // qualifications and OPERATOR(...) wrappers are deparser decoration,
        // not drift in the already-created defaults and format CHECKs.
        let clean_shadow_schema = format!("{schema}_clean_shadow");
        let quoted_clean_shadow_schema = quote_ident(&clean_shadow_schema);
        let clean_shadowed_builtins = format!(
            "CREATE SCHEMA {quoted_clean_shadow_schema}; \
             CREATE FUNCTION {quoted_schema}.nextval(regclass) RETURNS bigint \
             LANGUAGE sql IMMUTABLE AS $$ SELECT 42::bigint $$; \
             CREATE FUNCTION {quoted_clean_shadow_schema}.lower(text) RETURNS text \
             LANGUAGE sql IMMUTABLE AS $$ SELECT $1 $$; \
             CREATE FUNCTION {quoted_clean_shadow_schema}.octet_length(text) RETURNS integer \
             LANGUAGE sql IMMUTABLE AS $$ SELECT 1 $$; \
             CREATE FUNCTION {quoted_clean_shadow_schema}.regex_shadow(text, text) \
             RETURNS boolean LANGUAGE sql IMMUTABLE AS $$ SELECT false $$; \
             CREATE OPERATOR {quoted_clean_shadow_schema}.~ (\
                 FUNCTION = {quoted_clean_shadow_schema}.regex_shadow, \
                 LEFTARG = text, RIGHTARG = text); \
             SET LOCAL search_path TO {quoted_clean_shadow_schema}, {quoted_schema}, pg_catalog"
        );
        let shadow_clean =
            snapshot_after_mutation(&session, &schema, &clean_shadowed_builtins).await?;
        let shadow_clean_drift = diff_snapshots(&expected, &shadow_clean);
        if !shadow_clean_drift.is_clean() {
            return Err(format!(
                "pg_catalog-qualified builtins caused phantom drift: {shadow_clean_drift:#?}"
            ));
        }

        let identity_table = format!("{quoted_schema}.{}", quote_ident("identity_facets"));
        for (mutation, column, actual) in [
            (
                format!(
                    "ALTER TABLE {identity_table} ALTER COLUMN {} DROP IDENTITY",
                    quote_ident("always_id")
                ),
                "always_id",
                "",
            ),
            (
                format!(
                    "ALTER TABLE {identity_table} ALTER COLUMN {} SET GENERATED ALWAYS",
                    quote_ident("default_id")
                ),
                "default_id",
                "always",
            ),
            (
                format!(
                    "ALTER TABLE {identity_table} ALTER COLUMN {} ADD GENERATED BY DEFAULT AS IDENTITY",
                    quote_ident("plain_id")
                ),
                "plain_id",
                "by default",
            ),
        ] {
            let actual_snapshot =
                snapshot_after_mutation(&session, &schema, &mutation).await?;
            require_altered(
                &diff_snapshots(&expected, &actual_snapshot),
                "identity_facets",
                &format!("column {column}"),
                "identity",
                actual,
            )?;
        }

        let uuid_table = format!("{quoted_schema}.{}", quote_ident("uuid_defaults"));
        for (mutation, column, actual) in [
            (
                format!(
                    "ALTER TABLE {uuid_table} ALTER COLUMN {} DROP DEFAULT",
                    quote_ident("v4_id")
                ),
                "v4_id",
                "absent",
            ),
            (
                format!(
                    "ALTER TABLE {uuid_table} ALTER COLUMN {} SET DEFAULT gen_random_uuid()",
                    quote_ident("manual_id")
                ),
                "manual_id",
                "uuidV4",
            ),
            (
                format!(
                    "ALTER TABLE {uuid_table} ALTER COLUMN {} SET DEFAULT uuidv7()",
                    quote_ident("v4_id")
                ),
                "v4_id",
                "uuidV7",
            ),
        ] {
            let actual_snapshot =
                snapshot_after_mutation(&session, &schema, &mutation).await?;
            require_altered(
                &diff_snapshots(&expected, &actual_snapshot),
                "uuid_defaults",
                &format!("column {column}"),
                "default",
                actual,
            )?;
        }

        // pg_get_expr follows search_path when it chooses whether to qualify a
        // function name. A user function can therefore deparse with exactly the
        // same text as the builtin generator. pg_depend provenance must keep the
        // shadowed function as an ordinary expression instead of silently
        // classifying it as UUIDv4.
        let shadow_uuid_v4 = format!(
            "CREATE FUNCTION {quoted_schema}.gen_random_uuid() RETURNS uuid \
             LANGUAGE sql IMMUTABLE \
             AS 'SELECT ''11111111-1111-4111-8111-111111111111''::uuid'; \
             SET LOCAL search_path TO {quoted_schema}, pg_catalog; \
             ALTER TABLE {uuid_table} ALTER COLUMN {} SET DEFAULT gen_random_uuid()",
            quote_ident("v4_id")
        );
        let actual_snapshot =
            snapshot_after_mutation(&session, &schema, &shadow_uuid_v4).await?;
        require_altered(
            &diff_snapshots(&expected, &actual_snapshot),
            "uuid_defaults",
            "column v4_id",
            "default",
            "call:gen_random_uuid()",
        )?;

        let sequence_table =
            format!("{quoted_schema}.{}", quote_ident("sequence_defaults"));
        let swap_nextval = format!(
            "ALTER TABLE {sequence_table} ALTER COLUMN {} SET DEFAULT nextval('{}.{}'::regclass)",
            quote_ident("counter"),
            schema,
            "counter_b"
        );
        let actual_snapshot =
            snapshot_after_mutation(&session, &schema, &swap_nextval).await?;
        require_altered(
            &diff_snapshots(&expected, &actual_snapshot),
            "sequence_defaults",
            "column counter",
            "default",
            "counter_b",
        )?;

        // The sequence dependency alone is insufficient: a user-defined
        // nextval(regclass) can shadow the pinned builtin while retaining the
        // same regclass dependency and deparsed spelling.
        let shadow_nextval = format!(
            "CREATE FUNCTION {quoted_schema}.nextval(regclass) RETURNS bigint \
             LANGUAGE sql IMMUTABLE AS 'SELECT 42::bigint'; \
             SET LOCAL search_path TO {quoted_schema}, pg_catalog; \
             ALTER TABLE {sequence_table} ALTER COLUMN {} \
             SET DEFAULT nextval('{}.{}'::regclass)",
            quote_ident("counter"),
            schema,
            "counter_a"
        );
        let actual_snapshot =
            snapshot_after_mutation(&session, &schema, &shadow_nextval).await?;
        require_altered(
            &diff_snapshots(&expected, &actual_snapshot),
            "sequence_defaults",
            "column counter",
            "default",
            "call:nextval(",
        )?;

        for (table, column) in [
            ("type_keys", "id"),
            ("ulid_keys", "id"),
            ("case_default_keys", "id"),
            ("trim_default_keys", "id"),
            ("single_child", "parent_id"),
        ] {
            let table_sql = format!("{quoted_schema}.{}", quote_ident(table));
            let mutation = format!(
                "ALTER TABLE {table_sql} ALTER COLUMN {} DROP DEFAULT",
                quote_ident(column)
            );
            let actual_snapshot =
                snapshot_after_mutation(&session, &schema, &mutation).await?;
            require_altered(
                &diff_snapshots(&expected, &actual_snapshot),
                table,
                &format!("column {column}"),
                "default",
                "absent",
            )?;
        }

        // A typed reference has no local format CHECK, so FK recovery must also
        // retain the default's pg_depend provenance. The shadowed function
        // deparses exactly like the expected built-in lower() call.
        let typed_reference_table =
            format!("{quoted_schema}.{}", quote_ident("single_child"));
        let reference_shadow_schema = format!("{schema}_reference_shadow");
        let quoted_reference_shadow_schema = quote_ident(&reference_shadow_schema);
        let shadowed_reference_default = format!(
            "CREATE SCHEMA {quoted_reference_shadow_schema}; \
             CREATE FUNCTION {quoted_reference_shadow_schema}.lower(text) \
             RETURNS text LANGUAGE sql IMMUTABLE AS $$ SELECT $1 $$; \
             SET LOCAL search_path TO {quoted_reference_shadow_schema}, pg_catalog; \
             ALTER TABLE {typed_reference_table} ALTER COLUMN parent_id \
             SET DEFAULT lower('ACCOUNT_00000000000000000000000000')"
        );
        let actual_snapshot =
            snapshot_after_mutation(&session, &schema, &shadowed_reference_default).await?;
        require_altered(
            &diff_snapshots(&expected, &actual_snapshot),
            "single_child",
            "column parent_id",
            "default",
            "user-defined:call:lower(",
        )?;

        let type_check = format_constraint_name(&session, &schema, "type_keys", "id").await?;
        let type_table = format!("{quoted_schema}.{}", quote_ident("type_keys"));
        let drop_type_check = format!(
            "ALTER TABLE {type_table} DROP CONSTRAINT {}",
            quote_ident(&type_check)
        );
        let actual_snapshot =
            snapshot_after_mutation(&session, &schema, &drop_type_check).await?;
        require_altered(
            &diff_snapshots(&expected, &actual_snapshot),
            "type_keys",
            "column id",
            "format",
            "",
        )?;

        let prefix_mismatch = format!(
            "ALTER TABLE {type_table} DROP CONSTRAINT {}; \
             ALTER TABLE {type_table} ADD CONSTRAINT {} \
             CHECK (id IS NULL OR (octet_length(id) = 31 AND \
             (id COLLATE \"C\") ~ '^team_[0-7][0123456789abcdefghjkmnpqrstvwxyz]{{25}}$'))",
            quote_ident(&type_check),
            quote_ident(&type_check),
        );
        let actual_snapshot =
            snapshot_after_mutation(&session, &schema, &prefix_mismatch).await?;
        require_altered(
            &diff_snapshots(&expected, &actual_snapshot),
            "type_keys",
            "column id",
            "format",
            "typeId(team)",
        )?;

        // pg_get_constraintdef resolves names through the active search_path.
        // A user function with the same spelling as a pinned builtin therefore
        // deparses to the engine-authored text unless CHECK recovery also proves
        // catalog provenance. The replacement deliberately rejects every
        // non-NULL TypeID while preserving the exact surface spelling.
        let shadow_schema = format!("{schema}_shadow");
        let quoted_shadow_schema = quote_ident(&shadow_schema);
        let shadowed_type_check = format!(
            "CREATE SCHEMA {quoted_shadow_schema}; \
             CREATE FUNCTION {quoted_shadow_schema}.octet_length(text) \
             RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT 1 $$; \
             CREATE FUNCTION {quoted_shadow_schema}.lower(text) \
             RETURNS text LANGUAGE sql IMMUTABLE AS $$ SELECT $1 $$; \
             SET LOCAL search_path TO {quoted_shadow_schema}, pg_catalog; \
             ALTER TABLE {type_table} ALTER COLUMN id \
             SET DEFAULT lower('ACCOUNT_00000000000000000000000000'); \
             ALTER TABLE {type_table} DROP CONSTRAINT {}; \
             ALTER TABLE {type_table} ADD CONSTRAINT {} \
             CHECK (id IS NULL OR (octet_length(id) = 34 AND \
             (id COLLATE \"C\") ~ '^account_[0-7][0123456789abcdefghjkmnpqrstvwxyz]{{25}}$'))",
            quote_ident(&type_check),
            quote_ident(&type_check),
        );
        let actual_snapshot =
            snapshot_after_mutation(&session, &schema, &shadowed_type_check).await?;
        require_altered(
            &diff_snapshots(&expected, &actual_snapshot),
            "type_keys",
            "column id",
            "format",
            "",
        )?;
        require_altered(
            &diff_snapshots(&expected, &actual_snapshot),
            "type_keys",
            "column id",
            "default",
            "user-defined:call:lower(",
        )?;

        let ulid_check = format_constraint_name(&session, &schema, "ulid_keys", "id").await?;
        let ulid_table = format!("{quoted_schema}.{}", quote_ident("ulid_keys"));
        for mutation in [
            format!(
                "ALTER TABLE {ulid_table} DROP CONSTRAINT {}",
                quote_ident(&ulid_check)
            ),
            format!(
                "ALTER TABLE {ulid_table} DROP CONSTRAINT {}; \
                 ALTER TABLE {ulid_table} ADD CONSTRAINT {} \
                 CHECK (id IS NULL OR octet_length(id) = 25)",
                quote_ident(&ulid_check),
                quote_ident(&ulid_check)
            ),
            format!(
                "ALTER TABLE {ulid_table} DROP CONSTRAINT {}; \
                 ALTER TABLE {ulid_table} ADD CONSTRAINT {} \
                 CHECK (id IS NULL OR (octet_length(id) = 26 AND \
                 (id COLLATE \"C\") ~ '^[0-7][0123456789ABCDEFGHJKMNPQRSTVWXYZ]{{25}}$')) \
                 NOT VALID",
                quote_ident(&ulid_check),
                quote_ident(&ulid_check)
            ),
        ] {
            let actual_snapshot =
                snapshot_after_mutation(&session, &schema, &mutation).await?;
            require_altered(
                &diff_snapshots(&expected, &actual_snapshot),
                "ulid_keys",
                "column id",
                "format",
                "",
            )?;
        }

        let single_fk = foreign_key_name(&expected, "single_child")?;
        let single_table = format!("{quoted_schema}.{}", quote_ident("single_child"));
        let single_constraint = quote_ident(&single_fk);
        let single_mutations = [
            (
                format!(
                    "ALTER TABLE {single_table} DROP CONSTRAINT {single_constraint}"
                ),
                "drop",
            ),
            (
                format!(
                    "ALTER TABLE {single_table} DROP CONSTRAINT {single_constraint}; \
                     ALTER TABLE {single_table} ADD CONSTRAINT {single_constraint} \
                     FOREIGN KEY (parent_id) REFERENCES {quoted_schema}.single_parent_alt(id) \
                     ON DELETE CASCADE ON UPDATE RESTRICT"
                ),
                "single_parent_alt",
            ),
            (
                format!(
                    "ALTER TABLE {single_table} DROP CONSTRAINT {single_constraint}; \
                     ALTER TABLE {single_table} ADD CONSTRAINT {single_constraint} \
                     FOREIGN KEY (parent_id) REFERENCES {quoted_schema}.single_parent(id) \
                     ON DELETE RESTRICT ON UPDATE CASCADE"
                ),
                "ON UPDATE CASCADE",
            ),
        ];
        for (mutation, expected_actual) in single_mutations {
            let actual_snapshot =
                snapshot_after_mutation(&session, &schema, &mutation).await?;
            let drift = diff_snapshots(&expected, &actual_snapshot);
            if expected_actual == "drop" {
                require_missing(
                    &drift,
                    &format!("single_child constraint {single_fk}"),
                )?;
            } else {
                require_altered(
                    &drift,
                    "single_child",
                    &format!("constraint {single_fk}"),
                    "definition",
                    expected_actual,
                )?;
            }
        }

        let composite_fk = foreign_key_name(&expected, "composite_child")?;
        let composite_table =
            format!("{quoted_schema}.{}", quote_ident("composite_child"));
        let composite_constraint = quote_ident(&composite_fk);
        let composite_mutations = [
            (
                format!(
                    "ALTER TABLE {composite_table} DROP CONSTRAINT {composite_constraint}"
                ),
                "drop",
            ),
            (
                format!(
                    "ALTER TABLE {composite_table} DROP CONSTRAINT {composite_constraint}; \
                     ALTER TABLE {composite_table} ADD CONSTRAINT {composite_constraint} \
                     FOREIGN KEY (parent_left, parent_right) \
                     REFERENCES {quoted_schema}.composite_parent_alt(left_id, right_id) \
                     ON DELETE SET NULL ON UPDATE CASCADE DEFERRABLE INITIALLY DEFERRED"
                ),
                "composite_parent_alt",
            ),
            (
                format!(
                    "ALTER TABLE {composite_table} DROP CONSTRAINT {composite_constraint}; \
                     ALTER TABLE {composite_table} ADD CONSTRAINT {composite_constraint} \
                     FOREIGN KEY (parent_right, parent_left) \
                     REFERENCES {quoted_schema}.composite_parent(left_id, right_id) \
                     ON DELETE SET NULL ON UPDATE CASCADE DEFERRABLE INITIALLY DEFERRED"
                ),
                "FOREIGN KEY (parent_right, parent_left)",
            ),
            (
                format!(
                    "ALTER TABLE {composite_table} DROP CONSTRAINT {composite_constraint}; \
                     ALTER TABLE {composite_table} ADD CONSTRAINT {composite_constraint} \
                     FOREIGN KEY (parent_left, parent_right) \
                     REFERENCES {quoted_schema}.composite_parent(left_id, right_id) \
                     ON DELETE CASCADE ON UPDATE RESTRICT"
                ),
                "ON UPDATE RESTRICT",
            ),
            (
                format!(
                    "ALTER TABLE {composite_table} DROP CONSTRAINT {composite_constraint}; \
                     ALTER TABLE {composite_table} ADD CONSTRAINT {composite_constraint} \
                     FOREIGN KEY (parent_left, parent_right) \
                     REFERENCES {quoted_schema}.composite_parent(left_id, right_id) \
                     ON DELETE SET NULL (parent_left) ON UPDATE CASCADE \
                     DEFERRABLE INITIALLY DEFERRED"
                ),
                "SET NULL (parent_left)",
            ),
        ];
        for (mutation, expected_actual) in composite_mutations {
            let actual_snapshot =
                snapshot_after_mutation(&session, &schema, &mutation).await?;
            let drift = diff_snapshots(&expected, &actual_snapshot);
            if expected_actual == "drop" {
                require_missing(
                    &drift,
                    &format!("composite_child constraint {composite_fk}"),
                )?;
            } else {
                require_altered(
                    &drift,
                    "composite_child",
                    &format!("constraint {composite_fk}"),
                    "definition",
                    expected_actual,
                )?;
            }
        }

        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE",
            quote_ident(&format!("{schema}_clean_shadow")),
            quote_ident(&format!("{schema}_reference_shadow")),
            quote_ident(&format!("{schema}_shadow")),
        ))
        .await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => panic!("live PostgreSQL drift regression failed: {work}"),
        (Ok(()), Err(cleanup)) => panic!("drop PostgreSQL drift schema: {cleanup}"),
        (Err(work), Err(cleanup)) => {
            panic!("live PostgreSQL drift regression failed: {work}; cleanup failed: {cleanup}")
        }
    }
}

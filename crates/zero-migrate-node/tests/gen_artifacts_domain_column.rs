//! **The runtime descriptor must report a domain column's BASE type, not `"string"`.**
//!
//! `ColType::Domain` shared a `col_type_to_token` match arm with `ColType::Enum`:
//!
//! ```text
//!   ColType::Enum { .. } | ColType::Domain { .. } => ("string".into(), None)
//! ```
//!
//! so a column typed by a domain over `int` reported `{"type":"string"}` in
//! `runtimeJson`. `RuntimeSchemaDescriptor` is what a deployed app installs `env.db`
//! from, so the app validated an integer column as text. The database disagrees on
//! EVERY dialect, which is why all three are exercised here:
//!
//! ```text
//!   postgres  CREATE DOMAIN "public"."positive_number" AS integer CHECK ((VALUE > 0))
//!             CREATE TABLE  ... ("amount" "public"."positive_number" NOT NULL, ...)
//!   sqlite    CREATE TABLE  ... ("amount" INTEGER NOT NULL CHECK (("amount" > 0)), ...)
//!   mysql     CREATE TABLE  ... (`amount` INT NOT NULL CHECK ((`amount` > 0)), ...)
//! ```
//!
//! WHY THE FIX IS NOT A MATCH ARM. Same reason as the enum half:
//! `ColType::Domain { name, schema }` carries the NAME only, and the base type lives in
//! a separate `Op::CreateDomain`. The resolution has to happen where the op stream is in
//! scope, so `fold_to_field_defs` does it through the same `NamedTypeRegistry` the DDL
//! lower and the snapshot fold already use.
//!
//! WHAT IS ASSERTED. Content, never `ok`: a column silently described as text also
//! returns `ok=true`, which is exactly how this shipped. The controls carry the weight:
//! a plain `t.int()` proves the token is preserved when it is NOT behind a domain, a
//! plain `t.text()` proves `"string"` is still reachable, and a SECOND domain over a
//! DIFFERENT base proves the value is resolved rather than hardcoded to one answer.
//!
//! Runs on the napi-free build (`--no-default-features`).

mod support;

use serde_json::{json, Value};

use zero_migrate::model::ir::TableRuntimeOptions;
use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor};
use zero_migrate_node::api::{gen_artifacts_from_descriptors, gen_artifacts_from_envelopes};

const SCHEMA: &str = "public";
const DIALECTS: [&str; 3] = ["sqlite", "postgres", "mysql"];

fn charter() -> String {
    support::no_inject_charter_toml(SCHEMA)
}

fn envelope(name: &str, ops: Value) -> Value {
    json!({ "ir_version": 1, "name": name, "ops": ops })
}

/// `CHECK (VALUE > 0)` over the domain value - the constraint a domain actually
/// carries, and the one the descriptor still has no slot for.
fn value_is_positive() -> Value {
    json!({
        "node": "binOp",
        "op": "gt",
        "lhs": { "node": "colRef", "name": "VALUE" },
        "rhs": { "node": "literal", "value": 0 },
    })
}

/// The reproduction: two domains over DIFFERENT base types, plus the two plain
/// controls, in one table in one call.
fn amounts_history() -> Vec<Value> {
    vec![envelope(
        "create_amounts",
        json!([
            {
                "op": "createDomain",
                "name": "positive_number",
                "as": "int",
                "check": value_is_positive(),
            },
            { "op": "createDomain", "name": "short_code", "as": { "string": { "length": 40 } } },
            {
                "op": "createTable",
                "name": "amounts",
                "columns": [
                    {
                        "name": "amount",
                        "type": { "domain": { "name": "positive_number" } },
                        "nullable": false,
                    },
                    {
                        "name": "code",
                        "type": { "domain": { "name": "short_code" } },
                        "nullable": false,
                    },
                    { "name": "weight", "type": "int", "nullable": false },
                    { "name": "note", "type": "text", "nullable": false },
                ],
                "primaryKey": null,
            },
        ]),
    )]
}

/// The same table with both `createDomain`s in an EARLIER migration file.
fn amounts_history_split_across_migrations() -> Vec<Value> {
    let mut later = amounts_history().remove(0);
    let ops = later["ops"].as_array_mut().expect("ops is an array");
    let create_table = ops.pop().expect("the createTable is last");
    let declarations = std::mem::take(ops);
    vec![
        envelope("create_domains", Value::Array(declarations)),
        envelope("create_amounts_only", json!([create_table])),
    ]
}

/// The table with NO `createDomain` anywhere: the base type is genuinely unprovable.
fn amounts_history_without_the_domain_declarations() -> Vec<Value> {
    let mut orphan = amounts_history();
    let ops = orphan[0]["ops"].as_array_mut().expect("ops is an array");
    ops.remove(0);
    ops.remove(0);
    orphan
}

fn runtime_fields(runtime_json: &str, collection: &str) -> Value {
    let parsed: Value = serde_json::from_str(runtime_json).expect("runtime json parses");
    parsed["collections"][collection]["fields"].clone()
}

/// `runtimeJson`'s fields for `amounts`, on a reply that must be `ok`.
fn fields_for(envelopes: &[Value], dialect: &str) -> (Value, String) {
    let reply =
        gen_artifacts_from_envelopes(envelopes, dialect, Some(SCHEMA), &[charter().as_str()]);
    assert!(reply.ok, "{dialect}: the history folds: {:?}", reply.error);
    let runtime_json = reply.runtime_json.expect("ok reply carries runtimeJson");
    let fields = runtime_fields(&runtime_json, "amounts");
    (fields, runtime_json)
}

/// THE DEFECT, on every dialect that can express a domain column - which is all
/// three: PostgreSQL has a native `CREATE DOMAIN`, and SQLite/MySQL inline the base
/// type plus the constraint into the column's storage.
#[test]
fn a_domain_column_reports_its_base_type_on_every_dialect() {
    for dialect in DIALECTS {
        let reply = gen_artifacts_from_envelopes(
            &amounts_history(),
            dialect,
            Some(SCHEMA),
            &[charter().as_str()],
        );
        assert!(reply.ok, "{dialect}: the history folds: {:?}", reply.error);
        let runtime_json = reply.runtime_json.expect("ok reply carries runtimeJson");
        let env_db_ts = reply.env_db_ts.expect("ok reply carries envDbTs");
        let fields = runtime_fields(&runtime_json, "amounts");

        // The half that was already right: the authoring artifact keeps the NAME, so
        // it never lost anything and must not start to.
        assert!(
            env_db_ts.contains(r#"t.domain("positive_number")"#)
                && env_db_ts.contains(r#"t.domain("short_code")"#),
            "{dialect}: envDbTs keeps both domain builders:\n{env_db_ts}"
        );

        // THE SUBJECT. The database stores an integer; the descriptor said "string".
        assert_eq!(
            fields["amount"]["type"], "int",
            "{dialect}: a domain over `int` reports the integer token:\n{runtime_json}"
        );

        // THE SECOND DOMAIN, over a DIFFERENT base. One case cannot tell a fix from a
        // coincidence: if the arm had simply been re-hardcoded, this one would be
        // wrong. And `varchar(40)` proves the token is not the whole type - the
        // parameter has to move with it, and before this it did not.
        assert_eq!(
            fields["code"]["type"], "string",
            "{dialect}: a domain over `varchar(40)` is still string-shaped:\n{runtime_json}"
        );
        assert_eq!(
            fields["code"]["maxLength"], 40,
            "{dialect}: and it carries the base type's LENGTH, which the token has no \
             room for:\n{runtime_json}"
        );

        // CONTROL A: the token is preserved when the column is not behind a domain.
        assert_eq!(
            fields["weight"]["type"], "int",
            "{dialect}: a plain t.int() is unchanged:\n{runtime_json}"
        );
        // CONTROL B: "string" is still reachable, so the fix did not simply move every
        // column off it.
        assert_eq!(
            fields["note"]["type"], "string",
            "{dialect}: a plain t.text() is unchanged:\n{runtime_json}"
        );
        // The loss was legible precisely because these two serialized identically.
        assert_ne!(
            fields["amount"], fields["note"],
            "{dialect}: an integer domain and free text must not serialize \
             identically:\n{runtime_json}"
        );

        // AND WHAT IS STILL DROPPED, pinned as current behaviour rather than left to be
        // rediscovered: the domain's own `CHECK (VALUE > 0)` reaches no descriptor
        // slot. `min`/`max` are INCLUSIVE bounds, so `min: 0` would tell the runtime to
        // accept a row the database rejects; an arbitrary predicate has no image at
        // all. Recorded in `docs/review-log.md` as a separate defect.
        assert_eq!(
            fields["amount"].get("min"),
            None,
            "{dialect}: an exclusive domain CHECK is not fabricated into an inclusive \
             min:\n{runtime_json}"
        );
        assert_eq!(
            fields["amount"].get("max"),
            None,
            "{dialect}: nor into a max:\n{runtime_json}"
        );
    }
}

/// The base type reaches the descriptor when the `createDomain` is in an EARLIER
/// envelope, because the envelope source folds one concatenated op stream.
#[test]
fn a_domain_declared_in_an_earlier_migration_still_reaches_the_descriptor() {
    for dialect in DIALECTS {
        let (fields, runtime_json) =
            fields_for(&amounts_history_split_across_migrations(), dialect);
        assert_eq!(
            fields["amount"]["type"], "int",
            "{dialect}: the earlier file's base type reaches the later file's \
             column:\n{runtime_json}"
        );
        assert_eq!(
            fields["code"]["maxLength"], 40,
            "{dialect}: including its length parameter:\n{runtime_json}"
        );
    }
}

/// UNCHANGED beats invented.
///
/// The token is not optional - something is always emitted - so the enum half's
/// "leave the slot ABSENT" has no analogue. With no `createDomain` in the stream the
/// base type is unprovable, and the two dialect families answer differently for a
/// measured reason: SQLite and MySQL INLINE the base type into the column's storage,
/// so the fold already fails closed; PostgreSQL needs only the type's NAME, so the
/// fold succeeds and the descriptor must keep the answer it already had rather than
/// guess a new one.
#[test]
fn an_unprovable_base_type_keeps_the_token_it_already_had() {
    let (fields, runtime_json) = fields_for(
        &amounts_history_without_the_domain_declarations(),
        "postgres",
    );
    assert_eq!(
        fields["amount"]["type"], "string",
        "postgres renders a native domain column from the NAME alone, and the \
         descriptor falls back to the pre-fix token rather than inventing one:\n{runtime_json}"
    );
    assert_eq!(
        fields["amount"].get("maxLength"),
        None,
        "and it invents no parameter either:\n{runtime_json}"
    );

    for inlining in ["sqlite", "mysql"] {
        let reply = gen_artifacts_from_envelopes(
            &amounts_history_without_the_domain_declarations(),
            inlining,
            Some(SCHEMA),
            &[charter().as_str()],
        );
        assert!(
            !reply.ok,
            "{inlining} inlines the base type into the column, so it cannot render \
             the column without the declaration"
        );
        assert!(
            reply
                .error
                .as_deref()
                .is_some_and(|e| e.contains("domain `positive_number` is not registered")),
            "{inlining}: the refusal names the missing definition: {:?}",
            reply.error
        );
    }
}

/// The registry is REPLAYED, not read once: a domain dropped and re-created over a
/// different base type retypes the column that names it. A lookup that ignored
/// `dropDomain` would still answer `text` here.
#[test]
fn a_domain_recreated_over_a_new_base_type_moves_the_column_with_it() {
    let history = vec![envelope(
        "recreate_the_domain",
        json!([
            { "op": "createDomain", "name": "d", "as": "text" },
            { "op": "dropDomain", "name": "d" },
            { "op": "createDomain", "name": "d", "as": "int" },
            {
                "op": "createTable",
                "name": "amounts",
                "columns": [
                    { "name": "amount", "type": { "domain": { "name": "d" } }, "nullable": false },
                ],
                "primaryKey": null,
            },
        ]),
    )];
    for dialect in DIALECTS {
        let (fields, runtime_json) = fields_for(&history, dialect);
        assert_eq!(
            fields["amount"]["type"], "int",
            "{dialect}: the LATEST definition wins, not the first:\n{runtime_json}"
        );
    }
}

/// The other two sites a column acquires a type. Both are PostgreSQL-only for a
/// named type: `fold_ops` refuses `addColumn`/`setColumnType` into a domain on the
/// inlining dialects with `unreachable use-site`, which is pinned here so a later
/// change cannot turn that clean refusal into a silent wrong answer.
#[test]
fn an_added_or_retyped_domain_column_earns_the_same_base_type() {
    let history = vec![envelope(
        "add_and_retype",
        json!([
            { "op": "createDomain", "name": "positive_number", "as": "int" },
            {
                "op": "createTable",
                "name": "amounts",
                "columns": [
                    { "name": "note", "type": "text", "nullable": false },
                    { "name": "later", "type": "text", "nullable": true },
                ],
                "primaryKey": null,
            },
            {
                "op": "addColumn",
                "table": "amounts",
                "column": "added",
                "type": { "domain": { "name": "positive_number" } },
                "nullable": true,
            },
            {
                "op": "setColumnType",
                "table": "amounts",
                "column": "later",
                "toType": { "domain": { "name": "positive_number" } },
            },
        ]),
    )];

    let (fields, runtime_json) = fields_for(&history, "postgres");
    assert_eq!(
        fields["added"]["type"], "int",
        "an addColumn into a domain earns the base type:\n{runtime_json}"
    );
    assert_eq!(
        fields["later"]["type"], "int",
        "and so does a setColumnType into the same domain - a retype to `T` and a \
         create of `T` must describe the same column:\n{runtime_json}"
    );
    assert_eq!(
        fields["note"]["type"], "string",
        "the untouched control keeps its own token:\n{runtime_json}"
    );

    for inlining in ["sqlite", "mysql"] {
        let reply =
            gen_artifacts_from_envelopes(&history, inlining, Some(SCHEMA), &[charter().as_str()]);
        assert!(
            !reply.ok,
            "{inlining}: a named type at an addColumn/setColumnType use site is refused"
        );
        assert!(
            reply
                .error
                .as_deref()
                .is_some_and(|e| e.contains("unreachable use-site")),
            "{inlining}: and the refusal still names the reason: {:?}",
            reply.error
        );
    }
}

/// `Op::CreateDomain`'s `as` is a full `ColType`, so a domain can name another domain.
/// PostgreSQL's fold does NOT refuse that (it resolves the use site by name alone), so
/// both a CHAIN and a CYCLE reach the replay through the real API. The chain resolves;
/// the cycle terminates and changes nothing.
#[test]
fn a_domain_chain_resolves_and_a_domain_cycle_terminates() {
    let chain = vec![envelope(
        "domain_chain",
        json!([
            { "op": "createDomain", "name": "base_number", "as": "int" },
            {
                "op": "createDomain",
                "name": "positive_number",
                "as": { "domain": { "name": "base_number" } },
            },
            {
                "op": "createTable",
                "name": "amounts",
                "columns": [
                    {
                        "name": "amount",
                        "type": { "domain": { "name": "positive_number" } },
                        "nullable": false,
                    },
                ],
                "primaryKey": null,
            },
        ]),
    )];
    let (fields, runtime_json) = fields_for(&chain, "postgres");
    assert_eq!(
        fields["amount"]["type"], "int",
        "a domain over a domain over `int` resolves to the integer token:\n{runtime_json}"
    );

    let cycle = vec![envelope(
        "domain_cycle",
        json!([
            { "op": "createDomain", "name": "a", "as": { "domain": { "name": "b" } } },
            { "op": "createDomain", "name": "b", "as": { "domain": { "name": "a" } } },
            {
                "op": "createTable",
                "name": "amounts",
                "columns": [
                    { "name": "amount", "type": { "domain": { "name": "a" } }, "nullable": false },
                ],
                "primaryKey": null,
            },
        ]),
    )];
    // Reaching this assertion at all is the termination proof: a walk without the
    // `seen` set does not return here.
    let (fields, runtime_json) = fields_for(&cycle, "postgres");
    assert_eq!(
        fields["amount"]["type"], "string",
        "a cycle has no base type, so the token is left exactly as it was:\n{runtime_json}"
    );

    // The inlining dialects refuse a nested base type outright, before any of this.
    for inlining in ["sqlite", "mysql"] {
        for history in [&chain, &cycle] {
            let reply = gen_artifacts_from_envelopes(
                history,
                inlining,
                Some(SCHEMA),
                &[charter().as_str()],
            );
            assert!(
                reply
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("nested named base type")),
                "{inlining}: a nested base type is refused, not silently answered: {:?}",
                reply.error
            );
        }
    }
}

/// `ColType::Encrypted` recurses into its inner type in `col_type_to_token`, so an
/// encrypted DOMAIN column routes through the same arm. It is deliberately left
/// alone, and this pins that.
///
/// MEASURED, and the reason: the descriptor's `encrypted.wraps` is also stamped into
/// the catalog by the LOWER, which has no registry, as
/// `zero-migrate:enc:randomised:default:string`. With the recursion enabled as a
/// scaffold, an unrelated column rename on SQLite rebuilt the table as
/// `"amount" BLOB /* zero-migrate:enc:randomised:default:number */` - silently
/// rewriting a live table's recorded encryption posture. Closing this half needs the
/// lower's sentinel moved with it; recorded in `docs/review-log.md` as its own defect.
#[test]
fn an_encrypted_domain_column_is_left_exactly_as_it_was() {
    let history = vec![envelope(
        "encrypted_domain",
        json!([
            { "op": "createDomain", "name": "positive_number", "as": "int" },
            {
                "op": "createTable",
                "name": "amounts",
                "columns": [
                    {
                        "name": "amount",
                        "type": { "encrypted": { "of": { "domain": { "name": "positive_number" } } } },
                        "nullable": false,
                    },
                ],
                "primaryKey": null,
            },
        ]),
    )];
    for dialect in DIALECTS {
        let (fields, runtime_json) = fields_for(&history, dialect);
        assert_eq!(
            fields["amount"]["type"], "string",
            "{dialect}: an encrypted domain column keeps the pre-fix token:\n{runtime_json}"
        );
        assert_eq!(
            fields["amount"]["encrypted"]["wraps"], "string",
            "{dialect}: and the `wraps` that must agree with the catalog \
             sentinel:\n{runtime_json}"
        );
    }
}

/// The DESCRIPTOR source never had this defect and structurally cannot: a
/// `CollectionDescriptor` has no way to NAME a domain. `token_to_col_type` is the only
/// way a descriptor becomes a `ColType`, and no token in its set maps to
/// `ColType::Domain`, so the producer refuses the spelling outright instead of
/// producing a domain column with the wrong base type.
#[test]
fn the_descriptor_source_cannot_name_a_domain_at_all() {
    let descriptors = vec![CollectionDescriptor {
        name: "amounts".to_string(),
        owner_app: "app_test".to_string(),
        fields: vec![FieldDescriptor {
            name: "amount".to_string(),
            ty: "domain".to_string(),
            required: true,
            ..Default::default()
        }],
        indexes: Vec::new(),
        runtime_options: TableRuntimeOptions::default(),
    }];
    for dialect in DIALECTS {
        let reply = gen_artifacts_from_descriptors(
            &descriptors,
            dialect,
            Some(SCHEMA),
            &[charter().as_str()],
        );
        assert!(
            !reply.ok,
            "{dialect}: a descriptor cannot name a domain, so `domain` is not a token"
        );
        assert!(
            reply.error.as_deref().is_some_and(|e| e.contains("domain")),
            "{dialect}: and the refusal names the unknown token: {:?}",
            reply.error
        );
    }
}

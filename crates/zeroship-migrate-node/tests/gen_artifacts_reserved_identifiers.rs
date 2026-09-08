//! **The generation verb refuses a creator-declared reserved identifier.**
//!
//! `genArtifacts` is the one production entry family that writes a creator's
//! `generated/zeroship/` (`sdks/vite-plugin/src/gen-types/index.ts` calls it for BOTH
//! the recorded-migration source and the manual `schema.ts` source, and the addon's
//! two `api` functions are its only shipped Rust callers). `loadVerify` - the OTHER
//! DB-free verb in the same addon, the same binary - has run the engine's declaration
//! gate since #61. This one did not, so a creator could declare `ssn_masked`, get a
//! green `pnpm build`, a typed `env.db.ts` field, a committed `schema.runtime.json`
//! and a recorded `descriptor_sha256`, and learn about the reservation only when the
//! migration service refused the deploy.
//!
//! # This drives the REAL path, and the control says so
//!
//! Everything here is committed production input, not a fixture:
//!
//! - the envelopes are `examples/db-todos/generated/zeroship/migrations.ir.json`,
//!   exactly the `documents[].body` array the vite plugin hands to `genArtifacts`;
//! - the charter is `policy_version = 1` + `policies/confined-system-shape.inject.toml`,
//!   which is byte-for-byte what `CONFINED_SCHEMA_EMIT_CEILING_TOML` composes in
//!   `sdks/vite-plugin/src/gen-types/confined-ceiling.ts` (TypeScript reads the same
//!   fragment through `policies/codegen.mjs`);
//! - the dialect (`postgres`) and project schema (`public`) are the plugin's literals.
//!
//! [`the_real_committed_schema_generates_byte_identical_artifacts`] pins that: the
//! UNMUTATED input must reproduce the committed `schema.runtime.json` byte for byte.
//! Without it every refusal below could be an artifact of a harness that never
//! reached the renderer.
//!
//! # What this suite does NOT prove
//!
//! The fence is FAIL-FAST ERGONOMICS, not containment. `genArtifacts` runs on the
//! creator's machine; a hand-edited `schema.runtime.json` never passes through it.
//! The unbypassable gate is still the migration service's guarded load
//! (`crates/zeroship-migrate-server/src/apply.rs` -> `load_and_lower_guarded`), and it
//! must not be weakened on the grounds that the generator checks now.

use serde_json::Value;
use zeroship_migrate_node::api::gen_artifacts_from_envelopes;
use zeroship_migrate_node::wire::GenArtifactsReply;

/// The platform-wide injection fragment, taken from the SAME file the TypeScript
/// ceiling is generated from. Rust `include_str!`s it; `policies/codegen.mjs` mirrors
/// it into `sdks/vite-plugin/src/gen-types/confined-system-shape.generated.ts`, and
/// `tests/inject_policy_mirror_gate.sh` byte-compares the two.
const CONFINED_SYSTEM_SHAPE_INJECT_TOML: &str =
    include_str!("../../../policies/confined-system-shape.inject.toml");

/// The real committed IR envelope document for `examples/db-todos`.
const REAL_MIGRATIONS_IR: &str =
    include_str!("../../../examples/db-todos/generated/zeroship/migrations.ir.json");

/// The real committed artifact the unmutated input must reproduce.
const REAL_RUNTIME_JSON: &str =
    include_str!("../../../examples/db-todos/generated/zeroship/schema.runtime.json");

/// `CONFINED_SCHEMA_EMIT_CEILING_TOML`, composed the way `confined-ceiling.ts` does.
fn schema_emit_ceiling() -> String {
    format!("policy_version = 1\n\n{CONFINED_SYSTEM_SHAPE_INJECT_TOML}")
}

/// The `documents[].body` array, which is literally `envelopes` in
/// `genTypesFromMigrations`.
fn real_envelopes() -> Vec<Value> {
    let document: Value =
        serde_json::from_str(REAL_MIGRATIONS_IR).expect("the committed migrations.ir.json parses");
    document["documents"]
        .as_array()
        .expect("migrations.ir.json carries a `documents` array")
        .iter()
        .map(|entry| entry["body"].clone())
        .collect()
}

/// Drive the production verb with the production inputs.
fn generate(envelopes: &[Value]) -> GenArtifactsReply {
    let ceiling = schema_emit_ceiling();
    gen_artifacts_from_envelopes(envelopes, "postgres", Some("public"), &[ceiling.as_str()])
}

/// Rename one REAL declared column in place. Panics if the column is not there, so a
/// corpus change cannot silently turn a mutation into a no-op that "passes".
fn rename_declared_column(envelopes: &mut [Value], table: &str, from: &str, to: &str) {
    let mut renamed = 0_usize;
    for envelope in envelopes.iter_mut() {
        let ops = envelope["ops"]
            .as_array_mut()
            .expect("envelope carries ops");
        for op in ops {
            if op["op"] != "createTable" || op["name"] != table {
                continue;
            }
            for column in op["columns"]
                .as_array_mut()
                .expect("createTable has columns")
            {
                if column["name"] == from {
                    column["name"] = Value::String(to.to_string());
                    renamed += 1;
                }
            }
        }
    }
    assert_eq!(
        renamed, 1,
        "expected exactly one `{table}.{from}` declaration in the committed corpus"
    );
}

/// Rename one REAL declared table in place, with the same no-op guard.
fn rename_declared_table(envelopes: &mut [Value], from: &str, to: &str) {
    let mut renamed = 0_usize;
    for envelope in envelopes.iter_mut() {
        let ops = envelope["ops"]
            .as_array_mut()
            .expect("envelope carries ops");
        for op in ops {
            if op["op"] == "createTable" && op["name"] == from {
                op["name"] = Value::String(to.to_string());
                renamed += 1;
            }
        }
    }
    assert_eq!(
        renamed, 1,
        "expected exactly one `createTable {from}` in the committed corpus"
    );
}

/// **The control.** The unmutated committed input must still render, and must render
/// the committed bytes. This is what makes every refusal below evidence about the
/// production path rather than about a harness that never got there.
#[test]
fn the_real_committed_schema_generates_byte_identical_artifacts() {
    let reply = generate(&real_envelopes());
    assert!(
        reply.ok,
        "the committed db-todos schema must still generate: {:?}",
        reply.error
    );
    assert_eq!(
        reply.runtime_json.as_deref(),
        Some(REAL_RUNTIME_JSON),
        "the generated schema.runtime.json must be byte-identical to the committed one"
    );
}

/// Every reserved COLUMN shape, declared where a creator would declare it.
///
/// `(replacement, the token the refusal must name)`. The second element is not
/// decoration: a message that only says "invalid" costs the creator a search through
/// our source, so each case asserts the reserved token itself is in the text.
const RESERVED_COLUMN_NAMES: &[(&str, &str)] = &[
    // `ReservedName::Prefix("_")` - the synthetic result columns (`_distance`,
    // `_score`) the runtime emits on vector / spatial search.
    ("_secret", "_secret"),
    // The raw-column prefix the masking storage flip writes siblings under. It is
    // deliberately spelled so the `_` prefix above already claims it.
    ("__zs_raw__title", "__zs_raw__title"),
    // `ReservedName::Suffix("_masked")` - the `.mask()` / `.encrypted()` sibling.
    ("ssn_masked", "_masked"),
    // The six classification names, reserved at the column level so a creator schema
    // cannot collide with the taxonomy audit and authorization key on.
    ("pii", "pii"),
    ("internal", "internal"),
    // A REGISTERED backend's catalog namespace, declared by the backend rather than
    // by a table in the engine (`Limits::reserved_identifier_prefixes`).
    ("pg_class", "pg_"),
    ("sqlite_master", "sqlite_"),
];

/// The columns `policies/confined-system-shape.inject.toml` adds to every creator
/// table. None of them is refused by the declaration gate, and none of them may be
/// author-declared either - which is why the manual-source arm below strips them
/// before handing a descriptor set back to the producer.
const CHARTER_INJECTED_COLUMNS: &[&str] = &[
    "id",
    "created_at",
    "updated_at",
    "created_by",
    "updated_by",
    "version",
    "deleted_at",
];

/// Every reserved TABLE shape.
const RESERVED_TABLE_NAMES: &[(&str, &str)] = &[
    ("__zeroship_reserved", "__zeroship"),
    ("pg_todos", "pg_"),
    ("sqlite_todos", "sqlite_"),
];

#[test]
fn a_reserved_column_name_is_refused_by_the_generator() {
    for (declared, token) in RESERVED_COLUMN_NAMES {
        let mut envelopes = real_envelopes();
        rename_declared_column(&mut envelopes, "todos", "title", declared);
        let reply = generate(&envelopes);
        assert!(
            !reply.ok,
            "declaring column `{declared}` generated artifacts instead of being refused; \
             the creator gets a green build and a typed field for a name the migration \
             service will reject"
        );
        let error = reply.error.unwrap_or_default();
        assert!(
            error.contains(token),
            "the refusal for `{declared}` must name the reserved token `{token}`, so the \
             creator does not have to search our source. It said:\n  {error}"
        );
        assert!(
            error.contains(declared),
            "the refusal for `{declared}` must name the offending column. It said:\n  {error}"
        );
        assert!(
            error.contains("create_todos"),
            "the refusal for `{declared}` must name the migration it came from, or the \
             creator cannot find the file. It said:\n  {error}"
        );
    }
}

#[test]
fn a_reserved_table_name_is_refused_by_the_generator() {
    for (declared, token) in RESERVED_TABLE_NAMES {
        let mut envelopes = real_envelopes();
        rename_declared_table(&mut envelopes, "todos", declared);
        let reply = generate(&envelopes);
        assert!(
            !reply.ok,
            "declaring table `{declared}` generated artifacts instead of being refused"
        );
        let error = reply.error.unwrap_or_default();
        assert!(
            error.contains(token),
            "the refusal for `{declared}` must name the reserved token `{token}`. It \
             said:\n  {error}"
        );
        assert!(
            error.contains(declared),
            "the refusal for `{declared}` must name the offending table. It said:\n  {error}"
        );
    }
}

/// **The whole refusal string, pinned.**
///
/// The other tests assert that the message CONTAINS the pieces a creator needs. This
/// one shows what they actually read, because a refusal is a user-facing surface and
/// "it contains the token" is satisfiable by text nobody can act on. It names the
/// migration, the reserved suffix, a concrete alternative, and the op index.
#[test]
fn the_refusal_reads_as_something_a_creator_can_act_on() {
    let mut envelopes = real_envelopes();
    rename_declared_column(&mut envelopes, "todos", "title", "ssn_masked");
    let error = generate(&envelopes).error.unwrap_or_default();
    // The engine's own hint spells its dash as U+2014; written as an escape so this
    // file stays ASCII while the comparison stays exact.
    let expected = concat!(
        "migration \"create_todos\": rename the declared table column so it uses only the ",
        "portable identifier shape and no platform- or backend-reserved name ",
        "[OP_INVALID kind=op op_index=1 dialect=postgres]: invalid identifier: reserved ",
        "field name 'ssn_masked': suffix '_masked' is reserved for sibling columns ",
        "generated by .mask()/.encrypted() \u{2014} try 'ssn_view' or 'ssn_display' instead",
    );
    assert_eq!(error, expected);
}

/// **The MANUAL source, wired.**
///
/// `gen-types` has two sources and both write `generated/zeroship/`: recorded
/// migrations (every test above) and a declared `schema.ts` descriptor set, which
/// enters through `gen_artifacts_from_descriptors`. Fencing one and not the other
/// would leave a creator on the manual source with exactly the defect this change
/// removes.
///
/// The descriptors here are not invented: they are the collections the GENERATED path
/// recovers from the real committed db-todos envelopes, crossed through the same
/// boundary DTO the addon hands JavaScript and converted back. The control proves that
/// round trip still renders before any name is mutated.
#[test]
fn the_manual_descriptor_source_refuses_the_same_declarations() {
    let ceiling = schema_emit_ceiling();
    let recovered = generate(&real_envelopes())
        .collections
        .expect("genArtifacts reports its collections");
    // The recovered collections are POST-injection: the charter has already added the
    // seven system columns and its indexes. A `schema.ts` never declares those, and
    // feeding them back in collides ("declares column \"id\", which collides with an
    // injected system column") - so reconstruct the AUTHOR-DECLARED shape, which is
    // what the manual source actually carries.
    let descriptors: Vec<_> = recovered
        .into_iter()
        .map(|dto| {
            zeroship_migrate_node::descriptors::descriptor_dto_to_engine(dto)
                .expect("a descriptor the addon itself emitted converts back")
        })
        .map(|mut descriptor| {
            descriptor
                .fields
                .retain(|field| !CHARTER_INJECTED_COLUMNS.contains(&field.name.as_str()));
            descriptor.indexes.clear();
            descriptor
        })
        .collect();
    assert!(
        descriptors.len() >= 2,
        "db-todos declares users and todos: {}",
        descriptors.len()
    );

    let run = |set: &[zeroship_migrate::render::declarative::CollectionDescriptor]| {
        zeroship_migrate_node::api::gen_artifacts_from_descriptors(
            set,
            "postgres",
            Some("public"),
            &[ceiling.as_str()],
        )
    };

    let control = run(&descriptors);
    assert!(
        control.ok,
        "the recovered descriptor set must still render: {:?}",
        control.error
    );

    for (declared, token) in RESERVED_COLUMN_NAMES {
        let mut mutated = descriptors.clone();
        let target = mutated
            .iter_mut()
            .find(|d| d.name == "todos")
            .expect("the todos collection is recovered");
        let field = target
            .fields
            .iter_mut()
            .find(|f| f.name == "title")
            .expect("todos declares title");
        field.name = (*declared).to_string();

        let reply = run(&mutated);
        assert!(
            !reply.ok,
            "the manual source accepted the reserved column `{declared}`"
        );
        let error = reply.error.unwrap_or_default();
        assert!(
            error.contains(token) && error.contains(declared),
            "the manual source's refusal for `{declared}` must name it and the reserved \
             token `{token}`. It said:\n  {error}"
        );
    }

    for (declared, token) in RESERVED_TABLE_NAMES {
        let mut mutated = descriptors.clone();
        let target = mutated
            .iter_mut()
            .find(|d| d.name == "todos")
            .expect("the todos collection is recovered");
        target.name = (*declared).to_string();

        let reply = run(&mutated);
        assert!(
            !reply.ok,
            "the manual source accepted the reserved table `{declared}`"
        );
        let error = reply.error.unwrap_or_default();
        assert!(
            error.contains(token) && error.contains(declared),
            "the manual source's refusal for `{declared}` must name it and the reserved \
             token `{token}`. It said:\n  {error}"
        );
    }
}

/// The two verbs in this addon must agree. `loadVerify` has refused these shapes since
/// #61; a generator that accepted them is the whole defect, and a generator that
/// refused something `loadVerify` accepts would be a NEW one - a build that fails on a
/// schema the platform would deploy.
///
/// The comparison runs over the same real corpus, mutated the same way, so the two
/// sides differ in the VERB and nothing else.
#[test]
fn the_generator_and_load_verify_agree_on_every_declared_name() {
    let registry = std::collections::HashMap::from([("".to_string(), "public".to_string())]);
    let mut checked = 0_usize;

    let mut cases: Vec<(String, Vec<Value>)> = vec![("control".to_string(), real_envelopes())];
    for (declared, _) in RESERVED_COLUMN_NAMES {
        let mut envelopes = real_envelopes();
        rename_declared_column(&mut envelopes, "todos", "title", declared);
        cases.push((format!("column:{declared}"), envelopes));
    }
    for (declared, _) in RESERVED_TABLE_NAMES {
        let mut envelopes = real_envelopes();
        rename_declared_table(&mut envelopes, "todos", declared);
        cases.push((format!("table:{declared}"), envelopes));
    }

    for (label, envelopes) in cases {
        let generated = generate(&envelopes).ok;
        // `loadVerify` takes ONE envelope document; the corpus has exactly one, and the
        // assertion in `real_envelopes` callers keeps it that way.
        assert_eq!(envelopes.len(), 1, "{label}: one committed envelope");
        let verified = zeroship_migrate_node::api::load_verify(
            &serde_json::to_string(&envelopes[0]).expect("envelope re-serializes"),
            "",
            "postgres",
            &registry,
            "public",
        )
        .ok;
        assert_eq!(
            generated, verified,
            "{label}: genArtifacts and loadVerify disagree. The creator would learn about \
             this name from one verb and not the other."
        );
        checked += 1;
    }

    assert_eq!(
        checked,
        1 + RESERVED_COLUMN_NAMES.len() + RESERVED_TABLE_NAMES.len(),
        "every case must be ruled on, or the agreement is vacuous"
    );
}

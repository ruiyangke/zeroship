//! Prove the monorepo can AUTHOR a `.ts` migration into an
//! `ir_version:1` IR envelope using **zeroship-runtime's V8**, then apply it
//! end-to-end via the native `compio-postgres` seam.
//!
//! This is the whole native authoring+apply loop with NO Node in it:
//!
//! ```text
//!   sample .ts migration  (createTable notes(title, body) + addColumn tag)
//!        │
//!        ▼  zeroship-runtime V8 isolate
//!   @zeroship/migrate recorder (dist/embedded-recorder.js, the v1 DSL)
//!        │      run schema() under __begin/__drain, emit { ir_version:1, name, ops }
//!        ▼
//!   ir_version-1 envelope JSON  (authored in V8 — NOT hand-built)
//!        │
//!        ▼  zeroship-migrate engine (Rust)
//!   fail-closed load gate → IrAuthor::load_and_lower (Postgres)
//!        │
//!        ▼
//!   PostgresBackend::new_generic(&CompioPgSession)
//!        │
//!        ▼  compio io_uring, live PG :5440
//!   engine.apply → real DDL + journal
//! ```
//!
//! The envelope is AUTHORED by running the package recorder in
//! zeroship-runtime's V8 rather than hand-built; everything downstream of it is
//! the same native apply path.
//!
//! `PostgreSQL` apply owns its server through Testcontainers. V8 authoring is
//! also tested independently of the database.

mod fixture;

use zeroship_migrate::driver::SqlSession;
use zeroship_migrate::{
    effective_policy_from_charter_toml, resolve_create_table_policy, Approval, EffectivePolicy,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, MigrationIr,
};
// PG-shaped surfaces live in the vendor crate: `PostgresBackend`, the dialect id
// and the journal reader are off the facade, and the dialect is a `DialectId` const.
use zeroship_migrate_postgres::backend::journal_sql::applied as read_journal;
use zeroship_migrate_postgres::{PostgresBackend, DIALECT as POSTGRES};
use zeroship_migrate_server::session::CompioPgSession;
use zeroship_runtime::{ModuleEntry, Runtime};

/// The vendor backends handed to every engine entry point. `zeroship-migrate` is the
/// composition root; a host takes the set rather than naming vendors itself.
const VENDORS: zeroship_migrate_backend::registry::VendorSet = zeroship_migrate::shipping_vendors();

/// The confined table-shape ceiling composed into an `EffectivePolicy`.
///
/// Only the GRANTS are written here; the `[[inject]]` rule is the platform-wide
/// fragment in `policies/`, concatenated in at compile time so this fixture cannot
/// describe a table shape the deployed server does not produce.
const CONFINED_CEILING_TOML: &str = concat!(
    r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["__PROJECT_SCHEMA__"] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["__PROJECT_SCHEMA__"] }

[[grant]]
key = "schema.rename"
value = true
scope = { include = ["__PROJECT_SCHEMA__"] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

"#,
    include_str!("../../../policies/confined-system-shape.inject.toml"),
);

// ── The V8 authoring front-end ───────────────────────────────────────────────
//    Mechanism: build a module graph that maps `@zeroship/migrate` to a recorder
//    bundle and `__migration__.js` to the creator migration, run `schema()` under a
//    fresh ambient recorder, and read the drained envelope back off a global.
//    Here that graph wires the package's v1 recorder and emits ir_version:1.

/// The Stage-2 authoring glue (imports the migration + the recorder seam, runs
/// `schema()`, emits the v1 envelope on `globalThis.__zsStage2IR`).
const STAGE2_RECORDER_JS: &str = include_str!("stage2_recorder.js");

/// The `@zeroship/migrate` recorder bundle — the CURRENT v1 DSL + recorder
/// (`table()`/`t.*` → `__begin`/`__drain`). This is the engine package's
/// authoring artifact. Mapping `@zeroship/migrate` to THIS file keeps the DSL
/// producers and the ambient recorder on the same module instance.
const MIGRATE_RECORDER_JS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../packages/zero-migrate/dist/embedded-recorder.js"
));

/// The deserialized adapter result mirroring the JSON the glue emits.
#[derive(serde::Deserialize)]
struct AuthoredEnvelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    ir: Option<serde_json::Value>,
}

/// Author a `.ts` migration module source into its `ir_version:1` IR envelope
/// JSON by running the package recorder in zeroship-runtime's V8 isolate.
///
/// `name` is the filename-derived fallback used when the module declares none.
/// Returns the envelope JSON string (`{ ir_version:1, name, ops }`).
fn author_v1_envelope(migration_source: &str, name: &str) -> String {
    zeroship_runtime::init_v8();

    // The in-memory module graph. The GLUE is the entry (compiled eagerly); it
    // imports the migration under `./__migration__.js` and the recorder seam from
    // `@zeroship/migrate` (mapped to the package's v1 bundle).
    let modules = vec![
        ModuleEntry {
            specifier: "stage2_recorder.js".to_string(),
            source: STAGE2_RECORDER_JS.to_string(),
        },
        ModuleEntry {
            // The registry is keyed on NORMALISED specifiers: `resolve_specifier`
            // strips the leading `./` off the glue's import before looking it
            // up, so a key carrying one matches nothing.
            specifier: "__migration__.js".to_string(),
            source: migration_source.to_string(),
        },
        ModuleEntry {
            specifier: "@zeroship/migrate".to_string(),
            source: MIGRATE_RECORDER_JS.to_string(),
        },
    ];

    let runtime = Runtime::builder().build();

    let authored: Result<String, String> = runtime.with_scope(|scope| {
        // The minimal WinterCG globals the recorder needs. The migration recorder
        // only touches `globalThis.crypto` (defensively) + Text encoding streams —
        // the same profile the in-tree `FrontendGlobals::Migration` installs.
        zeroship_runtime::init::setup_globals(scope)?;
        zeroship_runtime::init::install_text_encoding_streams(scope);

        // Expose ONLY the filename-derived name to the glue (owner_app is
        // deliberately NOT a JS-reachable global — Rust stamps provenance).
        {
            let global = scope.get_current_context().global(scope);
            let k =
                v8::String::new(scope, "__zsMigrationName").ok_or("alloc __zsMigrationName key")?;
            let v = v8::String::new(scope, name).ok_or("alloc __zsMigrationName value")?;
            global.set(scope, k.into(), v.into());
        }

        zeroship_runtime::modules::load_modules(scope, &modules)?;
        scope.perform_microtask_checkpoint();

        let global = scope.get_current_context().global(scope);
        let k = v8::String::new(scope, "__zsStage2IR").ok_or("alloc __zsStage2IR key")?;
        let v = global
            .get(scope, k.into())
            .filter(|v| v.is_string())
            .ok_or("glue left no __zsStage2IR string")?;
        Ok(v.to_rust_string_lossy(scope))
    });

    let ir_json = authored.expect("V8 authoring must complete");
    let envelope: AuthoredEnvelope =
        serde_json::from_str(&ir_json).expect("authored envelope JSON parses");
    assert!(
        envelope.ok,
        "recorder reported an authoring error: {:?}",
        envelope.error
    );
    let ir = envelope.ir.expect("successful authoring carries an ir");
    serde_json::to_string(&ir).expect("re-serialize authored ir")
}

/// The SAMPLE `.ts` migration authored through the V8 recorder: createTable
/// notes(title, body) then addColumn notes.tag. This is the ONLY hand-written
/// artifact — the envelope itself is derived by running this through the recorder.
const SAMPLE_MIGRATION_TS: &str = r#"
import { table, t } from "@zeroship/migrate";

export const name = "create_notes_and_add_tag";

export function schema() {
  table("notes").create({
    columns: {
      title: t.text().required(),
      body: t.text(),
    },
  });
  table("notes").column("tag").add({ type: t.text() });
}
"#;

// ── The live-PG apply path (identical to Stage 1) ────────────────────────────

const PROJECT: &str = "prj_stage2";
const APP: &str = "app_stage2";

fn token() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}")
}

fn confined_effective(project_schema: &str) -> EffectivePolicy {
    const PLACEHOLDER: &str = "\"__PROJECT_SCHEMA__\"";
    assert_eq!(CONFINED_CEILING_TOML.matches(PLACEHOLDER).count(), 3);
    let project_schema = serde_json::to_string(project_schema).expect("schema serializes");
    let charter = CONFINED_CEILING_TOML.replace(PLACEHOLDER, &project_schema);
    effective_policy_from_charter_toml(&charter).expect("confined charter composes")
}

fn cfg_for(tok: &str) -> (ExecutorConfig, EffectivePolicy) {
    let project_schema = format!("proj_{tok}");
    let effective = confined_effective(&project_schema);
    let mut c = ExecutorConfig::new(
        format!("{PROJECT}_{tok}"),
        project_schema,
        effective.clone(),
    );
    c.confinement.meta_schema = format!("meta_{tok}");
    (c, effective)
}

async fn ensure_project_schema(session: &CompioPgSession, cfg: &ExecutorConfig) {
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
}

async fn drop_schemas(session: &CompioPgSession, cfg: &ExecutorConfig) {
    let _ = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.confinement.meta_schema
        ))
        .await;
}

/// Resolve the authored envelope's `createTable` ops through the confined
/// table-shape policy (the platform's create-table policy) before lowering —
/// the same normalisation Stage 1 uses.
fn resolved_envelope_json(raw: &str, effective: &EffectivePolicy, default_schema: &str) -> String {
    let ir: MigrationIr = serde_json::from_str(raw).expect("authored IR parses as MigrationIr");
    let resolved =
        resolve_create_table_policy(&ir, effective, default_schema).expect("authored IR resolves");
    serde_json::to_string(&resolved).expect("resolved authored IR serializes")
}

async fn table_exists(session: &CompioPgSession, schema: &str, table: &str) -> bool {
    let row = session
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2) AS present",
            &[schema.into(), table.into()],
        )
        .await
        .expect("table_exists probe");
    row.try_get::<_, bool>("present").expect("decode present")
}

async fn column_exists(session: &CompioPgSession, schema: &str, table: &str, column: &str) -> bool {
    let row = session
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3) AS present",
            &[schema.into(), table.into(), column.into()],
        )
        .await
        .expect("column_exists probe");
    row.try_get::<_, bool>("present").expect("decode present")
}

/// V8 authoring is DB-free — assert it unconditionally so the proof holds even in
/// DB-less CI: running the sample `.ts` through the package recorder in
/// zeroship-runtime's V8 yields exactly the createTable+addColumn v1 envelope.
#[test]
fn sample_ts_authors_ir_version_1_envelope_in_v8() {
    let raw = author_v1_envelope(SAMPLE_MIGRATION_TS, "create_notes_and_add_tag");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("authored envelope parses");

    assert_eq!(v["ir_version"], serde_json::json!(1), "must author v1");
    assert_eq!(v["name"], serde_json::json!("create_notes_and_add_tag"));

    let ops = v["ops"].as_array().expect("ops is an array");
    assert_eq!(ops.len(), 2, "createTable + addColumn");
    assert_eq!(ops[0]["op"], serde_json::json!("createTable"));
    assert_eq!(ops[0]["name"], serde_json::json!("notes"));
    let cols = ops[0]["columns"].as_array().expect("createTable columns");
    let col_names: Vec<&str> = cols.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(col_names, vec!["title", "body"]);
    assert_eq!(ops[1]["op"], serde_json::json!("addColumn"));
    assert_eq!(ops[1]["table"], serde_json::json!("notes"));
    assert_eq!(ops[1]["column"], serde_json::json!("tag"));
}

/// The full native loop: author in V8 → v1 envelope → published-engine lower+apply
/// over the compio seam to an owned `PostgreSQL` server.
#[compio::test]
async fn authored_v1_envelope_lowers_and_applies_over_native_compio_seam() {
    let postgres = fixture::Postgres::start();
    let url = postgres.url();

    // (1) AUTHOR the envelope in zeroship-runtime's V8 (the whole point of Stage 2).
    let authored = author_v1_envelope(SAMPLE_MIGRATION_TS, "create_notes_and_add_tag");
    let tok = token();
    let (cfg, effective) = cfg_for(&tok);
    // (2) confined-policy normalisation before lowering.
    let ir = resolved_envelope_json(&authored, &effective, &cfg.project_schema);

    // (3) live compio client wrapped in this crate's SqlSession adapter.
    let session = CompioPgSession::connect(url)
        .await
        .expect("connect compio session to test PG");
    drop_schemas(&session, &cfg).await;
    ensure_project_schema(&session, &cfg).await;

    // (4) the REAL fail-closed load gate + lower, Postgres dialect.
    let author = IrAuthor::new(VENDORS, &cfg.project_schema, APP, &POSTGRES, &effective);
    let migrations = author
        .load_and_lower(&ir, APP, &Default::default(), &LiveSchema::default())
        .expect("the V8-authored v1 envelope must lower on Postgres");
    assert!(!migrations.is_empty(), "lowering must yield migration(s)");

    // (5) PostgresBackend over the compio adapter + MigrationEngine.
    let engine = MigrationEngine::new(VENDORS);
    let guard_cfg = GuardConfig::from_policy(effective.clone(), POSTGRES, &cfg.project_schema);
    let plan = engine.plan(&migrations, &guard_cfg);
    assert!(
        plan.denied.is_empty(),
        "no denials on a clean authored IR set: {:?}",
        plan.denied
    );

    let backend = PostgresBackend::new_generic(&session);

    // (6) apply the lowered, V8-authored IR envelope through the NATIVE compio seam.
    let outcome = engine
        .apply(&plan, Approval::None, &backend, &cfg, "phase-f-stage2")
        .await
        .expect("apply the lowered authored IR over the native compio PG seam");
    assert!(!outcome.applied.is_empty(), "the authored IR must apply");

    // (7) INDEPENDENT assertions: table + both authored columns + journal exist.
    assert!(
        table_exists(&session, &cfg.project_schema, "notes").await,
        "the authored 'notes' table must exist on real PG"
    );
    assert!(
        column_exists(&session, &cfg.project_schema, "notes", "title").await,
        "the authored createTable 'title' column must exist"
    );
    assert!(
        column_exists(&session, &cfg.project_schema, "notes", "tag").await,
        "the authored addColumn 'tag' column must exist"
    );

    let applied = read_journal(&session, &cfg)
        .await
        .expect("journal read over the seam");
    assert_eq!(
        applied.len(),
        migrations.len(),
        "one journal row per lowered migration"
    );

    // Idempotent re-run: no-op, no second journal row.
    let plan2 = engine.plan(&migrations, &guard_cfg);
    let out2 = engine
        .apply(&plan2, Approval::None, &backend, &cfg, "phase-f-stage2")
        .await
        .expect("idempotent re-apply");
    assert!(out2.is_noop(), "second apply is a no-op");
    let applied2 = read_journal(&session, &cfg).await.expect("journal re-read");
    assert_eq!(
        applied2.len(),
        migrations.len(),
        "no duplicate journal rows on idempotent re-apply"
    );

    drop_schemas(&session, &cfg).await;
}

//! Live PostgreSQL oracle for the two rules a `setColumnType` must obey when the
//! column it names carries a GENERATION contract, and the end-to-end proof that
//! the engine now obeys them.
//!
//! Both rules were measured as plans that cleared `validate` AND `preview` and
//! then died partway through `apply` — the worst failure this engine can produce,
//! because the operator is left with a half-applied schema. Reproduced here
//! through the real path (author → lower → apply → introspect), on the engine's
//! OWN emitted SQL rather than hand-written DDL, because for the second rule the
//! defect IS what the renderer emits:
//!
//! ```text
//!   int identity  -> text     APPLY DIED: identity column type must be
//!                             smallint, integer, or bigint
//!   int generated -> bigint   APPLY DIED: cannot specify USING when altering
//!                             type of generated column
//! ```
//!
//! THE TWO ANSWERS ARE NOT THE SAME, and the difference is the whole point of
//! `postgresql_retypes_a_generated_column_only_without_the_using_cast` below. The
//! identity refusal is the SERVER refusing the CHANGE: no spelling of the `ALTER`
//! is accepted, with or without a cast, so the op has to be refused at authoring
//! time. The generated failure is the server refusing OUR CLAUSE: the same `ALTER`
//! WITHOUT `USING` is accepted, and `pg_attribute.attgenerated` survives it. So
//! that one is a rendering bug, not an illegal migration, and refusing it would
//! have denied a migration the database accepts and honours.
//!
//! WHAT EACH LEG PROVES, since they are not interchangeable. The
//! `within_one_envelope` legs create the table and retype it in ONE artifact, so
//! the column's contract is known only from the op stream. The `across_envelopes`
//! legs apply the create, INTROSPECT the live database, and lower the retype
//! against that snapshot — so the contract arrives from `pg_attribute` by way of
//! `ColumnSnapshot::identity` / `generated_kind`, which is the ordinary case and
//! the one no envelope-local replay can see.

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, fold_ops, resolve_create_table_policy, snapshot_schema, Approval,
    EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine,
    MigrationIr, PostgresBackend, SqlDialect, StructuralDrift,
};

/// The test-side PostgreSQL identifier spelling, written out here rather than
/// imported from the crate. It used to be
/// `zero_migrate::schema::query::quote_ident`, which was `pub`, un-dialected, and a
/// SECOND physical home for the spelling `render::backends::ansi_double_quote_ident`
/// owns; it is gone. A probe that builds its expectation by calling the emitter it is
/// checking is not an oracle anyway, so the replacement is deliberately independent —
/// the same shape the sibling `fold_rename_column_constraint_definition_pg` and
/// `fold_rename_column_check_body_pg` probes already use.
fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

const OWNER: &str = "app_set_column_type_generation";
const TABLE: &str = "retyped";

const IDENTITY_COL: &str =
    r#"{"name":"v","type":"int","nullable":false,"identity":{"always":false}}"#;
const GENERATED_COL: &str =
    r#"{"name":"v","type":"int","generated":{"expr":{"node":"colRef","name":"c0"},"stored":true}}"#;
const ORDINARY_COL: &str = r#"{"name":"v","type":"int"}"#;

fn token(tag: &str) -> String {
    format!(
        "zm_gencontract_{tag}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    )
}

/// The operator charter these legs need: cross-schema, table creation in the
/// per-test schema, and the destructive-op allowance a type change lowers under.
fn charter(schema: &str) -> EffectivePolicy {
    let toml = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#
    );
    zero_migrate::effective_policy_from_charter_toml(&toml).expect("charter parses")
}

fn create_envelope(column: &str) -> String {
    format!(
        r#"{{"ir_version":1,"name":"m1","owner_app":"{OWNER}","ops":[
            {{"op":"createTable","name":"{TABLE}","columns":[
                {{"name":"c0","type":"int","nullable":false}},
                {column}
            ],"primaryKey":["c0"]}}
        ]}}"#
    )
}

fn retype_envelope(name: &str, to_type: &str) -> String {
    format!(
        r#"{{"ir_version":1,"name":"{name}","owner_app":"{OWNER}","ops":[
            {{"op":"setColumnType","table":"{TABLE}","column":"v","toType":{to_type}}}
        ]}}"#
    )
}

/// The whole thing in ONE artifact: the column's generation contract exists only
/// in the op stream when the retype lowers.
fn one_envelope(name: &str, column: &str, to_type: &str) -> String {
    format!(
        r#"{{"ir_version":1,"name":"{name}","owner_app":"{OWNER}","ops":[
            {{"op":"createTable","name":"{TABLE}","columns":[
                {{"name":"c0","type":"int","nullable":false}},
                {column}
            ],"primaryKey":["c0"]}},
            {{"op":"setColumnType","table":"{TABLE}","column":"v","toType":{to_type}}}
        ]}}"#
    )
}

/// The cross-artifact ownership-registry entry a first artifact's `createTable`
/// installs, which every leg that deploys a SECOND artifact against the same table
/// has to hand to the lower.
fn owned() -> BTreeMap<String, String> {
    BTreeMap::from([(TABLE.to_string(), OWNER.to_string())])
}

/// What one deployed artifact left behind: the statements the engine emitted, and
/// the catalog facts the rules key on.
#[derive(Debug)]
struct Applied {
    statements: Vec<String>,
    /// `pg_attribute.attidentity` for column `v` (`""` when not an identity).
    attidentity: String,
    /// `pg_attribute.attgenerated` for column `v` (`""` when not generated).
    attgenerated: String,
    /// The live type of column `v`.
    data_type: String,
}

/// A live session bound to one throwaway schema, so a leg can deploy several
/// artifacts in order against the SAME database — which is what makes the
/// across-envelope legs mean anything.
struct Deployment<'a> {
    session: &'a PgDevSession,
    cfg: ExecutorConfig,
    policy: EffectivePolicy,
}

impl<'a> Deployment<'a> {
    async fn open(session: &'a PgDevSession, tag: &str) -> Self {
        let schema = token(tag);
        let policy = charter(&schema);
        let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
        session
            .batch(&format!(
                "CREATE SCHEMA {}",
                quote_ident(&cfg.project_schema)
            ))
            .await
            .expect("create the isolated retype schema");
        let policy = charter(&cfg.project_schema);
        let backend = PostgresBackend::new_generic(session);
        backend
            .ensure_journal(&cfg)
            .await
            .expect("ensure migration journal");
        Self {
            session,
            cfg,
            policy,
        }
    }

    /// Author → lower → apply ONE artifact against the CURRENT live catalog.
    ///
    /// The live schema is re-introspected per call rather than carried, so an
    /// artifact lowered here sees exactly what a later migration would see: the
    /// database as the previous artifact left it.
    ///
    /// `registry` is the cross-artifact ownership registry the deploy loop folds
    /// each artifact's created tables into. A SECOND artifact touching a table the
    /// FIRST one created must be handed it, or the ownership gate refuses the op
    /// before any of this file's rules are reached — which is a real gate doing its
    /// job, not a workaround.
    async fn deploy(
        &self,
        source: &str,
        registry: &BTreeMap<String, String>,
    ) -> Result<Applied, String> {
        let backend = PostgresBackend::new_generic(self.session);
        let authored: MigrationIr =
            serde_json::from_str(source).map_err(|error| format!("parse test IR: {error}"))?;
        let resolved =
            resolve_create_table_policy(&authored, &self.policy, &self.cfg.project_schema)
                .map_err(|error| format!("resolve create-table policy: {error}"))?;
        let resolved_source = serde_json::to_string(&resolved)
            .map_err(|error| format!("serialize resolved test IR: {error}"))?;
        let catalog = snapshot_schema(self.session, &self.cfg.project_schema)
            .await
            .map_err(|error| format!("introspect the live PostgreSQL schema: {error}"))?;
        let live = LiveSchema::from_catalog_snapshot(catalog, OWNER);
        let author = IrAuthor::new(
            &self.cfg.project_schema,
            OWNER,
            SqlDialect::Postgres,
            &self.policy,
        );
        let guard = GuardConfig::from_policy(self.policy.clone(), SqlDialect::Postgres);
        let artifact = author
            .load_and_lower_guarded(&resolved_source, OWNER, registry, &live, &guard)
            .map_err(|error| format!("AUTHORING REFUSED: {error}"))?;
        let statements = artifact
            .plan
            .steps
            .iter()
            .filter_map(|step| match step {
                zero_migrate::PlanStep::Ddl(migration) => Some(migration.up.clone()),
                _ => None,
            })
            .collect();
        MigrationEngine::new()
            .apply_plan(
                &artifact.plan.steps,
                Approval::Approved,
                &backend,
                &self.cfg,
                "set-column-type-generation-pg",
                LockMode::Acquire,
            )
            .await
            .map_err(|error| format!("APPLY DIED: {error}"))?;
        let (attidentity, attgenerated, data_type) = self.column_facts().await?;
        Ok(Applied {
            statements,
            attidentity,
            attgenerated,
            data_type,
        })
    }

    /// `pg_attribute`'s own record of the two facets, so the survival claims are
    /// the catalog's rather than the fold's.
    async fn column_facts(&self) -> Result<(String, String, String), String> {
        let sql = format!(
            "SELECT a.attidentity::text, a.attgenerated::text, \
             format_type(a.atttypid, a.atttypmod) \
             FROM pg_attribute a \
             WHERE a.attrelid = '{}.{}'::regclass AND a.attname = 'v'",
            quote_ident(&self.cfg.project_schema),
            quote_ident(TABLE),
        );
        let rows = self
            .session
            .query(&sql, &[])
            .await
            .map_err(|error| format!("read pg_attribute: {error}"))?;
        let row = rows.first().ok_or("column v is not in pg_attribute")?;
        let read = |idx: usize| -> Result<String, String> {
            row.try_get::<_, String>(idx)
                .map_err(|error| format!("decode pg_attribute column {idx}: {error}"))
        };
        Ok((read(0)?, read(1)?, read(2)?))
    }

    /// The fold of every op applied so far, compared against the live catalog.
    async fn drift(&self, ops: &[zero_migrate::model::ir::Op]) -> Result<StructuralDrift, String> {
        let expected = fold_ops(
            ops,
            SqlDialect::Postgres,
            &self.cfg.project_schema,
            &self.policy,
        )
        .map_err(|error| format!("fold the applied PostgreSQL ops: {error}"))?;
        let actual = snapshot_schema(self.session, &self.cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;
        Ok(diff_snapshots(&expected, &actual))
    }

    async fn close(&self) -> Result<(), String> {
        self.session
            .batch(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE",
                quote_ident(&self.cfg.project_schema),
                quote_ident(&self.cfg.confinement.meta_schema),
            ))
            .await
            .map_err(|error| format!("drop PostgreSQL test schemas: {error}"))
    }
}

/// Run `body` against a fresh throwaway schema and drop it either way, so a failing
/// assertion never leaves residue behind.
async fn with_deployment<F>(tag: &str, body: F)
where
    F: AsyncFnOnce(&Deployment<'_>) -> Result<(), String>,
{
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return;
    };
    let session = PgDevSession::connect(&url);
    let deployment = Deployment::open(&session, tag).await;
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [
            deployment.cfg.project_schema.clone(),
            deployment.cfg.confinement.meta_schema.clone(),
        ],
    );
    let outcome = body(&deployment).await;
    let cleanup = deployment.close().await;
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(()), Err(cleanup)) => panic!("{cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}

fn assert_clean(label: &str, drift: &StructuralDrift) -> Result<(), String> {
    if drift.altered_objects.is_empty()
        && drift.missing_objects.is_empty()
        && drift.unexpected_objects.is_empty()
    {
        return Ok(());
    }
    Err(format!(
        "{label}: applying a migration and changing NOTHING must leave the fold and the \
         catalog agreeing. altered={:?} missing={:?} unexpected={:?}",
        drift.altered_objects, drift.missing_objects, drift.unexpected_objects,
    ))
}

// ---------------------------------------------------------------------------
// GAP 1 — the identity refusal, and the neighbours it must not take with it.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a_retype_of_an_identity_column_to_a_non_integer_type_never_reaches_the_server() {
    with_deployment("id_refuse", async |deployment| {
        // WITHIN ONE ENVELOPE: the contract is in the op stream only.
        let refusal = deployment
            .deploy(
                &one_envelope("m", IDENTITY_COL, r#""text""#),
                &BTreeMap::new(),
            )
            .await
            .expect_err(
                "an identity column cannot become text: PostgreSQL answers `identity column \
                 type must be smallint, integer, or bigint` and dies mid-deploy",
            );
        if !refusal.starts_with("AUTHORING REFUSED") {
            return Err(format!(
                "the refusal must land BEFORE anything applies — a plan that reaches the \
                 server here leaves the createTable applied and the schema half-migrated. \
                 Got: {refusal}"
            ));
        }
        if !refusal.contains("IDENTITY") || !refusal.contains("\"v\"") {
            return Err(format!(
                "the refusal must name the column and why the server will not honour it. \
                 Got: {refusal}"
            ));
        }

        // ACROSS ENVELOPES: the create really applies, and the contract then comes
        // back from `pg_attribute` through introspection.
        let created = deployment
            .deploy(&create_envelope(IDENTITY_COL), &BTreeMap::new())
            .await?;
        if created.attidentity != "d" {
            return Err(format!(
                "the fixture must really deploy an identity column, or the leg below \
                 proves nothing. attidentity={:?}",
                created.attidentity
            ));
        }
        let refusal = deployment
            .deploy(&retype_envelope("m2", r#""text""#), &owned())
            .await
            .expect_err("the live identity contract is just as binding as the declared one");
        if !refusal.starts_with("AUTHORING REFUSED") {
            return Err(format!(
                "a column whose identity contract is known only from the catalog must be \
                 refused the same way. Got: {refusal}"
            ));
        }
        Ok(())
    })
    .await;
}

#[compio::test]
async fn a_retype_of_an_identity_column_within_the_integer_family_still_applies() {
    // THE OVER-REFUSAL CONTROL. PostgreSQL honours these, so the refusal above must
    // not reach them — and the identity property has to survive, or "it applied" is
    // not the same as "it worked".
    with_deployment("id_widen", async |deployment| {
        let ops = |to_type: &str| -> Vec<zero_migrate::model::ir::Op> {
            let create: MigrationIr =
                serde_json::from_str(&create_envelope(IDENTITY_COL)).expect("create parses");
            let retype: MigrationIr =
                serde_json::from_str(&retype_envelope("m2", to_type)).expect("retype parses");
            create.ops.into_iter().chain(retype.ops).collect()
        };
        deployment
            .deploy(&create_envelope(IDENTITY_COL), &BTreeMap::new())
            .await?;
        let applied = deployment
            .deploy(&retype_envelope("m2", r#""bigInt""#), &owned())
            .await?;
        if applied.data_type != "bigint" || applied.attidentity != "d" {
            return Err(format!(
                "int -> bigint on an identity column applies and KEEPS the identity: \
                 data_type={:?} attidentity={:?}",
                applied.data_type, applied.attidentity
            ));
        }
        if !applied.statements.iter().any(|up| up.contains(" USING ")) {
            return Err(format!(
                "an identity column is not a generated column, so the cast the engine has \
                 always emitted stays. statements={:?}",
                applied.statements
            ));
        }
        assert_clean(
            "identity int -> bigint",
            &deployment.drift(&ops(r#""bigInt""#)).await?,
        )
    })
    .await;
}

#[compio::test]
async fn a_retype_of_an_ordinary_column_beside_an_identity_column_still_applies() {
    // The other half of the over-refusal control: the rule keys on THE COLUMN BEING
    // RETYPED, not on the table having an identity column somewhere.
    with_deployment("id_neighbour", async |deployment| {
        let source = format!(
            r#"{{"ir_version":1,"name":"m","owner_app":"{OWNER}","ops":[
                {{"op":"createTable","name":"{TABLE}","columns":[
                    {{"name":"c0","type":"int","nullable":false}},
                    {{"name":"id","type":"int","nullable":false,"identity":{{"always":false}}}},
                    {ORDINARY_COL}
                ],"primaryKey":["c0"]}},
                {{"op":"setColumnType","table":"{TABLE}","column":"v","toType":"text"}}
            ]}}"#
        );
        let applied = deployment.deploy(&source, &BTreeMap::new()).await?;
        if applied.data_type != "text" {
            return Err(format!(
                "the ordinary column retypes freely: data_type={:?}",
                applied.data_type
            ));
        }
        let ir: MigrationIr = serde_json::from_str(&source).expect("envelope parses");
        assert_clean(
            "ordinary column beside an identity",
            &deployment.drift(&ir.ops).await?,
        )
    })
    .await;
}

// ---------------------------------------------------------------------------
// GAP 2 — the generated column retypes, once the engine stops attaching a cast.
// ---------------------------------------------------------------------------

#[compio::test]
async fn a_retype_of_a_generated_column_applies_within_one_envelope() {
    with_deployment("gen_one", async |deployment| {
        // Each leg needs its OWN artifact name: two bodies under one name is
        // checksum drift in the journal, which the executor refuses on sight —
        // correctly, and it would have masked whatever this leg was measuring.
        for (name, label, to_type, expected_type) in [
            ("m_big", "int -> bigint", r#""bigInt""#, "bigint"),
            ("m_text", "int -> text", r#""text""#, "text"),
        ] {
            let source = one_envelope(name, GENERATED_COL, to_type);
            let applied = deployment
                .deploy(&source, &BTreeMap::new())
                .await
                .map_err(|error| {
                    format!(
                        "{label}: PostgreSQL ACCEPTS this retype — it refuses only the USING \
                     clause the engine used to attach. {error}"
                    )
                })?;
            if applied.data_type != expected_type || applied.attgenerated != "s" {
                return Err(format!(
                    "{label}: the column must really change type AND stay generated: \
                     data_type={:?} attgenerated={:?}",
                    applied.data_type, applied.attgenerated
                ));
            }
            if applied.statements.iter().any(|up| up.contains(" USING ")) {
                return Err(format!(
                    "{label}: the emitted statement must carry no cast. statements={:?}",
                    applied.statements
                ));
            }
            let ir: MigrationIr = serde_json::from_str(&source).expect("envelope parses");
            assert_clean(label, &deployment.drift(&ir.ops).await?)?;
            deployment
                .session
                .batch(&format!(
                    "DROP TABLE {}.{}",
                    quote_ident(&deployment.cfg.project_schema),
                    quote_ident(TABLE)
                ))
                .await
                .map_err(|error| format!("{label}: drop between legs: {error}"))?;
        }
        Ok(())
    })
    .await;
}

#[compio::test]
async fn a_retype_of_a_generated_column_applies_across_envelopes() {
    // The leg that only the LIVE route can pass: the retype is authored alone, and
    // the only thing that says column `v` is generated is `pg_attribute.attgenerated`
    // as introspection recovered it into `ColumnSnapshot::generated_kind`.
    with_deployment("gen_across", async |deployment| {
        let created = deployment
            .deploy(&create_envelope(GENERATED_COL), &BTreeMap::new())
            .await?;
        if created.attgenerated != "s" {
            return Err(format!(
                "the fixture must really deploy a generated column: attgenerated={:?}",
                created.attgenerated
            ));
        }
        let applied = deployment
            .deploy(&retype_envelope("m2", r#""bigInt""#), &owned())
            .await?;
        if applied.data_type != "bigint" || applied.attgenerated != "s" {
            return Err(format!(
                "data_type={:?} attgenerated={:?}",
                applied.data_type, applied.attgenerated
            ));
        }
        if applied.statements.iter().any(|up| up.contains(" USING ")) {
            return Err(format!(
                "the live generation contract must reach the renderer too. statements={:?}",
                applied.statements
            ));
        }
        let create: MigrationIr =
            serde_json::from_str(&create_envelope(GENERATED_COL)).expect("create parses");
        let retype: MigrationIr =
            serde_json::from_str(&retype_envelope("m2", r#""bigInt""#)).expect("retype parses");
        let ops: Vec<_> = create.ops.into_iter().chain(retype.ops).collect();
        assert_clean(
            "generated int -> bigint across envelopes",
            &deployment.drift(&ops).await?,
        )
    })
    .await;
}

#[compio::test]
async fn a_retype_of_an_ordinary_column_keeps_the_cast_that_makes_it_work() {
    // THE OVER-SUPPRESSION CONTROL, and it is not decorative: `text -> int` is the
    // retype the cast exists for. A fix that dropped `USING` everywhere would pass
    // every generated-column leg above and break this one at the server.
    with_deployment("plain_cast", async |deployment| {
        let source = format!(
            r#"{{"ir_version":1,"name":"m","owner_app":"{OWNER}","ops":[
                {{"op":"createTable","name":"{TABLE}","columns":[
                    {{"name":"c0","type":"int","nullable":false}},
                    {{"name":"v","type":"text"}}
                ],"primaryKey":["c0"]}},
                {{"op":"setColumnType","table":"{TABLE}","column":"v","toType":"int"}}
            ]}}"#
        );
        let applied = deployment.deploy(&source, &BTreeMap::new()).await?;
        if applied.data_type != "integer" {
            return Err(format!(
                "text -> int applies: data_type={:?}",
                applied.data_type
            ));
        }
        if !applied.statements.iter().any(|up| up.contains(" USING ")) {
            return Err(format!(
                "PostgreSQL has no implicit text -> integer cast, so this retype needs the \
                 clause. statements={:?}",
                applied.statements
            ));
        }
        let ir: MigrationIr = serde_json::from_str(&source).expect("envelope parses");
        assert_clean("ordinary text -> int", &deployment.drift(&ir.ops).await?)
    })
    .await;
}

// ---------------------------------------------------------------------------
// THE SERVER ORACLE. Why gap 2 is a rendering fix and gap 1 is a refusal.
// ---------------------------------------------------------------------------

#[compio::test]
async fn postgresql_retypes_a_generated_column_only_without_the_using_cast() {
    // The measurement that CHOSE the fix, held to the server so a future reader
    // cannot re-litigate it from a comment. Both halves are asserted: PostgreSQL
    // refuses the clause AND accepts the statement without it. If it ever started
    // accepting the cast, or stopped accepting the bare ALTER, this is where the
    // decision has to be revisited.
    with_deployment("gen_oracle", async |deployment| {
        deployment
            .deploy(&create_envelope(GENERATED_COL), &BTreeMap::new())
            .await?;
        let table = format!(
            "{}.{}",
            quote_ident(&deployment.cfg.project_schema),
            quote_ident(TABLE)
        );
        let with_cast = format!(
            "ALTER TABLE {table} ALTER COLUMN {} TYPE bigint USING {}::bigint",
            quote_ident("v"),
            quote_ident("v"),
        );
        let refusal = deployment
            .session
            .batch(&with_cast)
            .await
            .expect_err("PostgreSQL refuses a USING clause on a generated column");
        let refusal = refusal.to_string();
        if !refusal.contains("cannot specify USING when altering type of generated column") {
            return Err(format!(
                "the reason the engine stopped emitting the cast is this exact refusal. \
                 Got: {refusal}"
            ));
        }
        let without_cast = format!(
            "ALTER TABLE {table} ALTER COLUMN {} TYPE bigint",
            quote_ident("v")
        );
        deployment
            .session
            .batch(&without_cast)
            .await
            .map_err(|error| {
                format!(
                    "and this is why the op is NOT refused: the server accepts the same change \
                 without the clause, so refusing it would deny a migration PostgreSQL \
                 honours. Got: {error}"
                )
            })?;
        let (_, attgenerated, data_type) = deployment.column_facts().await?;
        if attgenerated != "s" || data_type != "bigint" {
            return Err(format!(
                "and it stays generated afterwards — the server recomputes the expression \
                 under the new type. attgenerated={attgenerated:?} data_type={data_type:?}"
            ));
        }
        Ok(())
    })
    .await;
}

#[compio::test]
async fn postgresql_refuses_an_identity_retype_with_or_without_the_cast() {
    // The asymmetry, measured rather than assumed: for an IDENTITY column dropping
    // the cast changes nothing, which is exactly why gap 1 gets a refusal where gap
    // 2 gets a renderer fix.
    with_deployment("id_oracle", async |deployment| {
        deployment
            .deploy(&create_envelope(IDENTITY_COL), &BTreeMap::new())
            .await?;
        let table = format!(
            "{}.{}",
            quote_ident(&deployment.cfg.project_schema),
            quote_ident(TABLE)
        );
        for alter in [
            format!(
                "ALTER TABLE {table} ALTER COLUMN {} TYPE text USING {}::text",
                quote_ident("v"),
                quote_ident("v"),
            ),
            format!(
                "ALTER TABLE {table} ALTER COLUMN {} TYPE text",
                quote_ident("v")
            ),
        ] {
            let refusal = deployment
                .session
                .batch(&alter)
                .await
                .expect_err("PostgreSQL confines an identity column to its three types")
                .to_string();
            if !refusal.contains("identity column type must be smallint, integer, or bigint") {
                return Err(format!("for {alter:?} the server answered: {refusal}"));
            }
        }
        Ok(())
    })
    .await;
}

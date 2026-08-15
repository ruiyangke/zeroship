//! Behavioral integration test for the addon's `rollback` verb body: deploy an
//! authored envelope onto a real temp-file SQLite database, then drive
//! `verbs::rollback_with_locked_backend` over the SAME backend and read the
//! catalog to see whether the table actually went away.
//!
//! Real SQLite rather than a mock host driver, because the question this verb
//! raises is not "does the projection compile" but "does the `down` the verb
//! reconstructs from the authored envelope reverse what apply did". A canned
//! rowset would answer neither.
//!
//! What it pins:
//!
//! 1. a rollback driven through the verb removes the object the deploy created,
//!    and names the journaled version it unwound;
//! 2. the project lock is taken and released around the whole unwind, so a
//!    second acquisition succeeds afterwards -- SQLite refuses a same-instance
//!    re-acquire, which is what makes the release observable in-process;
//! 3. a version the journal holds but the supplied envelopes do not describe is
//!    REFUSED before any `down` runs, rather than skipped. That refusal is the
//!    whole reason the verb may leave a plan out of the migration set it builds;
//! 4. a plan carrying a data step is refused by the name its author gave it, not
//!    by the derived version the journal keys it under.

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::executor::{RollbackOptions, RollbackTarget};
use zero_migrate::approval::Approval;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::{MigrationEngine, MigrationIr, SqlDialect, SqliteBackend};

use zero_migrate_node::verbs::rollback_with_locked_backend;

const OWNER_APP: &str = "app_rollback_host";
const PROJECT_SCHEMA: &str = "app_rollback_host";

/// One authored envelope that lowers to a single DDL step, which is the shape the
/// verb can reverse from its authored source.
const CREATE_NOTES: &str = r#"{"ir_version":1,"name":"create_notes","ops":[
    {"op":"createTable","name":"notes","columns":[
        {"name":"id","type":"bigInt","nullable":false},
        {"name":"title","type":"text","nullable":false}
    ],"primaryKey":["id"]}
]}"#;

/// A view created in one envelope and dropped in a later one. The pair exists to
/// prove the SYNTHESISED inverse survives the verb, which is a different claim
/// from the engine rendering it.
///
/// The engine tests hand `Vec<Migration>` built during apply straight to
/// `rollback(...)`, so the `down` they exercise is the one apply produced. The verb
/// cannot do that: the `down` is not journaled, so it RE-LOWERS the authored
/// envelopes against a catalog read after the lock - a catalog in which the view is
/// already gone. The inverse only survives because the envelope loop folds each
/// envelope onto the running live schema before lowering the next, so the authored
/// `createView` puts the view back (with its typed body) before the `dropView`
/// envelope lowers.
const CREATE_ACTIVE_USERS: &str = r#"{"ir_version":1,"name":"create_active_users","ops":[
    {"op":"createTable","name":"users","columns":[
        {"name":"id","type":"bigInt","nullable":false},
        {"name":"email","type":"text","nullable":false}
    ],"primaryKey":["id"]},
    {"op":"createView","name":"active_users","query":{"kind":"structured","select":{
        "from":{"name":"users"},
        "projection":[{"kind":"colRef","name":"email"}],
        "joins":[],
        "groupBy":[]
    }}}
]}"#;

const DROP_ACTIVE_USERS: &str = r#"{"ir_version":1,"name":"drop_active_users","ops":[
    {"op":"dropView","name":"active_users"}
]}"#;

/// The table the multi-step data migration below writes into.
///
/// This used to be one envelope carrying the createTable AND the insert. F656
/// retired that shape: DDL and DML may no longer share an op list, so the fixture
/// has to reach a multi-step plan a way an author could actually write.
const CREATE_SEEDS: &str = r#"{"ir_version":1,"name":"create_seeds","ops":[
    {"op":"createTable","name":"seeds","columns":[
        {"name":"id","type":"bigInt","nullable":false}
    ],"primaryKey":["id"]}
]}"#;

/// One authored envelope that lowers to TWO journaled DML steps.
///
/// Two inserts rather than one createTable plus one insert: the property under
/// test is that a plan with more than one journaled step has no reverse the verb
/// can hand over, and that is reached by op COUNT, not by op kind. It declares a
/// real inverse precisely so the refusal cannot be mistaken for "no reverse was
/// recorded" -- one IS recorded, and the step count is still what stops it.
const SEED_NOTES: &str = r#"{"ir_version":1,"name":"seed_notes","ops":[
    {"op":"insert","table":"seeds","columns":["id"],"rows":[[1]]},
    {"op":"insert","table":"seeds","columns":["id"],"rows":[[2]]}
],"inverse_ops":[
    {"op":"delete","table":"seeds","where":{"node":"inList",
        "expr":{"node":"colRef","name":"id"},"elems":[1,2],"negated":false}}
]}"#;

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(app_id: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{app_id}.sqlite"));
    let journal = dir.path().join(format!("zs-{app_id}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(
        PROJECT_SCHEMA,
        PROJECT_SCHEMA,
        support::no_inject(PROJECT_SCHEMA),
    )
}

async fn table_exists(be: &SqliteBackend, name: &str) -> bool {
    let rows = be
        .actor()
        .query(&format!(
            "SELECT name FROM main.sqlite_master WHERE type='table' AND name='{name}'"
        ))
        .await
        .expect("catalog probe");
    !rows.is_empty()
}

/// Deploy the authored envelopes the same way the addon's in-process SQLite apply
/// does, so the journal this test rolls back is the one that path writes.
async fn deploy(be: &SqliteBackend, envelopes: &[&str]) -> Vec<String> {
    let parsed: Vec<MigrationIr> = envelopes
        .iter()
        .map(|envelope| serde_json::from_str(envelope).expect("envelope parses as MigrationIr"))
        .collect();
    let policy = support::no_inject(PROJECT_SCHEMA);
    MigrationEngine::new()
        .deploy_envelopes(
            &parsed,
            be,
            &policy,
            SqlDialect::Sqlite,
            PROJECT_SCHEMA,
            OWNER_APP,
            &BTreeMap::new(),
            Approval::Approved,
            &exec_cfg(),
        )
        .await
        .expect("the authored envelopes deploy")
        .applied
}

async fn view_exists(be: &SqliteBackend, name: &str) -> bool {
    let rows = be
        .actor()
        .query(&format!(
            "SELECT name FROM main.sqlite_master WHERE type='view' AND name='{name}'"
        ))
        .await
        .expect("catalog probe");
    !rows.is_empty()
}

/// A dropped view comes back when the unwind runs through the VERB, not just when
/// the engine is handed the migrations apply built.
///
/// This is the one that proves re-lowering reconstructs the synthesised `down`.
/// The verb reads the live catalog AFTER taking the lock, and at that moment the
/// view is already dropped - so if the envelope loop lowered every envelope against
/// that one static snapshot, the `dropView` would find no recorded view, render
/// `down: None`, and the rollback would refuse the very migration the engine tests
/// prove reversible.
#[test]
fn a_view_dropped_by_a_later_envelope_comes_back_through_the_verb() {
    let p = paths("rb_verb_view_inverse");
    let charter = support::no_inject_charter_toml(PROJECT_SCHEMA);

    let (reply, view_back) = futures::executor::block_on(async {
        let be = backend(&p);
        deploy(&be, &[CREATE_ACTIVE_USERS]).await;
        assert!(
            view_exists(&be, "active_users").await,
            "the first envelope must create the view this test then drops"
        );
        deploy(&be, &[CREATE_ACTIVE_USERS, DROP_ACTIVE_USERS]).await;
        assert!(
            !view_exists(&be, "active_users").await,
            "the second envelope must actually drop the view"
        );

        let reply = rollback_with_locked_backend(
            &be,
            &exec_cfg(),
            &[
                CREATE_ACTIVE_USERS.to_string(),
                DROP_ACTIVE_USERS.to_string(),
            ],
            OWNER_APP,
            PROJECT_SCHEMA,
            "sqlite",
            "{}",
            std::slice::from_ref(&charter),
            RollbackTarget::Steps(1),
            RollbackOptions::default(),
            Approval::Approved,
            "operator",
        )
        .await;

        (reply, view_exists(&be, "active_users").await)
    });

    let reply = reply.expect("the verb rolls the dropped view back");
    assert!(
        reply.skipped_irreversible.is_empty(),
        "the drop has a synthesised inverse, so nothing may be skipped as irreversible: {:?}",
        reply.skipped_irreversible
    );
    assert!(
        view_back,
        "the view the verb rolled back must be in the catalog again"
    );
}

#[test]
fn a_rollback_driven_through_the_verb_removes_the_table_the_deploy_created() {
    let p = paths("rb_verb_roundtrip");
    let charter = support::no_inject_charter_toml(PROJECT_SCHEMA);

    let (reply, still_there, lock_free_after) = futures::executor::block_on(async {
        let be = backend(&p);
        let applied = deploy(&be, &[CREATE_NOTES]).await;
        assert!(
            table_exists(&be, "notes").await,
            "the deploy must create the table this test then unwinds"
        );

        let reply = rollback_with_locked_backend(
            &be,
            &exec_cfg(),
            &[CREATE_NOTES.to_string()],
            OWNER_APP,
            PROJECT_SCHEMA,
            "sqlite",
            "{}",
            std::slice::from_ref(&charter),
            RollbackTarget::All,
            RollbackOptions::default(),
            Approval::Approved,
            "operator",
        )
        .await;

        // A same-instance re-acquire is refused while the lock is held, so this
        // succeeding is the evidence the verb released what it took.
        let lock_free_after = {
            use zero_migrate::MigrationBackend;
            be.acquire_project_lock(&exec_cfg()).await.is_ok()
        };
        (
            reply.map(|reply| (reply, applied)),
            table_exists(&be, "notes").await,
            lock_free_after,
        )
    });

    let (reply, applied) = reply.expect("the verb rolls the authored envelope back");
    assert_eq!(
        reply.rolled_back, applied,
        "the verb unwinds exactly the versions the deploy journaled"
    );
    assert!(
        reply.skipped_irreversible.is_empty(),
        "nothing was forced, so nothing may be skipped as irreversible"
    );
    assert!(
        !still_there,
        "the rolled-back table is gone from the catalog"
    );
    assert!(
        lock_free_after,
        "the project lock is released whichever way the unwind went"
    );
}

#[test]
fn a_journaled_version_the_supplied_envelopes_do_not_describe_is_refused() {
    let p = paths("rb_verb_missing");
    let charter = support::no_inject_charter_toml(PROJECT_SCHEMA);

    let (error, still_there) = futures::executor::block_on(async {
        let be = backend(&p);
        deploy(&be, &[CREATE_NOTES]).await;

        // The operator asks to unwind everything while handing over no authored
        // source at all -- the same position the verb is in when a plan lowered
        // to more than one journaled step and was left out of the set.
        let error = rollback_with_locked_backend(
            &be,
            &exec_cfg(),
            &[],
            OWNER_APP,
            PROJECT_SCHEMA,
            "sqlite",
            "{}",
            std::slice::from_ref(&charter),
            RollbackTarget::All,
            RollbackOptions::default(),
            Approval::Approved,
            "operator",
        )
        .await
        .expect_err("a version with no authored source has no reverse SQL to run");

        (error, table_exists(&be, "notes").await)
    });

    assert!(
        error.contains("absent from the supplied set"),
        "the refusal must say the version is absent from the set, got: {error}"
    );
    assert!(
        still_there,
        "the refusal happens before any down runs, so the table survives"
    );
}

#[test]
fn a_plan_with_a_data_step_is_refused_by_the_name_its_author_gave_it() {
    let p = paths("rb_verb_multistep");
    let charter = support::no_inject_charter_toml(PROJECT_SCHEMA);

    let (error, still_there) = futures::executor::block_on(async {
        let be = backend(&p);
        deploy(&be, &[CREATE_SEEDS, SEED_NOTES]).await;

        // The authored source IS supplied here. It is the verb that leaves the plan
        // out, because a plan whose steps include a DML identity cannot be handed
        // over as a reversible `Migration`.
        let error = rollback_with_locked_backend(
            &be,
            &exec_cfg(),
            &[CREATE_SEEDS.to_string(), SEED_NOTES.to_string()],
            OWNER_APP,
            PROJECT_SCHEMA,
            "sqlite",
            "{}",
            std::slice::from_ref(&charter),
            RollbackTarget::All,
            RollbackOptions::default(),
            Approval::Approved,
            "operator",
        )
        .await
        .expect_err("a plan carrying a data step has no reverse SQL to run");

        (error, table_exists(&be, "seeds").await)
    });

    assert!(
        error.contains("seed_notes"),
        "the refusal must name the migration the operator authored, not only the \
         derived version, got: {error}"
    );
    assert!(
        error.contains("more than one journaled step"),
        "the refusal must say why the plan could not be reversed, got: {error}"
    );
    assert!(
        still_there,
        "the refusal happens before any down runs, so the table survives"
    );
}

//! Behavioral integration test for the addon's plan-aware `statusIr` verb body:
//! drive `verbs::status_ir_with_locked_backend` through the addon's
//! `NapiHostSession` over a MOCK `VerbDispatch` answering canned PostgreSQL rows.
//! NO Node host, NO database, so it runs in the napi-free build the workspace
//! gate gets.
//!
//! It is the Rust peer of `__test__/status_ir.mjs`, which drives the same verb
//! through the real `.node`. That script needs a built addon and a Node runtime;
//! this one needs neither, so the reply projection and the lock bracket stay
//! covered by a test that always runs.
//!
//! What it pins:
//!
//! 1. every operator-facing detail of the reply (net rollbacks, outstanding
//!    contracts with their orphan diagnosis, unexpected journal identities and
//!    their applied/inflight state) survives the projection into `StatusReply`;
//! 2. the project lock is acquired BEFORE the live catalog and journal reads and
//!    released after them, so one coherent snapshot backs the reconcile, and the
//!    acquisition is the NON-WAITING `pg_try_advisory_lock` rather than the
//!    unbounded `pg_advisory_lock` a deploy takes, so a reader never sits behind a
//!    peer's deploy;
//! 3. a contended acquisition reads NOTHING -- no catalog, no journal, no
//!    contracts -- and returns the busy reply, because those reads are composite
//!    and unbracketed and a live deploy's own halfway state would read as drift;
//! 4. the journal net-state read decodes `event_seq`, so a host reply that omits
//!    the column is rejected instead of silently reconciling on partial rows.

mod support;

use std::cell::RefCell;

use zero_migrate::conn::ExecutorConfig;
use zero_migrate::PostgresBackend;

use zero_migrate_node::marshal::{JsCell, JsReply, JsRequest, JsRow};
use zero_migrate_node::session::{NapiHostSession, VerbDispatch, VerbReply};
use zero_migrate_node::verbs::status_ir_with_locked_backend;

const COMPLETED: &str = "mig_0000000000000000000001";
const INFLIGHT: &str = "mig_0000000000000000000002";
const ROLLED_BACK: &str = "mig_0000000000000000000003";
const PENDING_CONTRACT: &str = "mig_0000000000000000000004";
const PLAN_VERSION: &str = "mig_0000000000000000000005";

const OWNER_APP: &str = "app_status_host";
const PROJECT_SCHEMA: &str = "proj_status_host";

/// A recording mock host driver answering the reads a plan-aware status makes.
/// Routing is by SQL shape, exactly as the JS canned driver routes.
struct MockPgDispatch {
    log: RefCell<Vec<String>>,
    /// When false the journal net-state rows omit `event_seq`, standing in for a
    /// host reply built against an older projection.
    journal_carries_event_seq: bool,
    /// When false every `pg_try_advisory_lock` answers false, standing in for a
    /// peer's deploy holding the project lock for the length of its run.
    grants_project_lock: bool,
}

/// Process id the canned holder probe reports, so the busy reply can be checked
/// for the holder detail an operator message names.
const HOLDER_PID: i64 = 4242;

impl MockPgDispatch {
    const fn new(journal_carries_event_seq: bool) -> Self {
        Self {
            log: RefCell::new(Vec::new()),
            journal_carries_event_seq,
            grants_project_lock: true,
        }
    }

    const fn contended() -> Self {
        Self {
            log: RefCell::new(Vec::new()),
            journal_carries_event_seq: true,
            grants_project_lock: false,
        }
    }

    fn rows_for(&self, sql: &str) -> Vec<JsRow> {
        if sql.contains("pg_try_advisory_lock") {
            return vec![row(&["got"], vec![bool_cell(self.grants_project_lock)])];
        }
        if sql.contains("pg_stat_activity") {
            return vec![row(
                &["pid", "application_name", "state", "query"],
                vec![
                    int_cell(HOLDER_PID),
                    text_cell("zero-migrate"),
                    text_cell("active"),
                    text_cell("CREATE INDEX CONCURRENTLY ix_widgets_name ON widgets (name)"),
                ],
            )];
        }
        if sql.contains("c.relname = 'schema_backfills'") {
            return vec![row(
                &["table_exists", "checksum_exists"],
                vec![bool_cell(false), bool_cell(false)],
            )];
        }
        if sql.contains("union_all") {
            return self.journal_net_state_rows();
        }
        if sql.contains("schema_migrations") && sql.contains("event_kind = 'rolled_back'") {
            return vec![row(
                &["version", "name", "checksum", "actor", "exec_ms", "at"],
                vec![
                    text_cell(ROLLED_BACK),
                    text_cell("rolled back migration"),
                    text_cell("checksum-rolled-back"),
                    text_cell("operator"),
                    null_cell(),
                    text_cell("2026-07-15T00:00:00.000000+00:00"),
                ],
            )];
        }
        if sql.contains("schema_pending_contracts") && sql.contains("WHERE state = 'resolved'") {
            return Vec::new();
        }
        if sql.contains("schema_pending_contracts") {
            return vec![row(
                &[
                    "pending_version",
                    "plan_version",
                    "owner_app",
                    "table",
                    "from_col",
                    "to_col",
                    "ty",
                    "contract_versions",
                ],
                vec![
                    text_cell(PENDING_CONTRACT),
                    text_cell(PLAN_VERSION),
                    text_cell(OWNER_APP),
                    text_cell("widgets"),
                    text_cell("old_name"),
                    text_cell("new_name"),
                    text_cell("text"),
                    text_cell("[]"),
                ],
            )];
        }
        Vec::new()
    }

    /// One completed and one inflight journal identity, neither declared by the
    /// supplied plans, so both must surface as unexpected journal entries.
    fn journal_net_state_rows(&self) -> Vec<JsRow> {
        if !self.journal_carries_event_seq {
            return vec![row(
                &["version", "checksum", "mig_kind", "phase"],
                vec![
                    text_cell(COMPLETED),
                    text_cell("checksum-completed"),
                    text_cell("apply"),
                    text_cell("completed"),
                ],
            )];
        }
        let columns = ["version", "checksum", "mig_kind", "event_seq", "phase"];
        vec![
            row(
                &columns,
                vec![
                    text_cell(COMPLETED),
                    text_cell("checksum-completed"),
                    text_cell("apply"),
                    int_cell(1),
                    text_cell("completed"),
                ],
            ),
            row(
                &columns,
                vec![
                    text_cell(INFLIGHT),
                    text_cell("checksum-inflight"),
                    null_cell(),
                    int_cell(2),
                    text_cell("started"),
                ],
            ),
        ]
    }
}

impl VerbDispatch for MockPgDispatch {
    async fn dispatch(&self, req: JsRequest) -> VerbReply {
        self.log.borrow_mut().push(req.sql.clone());
        let rows = self.rows_for(&req.sql);
        let row_count = rows.len() as i64;
        Ok(JsReply {
            rows,
            row_count: Some(row_count),
        })
    }
}

fn row(columns: &[&str], cells: Vec<JsCell>) -> JsRow {
    JsRow {
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        cells,
    }
}

fn text_cell(s: &str) -> JsCell {
    JsCell {
        kind: "text".into(),
        text: Some(s.to_string()),
        int: None,
        int_str: None,
        bool: None,
        text_array: None,
    }
}

fn int_cell(value: i64) -> JsCell {
    JsCell {
        kind: "int".into(),
        text: None,
        int: None,
        int_str: Some(value.to_string()),
        bool: None,
        text_array: None,
    }
}

fn bool_cell(value: bool) -> JsCell {
    JsCell {
        kind: "bool".into(),
        text: None,
        int: None,
        int_str: None,
        bool: Some(value),
        text_array: None,
    }
}

fn null_cell() -> JsCell {
    JsCell {
        kind: "null".into(),
        text: None,
        int: None,
        int_str: None,
        bool: None,
        text_array: None,
    }
}

/// Reconcile an empty authored plan set against the canned journal.
fn run_status(
    journal_carries_event_seq: bool,
) -> (
    Result<zero_migrate_node::wire::StatusReply, String>,
    Vec<String>,
) {
    futures::executor::block_on(async {
        let session = NapiHostSession::new(MockPgDispatch::new(journal_carries_event_seq));
        let cfg = ExecutorConfig::new(
            PROJECT_SCHEMA,
            PROJECT_SCHEMA,
            support::no_inject(PROJECT_SCHEMA),
        );
        let backend = PostgresBackend::new_generic(&session);
        let charter_layers = vec![support::no_inject_charter_toml(PROJECT_SCHEMA)];
        let reply = status_ir_with_locked_backend(
            &backend,
            &cfg,
            &[],
            OWNER_APP,
            PROJECT_SCHEMA,
            "postgres",
            "{}",
            &charter_layers,
            false,
        )
        .await;
        let log = session.into_dispatch().log.into_inner();
        (reply, log)
    })
}

#[test]
fn status_ir_projects_every_operator_detail_over_the_host_bridge() {
    let (reply, _log) = run_status(true);
    let reply = reply.expect("plan-aware status succeeds over the canned host driver");

    assert_eq!(
        reply.rolled_back,
        vec![ROLLED_BACK.to_string()],
        "the net rollback must survive the projection"
    );
    assert_eq!(
        reply.pending_contracts.len(),
        1,
        "the outstanding contract must survive the projection: {:?}",
        reply.pending_contracts
    );
    assert_eq!(reply.pending_contracts[0].pending_version, PENDING_CONTRACT);
    assert_eq!(reply.pending_contracts[0].table, "widgets");
    assert!(
        reply.pending_contracts[0].orphaned,
        "a contract whose supplying plan is absent is orphaned"
    );
    assert!(
        reply.blocked.is_empty(),
        "no supplied plan depends on the contract: {:?}",
        reply.blocked
    );

    assert_eq!(
        reply.unexpected_journal.len(),
        2,
        "both undeclared journal identities must be reported: {:?}",
        reply.unexpected_journal
    );
    assert_eq!(reply.unexpected_journal[0].version, COMPLETED);
    assert_eq!(reply.unexpected_journal[0].state, "applied");
    assert_eq!(
        reply.unexpected_journal[0].journal_kind.as_deref(),
        Some("apply")
    );
    assert_eq!(reply.unexpected_journal[1].version, INFLIGHT);
    assert_eq!(reply.unexpected_journal[1].state, "inflight");

    let plans = reply
        .plans
        .as_deref()
        .expect("an empty authored set reconciles to an empty plan list, not a missing one");
    assert!(plans.is_empty(), "no plan was supplied: {plans:?}");
    assert!(reply.applied.is_empty());
    assert!(reply.pending.is_empty());
}

/// The bracket contract, pinned to the NON-WAITING acquisition.
///
/// The needle moved from `pg_advisory_lock` to `pg_try_advisory_lock` because the
/// acquisition itself changed, not because the old assertion was inconvenient: the
/// three orderings it pinned (lock before catalog, lock before journal, journal
/// before unlock) are all still asserted, over the same log, on the same reads.
/// The needles are anchored rather than loosened -- `pg_advisory_lock` is asserted
/// ABSENT, so a regression back to the blocking acquisition fails here instead of
/// passing on a substring that matches both spellings.
#[test]
fn status_ir_brackets_the_reads_inside_one_non_blocking_project_lock() {
    let (reply, log) = run_status(true);
    reply.expect("plan-aware status succeeds over the canned host driver");

    let idx = |needle: &str| {
        log.iter()
            .position(|sql| sql.contains(needle))
            .unwrap_or_else(|| panic!("verb log is missing {needle:?}: {log:#?}"))
    };
    let lock = idx("pg_try_advisory_lock");
    let catalog_read = idx("FROM pg_class child");
    let journal_read = idx("union_all");
    let unlock = idx("pg_advisory_unlock");

    assert!(
        lock < catalog_read,
        "the project lock must precede the live catalog read: lock@{lock} catalog@{catalog_read}"
    );
    assert!(
        lock < journal_read,
        "the project lock must precede the journal read: lock@{lock} journal@{journal_read}"
    );
    assert!(
        journal_read < unlock,
        "the lock is released after the journal read: journal@{journal_read} unlock@{unlock}"
    );
    assert!(
        !log.iter()
            .any(|sql| sql.contains("SELECT pg_advisory_lock")),
        "a status read must never take the unbounded acquisition a deploy takes: {log:#?}"
    );
    assert!(
        !log.iter().any(|sql| sql.contains("pg_stat_activity")),
        "an uncontended acquisition has no holder to probe for: {log:#?}"
    );
}

/// A contended acquisition ends the verb before any read.
///
/// This is the half the ordering assertions above cannot reach: they prove the
/// reads sit inside the bracket, not that a FAILED acquisition skips them. Without
/// it, a verb that reported busy and then read anyway would still pass every
/// ordering arm.
#[test]
fn status_ir_reads_nothing_when_a_peer_holds_the_project_lock() {
    let (reply, log) = futures::executor::block_on(async {
        let session = NapiHostSession::new(MockPgDispatch::contended());
        let cfg = ExecutorConfig::new(
            PROJECT_SCHEMA,
            PROJECT_SCHEMA,
            support::no_inject(PROJECT_SCHEMA),
        );
        let backend = PostgresBackend::new_generic(&session);
        let charter_layers = vec![support::no_inject_charter_toml(PROJECT_SCHEMA)];
        let reply = status_ir_with_locked_backend(
            &backend,
            &cfg,
            &[],
            OWNER_APP,
            PROJECT_SCHEMA,
            "postgres",
            "{}",
            &charter_layers,
            true,
        )
        .await;
        let log = session.into_dispatch().log.into_inner();
        (reply, log)
    });

    let reply = reply.expect("contention is an outcome, not a verb error");
    assert!(reply.busy, "a contended read must say so: {reply:#?}");
    assert_eq!(
        reply.lock_holders.iter().map(|h| h.pid).collect::<Vec<_>>(),
        vec![HOLDER_PID],
        "the busy reply names the holder the probe found"
    );
    assert_eq!(
        reply.lock_holders[0].query.as_deref(),
        Some("CREATE INDEX CONCURRENTLY ix_widgets_name ON widgets (name)")
    );
    assert!(reply.applied.is_empty() && reply.pending.is_empty());
    assert!(
        reply.plans.is_none(),
        "a busy reply reconciles nothing, so it carries no plan list: {:?}",
        reply.plans
    );

    for forbidden in [
        "FROM pg_class child",
        "union_all",
        "schema_pending_contracts",
        "schema_backfills",
        "pg_advisory_unlock",
    ] {
        assert!(
            !log.iter().any(|sql| sql.contains(forbidden)),
            "a contended status must not run {forbidden:?}: {log:#?}"
        );
    }
    assert_eq!(
        log.iter()
            .filter(|sql| sql.contains("pg_try_advisory_lock"))
            .count(),
        3,
        "the retry is bounded at three attempts, never a loop until acquired: {log:#?}"
    );
}

#[test]
fn a_journal_reply_without_event_seq_is_rejected() {
    let (reply, _log) = run_status(false);
    let error = reply.expect_err("a journal row missing event_seq must not reconcile");
    assert!(
        error.contains("column not found in row"),
        "the missing journal column must surface as a decode failure: {error}"
    );
}

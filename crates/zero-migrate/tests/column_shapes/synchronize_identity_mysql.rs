//! Live MySQL coverage for import-time identity synchronization - the third leg of
//! the "end-to-end PostgreSQL, MySQL, and SQLite DDL tests" the id-system design asks
//! for, beside `synchronize_identity_pg.rs` and `synchronize_identity_sqlite.rs`.
//!
//! Gated behind `ZERO_MIGRATE_MYSQL_URL`; DB-free runs skip cleanly through
//! `announce_live_db_skip`, so `ZERO_MIGRATE_REQUIRE_LIVE_DB=1` turns a missing DSN
//! into a failure rather than a green run with no coverage. These tests drive the
//! shipped generic `MysqlBackend<MysqlDevSession>` seam.
//!
//! # This is NOT the PostgreSQL file with the nouns swapped, and the reason is a
//! # MEASURED property of MySQL rather than a stylistic one
//!
//! MySQL's identity is a per-table `AUTO_INCREMENT` counter, not a sequence OBJECT:
//! no `setval`, no owned relation to rename or drop, and no way to read the next
//! value without CONSUMING it. That much is expected. The consequential difference is
//! this one, MEASURED on MySQL 8.4.11 / InnoDB by
//! [`the_counter_cannot_be_driven_below_the_live_maximum`]:
//!
//! > **A MySQL AUTO_INCREMENT counter cannot be BEHIND its live maximum.**
//!
//! Both sibling legs construct exactly that state, and each has a writable allocator
//! to construct it with. PostgreSQL has `setval`. SQLite has `sqlite_sequence`, an
//! ordinary writable table, plus the fact that its `seq` tracks INSERTs and ignores a
//! later UPDATE of the rowid alias. InnoDB has neither escape:
//!
//! - an INSERT naming an explicit higher value advances the counter to `MAX + 1`;
//! - so does a later UPDATE that raises the column - the SQLite leg's exact trick,
//!   MEASURED here NOT to work (insert 10, update to 50, and the counter is 51);
//! - `ALTER TABLE … AUTO_INCREMENT = n` is CLAMPED to `MAX + 1` when `n` would land
//!   at or below the live maximum (`= 20` against a maximum of 50 leaves 51);
//! - re-adding `AUTO_INCREMENT` to a column that already holds rows initialises the
//!   counter to `MAX + 1` (77 -> 78);
//! - and since 8.0 the counter is redo-logged, so a restart no longer loses it.
//!
//! An earlier draft of this file asserted a behind counter of 11 after that UPDATE
//! and PASSED its probe. The probe was reading `information_schema` THROUGH the
//! statistics cache; the true value was 51 the whole time. Which is the second reason
//! this file has to be live, and the reason every read below goes through
//! [`live_counter`].
//!
//! ## So what does the operation DO on MySQL?
//!
//! It enforces a floor of `MAX(column) + @@SESSION.auto_increment_increment`, and the
//! natural counter is `MAX + 1`. Those are the same number whenever the session
//! increment is 1 - which is the stock configuration of every single-writer MySQL.
//! [`on_a_stock_single_writer_mysql_the_operation_can_never_emit_ddl`] pins that:
//! **at `auto_increment_increment = 1` this operation is unconditionally a no-op on
//! MySQL**, whatever the table contains. It emits DDL only under a NON-unit session
//! increment, whose production use is multi-source replication, where the increment
//! is the number of writers and clearing the maximum by a full increment is what
//! keeps two writers' lattices from colliding.
//!
//! That is also why the non-unit increment is this file's discriminator, the exact
//! counterpart of `INCREMENT BY 5` in the PostgreSQL leg - except that MySQL spells
//! it as a SESSION variable rather than in the column's declaration. MEASURED:
//! neither `snapshot_session` nor `configure_session` reads or writes
//! `auto_increment_increment`, so the value a test sets is the value the engine reads.
//!
//! # What only a LIVE MySQL server can prove
//!
//! `apply/backend/mysql/identity_sql.rs` already has four unit tests over a
//! `RecordingSession` fake, and they DO pin the emitted DDL text
//! (``ALTER TABLE `app`.`orders` AUTO_INCREMENT = 23``) and its ordering against the
//! lock. Four things are out of a fake's reach, and each has a test here:
//!
//! 1. **The stale-catalog trap.** `information_schema.TABLES.AUTO_INCREMENT` is
//!    served from a server-wide statistics cache whose default lifetime is
//!    `information_schema_stats_expiry = 86400`. MEASURED: with the cache primed at
//!    11 and the true counter at 1001, the cached answer persisted - and an expiry-0
//!    read that bypassed the cache did NOT repopulate it, so the stale answer
//!    survived a fresh reader. The engine pins `information_schema_stats_expiry = 0`
//!    for exactly this. A canned row in a fake is fresh by construction, so the fake
//!    passes with the pin deleted;
//!    [`a_stale_catalog_counter_never_moves_the_allocator_backward`] does not.
//! 2. **That the monotonic guard is load-bearing.** MEASURED: MySQL ACCEPTS an
//!    `ALTER TABLE … AUTO_INCREMENT = n` that lowers the counter, so long as `n`
//!    stays above the live maximum (200 -> 60 against a maximum of 50). The engine's
//!    `desired <= current` early return is the only thing standing between an
//!    already-advanced allocator and a backward jump onto issued identities.
//! 3. **That `UNLOCK TABLES` really unlocked.** The fake asserts the STRING reached
//!    its log. Only a server can be asked whether the session is still inside
//!    `LOCK TABLES`, and it answers by refusing any table the lock did not name.
//! 4. **That the catalog is readable at all under the engine's own write lock**, and
//!    that backticked identifiers survive as identifiers. Every table below is named
//!    `order` and every identity column `key` - both MySQL reserved words - so an
//!    unquoted render is a syntax error rather than a passing test.
//!
//! # The altitude the dialect corpus does NOT reach
//!
//! `synchronizeIdentity` is in the dialect corpus, so `dialect_conformance_live.rs`
//! drives it on MySQL and records a `Verdict`. MEASURED: that row's prelude
//! (`dialect_corpus/mod.rs`, the `("synchronizeIdentity", _)` arm) creates `t` and
//! inserts NOTHING. `MAX(column)` over an empty table is NULL, and
//! `resolve_counter_advance` returns `Ok(None)` on a NULL maximum - so the
//! conformance leg reaches `Applied` through that early return, and no `ALTER TABLE`
//! is ever emitted against a live MySQL server by that suite.
//! [`an_empty_table_is_a_journaled_no_op_which_is_the_path_conformance_rides`] pins
//! the branch deliberately so the distinction is a test rather than a claim.

use crate::support;

use crate::support::mysql::{database_token, quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::apply::backend::{MigrationBackend, MysqlBackend};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::CURRENT_IR_VERSION;
use zero_migrate::{
    ApplyError, ExecutorConfig, IrAuthor, LiveSchema, PlanStep, SqlDialect, SynchronizeIdentityStep,
};

const OWNER: &str = "app_synchronize_identity_mysql";

/// MySQL reserved words, both of them, so an unquoted render cannot pass.
const TABLE: &str = "order";
const COLUMN: &str = "key";

/// The non-unit session increment, the counterpart of the PostgreSQL leg's
/// `INCREMENT BY 5`. It is the ONLY configuration under which this operation emits
/// any DDL on MySQL - see the module header and
/// [`on_a_stock_single_writer_mysql_the_operation_can_never_emit_ddl`].
const INCREMENT: i64 = 5;

/// MySQL's stock `auto_increment_increment`, and the one every single-writer server
/// runs.
const STOCK_INCREMENT: i64 = 1;

fn cfg_for(database: &str) -> ExecutorConfig {
    ExecutorConfig::new(
        format!("project_{database}"),
        database,
        support::no_inject(database),
    )
}

/// Lower the authored `synchronizeIdentity` op through the shipped MySQL author, so
/// the step under test is the one the engine builds rather than one this file
/// hand-assembles.
fn step(database: &str, name: &str) -> SynchronizeIdentityStep {
    let policy = support::no_inject(database);
    let ir: zero_migrate::MigrationIr = serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": name,
        "owner_app": OWNER,
        "ops": [{
            "op": "synchronizeIdentity",
            "table": TABLE,
            "column": COLUMN,
            "writesQuiesced": "mysql_identity_import_window"
        }]
    }))
    .expect("synchronizeIdentity IR parses");
    let plan = IrAuthor::new(database, OWNER, SqlDialect::Mysql, &policy)
        .lower_plan(&ir, &LiveSchema::default())
        .expect("synchronizeIdentity IR lowers for MySQL");
    match plan.steps.into_iter().next().expect("exactly one step") {
        PlanStep::SynchronizeIdentity(step) => step,
        other => panic!("expected SynchronizeIdentity step, got {other:?}"),
    }
}

/// Create the project database, its journal, and an `order` table, and pin the
/// session increment. Returns the guard so a panicking test still drops the database
/// (and the `_migrations` one the engine creates beside it).
async fn setup<'a>(
    session: &'a MysqlDevSession,
    database: &str,
    cfg: &ExecutorConfig,
    auto_increment: bool,
    increment: i64,
) -> DatabaseGuard<'a> {
    let guard = DatabaseGuard::arm(session, [database.to_string()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(database)))
        .await
        .expect("create the project database");
    MysqlBackend::new_generic(session)
        .ensure_journal(cfg)
        .await
        .expect("create the MySQL journal");
    let generated = if auto_increment {
        " AUTO_INCREMENT"
    } else {
        ""
    };
    session
        .batch(&format!(
            "CREATE TABLE {}.{} ({} BIGINT NOT NULL{generated} PRIMARY KEY, payload TEXT)",
            quote_ident(database),
            quote_ident(TABLE),
            quote_ident(COLUMN),
        ))
        .await
        .expect("create the identity fixture table");
    session
        .batch(&format!(
            "SET SESSION auto_increment_increment = {increment}"
        ))
        .await
        .expect("pin the session increment");
    guard
}

async fn exec(session: &MysqlDevSession, sql: &str) {
    session
        .batch(sql)
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
}

/// Drive the SHIPPED seam, under the project lock the executor takes.
async fn run(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    step: &SynchronizeIdentityStep,
) -> Result<bool, ApplyError> {
    let backend = MysqlBackend::new_generic(session);
    backend.acquire_project_lock(cfg).await?;
    let result = backend
        .synchronize_identity(cfg, step, "mysql-identity-test")
        .await;
    backend.release_project_lock(cfg).await?;
    result
}

async fn scalar(session: &MysqlDevSession, sql: &str) -> Option<i64> {
    session
        .query(sql, &[])
        .await
        .unwrap_or_else(|error| panic!("query {sql:?}: {error}"))
        .first()
        .and_then(|row| row.try_get::<_, Option<String>>(0usize).ok())
        .flatten()
        .and_then(|text| text.trim().parse().ok())
}

/// The table's TRUE `AUTO_INCREMENT`, read with the statistics cache BYPASSED.
///
/// Not defensive. Without the `information_schema_stats_expiry = 0` this sets, the
/// read is served from a server-wide cache with a one-day default lifetime and can be
/// arbitrarily far behind the truth - which is how an earlier draft of this file
/// convinced itself of a behind counter that did not exist.
async fn live_counter(session: &MysqlDevSession, database: &str) -> Option<i64> {
    exec(session, "SET SESSION information_schema_stats_expiry = 0").await;
    counter_read(session, database, TABLE).await
}

/// Read the counter through the CACHE, priming it, and leave the session on the
/// server default so the next reader is served the cached answer too.
async fn prime_cached_counter(session: &MysqlDevSession, database: &str) -> Option<i64> {
    exec(
        session,
        "SET SESSION information_schema_stats_expiry = DEFAULT",
    )
    .await;
    counter_read(session, database, TABLE).await
}

async fn counter_read(session: &MysqlDevSession, database: &str, table: &str) -> Option<i64> {
    scalar(
        session,
        &format!(
            "SELECT CAST(AUTO_INCREMENT AS CHAR) FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = '{database}' AND TABLE_NAME = '{table}'"
        ),
    )
    .await
}

async fn maximum(session: &MysqlDevSession, database: &str) -> Option<i64> {
    scalar(
        session,
        &format!(
            "SELECT CAST(MAX({}) AS CHAR) FROM {}.{}",
            quote_ident(COLUMN),
            quote_ident(database),
            quote_ident(TABLE),
        ),
    )
    .await
}

/// Insert one row WITHOUT naming the identity column and report the value the server
/// allocated. The cache-free oracle: it asks the allocator itself.
async fn allocate(session: &MysqlDevSession, database: &str) -> Option<i64> {
    exec(
        session,
        &format!(
            "INSERT INTO {}.{} (payload) VALUES ('generated')",
            quote_ident(database),
            quote_ident(TABLE),
        ),
    )
    .await;
    maximum(session, database).await
}

/// Insert one row NAMING the identity column, the way an import does.
async fn import(session: &MysqlDevSession, database: &str, value: i64) {
    exec(
        session,
        &format!(
            "INSERT INTO {}.{} ({}, payload) VALUES ({value}, 'imported')",
            quote_ident(database),
            quote_ident(TABLE),
            quote_ident(COLUMN),
        ),
    )
    .await;
}

/// How many journal events the step's version has.
async fn journal_rows(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    step: &SynchronizeIdentityStep,
) -> Option<i64> {
    let version = step.migration.version.as_str().replace('\'', "''");
    scalar(
        session,
        &format!(
            "SELECT CAST(count(*) AS CHAR) FROM {}.schema_migrations \
             WHERE version = '{version}'",
            quote_ident(&cfg.pg.meta_schema),
        ),
    )
    .await
}

/// **The state both sibling legs construct is UNREACHABLE here**, and this test is
/// the measurement that says so rather than a sentence in a comment.
///
/// Every route to a counter below `MAX(column)` that MySQL offers is tried, and each
/// one is shown to land at or above `MAX + 1`. If a future MySQL - or a future
/// storage engine - ever does allow a behind counter, this test FAILS, which is the
/// signal to give the MySQL leg the same behind-generator test the other two have.
/// It is written to fail in that direction on purpose: a claim about what cannot
/// happen is only worth keeping if something checks it.
#[compio::test]
async fn the_counter_cannot_be_driven_below_the_live_maximum() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = database_token("ident_floor");
    let cfg = cfg_for(&database);
    let _guard = setup(&session, &database, &cfg, true, INCREMENT).await;
    let table = format!("{}.{}", quote_ident(&database), quote_ident(TABLE));
    let column = quote_ident(COLUMN);

    // 1. An explicit import advances the counter to MAX + 1, not to the session
    //    increment's lattice: the increment is a SESSION allocation rule, not a
    //    property of the stored counter.
    import(&session, &database, 10).await;
    assert_eq!(live_counter(&session, &database).await, Some(11));

    // 2. The SQLite leg's trick. `sqlite_sequence.seq` tracks INSERTs and ignores a
    //    later UPDATE of the rowid alias; InnoDB does NOT ignore it.
    exec(
        &session,
        &format!("UPDATE {table} SET {column} = 50 WHERE {column} = 10"),
    )
    .await;
    assert_eq!(maximum(&session, &database).await, Some(50));
    assert_eq!(
        live_counter(&session, &database).await,
        Some(51),
        "a raising UPDATE advances the InnoDB counter, so the SQLite leg's \
         behind-generator fixture has no MySQL equivalent"
    );

    // 3. The only writable handle on the counter is clamped upward.
    exec(
        &session,
        &format!("ALTER TABLE {table} AUTO_INCREMENT = 20"),
    )
    .await;
    assert_eq!(
        live_counter(&session, &database).await,
        Some(51),
        "ALTER cannot seat the counter at or below the live maximum"
    );

    // 4. But it CAN lower a counter that stays above the maximum - which is why the
    //    engine's own `desired <= current` guard is load-bearing rather than
    //    belt-and-braces. Nothing in MySQL would have refused the backward jump.
    exec(
        &session,
        &format!("ALTER TABLE {table} AUTO_INCREMENT = 200"),
    )
    .await;
    exec(
        &session,
        &format!("ALTER TABLE {table} AUTO_INCREMENT = 60"),
    )
    .await;
    assert_eq!(
        live_counter(&session, &database).await,
        Some(60),
        "MySQL accepts a lowering ALTER above the maximum; only the engine refuses it"
    );

    // 5. Adding AUTO_INCREMENT to a column that already holds rows seats the counter
    //    above them, so an import that generates the column afterwards is not behind
    //    either.
    let db = quote_ident(&database);
    exec(
        &session,
        &format!("CREATE TABLE {db}.late (n BIGINT NOT NULL PRIMARY KEY)"),
    )
    .await;
    exec(&session, &format!("INSERT INTO {db}.late (n) VALUES (77)")).await;
    exec(
        &session,
        &format!("ALTER TABLE {db}.late MODIFY n BIGINT NOT NULL AUTO_INCREMENT"),
    )
    .await;
    exec(&session, "SET SESSION information_schema_stats_expiry = 0").await;
    assert_eq!(
        counter_read(&session, &database, "late").await,
        Some(78),
        "a late-added AUTO_INCREMENT starts above the rows already present"
    );
}

/// The one configuration in which this operation emits DDL on MySQL: a NON-unit
/// session increment, where the required floor `MAX + increment` is genuinely ahead
/// of the counter an import leaves behind.
///
/// The counterpart of
/// `advances_an_uncalled_identity_sequence_by_its_non_unit_increment` in the
/// PostgreSQL leg. `50 + 5` is the assertion that matters: a `MAX + 1` render would
/// leave 51 here and pass every other check in this file.
#[compio::test]
async fn a_counter_short_of_the_required_floor_is_raised_by_the_session_increment() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = database_token("ident_raise");
    let cfg = cfg_for(&database);
    let _guard = setup(&session, &database, &cfg, true, INCREMENT).await;

    import(&session, &database, 50).await;
    assert_eq!(maximum(&session, &database).await, Some(50));
    assert_eq!(
        live_counter(&session, &database).await,
        Some(51),
        "the post-import counter, one past the maximum and short of the floor"
    );

    let sync = step(&database, "synchronize_order_key_raise");
    assert!(run(&session, &cfg, &sync)
        .await
        .expect("synchronize a counter short of the required floor"));
    assert_eq!(
        live_counter(&session, &database).await,
        Some(50 + INCREMENT),
        "the emitted ALTER carries MAX + @@SESSION.auto_increment_increment; \
         a MAX + 1 render would leave 51"
    );

    assert!(
        !run(&session, &cfg, &sync)
            .await
            .expect("a completed synchronization is an idempotent skip"),
        "the second run must report that it did nothing"
    );
    assert_eq!(
        live_counter(&session, &database).await,
        Some(50 + INCREMENT),
        "the idempotent skip touches the allocator not at all"
    );
    assert_eq!(
        journal_rows(&session, &cfg, &sync).await,
        Some(1),
        "the no-op retry writes no duplicate journal event"
    );

    // The allocator's own answer, which no catalog cache can colour: the first value
    // at or past the new floor that MySQL's increment/offset lattice admits.
    assert_eq!(
        allocate(&session, &database).await,
        Some(56),
        "the next generated identity clears every imported row"
    );
}

/// **On a stock single-writer MySQL this operation is unconditionally a no-op**, and
/// that is a coverage fact worth an executable claim.
///
/// At `auto_increment_increment = 1` the required floor `MAX + 1` is exactly the
/// counter an import already leaves, and
/// [`the_counter_cannot_be_driven_below_the_live_maximum`] shows the counter cannot
/// start lower. So the `ALTER` branch is unreachable on the configuration nearly
/// every MySQL deployment runs, and everything the operation does there is the
/// journal event. Anyone reading the passing MySQL row in
/// `dialect_conformance_live.rs` should read it in that light.
#[compio::test]
async fn on_a_stock_single_writer_mysql_the_operation_can_never_emit_ddl() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = database_token("ident_stock");
    let cfg = cfg_for(&database);
    let _guard = setup(&session, &database, &cfg, true, STOCK_INCREMENT).await;

    import(&session, &database, 50).await;
    assert_eq!(live_counter(&session, &database).await, Some(51));

    let sync = step(&database, "synchronize_order_key_stock");
    assert!(run(&session, &cfg, &sync)
        .await
        .expect("the stock-increment case is a journaled no-op"));
    assert_eq!(
        live_counter(&session, &database).await,
        Some(51),
        "MAX + 1 equals the live counter, so there is nothing to raise"
    );
    assert_eq!(journal_rows(&session, &cfg, &sync).await, Some(1));
    assert_eq!(
        allocate(&session, &database).await,
        Some(51),
        "and allocation was already collision-free without the operation"
    );
}

/// A counter already past the required floor is never pulled back to it.
///
/// The mirror of `never_moves_an_already_ahead_sequence_backward` in the PostgreSQL
/// leg, reached through MySQL's own physics: an INSERT of 100 advances the counter to
/// 101, and DELETEing that row does not lower it. The floor here is `10 + 5 = 15`,
/// and MySQL would have ACCEPTED an `ALTER … AUTO_INCREMENT = 15` - measured in
/// [`the_counter_cannot_be_driven_below_the_live_maximum`] - so the engine's guard is
/// the only thing preventing 86 already-issued identities from being reissued.
#[compio::test]
async fn an_already_ahead_counter_is_never_lowered_to_the_required_floor() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = database_token("ident_ahead");
    let cfg = cfg_for(&database);
    let _guard = setup(&session, &database, &cfg, true, INCREMENT).await;
    let table = format!("{}.{}", quote_ident(&database), quote_ident(TABLE));
    let column = quote_ident(COLUMN);

    import(&session, &database, 100).await;
    exec(
        &session,
        &format!("DELETE FROM {table} WHERE {column} = 100"),
    )
    .await;
    import(&session, &database, 10).await;
    assert_eq!(live_counter(&session, &database).await, Some(101));
    assert_eq!(maximum(&session, &database).await, Some(10));

    let sync = step(&database, "synchronize_order_key_ahead");
    assert!(run(&session, &cfg, &sync)
        .await
        .expect("an ahead allocator is a journaled no-op, not a refusal"));
    assert_eq!(
        live_counter(&session, &database).await,
        Some(101),
        "the floor is 15 here, and an ALTER to 15 would be a silent reissue of \
         identities this table has already handed out"
    );
    assert_eq!(journal_rows(&session, &cfg, &sync).await, Some(1));
    assert_eq!(allocate(&session, &database).await, Some(101));
}

/// The monotonicity guarantee survives a STALE catalog, which is the one thing the
/// `RecordingSession` unit tests in `identity_sql.rs` cannot ask.
///
/// `information_schema.TABLES.AUTO_INCREMENT` is served from a server-wide statistics
/// cache. This test primes that cache at 11, moves the TRUE counter to 1001, and
/// drops the live maximum back to 10. The engine must still see 1001 and refuse to
/// move, because the floor is 15:
///
/// - with `information_schema_stats_expiry = 0` pinned: reads 1001, emits nothing.
/// - without it: reads the cached 11, decides 15 > 11, and emits
///   ``ALTER TABLE `db`.`order` AUTO_INCREMENT = 15`` - which MySQL ACCEPTS, since 15
///   clears the live maximum of 10 - dropping the allocator 986 values BACKWARD onto
///   identities the table has already issued.
///
/// A fake session's canned row is fresh by construction, so it cannot tell those two
/// engines apart. This is the test that fails when the pin is deleted.
#[compio::test]
async fn a_stale_catalog_counter_never_moves_the_allocator_backward() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = database_token("ident_stale");
    let cfg = cfg_for(&database);
    let _guard = setup(&session, &database, &cfg, true, INCREMENT).await;
    let table = format!("{}.{}", quote_ident(&database), quote_ident(TABLE));
    let column = quote_ident(COLUMN);

    import(&session, &database, 10).await;
    assert_eq!(
        prime_cached_counter(&session, &database).await,
        Some(11),
        "the cache is primed at the counter's value BEFORE it moves"
    );
    import(&session, &database, 1000).await;
    exec(
        &session,
        &format!("DELETE FROM {table} WHERE {column} = 1000"),
    )
    .await;
    assert_eq!(maximum(&session, &database).await, Some(10));
    assert_eq!(
        live_counter(&session, &database).await,
        Some(1001),
        "the TRUE counter is far ahead of the live maximum"
    );
    assert_eq!(
        prime_cached_counter(&session, &database).await,
        Some(11),
        "and the cached answer is still the stale one - a bypassing read does not \
         refresh it, so the trap is armed for the engine's own read"
    );

    let sync = step(&database, "synchronize_order_key_stale_catalog");
    assert!(run(&session, &cfg, &sync)
        .await
        .expect("a stale catalog is still a journaled no-op"));
    assert_eq!(
        live_counter(&session, &database).await,
        Some(1001),
        "the allocator must not move; reading the cached 11 would have emitted \
         AUTO_INCREMENT = 15 and reissued 986 identities"
    );
    assert_eq!(
        allocate(&session, &database).await,
        Some(1001),
        "and the next generated identity is still past everything ever issued"
    );
}

/// An EMPTY table is a journaled no-op, and this is the ONLY branch the MySQL leg of
/// `dialect_conformance_live.rs` reaches.
///
/// That suite's `synchronizeIdentity` prelude creates its table and inserts no rows,
/// so `MAX(column)` is NULL and `resolve_counter_advance` returns before it compares
/// anything. Its `Applied` verdict is therefore evidence that the operation is
/// EXECUTABLE on MySQL and no evidence at all about the counter advance. Pinned here
/// so the distinction is a test rather than a claim in a comment.
#[compio::test]
async fn an_empty_table_is_a_journaled_no_op_which_is_the_path_conformance_rides() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = database_token("ident_empty");
    let cfg = cfg_for(&database);
    let _guard = setup(&session, &database, &cfg, true, INCREMENT).await;
    assert_eq!(maximum(&session, &database).await, None);
    let before = live_counter(&session, &database).await;
    assert_eq!(before, Some(1));

    let sync = step(&database, "synchronize_order_key_empty");
    assert!(run(&session, &cfg, &sync)
        .await
        .expect("an empty table synchronizes as a journaled no-op"));
    assert_eq!(
        live_counter(&session, &database).await,
        before,
        "a NULL maximum must emit no ALTER at all"
    );
    assert_eq!(
        journal_rows(&session, &cfg, &sync).await,
        Some(1),
        "and it is still a journaled event, which is why conformance sees Applied"
    );
}

/// A column that is not `AUTO_INCREMENT` is refused before any journal row is
/// written, and the refusal releases the engine's explicit table lock FOR REAL.
///
/// The lock half is the live-only part. `identity_sql.rs`'s fake asserts that the
/// string `UNLOCK TABLES` reached its log; only a server can be asked whether the
/// session is still inside `LOCK TABLES`, and it answers by refusing any table the
/// lock did not name. A retained lock would leave every later statement on this
/// pinned connection failing, which is the shape of production outage the release
/// path exists to prevent.
#[compio::test]
async fn a_non_auto_increment_column_is_rejected_and_leaks_no_table_lock() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = database_token("ident_plain");
    let cfg = cfg_for(&database);
    let _guard = setup(&session, &database, &cfg, false, INCREMENT).await;
    exec(
        &session,
        &format!(
            "CREATE TABLE {}.witness (n BIGINT NOT NULL PRIMARY KEY)",
            quote_ident(&database)
        ),
    )
    .await;

    let sync = step(&database, "synchronize_order_key_plain");
    let error = run(&session, &cfg, &sync)
        .await
        .expect_err("a plain BIGINT column must be refused");
    let message = error.to_string();
    assert!(
        message.contains("mysql synchronizeIdentity")
            && message.contains("target column is not AUTO_INCREMENT"),
        "the refusal names the operation and the reason: {message}"
    );
    assert_eq!(
        journal_rows(&session, &cfg, &sync).await,
        Some(0),
        "a refused validation writes no journal event"
    );

    // The oracle for the lock: `witness` was never named by the engine's
    // `LOCK TABLES`, so a session still holding that lock cannot read it.
    session
        .query(
            &format!("SELECT n FROM {}.witness", quote_ident(&database)),
            &[],
        )
        .await
        .expect("the failed synchronization released its explicit table lock");
}

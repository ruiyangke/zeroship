//! The live-database preflight against a REAL `PostgreSQL`.
//!
//! WHAT THE UNIT TESTS CANNOT SETTLE. `live_db`'s unit tests prove the SHAPE of
//! a refusal -- that it names the database, the gap and the remedy, and that the
//! password does not travel with it. They cannot prove the preflight
//! DISCRIMINATES, because a preflight that refused everything would produce
//! exactly the same strings.
//!
//! SO EVERY CASE HERE IS A PAIR DIFFERING IN ONE VARIABLE.
//!
//!   same database, different schema asked for   -> Ready / Refused
//!   same schema asked for, different database   -> Ready / Refused
//!   same database and schemas, journal short    -> Refused / Ready
//!
//! Without both directions the file is worthless in the specific way the thing
//! it guards was worthless: a check that always says the same thing reads as a
//! check.
//!
//! THIS FILE TAKES THE SERVER FROM THE OVERLAY AND THE DATABASES FROM ITSELF,
//! AND THE SPLIT IS THE POINT. READ THIS BEFORE ADDING AN ARM.
//!
//! The first version of this file did not make that split. Its `Ready` side was
//! `inspect(&loaded.dsn, ..)` -- the overlay's OWN database -- under a comment
//! that stated the premise out loud: "The overlay's database is migrated by the
//! suite gates and answers `Ready`."
//!
//! That premise is precisely the thing this whole change exists because it is
//! FALSE. The overlay on the machine this was written on named the shared
//! `zeroship` database on :5440, which by then held none of `zeroship`,
//! `zeroship_migrations` or `service_authn` and 84 `cpg_*` schemas left by
//! another suite. So the arm asserted that the defect it was written to detect
//! was absent. It passed only because the author's overlay happened, for the
//! length of that run, to name a database they had created and migrated by
//! hand; it was RED for the reviewer within the hour, and would have been RED
//! on main for every developer here.
//!
//! It fails in the exact family this module's own header warns about: a check
//! bound to something other than what it is checking. And it fails INVISIBLY in
//! CI, because with no overlay at all `server()` returns `None` and the file
//! skips -- green forever centrally, red on every desk.
//!
//! So: the overlay supplies HOST, PORT, USER, PASSWORD and nothing else. Its
//! `database` field is read by nothing here. Every database an arm rules on is
//! created by that arm, populated by that arm, and dropped by that arm. `Ready`
//! is then CAUSED by a `CREATE SCHEMA` this file issued, rather than assumed
//! from the state of a database the tree does not own and cannot migrate.
//!
//! WHY NOT MIGRATE A SCRATCH DATABASE FOR THE `Ready` SIDE. `CREATE SCHEMA
//! zeroship` is not a migration and this file does not pretend it is -- the
//! preflight under test asks "does this schema exist", so a bare `CREATE
//! SCHEMA` is exactly the input that exercises it. Running the real
//! `zeroship-platform-migrate` here would cost a V8 boot per arm and would
//! drag in the cluster-wide role provisioning, whose advisory lock exists
//! because concurrent runs collide on `pg_authid`. That is a large, slow,
//! shared-state dependency bought for a property this file does not test.
//!
//! NOTHING HERE USES `WITH (FORCE)`. The databases are this run's own, so a
//! plain `DROP` is enough once the connections are closed -- and `drop_scratch`
//! closes them rather than assuming. FORCE terminates every backend on a
//! database, and a file that reaches for it habitually is one edit away from
//! doing that to a peer.

use std::path::{Path, PathBuf};

use compio_postgres::NoTls;
use zeroship_testkit::{admin, live_db, overlay};

/// The schemas a platform live-DB target requires, and what this file uses as
/// its `Ready` input.
///
/// IT IS THE PRODUCTION CONSTANT, NOT A COPY OF IT. This file used to spell the
/// pair out beside a comment asking that it be "kept identical" to
/// `crates/zeroship-control/tests/common/mod.rs`, which is a convention rather
/// than a mechanism: the day that list grew, this file would have gone on
/// testing the old one and reporting green.
const PLATFORM_SCHEMAS: &[&str] = live_db::PLATFORM_SCHEMAS;

/// A schema name nothing in this tree ever creates. The `Refused` input.
const NEVER_CREATED: &str = "zs_schema_that_is_never_created";

/// The journal table the ledger stage counts, in the shape that stage reads.
///
/// TWO COLUMNS, NOT TWELVE. The real table (see
/// `db/migrations-ts/`) carries an event sequence, immutability triggers and a
/// shape CHECK, none of which the preflight looks at. Reproducing them here
/// would pin this file to a schema it does not test and would go stale the
/// first time the journal grows a column.
const JOURNAL_TABLE: &str = "zeroship_migrations.__zeroship_schema_migrations";

/// How many migration files this checkout carries, which is what a current
/// journal must have consumed.
///
/// Re-derived from the tree on every run through the SAME enumeration the
/// preflight uses, so the two cannot drift apart and neither is written down.
fn migrations_carried() -> usize {
    zeroship_testkit::fingerprint::files_in(&repo_root())
        .expect("this checkout carries a migration set")
        .len()
}

/// Give `dsn` a journal that has consumed `consumed` distinct migrations.
///
/// The preflight counts DISTINCT `checksum` values on `applied` rows, so the
/// rows carry distinct checksums and nothing else that matters. Two rows per
/// migration, because one migration journals many step events in the real
/// corpus and a check that counted ROWS rather than distinct checksums would
/// pass here for the wrong reason.
fn seed_journal(dsn: &str, consumed: usize) {
    let mut sql = format!(
        "CREATE TABLE IF NOT EXISTS {JOURNAL_TABLE} (event_kind text NOT NULL, checksum text NOT NULL);"
    );
    for index in 0..consumed {
        sql.push_str(&format!(
            "INSERT INTO {JOURNAL_TABLE} (event_kind, checksum) \
             VALUES ('applied', 'checksum-{index}'), ('applied', 'checksum-{index}');"
        ));
    }
    run(dsn, &sql).unwrap_or_else(|e| panic!("seed a journal of {consumed} migrations: {e}"));
}

fn repo_root() -> PathBuf {
    // Substituted at COMPILE time by `env!`, so this is a constant in the
    // binary rather than a read of the process environment.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// The SERVER the overlay names. Its database is deliberately not returned.
///
/// The announcement is the marker `tests/lib/skip_census.sh` counts, so a green
/// tally cannot hide a run that exercised nothing.
fn server() -> Option<admin::Server> {
    let Ok(loaded) = overlay::load(&repo_root()) else {
        zeroship_test_support::skip(
            "no deploy/ops/zeroship.test.toml; run tests/provision_test_backends.sh",
        );
        return None;
    };
    let server = admin::Server::from_overlay(&loaded).ok()?;
    let mut probe = admin::PgAdmin::new(server.clone());
    if admin::DbAdmin::exists(&mut probe, "postgres").is_err() {
        zeroship_test_support::skip("the overlay's PostgreSQL is not reachable");
        return None;
    }
    Some(server)
}

/// A name no other run can collide with, and short enough for the 63-byte limit.
fn scratch_name(tag: &str) -> String {
    format!(
        "zs_preflight_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after the epoch")
            .subsec_nanos()
    )
}

/// A DSN for `database` on this server, with no database name borrowed from
/// anywhere.
fn dsn_for(server: &admin::Server, database: &str) -> String {
    let userinfo = if server.user.is_empty() {
        String::new()
    } else if server.pass.is_empty() {
        format!("{}@", server.user)
    } else {
        format!("{}:{}@", server.user, server.pass)
    };
    format!(
        "postgres://{userinfo}{}:{}/{database}",
        server.host, server.port
    )
}

/// A database this test created, which it drops on the way out.
struct Scratch {
    name: String,
    dsn: String,
}

/// Create a scratch database and, inside it, each schema in `schemas`.
///
/// An EMPTY `schemas` is the `Refused` input and a populated one is the `Ready`
/// input, so the two sides of an arm differ by this argument and by nothing
/// else.
fn scratch(server: &admin::Server, tag: &str, schemas: &[&str]) -> Scratch {
    let name = scratch_name(tag);
    let mut pg = admin::PgAdmin::new(server.clone());
    admin::DbAdmin::create(&mut pg, &name).expect("create the scratch database");
    let dsn = dsn_for(server, &name);

    if !schemas.is_empty() {
        // Identifiers, so they cannot travel as parameters. They are literals
        // in this file, and `"` is doubled anyway rather than trusted.
        let sql = schemas
            .iter()
            .map(|s| format!("CREATE SCHEMA \"{}\";", s.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        run(&dsn, &sql).unwrap_or_else(|e| panic!("seed {name} with {schemas:?}: {e}"));
    }

    Scratch { name, dsn }
}

/// Run one statement batch and CLOSE THE CONNECTION before returning.
///
/// The close is not tidiness. `drop_scratch` issues a plain `DROP DATABASE`,
/// which `PostgreSQL` refuses while any session is still attached -- including
/// this test's own, whose socket is owned by a detached driver task that only
/// shuts down once the client is dropped AND the driver is driven to
/// completion. Detaching the driver here would leave the database undroppable
/// without `WITH (FORCE)`, which is the thing this file will not do.
fn run(dsn: &str, sql: &str) -> Result<(), String> {
    let dsn = dsn.to_string();
    let sql = sql.to_string();
    compio::runtime::Runtime::new()
        .map_err(|e| format!("io_uring runtime: {e}"))?
        .block_on(async move {
            let (client, connection) = compio_postgres::connect(&dsn, NoTls)
                .await
                .map_err(|e| format!("connect: {e}"))?;
            let driver = compio::runtime::spawn(async move {
                let _ = connection.run().await;
            });
            let result = client.simple_query(&sql).await;
            drop(client);
            let _ = driver.await;
            result.map(|_| ()).map_err(|e| e.to_string())
        })
}

/// Drop a database this test created. PLAIN `DROP`, never `WITH (FORCE)`.
///
/// FORCE terminates every backend on the database first, so it always succeeds
/// -- including when the session it kills belongs to someone else. Nothing here
/// needs that: every connection this file opens is closed before it returns
/// (see [`run`] and `live_db::inspect`). The retry covers the remaining case,
/// which is the kernel not having reaped the socket yet; if it still will not
/// go, the name is PRINTED rather than swallowed, so the sweeper has something
/// to find.
fn drop_scratch(server: &admin::Server, scratch: &Scratch) {
    let maintenance = dsn_for(server, "postgres");
    let sql = format!("DROP DATABASE IF EXISTS {}", scratch.name);
    for attempt in 0..10 {
        match run(&maintenance, &sql) {
            Ok(()) => return,
            Err(error) if attempt == 9 => {
                eprintln!(
                    "LEAKED: could not drop {}: {error}\n\
                     Sweep it with: psql -d postgres -c 'DROP DATABASE {}'",
                    scratch.name, scratch.name
                );
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
}

/// ARM 1 -- the schema axis. ONE database this test built, two questions.
///
/// The database is created WITH the platform schemas, so the `Ready` side is
/// caused by a `CREATE SCHEMA` issued four lines earlier rather than by the
/// state of anything external. The `Refused` side asks for a name this tree
/// never creates, against that SAME database and that same connection.
///
/// This is the arm that would have caught the 2026-08-21 run: the shared
/// `zeroship` database was reachable and answered every connection, and the
/// only thing wrong with it was that the platform schema had been dropped out
/// of it.
///
/// THE `Ready` SIDE ALSO SEEDS A JOURNAL, because asking for the journal SCHEMA
/// now also asks whether that journal is current. A bare `CREATE SCHEMA
/// zeroship_migrations` is a schema with no journal in it, which is behind the
/// tree by every migration there is -- correctly refused, and not what this arm
/// is about.
#[test]
fn one_database_answers_ready_for_a_schema_it_has_and_refuses_for_one_it_does_not() {
    let Some(server) = server() else { return };
    let db = scratch(&server, "schema_axis", PLATFORM_SCHEMAS);
    seed_journal(&db.dsn, migrations_carried());

    let present = live_db::inspect(&db.dsn, PLATFORM_SCHEMAS);
    let absent = live_db::inspect(&db.dsn, &[NEVER_CREATED]);

    drop_scratch(&server, &db);

    assert!(
        present.is_ready(),
        "this test created {:?} in {}; the preflight refused anyway: {}",
        PLATFORM_SCHEMAS,
        db.name,
        present.refusal().unwrap_or("")
    );
    let text = absent
        .refusal()
        .expect("no database holds a schema this tree never creates");
    assert!(
        text.contains(&format!("/{}", db.name)),
        "the refusal must name the database it dialled; got {text}"
    );
    assert!(
        text.contains(&format!("no schema \"{NEVER_CREATED}\"")),
        "the refusal must name what was missing; got {text}"
    );
    assert!(
        text.contains("db-migrate.sh"),
        "the refusal must name the remediation; got {text}"
    );
}

/// ARM 2 -- the database axis. ONE question, two databases this test built.
///
/// Both are created by this arm, seconds apart, on the same server with the
/// same credentials. They differ in ONE thing: one had `CREATE SCHEMA` run in
/// it and the other did not. So a `Ready` here can only have come from the
/// schema being present.
///
/// THE `Ready` SIDE USED TO BE THE OVERLAY'S DATABASE. See this file's header
/// for why that made the arm assert the absence of the defect it exists to
/// detect. Do not reintroduce it: any arm whose expected answer depends on the
/// state of a database this tree neither creates nor migrates is not testing
/// the preflight, it is testing the machine.
#[test]
fn the_same_question_answers_ready_on_a_seeded_database_and_refuses_on_a_fresh_one() {
    let Some(server) = server() else { return };
    let seeded = scratch(&server, "db_axis_seeded", PLATFORM_SCHEMAS);
    seed_journal(&seeded.dsn, migrations_carried());
    let fresh = scratch(&server, "db_axis_fresh", &[]);

    let on_seeded = live_db::inspect(&seeded.dsn, PLATFORM_SCHEMAS);
    let on_fresh = live_db::inspect(&fresh.dsn, PLATFORM_SCHEMAS);

    drop_scratch(&server, &seeded);
    drop_scratch(&server, &fresh);

    assert!(
        on_seeded.is_ready(),
        "{} was created WITH {:?}; the preflight refused it: {}",
        seeded.name,
        PLATFORM_SCHEMAS,
        on_seeded.refusal().unwrap_or("")
    );
    let text = on_fresh
        .refusal()
        .unwrap_or_else(|| panic!("{} was created empty and cannot hold {PLATFORM_SCHEMAS:?}", fresh.name));
    assert!(
        text.contains(&format!("/{}", fresh.name)),
        "the refusal must name the database it dialled; got {text}"
    );
}

/// ARM 3 -- a server that is not there must refuse with the UNREACHABLE remedy,
/// not the unmigrated one. The two send a reader to different places.
///
/// Self-contained: it needs no server, no overlay and no database, so it runs
/// on every machine including a checkout that has never been provisioned.
#[test]
fn an_unreachable_server_refuses_differently_from_an_unmigrated_database() {
    // Port 1 is reserved (tcpmux) and nothing in this tree binds it, so the
    // connect fails without a timeout worth waiting on.
    let dead = "postgres://postgres:zeroship@127.0.0.1:1/zeroship";
    let text = live_db::inspect(dead, PLATFORM_SCHEMAS)
        .refusal()
        .expect("nothing listens on port 1")
        .to_string();
    assert!(
        text.contains("did not answer"),
        "an unreachable server must say so; got {text}"
    );
    assert!(
        !text.contains("db-migrate.sh"),
        "an unreachable server must not be blamed on a missing migration; got {text}"
    );
}

/// ARM 4 -- the LEDGER axis. ONE database, one question, two journals differing
/// by one migration.
///
/// This is the arm that would have caught the 2026-09-07 run. That database
/// held both platform schemas and a populated journal; the only thing wrong
/// with it was that the journal had never seen
/// `db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts`,
/// so `zeroship.apps` had no `organization_id` and seven targets across two
/// crates reported a missing column as a test failure.
///
/// SAME DATABASE FOR BOTH SIDES, and the short journal is measured FIRST. A
/// second database would differ in its name and its creation time as well as
/// its journal; topping the journal up in place leaves exactly one variable.
#[test]
fn one_database_refuses_on_a_short_journal_and_is_ready_once_it_is_topped_up() {
    let Some(server) = server() else { return };
    let carried = migrations_carried();
    let db = scratch(&server, "ledger_axis", PLATFORM_SCHEMAS);

    seed_journal(&db.dsn, carried - 1);
    let behind = live_db::inspect(&db.dsn, PLATFORM_SCHEMAS);
    seed_journal(&db.dsn, carried);
    let current = live_db::inspect(&db.dsn, PLATFORM_SCHEMAS);

    drop_scratch(&server, &db);

    let text = behind
        .refusal()
        .unwrap_or_else(|| panic!("{} was one migration short of the tree", db.name));
    assert!(
        text.contains("BEHIND the tree"),
        "the refusal must say which side is short; got {text}"
    );
    assert!(
        text.contains(&format!("/{}", db.name)),
        "the refusal must name the database it dialled; got {text}"
    );
    assert!(
        text.contains("db-migrate.sh"),
        "the refusal must name the applier to run; got {text}"
    );
    assert!(
        current.is_ready(),
        "the same database with a complete journal must be ready; got {}",
        current.refusal().unwrap_or("")
    );
}

/// ARM 5 -- the ledger stage is KEYED to the journal schema, live.
///
/// A caller that does not name `zeroship_migrations` is not claiming to need
/// the platform corpus. The same database that ARM 4 refuses for the platform
/// question must answer `Ready` to a question that never mentioned the journal,
/// or every non-platform live-DB target in the workspace inherits a refusal
/// about a corpus it does not use.
#[test]
fn a_caller_that_never_asked_for_the_journal_is_not_judged_on_it() {
    let Some(server) = server() else { return };
    let db = scratch(&server, "ledger_switch", PLATFORM_SCHEMAS);
    seed_journal(&db.dsn, 0);

    let with_journal = live_db::inspect(&db.dsn, PLATFORM_SCHEMAS);
    let without_journal = live_db::inspect(&db.dsn, &["zeroship"]);

    drop_scratch(&server, &db);

    assert!(
        with_journal.refusal().is_some(),
        "an empty journal is behind every checkout that carries migrations"
    );
    assert!(
        without_journal.is_ready(),
        "asking only for \"zeroship\" must not drag in the ledger stage; got {}",
        without_journal.refusal().unwrap_or("")
    );
}

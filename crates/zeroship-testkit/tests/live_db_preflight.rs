//! The live-database preflight against a REAL PostgreSQL.
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
//!
//! Without both directions the file is worthless in the specific way the thing
//! it guards was worthless: a check that always says the same thing reads as a
//! check.
//!
//! WHY THE UNMIGRATED SIDE IS A DATABASE THIS FILE CREATES. The obvious
//! candidate was the `postgres` maintenance database -- guaranteed to exist,
//! needs no setup, and "obviously" carries no platform schema. It does. Checked
//! 2026-08-21 on the shared :5440 cluster, `postgres` holds a `zeroship` schema,
//! put there by the live targets that default their DSN to `dbname=postgres`
//! (`crates/migrated/tests/health_endpoints_test.rs`). A control chosen because
//! it "obviously" lacks something is not a control; a freshly created database
//! provably lacks it. The scratch database is dropped at the end of the test.

use std::path::{Path, PathBuf};

use compio_postgres::NoTls;
use zeroship_testkit::{admin, live_db, overlay};

fn repo_root() -> PathBuf {
    // Substituted at COMPILE time by `env!`, so this is a constant in the
    // binary rather than a read of the process environment.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// The overlay names the server; without one there is nothing to dial.
///
/// The announcement is the marker `tests/lib/skip_census.sh` counts, so a green
/// tally cannot hide a run that exercised nothing.
fn server() -> Option<(overlay::Loaded, admin::Server)> {
    let loaded = match overlay::load(&repo_root()) {
        Ok(loaded) => loaded,
        Err(_) => {
            zeroship_test_support::skip(
                "no deploy/ops/zeroship.test.toml; run tests/provision_test_backends.sh",
            );
            return None;
        }
    };
    let server = admin::Server::from_overlay(&loaded).ok()?;
    let mut probe = admin::PgAdmin::new(server.clone());
    if admin::DbAdmin::exists(&mut probe, "postgres").is_err() {
        zeroship_test_support::skip("the overlay's PostgreSQL is not reachable");
        return None;
    }
    Some((loaded, server))
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

/// The overlay's DSN with a different database name, and nothing else changed.
fn with_database(dsn: &str, database: &str) -> String {
    let parts = overlay::split_dsn(dsn);
    let user = if parts.pass.is_empty() {
        parts.user.clone()
    } else {
        format!("{}:{}", parts.user, parts.pass)
    };
    let authority = if user.is_empty() {
        format!("{}:{}", parts.host, parts.port)
    } else {
        format!("{user}@{}:{}", parts.host, parts.port)
    };
    format!("postgres://{authority}/{database}")
}

/// Drop a database this test created.
///
/// `WITH (FORCE)` IS CORRECT HERE AND WRONG IN THE SWEEPER, and the difference
/// is whose connections it terminates. This database was created by this test
/// moments ago; the only sessions on it are this test's own, which the driver
/// may not have torn down yet. The sweeper drops databases OTHER runs created,
/// where a live session is a peer agent mid-suite -- see
/// `crates/zeroship-testkit/src/sweep.rs`.
fn drop_scratch(server: &admin::Server, name: &str) {
    let mut config = compio_postgres::Config::new();
    config
        .host(&server.host)
        .port(server.port)
        .dbname("postgres");
    if !server.user.is_empty() {
        config.user(&server.user);
    }
    if !server.pass.is_empty() {
        config.password(&server.pass);
    }
    let sql = format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)");
    let _ = compio::runtime::Runtime::new()
        .expect("io_uring runtime")
        .block_on(async move {
            let (client, connection) = config.connect(NoTls).await?;
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            client.simple_query(&sql).await
        });
}

/// ARM 1 -- the schema axis. One database, two questions.
///
/// This is the arm that would have caught the 2026-08-21 run: the shared
/// `zeroship` database was reachable and answered every connection, and the
/// only thing wrong with it was that the platform schema had been dropped out
/// of it.
#[test]
fn one_database_answers_ready_for_a_schema_it_has_and_refuses_for_one_it_does_not() {
    let Some((loaded, server)) = server() else {
        return;
    };
    let name = scratch_name("schema_axis");
    let mut pg = admin::PgAdmin::new(server.clone());
    admin::DbAdmin::create(&mut pg, &name).expect("create the scratch database");
    let dsn = with_database(&loaded.dsn, &name);

    // `public` exists in a freshly created database. This half is what stops a
    // preflight that refuses everything from passing this file.
    let present = live_db::inspect(&dsn, &["public"]);
    let absent = live_db::inspect(&dsn, &["zeroship"]);

    drop_scratch(&server, &name);

    assert!(
        present.is_ready(),
        "a fresh database has `public`; the preflight refused it: {:?}",
        present.refusal()
    );
    let text = absent
        .refusal()
        .expect("a fresh database carries no platform schema");
    assert!(
        text.contains(&format!("/{name}")),
        "the refusal must name the database it dialled; got {text}"
    );
    assert!(
        text.contains("no schema \"zeroship\""),
        "the refusal must name what was missing; got {text}"
    );
    assert!(
        text.contains("zeroship-platform-migrate"),
        "the refusal must name the remediation; got {text}"
    );
}

/// ARM 2 -- the database axis. One question, two databases.
///
/// The overlay's database is migrated by the suite gates and answers `Ready`
/// for the platform schema; a database created seconds ago does not. The
/// question is IDENTICAL on both sides, so a `Ready` here can only have come
/// from the database.
#[test]
fn the_same_question_answers_ready_on_a_migrated_database_and_refuses_on_a_fresh_one() {
    let Some((loaded, server)) = server() else {
        return;
    };
    let name = scratch_name("db_axis");
    let mut pg = admin::PgAdmin::new(server.clone());
    admin::DbAdmin::create(&mut pg, &name).expect("create the scratch database");

    let on_fresh = live_db::inspect(&with_database(&loaded.dsn, &name), &["zeroship"]);
    let on_overlay = live_db::inspect(&loaded.dsn, &["zeroship"]);

    drop_scratch(&server, &name);

    assert!(
        on_fresh.refusal().is_some(),
        "a database created seconds ago cannot hold the platform schema"
    );
    assert!(
        on_overlay.is_ready(),
        "the overlay must name a MIGRATED database. It names {}, and the \
         preflight says: {}",
        loaded.db,
        on_overlay.refusal().unwrap_or("")
    );
}

/// A server that is not there must refuse with the UNREACHABLE remedy, not with
/// the unmigrated one. The two send a reader to different places.
#[test]
fn an_unreachable_server_refuses_differently_from_an_unmigrated_database() {
    // Port 1 is reserved (tcpmux) and nothing in this tree binds it, so the
    // connect fails without a timeout worth waiting on.
    let dead = "postgres://postgres:zeroship@127.0.0.1:1/zeroship";
    let text = live_db::inspect(dead, &["zeroship"])
        .refusal()
        .expect("nothing listens on port 1")
        .to_string();
    assert!(
        text.contains("did not answer"),
        "an unreachable server must say so; got {text}"
    );
    assert!(
        !text.contains("zeroship-platform-migrate"),
        "an unreachable server must not be blamed on a missing migration; got {text}"
    );
}

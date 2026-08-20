//! The suite-database provisioner against a REAL PostgreSQL.
//!
//! WHY THIS EXISTS BESIDE THE UNIT TESTS. `suite_db`'s unit tests script a
//! `DbAdmin` and prove the DECISIONS: reuse, create-once, lost race, genuine
//! failure, could-not-tell. What they cannot prove is that the driver and the
//! server behave the way the script assumes -- that `CREATE DATABASE` outside a
//! transaction succeeds, that a duplicate create fails with a message rather
//! than hanging, that an absent database really reads as zero rows. A scripted
//! double answers whatever it was told to; that is its value and its limit.
//!
//! WHAT THE SHELL SELFTEST USED TO DO HERE, AND WHY IT CANNOT ANY MORE. It
//! injected a fake `run_psql` shell function that recorded the SQL it was
//! handed. That worked because the library was shell running in the selftest's
//! own process. The logic is a separate process now, so a shell function
//! defined in the caller is unreachable -- and the fake closed over the
//! selftest's non-exported `$TMP`, so even exporting the function would not
//! carry it. This file and `suite_db`'s unit tests are where those cases went.
//!
//! THE RACE IS FORCED, NOT WAITED FOR. `ensure` has an arm that only runs when
//! a peer creates the database between our probe and our `CREATE`. Two
//! processes started together might simply not collide, so the collision is
//! MANUFACTURED: a decorator creates the database for real, from a second
//! connection, immediately before delegating the create. The failure the server
//! then returns is a real 42P04, not a string a fake was told to produce.

use compio_postgres::NoTls;
use std::path::{Path, PathBuf};
use zeroship_testkit::{admin, overlay, suite_db};

fn repo_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is substituted at COMPILE time by `env!`, so this is
    // a constant in the binary rather than a read of the process environment.
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
        "zs_testkit_live_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    )
}

/// Drop a database this test created.
///
/// `WITH (FORCE)` IS CORRECT HERE AND WRONG IN THE SWEEPER, and the difference
/// is whose connections it terminates. This database was created by this test
/// moments ago; the only sessions on it are this test's own, which the driver
/// may not have torn down yet, and a plain DROP would fail on them and leak the
/// database. The sweeper drops databases OTHER runs created, where a live
/// session is a peer agent mid-suite -- so `crates/zeroship-testkit/src/sweep.rs`
/// has no drop at all and `tests/sweep_test_databases.sh` must never add FORCE.
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
    let _ = compio::runtime::Runtime::new().unwrap().block_on(async move {
        let (client, connection) = config.connect(NoTls).await?;
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        client.simple_query(&sql).await
    });
}

#[test]
fn an_absent_database_is_created_and_the_next_run_reuses_it_untouched() {
    let Some((_, server)) = server() else { return };
    let name = scratch_name("reuse");
    let mut admin = admin::PgAdmin::new(server.clone());
    let mut said = String::new();

    let first = suite_db::ensure(&mut admin, &name, &mut |l| said.push_str(l));
    assert_eq!(first.unwrap(), suite_db::Ensured::Created, "{said}");

    // A marker written into the new database. The second `ensure` must leave it
    // there: "reused" is not the same claim as "not recreated", and only the
    // marker separates them. A DROP+CREATE would report Reused just as happily
    // and lose the row.
    write_marker(&server, &name);

    let mut said2 = String::new();
    let second = suite_db::ensure(&mut admin, &name, &mut |l| said2.push_str(l));
    assert_eq!(second.unwrap(), suite_db::Ensured::Reused, "{said2}");
    assert!(marker_present(&server, &name), "the database was recreated under us");

    drop_scratch(&server, &name);
}

#[test]
fn losing_a_real_create_race_is_success() {
    let Some((_, server)) = server() else { return };
    let name = scratch_name("race");

    /// Creates the database for real just before delegating, so the delegate's
    /// `CREATE DATABASE` meets a genuine 42P04 from the server.
    struct Peer {
        inner: admin::PgAdmin,
        peer: admin::PgAdmin,
        raced: bool,
    }
    impl admin::DbAdmin for Peer {
        fn exists(&mut self, name: &str) -> Result<bool, String> {
            self.inner.exists(name)
        }
        fn create(&mut self, name: &str) -> Result<(), String> {
            if !self.raced {
                self.raced = true;
                self.peer.create(name).expect("the peer's create");
            }
            self.inner.create(name)
        }
    }

    let mut admin = Peer {
        inner: admin::PgAdmin::new(server.clone()),
        peer: admin::PgAdmin::new(server.clone()),
        raced: false,
    };
    let mut said = String::new();
    let out = suite_db::ensure(&mut admin, &name, &mut |l| said.push_str(l));
    assert_eq!(out.unwrap(), suite_db::Ensured::CreatedByPeer, "{said}");
    assert!(said.contains("appeared concurrently"), "{said}");

    drop_scratch(&server, &name);
}

#[test]
fn an_unreachable_server_is_could_not_tell_and_not_absent() {
    // One variable changed from every case above: a port nothing listens on.
    // Read as "absent", this becomes a CREATE DATABASE that fails for reasons
    // nobody can name.
    let mut admin = admin::PgAdmin::new(admin::Server {
        host: "127.0.0.1".into(),
        // Port 1 is `tcpmux`; nothing in this project binds it.
        port: 1,
        user: "postgres".into(),
        pass: String::new(),
    });
    let err = suite_db::ensure(&mut admin, "zs_testkit_unreachable", &mut |_| {}).unwrap_err();
    assert!(err.contains("could not ask the server"), "{err}");
}

#[test]
fn two_provision_processes_do_not_overlap() {
    // Asserted by OVERLAP, not by exit codes: two provisions that both succeed
    // prove nothing on their own, since they might simply not have collided.
    // The command each one runs records the interval it ran for, and the check
    // is that the two intervals do not intersect.
    let Some((loaded, server)) = server() else { return };
    let name = scratch_name("lock");
    let scratch = tempfile::tempdir().expect("scratch root");
    let root = scratch.path();

    // A tree that looks like a repo root to the binary: only the overlay is
    // read, and it names the same server so the lock key is the real one.
    std::fs::create_dir_all(root.join("deploy/ops")).unwrap();
    std::fs::copy(&loaded.overlay, root.join("deploy/ops/zeroship.test.toml")).unwrap();
    let interval_log = root.join("intervals.log");

    let spawn = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_zs-testkit"))
            .args(["suite-db", "provision"])
            .arg("--root")
            .arg(root)
            .arg("--name")
            .arg(&name)
            .arg("--lock-dir")
            .arg(root)
            .arg("--")
            .arg("sh")
            .arg("-c")
            .arg(format!(
                "printf 'start\\n' >> {log}; sleep 1; printf 'end\\n' >> {log}",
                log = interval_log.display()
            ))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn provision")
    };
    let (mut a, mut b) = (spawn(), spawn());
    let (ra, rb) = (a.wait().unwrap(), b.wait().unwrap());
    let order = std::fs::read_to_string(&interval_log).unwrap_or_default();
    drop_scratch(&server, &name);

    assert!(ra.success() && rb.success(), "a={ra:?} b={rb:?}");
    // start end start end = serialized. start start end end = overlapped.
    assert_eq!(
        order.split_whitespace().collect::<Vec<_>>(),
        vec!["start", "end", "start", "end"],
        "the two commands overlapped or did not both run"
    );
}

fn write_marker(server: &admin::Server, database: &str) {
    run_in(server, database, "CREATE TABLE zs_testkit_marker (n int)");
}

fn marker_present(server: &admin::Server, database: &str) -> bool {
    run_in(
        server,
        database,
        "SELECT 1 FROM pg_tables WHERE tablename = 'zs_testkit_marker'",
    )
}

/// Run one statement inside `database`; true when it produced a row.
fn run_in(server: &admin::Server, database: &str, sql: &str) -> bool {
    let mut config = compio_postgres::Config::new();
    config.host(&server.host).port(server.port).dbname(database);
    if !server.user.is_empty() {
        config.user(&server.user);
    }
    if !server.pass.is_empty() {
        config.password(&server.pass);
    }
    let sql = sql.to_string();
    compio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let (client, connection) = config.connect(NoTls).await.expect("connect to scratch");
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            let messages = client.simple_query(&sql).await.expect("statement");
            messages
                .iter()
                .any(|m| matches!(m, compio_postgres::SimpleQueryMessage::Row(_)))
        })
}

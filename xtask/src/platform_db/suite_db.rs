//! The SHARED database the live-PostgreSQL test GATES run against, named after
//! the schema they need rather than after whoever launched them.
//!
//! The remaining worker and billing shell runners use this shared database
//! policy. Auth package tests own containers instead. Shared migration state
//! establishes schema freshness; callers still need to isolate their fixtures.
//!
//! WHAT WAS WRONG WITH A NAME PER AGENT. Both gates used to take a database
//! name from whoever launched them and open with `DROP DATABASE IF EXISTS
//! <name> WITH (FORCE); CREATE DATABASE <name>`. Cleanup was therefore bounded
//! by a NAME COLLISION, not by the run's lifetime. MEASURED 2026-08-19 on the
//! shared cluster at :5440 -- 84 databases, an archaeology of every agent slot
//! this project has ever had. The population grew with AGENT COUNT.
//!
//! WHAT THE FRESH DATABASE ACTUALLY BUYS. Not test isolation -- the tests
//! already have that. SCHEMA FRESHNESS: a guarantee that the schema in the
//! database matches the migrations in the tree under test. That is a property
//! of the BRANCH, so the name is derived from the migration set itself (see
//! [`super::fingerprint`]). Every agent on the same commit shares one database;
//! an agent on a branch that edits a migration gets its own automatically, with
//! nobody deciding and nobody passing a flag; and cleanup becomes DECIDABLE,
//! which is what `tests/sweep_test_databases.sh` relies on.
//!
//! NOTHING IS EVER DROPPED HERE, AND THERE IS NO `WITH (FORCE)` IN THIS FILE.
//! Both halves of that are load-bearing and they are not the same statement.
//! Nothing is dropped because a failed suite's database is the primary
//! debugging artifact and a shared database is by definition not this run's to
//! destroy. `WITH (FORCE)` appears nowhere because it TERMINATES every other
//! backend on the database first -- it exists so a drop cannot fail on a live
//! connection, which is exactly the wrong property when the live connection is
//! a peer agent fifteen minutes into its own suite. `tests/lib/scratch_db.sh`
//! DOES use it, correctly: the database it drops is one this run created for
//! itself and the only connections left are its own. The sweeper must never use
//! it, for the same reason this module must never drop.
//!
//! THE HAZARD THIS DESIGN HAS, which no fingerprint can see: a DATABASE-SCOPED
//! SINGLETON. The migration set describes the schema; it says nothing about
//! whether two runs can both hold a row the schema allows only one of. The auth
//! suite had exactly one -- `zeroship.signing_keys` permits a single `active`
//! OP key per database -- and two runs sharing a database spent the whole run
//! retiring each other. MEASURED before the fixture fix: 168/97 and 168/103, of
//! which 96 failures were that one message. Advisory-lock keys, single-active-
//! row registries and fleet-wide sweep locks are the same class. The only
//! instrument that finds them is running two suites at once and reading what
//! breaks; a serial pass proves nothing, because serial already worked.

use super::admin::DbAdmin;
use super::fingerprint;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

/// How long the `suite-db provision` subcommand waits for a peer before giving
/// up. The provisioning itself lives in `main.rs`, because it is the one step
/// that runs a caller's command: the decision half is here and testable, the
/// process half is at the edge.
pub const PROVISION_LOCK_TIMEOUT: Duration = Duration::from_secs(900);

/// The resolved database name and who chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// `TEST_DB`.
    pub name: String,
    /// `ZS_SUITE_DB_EXPLICIT` -- true when the caller named it on the command
    /// line, false when it was derived from the migration set.
    pub explicit: bool,
}

/// A database name reaches `CREATE DATABASE` unquoted, so it must be a bare
/// identifier. This is the only caller-controlled string in that statement.
pub fn check_identifier(name: &str) -> Result<(), String> {
    let bare = {
        let mut bytes = name.bytes();
        match bytes.next() {
            Some(b) if b.is_ascii_lowercase() || b == b'_' => {
                bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            }
            _ => false,
        }
    };
    if !bare {
        return Err(format!(
            "FATAL: '{name}' is not a bare PostgreSQL identifier.\n\
             \x20      Use lowercase letters, digits and underscores only.\n"
        ));
    }
    if name.len() > 63 {
        return Err(format!(
            "FATAL: '{name}' is {} bytes; PostgreSQL truncates past 63,\n\
             \x20      which would silently merge two databases into one.\n",
            name.len()
        ));
    }
    Ok(())
}

/// Resolve the database name for a suite gate.
///
/// THE OVERRIDE IS A FLAG, NEVER AN ENVIRONMENT VARIABLE. An ambient
/// `TEST_DB=...` is REFUSED rather than ignored: a variable that silently
/// redirects a gate is how gates get silently disabled, and refusing is the
/// only behaviour that cannot be inherited from a shell the caller forgot
/// about. Ignoring it is no better -- a caller who set it deliberately must be
/// told their run went elsewhere.
///
/// `ambient_test_db` and `ambient_skip_db_recreate` are the environment as the
/// caller found it; empty means unset, which is what `[ -n "$X" ]` meant.
pub fn resolve(
    prefix: &str,
    explicit: &str,
    root: &Path,
    ambient_test_db: &str,
    ambient_skip_db_recreate: &str,
) -> Result<Resolved, String> {
    if !ambient_test_db.is_empty() {
        return Err(format!(
            "FATAL: TEST_DB is set in this environment ('{ambient_test_db}').\n\
             \x20      The suite database is derived from the migration set now, and\n\
             \x20      the override is a FLAG so it cannot be inherited from a shell:\n\
             \x20        <suite> --database {ambient_test_db}\n\
             \x20      Unset TEST_DB and pass it there if that is what you meant.\n"
        ));
    }
    if !ambient_skip_db_recreate.is_empty() {
        return Err(
            "FATAL: SKIP_DB_RECREATE is set, and there is nothing left for it to do.\n\
             \x20      The suite database is no longer recreated per run - it is named\n\
             \x20      after the migration set and reused by every run that needs the\n\
             \x20      same schema. Unset it.\n"
                .to_string(),
        );
    }

    if !explicit.is_empty() {
        check_identifier(explicit)?;
        return Ok(Resolved {
            name: explicit.to_string(),
            explicit: true,
        });
    }

    let fingerprint = fingerprint::of_dir(root)?;
    Ok(Resolved {
        name: format!("{prefix}_{fingerprint}"),
        explicit: false,
    })
}

/// What [`ensure`] decided, and what it wants said about it.
#[derive(Debug, PartialEq, Eq)]
pub enum Ensured {
    /// The database was already there. Reused, not recreated, never dropped.
    Reused,
    /// This run created it.
    Created,
    /// A peer created it between our probe and our `CREATE`. Success.
    CreatedByPeer,
}

/// Create the suite database if it is not there yet.
///
/// Never drops, never recreates, and tolerates LOSING the create race to a
/// concurrent run: two runs can both find the database absent and both issue
/// `CREATE DATABASE`, and the loser must proceed. What makes that safe to
/// swallow is that we re-ask the server rather than pattern-matching the
/// message -- "it exists now" is the condition we actually need, and matching
/// the text would swallow a permission failure too.
///
/// The caller must run the migration afterwards UNCONDITIONALLY, not only when
/// this created something. A run that dies between `CREATE` and the end of its
/// migration leaves a partially journalled database, and the next run's migrate
/// is what finishes it.
///
/// `say` receives each progress line AS IT IS DECIDED, not afterwards. The
/// order matters on the failure path: `==> Creating <name>` is printed BEFORE
/// the attempt, so a run that dies in `CREATE DATABASE` still shows which
/// database it was reaching for. Returning the narration instead would print it
/// only on success, which is the reading nobody needs.
pub fn ensure(
    admin: &mut dyn DbAdmin,
    name: &str,
    say: &mut dyn FnMut(&str),
) -> Result<Ensured, String> {
    match admin.exists(name) {
        Err(why) => Err(format!(
            "FATAL: could not ask the server whether {name} exists ({why}).\n"
        )),
        Ok(true) => {
            say(&format!(
                "==> Reusing {name} (schema-keyed; shared with every run on this migration set)\n"
            ));
            Ok(Ensured::Reused)
        }
        Ok(false) => {
            say(&format!("==> Creating {name}\n"));
            match admin.create(name) {
                Ok(()) => Ok(Ensured::Created),
                Err(server_said) => match admin.exists(name) {
                    Ok(true) => {
                        say(&format!(
                            "==> {name} appeared concurrently; another run created it first\n"
                        ));
                        Ok(Ensured::CreatedByPeer)
                    }
                    _ => Err(format!(
                        "{}\nFATAL: could not create {name}.\n",
                        server_said.trim_end()
                    )),
                },
            }
        }
    }
}

/// The lock file the provisioner serializes on.
///
/// Keyed on the SERVER as well as the database: two clusters can carry the same
/// database name, and a lock that ignored the port would serialize runs that
/// cannot touch each other. The key is the first 16 hex characters of the
/// sha256 of `host:port:database` and the file is
/// `<dir>/zeroship-suite-db-<key>.lock` -- byte-identical to what the shell
/// computed, so a run of the old harness and a run of this one still exclude
/// each other while the migration is in flight.
///
/// `dir` IS AN ARGUMENT, not `TMPDIR` read from the environment. The caller
/// resolves it once and every caller of this function is then visibly reaching
/// for the same file; a read in here would mean two processes with the same
/// arguments could take two different locks and neither would know.
pub fn lock_path(dir: &Path, host: &str, port: &str, name: &str) -> std::path::PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(format!("{host}:{port}:{name}").as_bytes());
    let key = format!("{:x}", hasher.finalize());
    dir.join(format!("zeroship-suite-db-{}.lock", &key[..16]))
}

/// WHY A LOCK AT ALL, when the migrate binary already takes one.
///
/// Because its lock is taken around the APPLY, and the step that races is the
/// one BEFORE it. MEASURED 2026-08-20 on :5444, two first-ever migrates of one
/// freshly created database started together:
///
/// ```text
/// migrate A exit=0   migrate B exit=1
/// zeroship-platform-migrate: FAILED: provision schema:
///   duplicate key value violates unique constraint "pg_namespace_nspname_index"
/// ```
///
/// `CREATE SCHEMA` happens before the project advisory lock is taken. That is
/// not a rare window either: it is exactly what two agents starting together on
/// a newly-written migration hit. The same experiment against an ALREADY
/// migrated database is safe and was measured so -- three rounds of 0/0.
///
/// THE LOCK IS HELD OVER PROVISIONING ONLY, never over the tests. A suite run
/// is tens of minutes; serializing that would mean two agents never overlap,
/// which is the entire property this module exists to deliver.
pub fn provision_lock_timeout_message(name: &str, lock: &Path) -> String {
    format!(
        "FATAL: waited {}s for another run to finish provisioning {name}.\n\
         \x20      Lock file: {}\n",
        PROVISION_LOCK_TIMEOUT.as_secs(),
        lock.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted server. Each `exists` answer is consumed in order, so the
    /// interleaving no test can schedule against a real cluster -- a peer
    /// creating the database between our probe and our CREATE -- is expressible
    /// as data.
    struct Scripted {
        exists_answers: Vec<Result<bool, String>>,
        create_answer: Option<Result<(), String>>,
        pub log: Vec<String>,
    }

    impl Scripted {
        fn new(exists: Vec<Result<bool, String>>, create: Result<(), String>) -> Scripted {
            Scripted {
                exists_answers: exists,
                create_answer: Some(create),
                log: Vec::new(),
            }
        }
    }

    impl DbAdmin for Scripted {
        fn exists(&mut self, name: &str) -> Result<bool, String> {
            self.log.push(format!("exists {name}"));
            if self.exists_answers.is_empty() {
                return Ok(false);
            }
            self.exists_answers.remove(0)
        }
        fn create(&mut self, name: &str) -> Result<(), String> {
            self.log.push(format!("create {name}"));
            self.create_answer.take().unwrap_or(Ok(()))
        }
    }

    /// Run `ensure` and collect what it said, so the narration is asserted from
    /// the same run as the decision rather than re-derived afterwards.
    fn run(admin: &mut Scripted, name: &str) -> (Result<Ensured, String>, String) {
        let mut said = String::new();
        let out = ensure(admin, name, &mut |line| said.push_str(line));
        (out, said)
    }

    #[test]
    fn an_existing_database_is_reused_and_nothing_is_issued_against_it() {
        let mut admin = Scripted::new(vec![Ok(true)], Ok(()));
        let (out, said) = run(&mut admin, "zeroship_auth_test_abc");
        assert_eq!(out.unwrap(), Ensured::Reused);
        // The whole point. A drop here would destroy a peer's run mid-suite and
        // would destroy the failed-run data a caller kept deliberately. There
        // is no `drop` on the trait at all, so the assertion is that nothing
        // beyond the probe was issued.
        assert_eq!(admin.log, vec!["exists zeroship_auth_test_abc"]);
        assert!(said.contains("Reusing zeroship_auth_test_abc"), "{said}");
    }

    #[test]
    fn an_absent_database_is_created_exactly_once() {
        let mut admin = Scripted::new(vec![Ok(false)], Ok(()));
        let (out, said) = run(&mut admin, "zeroship_auth_test_abc");
        assert_eq!(out.unwrap(), Ensured::Created);
        assert_eq!(
            admin.log,
            vec![
                "exists zeroship_auth_test_abc",
                "create zeroship_auth_test_abc"
            ]
        );
        assert!(
            said.contains("==> Creating zeroship_auth_test_abc"),
            "{said}"
        );
    }

    #[test]
    fn losing_the_create_race_is_success() {
        // Absent on the first probe, the CREATE fails, present on the second:
        // a peer won. Asserted through the second probe rather than by matching
        // the error text, because "it exists now" is the condition that
        // actually makes it safe to continue.
        let mut admin = Scripted::new(
            vec![Ok(false), Ok(true)],
            Err("ERROR:  database \"zeroship_auth_test_abc\" already exists".into()),
        );
        let (out, said) = run(&mut admin, "zeroship_auth_test_abc");
        assert_eq!(out.unwrap(), Ensured::CreatedByPeer);
        assert!(said.contains("appeared concurrently"), "{said}");
    }

    #[test]
    fn a_create_that_fails_for_any_other_reason_still_fails() {
        // One variable changed from the case above: the database is still
        // absent on the second probe. Without this, that arm would swallow
        // every create failure.
        let mut admin = Scripted::new(
            vec![Ok(false), Ok(false)],
            Err("ERROR:  permission denied to create database".into()),
        );
        let err = run(&mut admin, "zeroship_auth_test_abc").0.unwrap_err();
        assert!(
            err.contains("permission denied to create database"),
            "{err}"
        );
        assert!(
            err.contains("FATAL: could not create zeroship_auth_test_abc."),
            "{err}"
        );
    }

    #[test]
    fn could_not_tell_is_not_absent() {
        // Read as absent, an unreachable server becomes a CREATE DATABASE
        // failing for reasons nobody can name.
        let mut admin = Scripted::new(vec![Err("connect: connection refused".into())], Ok(()));
        let err = run(&mut admin, "zeroship_auth_test_abc").0.unwrap_err();
        assert!(err.contains("could not ask the server"), "{err}");
        assert_eq!(admin.log, vec!["exists zeroship_auth_test_abc"]);
    }

    #[test]
    fn the_derived_name_is_the_prefix_plus_the_fingerprint() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        std::fs::create_dir_all(root.join(fingerprint::MIGRATIONS_DIR)).unwrap();
        std::fs::write(
            root.join(fingerprint::MIGRATIONS_DIR)
                .join("20260101_one.ts"),
            "one",
        )
        .unwrap();
        let fp = fingerprint::of_dir(root).unwrap();
        let got = resolve("zeroship_auth_test", "", root, "", "").unwrap();
        assert_eq!(got.name, format!("zeroship_auth_test_{fp}"));
        assert!(!got.explicit);
        assert!(got.name.len() <= 63);
    }

    #[test]
    fn an_ambient_override_is_refused_but_the_same_name_as_an_argument_is_taken() {
        let refusal = resolve(
            "zeroship_auth_test",
            "",
            Path::new("/nonexistent"),
            "zeroship_auth_test_byhand",
            "",
        )
        .unwrap_err();
        assert!(refusal.contains("--database"), "{refusal}");
        assert!(
            resolve("zeroship_auth_test", "", Path::new("/nonexistent"), "", "1")
                .unwrap_err()
                .contains("SKIP_DB_RECREATE")
        );

        // One variable changed: the same name, passed as the argument.
        let taken = resolve(
            "zeroship_auth_test",
            "zeroship_auth_test_byhand",
            Path::new("/nonexistent"),
            "",
            "",
        )
        .unwrap();
        assert_eq!(taken.name, "zeroship_auth_test_byhand");
        assert!(taken.explicit);
    }

    #[test]
    fn an_override_that_is_not_a_bare_identifier_is_refused() {
        for evil in [
            "foo; DROP DATABASE zeroship",
            "Foo",
            "foo-bar",
            "1foo",
            "\"foo\"",
            "'x'",
        ] {
            let out = resolve("p", evil, Path::new("/nonexistent"), "", "");
            assert!(out.is_err(), "ACCEPTED an illegal identifier: {evil:?}");
        }
        let long = "a".repeat(64);
        let err = resolve("p", &long, Path::new("/nonexistent"), "", "").unwrap_err();
        assert!(err.contains("64 bytes"), "{err}");
        // 63 is legal, and only the pair says the bound is the bound.
        assert!(check_identifier(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn the_lock_key_separates_two_clusters_carrying_one_name() {
        let dir = Path::new("/tmp");
        let a = lock_path(dir, "127.0.0.1", "5440", "zeroship_auth_test_abc");
        let b = lock_path(dir, "127.0.0.1", "5444", "zeroship_auth_test_abc");
        assert_ne!(a, b);
        assert_eq!(
            a,
            lock_path(dir, "127.0.0.1", "5440", "zeroship_auth_test_abc")
        );
        // The shell computed this exact path; a run of the old harness and a
        // run of this one have to exclude each other, so the value is pinned
        // rather than left to whatever the hash happens to produce.
        assert_eq!(
            a.file_name().unwrap().to_str().unwrap(),
            "zeroship-suite-db-7e433ebb41447593.lock"
        );
    }
}

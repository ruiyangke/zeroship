//! The preflight a live-database test target runs before its first assertion:
//! is the database it was pointed at the database it needs?
//!
//! WHY THIS EXISTS. On 2026-08-21 the shared `zeroship` database on :5440 lost
//! its `zeroship`, `zeroship_migrations` and `service_authn` schemas while 84
//! `cpg_*` schemas left by `libs/compio-postgres`'s suite stayed behind. The
//! overlay (`deploy/ops/zeroship.test.toml`) still named that database in six
//! places, so every control and auth live-DB test connected fine, ran its
//! fixture, and failed inside an assertion with
//!
//!     db error: ERROR: relation "zeroship.plans" does not exist
//!
//! That is a VOID RUN -- the code under test never executed -- but `cargo test`
//! prints it as `test billing_credit_test::... FAILED` with a database error in
//! the body, which is indistinguishable from a real regression. It cost two
//! people real time the same evening: one password-reset verification and one
//! investigation that went seven red before anyone looked at the database.
//!
//! WHAT A REFUSAL IS, AND WHY IT IS NOT A FAILURE. A failure is a verdict about
//! the code. A refusal is the statement that no verdict was reachable. The
//! shell gates in `tests/` already draw that line -- `tests/project_config_gate.sh`
//! prints `FAIL: no zeroship binary at <path>` and emits ZERO arm lines rather
//! than ruling on nothing -- and this is the same shape for a cargo test binary.
//!
//! WHY IT EXITS THE PROCESS INSTEAD OF PANICKING. A panic in a shared fixture
//! helper is reported once per test that called it. The control live-DB target
//! is 41 modules over one process, so an unmigrated database would print
//! hundreds of FAILED lines -- which is EXACTLY the presentation this module
//! exists to remove. Exiting once, with one block, produces no per-test verdict
//! at all: the run is visibly void rather than visibly red.
//!
//! WHY IT WRITES TO `stderr()` AND NOT `eprintln!`. libtest captures the output
//! of a running test by installing a thread-local sink that `print!`/`eprintln!`
//! route through; a refusal printed with `eprintln!` would be swallowed unless
//! the caller happened to pass `--nocapture`. `std::io::stderr()` is the real
//! descriptor 2 and is not captured, so the block reaches the terminal on the
//! plain `cargo test` invocation the person is actually running.
//!
//! WHAT IT DOES NOT CATCH. The check is "these schemas exist", not "this
//! database matches this tree". A database migrated by an OLDER checkout has
//! every schema this asks for and will still fail inside an assertion on a
//! table that checkout never created. Catching that needs the migration
//! fingerprint ([`crate::fingerprint`]) compared against the journal, and the
//! journal does not record one today. The narrower check is what discriminates
//! the case that actually happened; it is not the general one.

use std::io::Write as _;

use compio_postgres::{Config, NoTls};

/// The exit status a refusal leaves behind.
///
/// Distinct from libtest's own `101` on a failing test, so a caller (a CI step,
/// a wrapper script) can tell "the suite ruled and said no" from "the suite
/// declined to rule".
pub const REFUSED_EXIT_CODE: i32 = 2;

/// What a preflight found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The database is reachable and holds every schema that was asked for.
    Ready,
    /// It is not, and this is the block to print. Multi-line, already
    /// formatted, ends with a newline.
    Refused(String),
}

impl Verdict {
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// The refusal text, or `None` when the verdict is [`Verdict::Ready`].
    #[must_use]
    pub fn refusal(&self) -> Option<&str> {
        match self {
            Self::Ready => None,
            Self::Refused(text) => Some(text),
        }
    }
}

/// Refuse the whole run unless `dsn` names a database holding `required_schemas`.
///
/// Call this once, before the first fixture touches the database. On a refusal
/// it prints and does not return.
///
/// # Panics
///
/// If the preflight thread cannot be joined, which means it panicked -- itself
/// a refusal, and re-raised rather than swallowed.
pub fn require(dsn: &str, required_schemas: &[&str]) {
    if let Verdict::Refused(text) = inspect(dsn, required_schemas) {
        refuse(&text);
    }
}

/// [`require`], for a caller whose DSN may not have been configured at all.
///
/// The two failures are ONE failure to a reader -- "the run could not reach the
/// database it needed" -- and they must present the same way, or the cheaper of
/// the two (no overlay at all) goes back to being a panic that prints once per
/// test. `zeroship_core::config::test_database_url` panics for exactly that
/// case, which is right for a caller with one test and wrong for a target with
/// forty-one; pass its `_opt` sibling here instead.
///
/// Returns the DSN when it is usable, and does not return when it is not.
#[must_use]
pub fn require_configured(dsn: Option<String>, required_schemas: &[&str]) -> String {
    let Some(dsn) = dsn.filter(|url| !url.trim().is_empty()) else {
        refuse(NO_DSN_REFUSAL);
    };
    require(&dsn, required_schemas);
    dsn
}

/// Print a refusal to the REAL descriptor 2 and end the process.
fn refuse(text: &str) -> ! {
    let mut err = std::io::stderr();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
    std::process::exit(REFUSED_EXIT_CODE);
}

const NO_DSN_REFUSAL: &str = "\n\
    REFUSED: the live-database tests were not told which database to use.\n\
    \n\
    \x20   Neither PG_TEST_URL nor [control] database_url in\n\
    \x20   deploy/ops/zeroship.test.toml names one.\n\
    \n\
    \x20   NO TEST RAN. This is not a failure; it is the absence of a verdict.\n\
    \n\
    \x20   Provision the test backends and their configuration with:\n\
    \x20     tests/provision_test_backends.sh\n";

/// The decision, without acting on it. This is what a test can assert against.
///
/// Runs on its OWN THREAD with its OWN compio runtime. The caller is usually
/// already inside a `#[compio::test]` runtime, and building a second one on
/// that thread is not something the driver promises to survive; a joined thread
/// costs one spawn per process (the callers memoise) and removes the question.
///
/// # Panics
///
/// If the preflight thread panicked -- re-raised rather than swallowed, because
/// a preflight that fails to run is not a preflight that passed.
#[must_use]
pub fn inspect(dsn: &str, required_schemas: &[&str]) -> Verdict {
    let dsn = dsn.to_string();
    let wanted: Vec<String> = required_schemas.iter().map(|s| (*s).to_string()).collect();
    std::thread::Builder::new()
        .name("zs-live-db-preflight".to_string())
        .spawn(move || inspect_here(&dsn, &wanted))
        .expect("spawn the live-database preflight thread")
        .join()
        .expect("the live-database preflight thread panicked")
}

fn inspect_here(dsn: &str, required_schemas: &[String]) -> Verdict {
    let where_ = Coordinates::of(dsn);

    let config: Config = match dsn.parse() {
        Ok(config) => config,
        Err(error) => {
            return Verdict::Refused(refusal(
                &where_,
                &format!("the DSN does not parse: {error}"),
                DSN_REMEDY,
            ))
        }
    };

    let probe = match compio::runtime::Runtime::new() {
        Ok(runtime) => runtime.block_on(schemas_present(&config)),
        Err(error) => {
            return Verdict::Refused(refusal(
                &where_,
                &format!("the io_uring runtime would not start: {error}"),
                DSN_REMEDY,
            ))
        }
    };

    match probe {
        Err(error) => Verdict::Refused(refusal(
            &where_,
            &format!("the server did not answer: {error}"),
            UNREACHABLE_REMEDY,
        )),
        Ok(found) => {
            let missing: Vec<&String> = required_schemas
                .iter()
                .filter(|want| !found.iter().any(|have| have == *want))
                .collect();
            if missing.is_empty() {
                return Verdict::Ready;
            }
            let names: Vec<String> = missing.iter().map(|s| format!("\"{s}\"")).collect();
            Verdict::Refused(refusal(
                &where_,
                &format!(
                    "it holds no schema {} (it has {} other schema(s))",
                    names.join(", "),
                    found.len()
                ),
                UNMIGRATED_REMEDY,
            ))
        }
    }
}

/// Every non-system schema the database holds, so the refusal can say what IS
/// there. "84 other schemas" is the line that told a reader the database had
/// been used by something else entirely; "no schema zeroship" alone does not.
async fn schemas_present(config: &Config) -> Result<Vec<String>, String> {
    let (client, connection) = config.connect(NoTls).await.map_err(|e| e.to_string())?;
    let driver = compio::runtime::spawn(async move {
        let _ = connection.run().await;
    });
    let result = client
        .query(
            "SELECT nspname FROM pg_namespace \
             WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
             ORDER BY nspname",
            &[],
        )
        .await;
    // Drop the client first so the driver is asked to shut down, then WAIT for
    // it rather than detaching: a detached driver parked on a read holds an
    // io_uring submission, and the submission holds the runtime's inner state
    // alive past `Runtime::drop`, stranding the ring, its eventfd and the
    // socket. `crates/zeroship-testkit/src/admin.rs` carries the long form.
    drop(client);
    let _ = driver.await;
    let rows = result.map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
}

/// Host, port and database name, with the password removed.
///
/// A refusal is printed to a terminal and pasted into issues; the DSN it names
/// carries userinfo by grammar. This prints enough to identify the server and
/// nothing that authenticates to it.
struct Coordinates {
    redacted: String,
    database: String,
}

impl Coordinates {
    fn of(dsn: &str) -> Self {
        let parts = crate::overlay::split_dsn(dsn);
        let host = if parts.port.is_empty() {
            parts.host.clone()
        } else {
            format!("{}:{}", parts.host, parts.port)
        };
        let user = if parts.user.is_empty() {
            String::new()
        } else if parts.pass.is_empty() {
            format!("{}@", parts.user)
        } else {
            format!("{}:***@", parts.user)
        };
        Self {
            redacted: format!("postgres://{user}{host}/{}", parts.db),
            database: parts.db,
        }
    }
}

const DSN_REMEDY: &str = "\
    Rewrite the overlay from the backends it names:\n\
    \x20     tests/provision_test_backends.sh\n";

const UNREACHABLE_REMEDY: &str = "\
    Start the test backends and rewrite the overlay from them:\n\
    \x20     tests/provision_test_backends.sh\n";

/// The remediation an unmigrated database gets.
///
/// IT NAMES A WRAPPER, NOT THE TOOL. `deploy/ops/db-migrate.sh` writes the DSN
/// into a 0600 config file and calls the `zero-migrate` CLI with `--config`; the
/// CLI's own `--database-url <value>` flag would put a superuser DSN in this
/// process list. Printing the wrapper is therefore the safe instruction as well
/// as the short one.
///
/// It replaced a `cargo run -p zeroship-migrate-adapter --features platform-cli
/// --bin zeroship-platform-migrate` line on 2026-08-28. That binary is deleted.
const UNMIGRATED_REMEDY: &str = "\
    Apply the platform schema to THAT database:\n\
    \x20     ZEROSHIP_MIGRATE_DSN='<dsn>' deploy/ops/db-migrate.sh\n\
    \n\
    \x20   That wrapper needs the CLI built once:\n\
    \x20     pnpm install && pnpm build && pnpm --filter zero-migrate-cli build\n\
    \n\
    \x20   or point the run at a database that is already migrated:\n\
    \x20     PG_TEST_URL=<dsn> cargo test ...\n\
    \n\
    \x20   The suite gates do this for you and name the database after the\n\
    \x20   migration set, which is why they do not hit this:\n\
    \x20     tests/run_billing_suite.sh   tests/run_auth_suite.sh\n";

fn refusal(where_: &Coordinates, what: &str, remedy: &str) -> String {
    format!(
        "\n\
         REFUSED: the live-database tests were pointed at a database they cannot run against.\n\
         \n\
         \x20   database    {redacted}\n\
         \x20   problem     {what}\n\
         \n\
         \x20   NO TEST RAN. This is not a failure; it is the absence of a verdict.\n\
         \x20   Had the run continued, every assertion touching {database} would have\n\
         \x20   reported `42P01 relation ... does not exist` from inside a named test,\n\
         \x20   which reads exactly like a regression and is not one.\n\
         \n\
         \x20   {remedy}\n",
        redacted = where_.redacted,
        database = where_.database,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal has to carry the three things a reader needs: WHICH
    /// database, WHAT was missing, and WHAT TO DO. Asserting on all three is
    /// the point -- a refusal that says only "not migrated" sends the reader
    /// back to guessing which of the eleven databases on :5440 it meant.
    #[test]
    fn a_refusal_names_the_database_the_gap_and_the_remedy() {
        let where_ = Coordinates::of("postgres://postgres:hunter2@127.0.0.1:5440/zeroship");
        let text = refusal(&where_, "it holds no schema \"zeroship\"", UNMIGRATED_REMEDY);
        assert!(text.contains("127.0.0.1:5440/zeroship"), "{text}");
        assert!(text.contains("no schema \"zeroship\""), "{text}");
        assert!(text.contains("zeroship-platform-migrate"), "{text}");
        assert!(text.contains("NO TEST RAN"), "{text}");
    }

    /// A refusal is pasted into issues and chat. The password must not travel
    /// with it.
    #[test]
    fn the_password_never_reaches_the_refusal() {
        let where_ = Coordinates::of("postgres://postgres:hunter2@127.0.0.1:5440/zeroship");
        let text = refusal(&where_, "unreachable", UNREACHABLE_REMEDY);
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("postgres:***@"), "{text}");
    }

    /// A DSN with no password must not print the USERNAME where the password
    /// would have been.
    #[test]
    fn a_password_less_dsn_prints_no_sentinel() {
        let where_ = Coordinates::of("postgres://postgres@127.0.0.1:5440/zeroship");
        assert_eq!(where_.redacted, "postgres://postgres@127.0.0.1:5440/zeroship");
        assert_eq!(where_.database, "zeroship");
    }

    /// An unparseable DSN must refuse rather than reach the connect.
    #[test]
    fn an_unparseable_dsn_refuses_without_dialling() {
        let verdict = inspect("this is not a dsn", &["zeroship"]);
        let text = verdict.refusal().expect("a bare word is not a DSN");
        assert!(text.contains("REFUSED"), "{text}");
        assert!(text.contains("provision_test_backends.sh"), "{text}");
    }
}

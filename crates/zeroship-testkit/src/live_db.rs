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
//! ```text
//! db error: ERROR: relation "zeroship.plans" does not exist
//! ```
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
//! THE SECOND STAGE: IS IT AS FAR ALONG AS THIS CHECKOUT? Schemas existing is
//! not the same as schemas being current. On 2026-09-07 the same `zeroship`
//! database on :5440 held every schema this module asks for and had never seen
//! `db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts`,
//! so `zeroship.apps` had no `organization_id`. Seven targets --
//! `crates/zeroship-authz/tests/{app_resolution_test,two_call_test}.rs` and
//! `crates/zeroship-gateway/tests/{sessions_test,auth_token_anchors_test,
//! backchannel_logout_test,browser_auth_test,oidc_rp_e2e}.rs` -- died on the
//! missing column and presented, again, as named tests FAILING. That is the
//! same void run as 2026-08-21 wearing different clothes, and the header above
//! declared it out of scope ("the journal does not record one today").
//!
//! HOW IT IS DECIDED, since the journal records no fingerprint. Every event the
//! platform runner writes to `zeroship_migrations.__zeroship_schema_migrations`
//! carries a `checksum`, and that checksum is one per migration FILE: it is
//! `Checksum::of_ir` over the file's canonical op list plus its flags, owner and
//! dependency edges (`crates/zeroship-migrate-ir/src/migration.rs`,
//! `of_ir_version_strings`). So the count of DISTINCT applied checksums is the
//! count of files the database has consumed, and it is compared against the
//! files this checkout carries ([`crate::fingerprint::files_in`]). Both sides
//! are re-derived on every run; neither is written down anywhere.
//!
//! IT IS NOT THE FILE'S SHA256, and `crate::fingerprint`'s header says
//! otherwise. Measured 2026-09-07 against the live journal:
//! `20260907000000_user_erasure_edges.ts` hashes to `e80e7b88...` as bytes and
//! is journaled under `0b02c6ad...`. A check built on the source hash would
//! refuse every database forever.
//!
//! WHAT THIS SECOND STAGE STILL DOES NOT CATCH, and both directions matter:
//!
//!   - A migration EDITED IN PLACE after it was applied. The database keeps the
//!     old checksum, the file count does not move, and the two agree. The
//!     branch-keyed suite database ([`crate::fingerprint`]) is what covers that
//!     case; this is not.
//!   - Two files that record the SAME canonical ops. The checksum omits the
//!     migration's name and version by construction (they are its identity, not
//!     its content), so such a pair journals one distinct checksum for two
//!     files and this check refuses a database that is in fact current. That is
//!     a loud, explained false refusal rather than a silent pass, and it has
//!     never occurred: measured 2026-09-07 on the :5440 corpus, distinct
//!     applied checksums equalled the file count exactly.
//!
//! IT ONLY RUNS WHEN THE CALLER ASKED FOR THE JOURNAL SCHEMA. A caller that
//! does not name `zeroship_migrations` in `required_schemas` is not claiming to
//! need the platform corpus, and counting a corpus it never wanted would refuse
//! databases that are correct for it.

use std::io::Write as _;

use compio_postgres::{Config, NoTls};

/// The schema the platform runner journals into.
///
/// Naming it in `required_schemas` is what turns the ledger stage on; see the
/// module header for why that is the switch.
pub const JOURNAL_SCHEMA: &str = "zeroship_migrations";

/// What a target reading the PLATFORM schema must ask for.
///
/// Asking for BOTH is what separates "never migrated" from "migrated and then
/// partly dismantled" - the second is what happened on 2026-08-21, and a check
/// for the journal alone would have called that database ready. Naming
/// [`JOURNAL_SCHEMA`] also turns the ledger stage on, which is what separates
/// both of those from "migrated by an older checkout".
///
/// IT IS ONE CONSTANT BECAUSE IT WAS THREE COPIES. `zeroship-control`'s two
/// preflights and this crate's own live test each spelled the pair out, so a
/// target that grew a third requirement would have left the others asking for
/// less and reporting Ready on a database that could not serve them.
pub const PLATFORM_SCHEMAS: &[&str] = &["zeroship", JOURNAL_SCHEMA];

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

/// [`require`], at most once per `(dsn, required_schemas)` per test binary.
///
/// WHY THE MEMOISATION IS HERE AND NOT AT THE CALL SITE. A preflight is a fact
/// about the process, so it wants to run once; but it also has to run before
/// the FIRST database touch, and any test can be the first when a filter
/// selects it (`cargo test --exact <one>`). That means every gate in a file
/// calls it -- `crates/zeroship-gateway/tests/backchannel_logout_test.rs` alone
/// has eleven -- and a `OnceLock` per call site is a `OnceLock` per file, which
/// is the seven copies this helper exists to avoid.
///
/// The lock is held across the probe on purpose: two threads arriving together
/// must not both dial, and libtest gives every test its own thread.
pub fn require_once(dsn: &str, required_schemas: &[&str]) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static CHECKED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let key = format!("{dsn}\u{0}{}", required_schemas.join("\u{0}"));
    let mut checked = CHECKED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if checked.contains(&key) {
        return;
    }
    require(dsn, required_schemas);
    checked.insert(key);
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
        Ok(runtime) => runtime.block_on(probe(&config, ledger_stage_wanted(required_schemas))),
        Err(error) => {
            return Verdict::Refused(refusal(
                &where_,
                &format!("the io_uring runtime would not start: {error}"),
                DSN_REMEDY,
            ))
        }
    };

    let found = match probe {
        Err(error) => {
            return Verdict::Refused(refusal(
                &where_,
                &format!("the server did not answer: {error}"),
                UNREACHABLE_REMEDY,
            ))
        }
        Ok(found) => found,
    };

    let missing: Vec<&String> = required_schemas
        .iter()
        .filter(|want| !found.schemas.iter().any(|have| have == *want))
        .collect();
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|s| format!("\"{s}\"")).collect();
        return Verdict::Refused(refusal(
            &where_,
            &format!(
                "it holds no schema {} (it has {} other schema(s))",
                names.join(", "),
                found.schemas.len()
            ),
            UNMIGRATED_REMEDY,
        ));
    }

    match found.applied_migrations {
        None => Verdict::Ready,
        Some(applied) => ledger_verdict(&where_, applied),
    }
}

/// The switch for stage two: did the caller declare it needs the journal?
///
/// A caller that does not name [`JOURNAL_SCHEMA`] is not claiming to need the
/// platform corpus, and counting a corpus it never wanted would refuse
/// databases that are correct for it.
fn ledger_stage_wanted(required_schemas: &[String]) -> bool {
    required_schemas.iter().any(|s| s == JOURNAL_SCHEMA)
}

/// Stage two: the journal is present, so ask whether it has consumed every
/// migration this checkout carries.
///
/// A checkout with no readable `db/migrations-ts/` refuses rather than passing.
/// The alternative -- "cannot tell, carry on" -- is the void run this module
/// exists to remove, one indirection further out.
fn ledger_verdict(where_: &Coordinates, applied: usize) -> Verdict {
    let carried = match crate::fingerprint::files_in(repo_root()) {
        Ok(files) => files.len(),
        Err(text) => {
            return Verdict::Refused(refusal(
                where_,
                &format!("this checkout's migration set could not be read: {}", text.trim()),
                UNMIGRATED_REMEDY,
            ))
        }
    };
    if applied >= carried {
        return Verdict::Ready;
    }
    Verdict::Refused(refusal(
        where_,
        &format!(
            "its migration journal has consumed {applied} of the {carried} migrations \
             this checkout carries, so the schema is BEHIND the tree"
        ),
        UNMIGRATED_REMEDY,
    ))
}

/// The checkout this binary was compiled from.
///
/// Baked at compile time rather than taken from the working directory: a cargo
/// test binary's cwd is its package root, and the harness also runs these
/// targets from `tests/*.sh` with a cwd of the repository. One of those two
/// answers would always be wrong.
fn repo_root() -> &'static std::path::Path {
    // <repo>/crates/zeroship-testkit -> <repo>
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the testkit manifest lives two directories below the repository root")
}

/// What the one preflight connection found.
struct Probe {
    /// Every non-system schema the database holds.
    schemas: Vec<String>,
    /// Distinct applied migration checksums, or `None` when the caller did not
    /// ask for the journal schema and the ledger stage is therefore off.
    applied_migrations: Option<usize>,
}

/// One connection, both questions.
///
/// The schema list is what lets the refusal say what IS there. "84 other
/// schemas" is the line that told a reader the database had been used by
/// something else entirely; "no schema zeroship" alone does not.
///
/// The ledger count is asked for in the SAME round trip rather than a second
/// dial, and it is asked with `to_regclass` in front of it: a database that
/// holds the journal SCHEMA but not the journal TABLE is a real state (a
/// half-dismantled database is exactly the 2026-08-21 case), and a bare
/// `SELECT` there would come back as "the server did not answer", which names
/// the wrong problem and prints the wrong remedy.
async fn probe(config: &Config, want_ledger: bool) -> Result<Probe, String> {
    let (client, connection) = config.connect(NoTls).await.map_err(|e| e.to_string())?;
    let driver = compio::runtime::spawn(async move {
        let _ = connection.run().await;
    });
    let schemas = client
        .query(
            "SELECT nspname FROM pg_namespace \
             WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
             ORDER BY nspname",
            &[],
        )
        .await;
    let ledger = if want_ledger {
        Some(
            client
                .query_one(
                    "SELECT CASE WHEN to_regclass($1) IS NULL THEN 0 ELSE ( \
                       SELECT count(DISTINCT checksum) \
                       FROM zeroship_migrations.__zeroship_schema_migrations \
                       WHERE event_kind = 'applied') END",
                    &[&"zeroship_migrations.__zeroship_schema_migrations"],
                )
                .await,
        )
    } else {
        None
    };
    // Drop the client first so the driver is asked to shut down, then WAIT for
    // it rather than detaching: a detached driver parked on a read holds an
    // io_uring submission, and the submission holds the runtime's inner state
    // alive past `Runtime::drop`, stranding the ring, its eventfd and the
    // socket. `crates/zeroship-testkit/src/admin.rs` carries the long form.
    drop(client);
    let _ = driver.await;
    let rows = schemas.map_err(|e| e.to_string())?;
    let applied_migrations = match ledger {
        None => None,
        Some(row) => {
            let count: i64 = row.map_err(|e| e.to_string())?.get(0);
            Some(usize::try_from(count).unwrap_or(0))
        }
    };
    Ok(Probe {
        schemas: rows.iter().map(|row| row.get::<_, String>(0)).collect(),
        applied_migrations,
    })
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
    \x20     pnpm install && pnpm build\n\
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
         \x20   reported `42P01 relation ... does not exist` or `42703 column ... does\n\
         \x20   not exist` from inside a named test, which reads exactly like a\n\
         \x20   regression and is not one.\n\
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
    ///
    /// THE APPLIER ASSERTION USED TO NAME A DELETED BINARY. It asked for
    /// `zeroship-platform-migrate`, which `ccda4bb42` removed on 2026-08-28 in
    /// the same change that rewrote `UNMIGRATED_REMEDY` to name
    /// `deploy/ops/db-migrate.sh`. The constant's own doc comment recorded the
    /// swap; the test did not, and stayed red from that day. It now asks for
    /// the wrapper the remedy actually prints, which is the string a reader
    /// would paste.
    #[test]
    fn a_refusal_names_the_database_the_gap_and_the_remedy() {
        let where_ = Coordinates::of("postgres://postgres:hunter2@127.0.0.1:5440/zeroship");
        let text = refusal(&where_, "it holds no schema \"zeroship\"", UNMIGRATED_REMEDY);
        assert!(text.contains("127.0.0.1:5440/zeroship"), "{text}");
        assert!(text.contains("no schema \"zeroship\""), "{text}");
        assert!(text.contains("deploy/ops/db-migrate.sh"), "{text}");
        assert!(text.contains("NO TEST RAN"), "{text}");
    }

    /// A database whose journal is BEHIND the checkout refuses, and the refusal
    /// says which side is short and what to run.
    ///
    /// `ledger_verdict` is the whole second stage: everything above it is the
    /// one query that produces `applied`, and everything below it is printing.
    #[test]
    fn a_journal_behind_the_checkout_refuses_and_names_the_applier() {
        let where_ = Coordinates::of("postgres://postgres:hunter2@127.0.0.1:5440/zeroship");
        let carried = crate::fingerprint::files_in(repo_root())
            .expect("this checkout carries a migration set")
            .len();
        let verdict = ledger_verdict(&where_, carried - 1);
        let text = verdict
            .refusal()
            .expect("one migration short of the tree is behind the tree");
        assert!(text.contains("BEHIND the tree"), "{text}");
        assert!(text.contains(&format!("of the {carried} migrations")), "{text}");
        assert!(text.contains("deploy/ops/db-migrate.sh"), "{text}");
        assert!(text.contains("NO TEST RAN"), "{text}");
    }

    /// The control: a journal that has consumed the whole set is ready, and one
    /// that has consumed MORE is ready too.
    ///
    /// The second half is not padding. A database migrated by a checkout AHEAD
    /// of this one -- another agent's worktree, a branch merged since -- has
    /// more distinct checksums than this tree has files, and refusing it would
    /// turn every shared database into a permanent refusal for whoever is one
    /// commit behind. This stage answers "is the database BEHIND me", and that
    /// is a one-sided question on purpose.
    #[test]
    fn a_journal_level_with_or_ahead_of_the_checkout_is_ready() {
        let where_ = Coordinates::of("postgres://postgres:hunter2@127.0.0.1:5440/zeroship");
        let carried = crate::fingerprint::files_in(repo_root())
            .expect("this checkout carries a migration set")
            .len();
        assert_eq!(ledger_verdict(&where_, carried), Verdict::Ready);
        assert_eq!(ledger_verdict(&where_, carried + 1), Verdict::Ready);
    }

    /// The ledger stage is off unless the caller said it needs the journal.
    ///
    /// A caller asking only for its own schema is not claiming to need the
    /// platform corpus, and counting a corpus it never wanted would refuse
    /// databases that are correct for it.
    #[test]
    fn the_ledger_stage_is_keyed_to_the_journal_schema() {
        let named = ["zeroship".to_string(), JOURNAL_SCHEMA.to_string()];
        let unnamed = ["zeroship".to_string()];
        assert!(ledger_stage_wanted(&named));
        assert!(!ledger_stage_wanted(&unnamed));
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

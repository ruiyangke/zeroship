// The preflight a live-database test target runs before its first assertion:
// is the database it was pointed at the database it needs?
//
// WHY THIS EXISTS. On 2026-08-21 the shared `zeroship` database on :5440 lost
// its `zeroship`, `zeroship_migrations` and `service_authn` schemas while 84
// `cpg_*` schemas left by `libs/compio-postgres`'s suite stayed behind. The
// overlay (`deploy/ops/zeroship.test.toml`) still named that database in six
// places, so every control and auth live-DB test connected fine, ran its
// fixture, and failed inside an assertion with
//
// ```text
// db error: ERROR: relation "zeroship.plans" does not exist
// ```
//
// That is a VOID RUN -- the code under test never executed -- but `cargo test`
// prints it as `test billing_credit_test::... FAILED` with a database error in
// the body, which is indistinguishable from a real regression. It cost two
// people real time the same evening: one password-reset verification and one
// investigation that went seven red before anyone looked at the database.
//
// WHAT A REFUSAL IS, AND WHY IT IS NOT A FAILURE. A failure is a verdict about
// the code. A refusal is the statement that no verdict was reachable. The
// shell gates in `tests/` already draw that line -- `tests/project_config_gate.sh`
// prints `FAIL: no zeroship binary at <path>` and emits ZERO arm lines rather
// than ruling on nothing -- and this is the same shape for a cargo test binary.
//
// WHY IT EXITS THE PROCESS INSTEAD OF PANICKING. A panic in a shared fixture
// helper is reported once per test that called it. The control live-DB target
// is 41 modules over one process, so an unmigrated database would print
// hundreds of FAILED lines -- which is EXACTLY the presentation this module
// exists to remove. Exiting once, with one block, produces no per-test verdict
// at all: the run is visibly void rather than visibly red.
//
// WHY IT WRITES TO `stderr()` AND NOT `eprintln!`. libtest captures the output
// of a running test by installing a thread-local sink that `print!`/`eprintln!`
// route through; a refusal printed with `eprintln!` would be swallowed unless
// the caller happened to pass `--nocapture`. `std::io::stderr()` is the real
// descriptor 2 and is not captured, so the block reaches the terminal on the
// plain `cargo test` invocation the person is actually running.
//
// THE SECOND STAGE: IS IT AS FAR ALONG AS THIS CHECKOUT? Schemas existing is
// not the same as schemas being current. On 2026-09-07 the same `zeroship`
// database on :5440 held every schema this module asks for and had never seen
// `db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts`,
// so `zeroship.apps` had no `organization_id`. Authz and gateway database
// targets died on the missing column and presented, again, as named tests
// FAILING. That is the
// same void run as 2026-08-21 wearing different clothes, and the header above
// declared it out of scope ("the journal does not record one today").
//
// HOW IT IS DECIDED, since the journal records no fingerprint. Every event the
// platform runner writes to `zeroship_migrations.__zeroship_schema_migrations`
// carries a `checksum`, and that checksum is one per migration FILE: it is
// `Checksum::of_ir` over the file's canonical op list plus its flags, owner and
// dependency edges (`crates/zeroship-migrate-ir/src/migration.rs`,
// `of_ir_version_strings`). So the count of DISTINCT applied checksums is the
// count of files the database has consumed, and it is compared against the
// files this checkout carries ([`super::fingerprint::files_in`]). Both sides
// are re-derived on every run; neither is written down anywhere.
//
// IT IS NOT THE FILE'S SHA256, and `super::fingerprint`'s header says
// otherwise. Measured 2026-09-07 against the live journal:
// `20260907000000_user_erasure_edges.ts` hashes to `e80e7b88...` as bytes and
// is journaled under `0b02c6ad...`. A check built on the source hash would
// refuse every database forever.
//
// WHAT THIS SECOND STAGE STILL DOES NOT CATCH, and both directions matter:
//
//   - A migration EDITED IN PLACE after it was applied. The database keeps the
//     old checksum, the file count does not move, and the two agree. The
//     branch-keyed suite database ([`super::fingerprint`]) is what covers that
//     case; this is not.
//   - Two files that record the SAME canonical ops. The checksum omits the
//     migration's name and version by construction (they are its identity, not
//     its content), so such a pair journals one distinct checksum for two
//     files and this check refuses a database that is in fact current. That is
//     a loud, explained false refusal rather than a silent pass, and it has
//     never occurred: measured 2026-09-07 on the :5440 corpus, distinct
//     applied checksums equalled the file count exactly.
//
// IT ONLY RUNS WHEN THE CALLER ASKED FOR THE JOURNAL SCHEMA. A caller that
// does not name `zeroship_migrations` in `required_schemas` is not claiming to
// need the platform corpus, and counting a corpus it never wanted would refuse
// databases that are correct for it.
//
// THE STATES A REFUSAL MUST KEEP APART. Every converted target in this
// workspace shows whatever this module prints, so a message that names the
// wrong cause sends every one of their readers to the wrong place. There are
// three answers, and each has a remedy the other two do not:
//
//   nothing answered at the DSN     -> provision and start the backends
//   a server answered, no corpus    -> apply the corpus to THAT database
//   a server answered, corpus there -> proceed
//
// The ledger question used to collapse the first two. It was ONE statement
// guarding the count with `to_regclass`, and `PostgreSQL` resolves every
// relation a statement names at PARSE time, before any `CASE` in it is
// evaluated -- so on a database with no journal table the statement ERRORED
// rather than returning zero, the error surfaced as "the server did not
// answer", and the remedy printed was `tests/provision_test_backends.sh`:
// restart a server that was already running, for a database that only needed
// `deploy/ops/db-migrate.sh`. The guard now takes the table name as a VALUE in
// its own round trip ([`count_journal`]), which is the only form that survives
// the table's absence.

use std::io::Write as _;

use compio_postgres::{Client, Config, NoTls};

/// The schema the platform runner journals into.
///
/// Naming it in `required_schemas` is what turns the ledger stage on; see the
/// module header for why that is the switch.
pub const JOURNAL_SCHEMA: &str = "zeroship_migrations";

/// The journal TABLE, qualified by [`JOURNAL_SCHEMA`].
///
/// Public because the live preflight test seeds a journal in this shape, and a
/// copy of the name there would go on testing the old one the day it moves.
pub const JOURNAL_TABLE: &str = "zeroship_migrations.__zeroship_schema_migrations";

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
/// Every filtered case must perform its preflight before touching the database.
/// Cache successful probes here so callers in the same binary share the result.
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
            ));
        }
    };

    let probe = match compio::runtime::Runtime::new() {
        Ok(runtime) => runtime.block_on(probe(&config, ledger_stage_wanted(required_schemas))),
        Err(error) => {
            return Verdict::Refused(refusal(
                &where_,
                &format!("the io_uring runtime would not start: {error}"),
                RUNTIME_REMEDY,
            ));
        }
    };

    let found = match probe {
        Err(ProbeFailure::NoServer(error)) => {
            return Verdict::Refused(refusal(
                &where_,
                &format!("nothing answered at that address: {error}"),
                UNREACHABLE_REMEDY,
            ));
        }
        Err(ProbeFailure::Refused { question, error }) => {
            return Verdict::Refused(refusal(
                &where_,
                &format!("a server answered, then refused to {question}: {error}"),
                ROLE_REMEDY,
            ));
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
                "a server answered, but the database holds no schema {} (it has {} other schema(s)), \
                 so the corpus was never applied to it",
                names.join(", "),
                found.schemas.len()
            ),
            UNMIGRATED_REMEDY,
        ));
    }

    match found.ledger {
        Ledger::NotAsked => Verdict::Ready,
        Ledger::Absent => Verdict::Refused(refusal(
            &where_,
            &format!(
                "a server answered and the database holds \"{JOURNAL_SCHEMA}\", but there is no \
                 {JOURNAL_TABLE} in it, so no migration has ever been recorded there"
            ),
            UNMIGRATED_REMEDY,
        )),
        Ledger::Applied(applied) => ledger_verdict(&where_, applied),
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
    let carried = match super::fingerprint::files_in(repo_root()) {
        Ok(files) => files.len(),
        Err(text) => {
            return Verdict::Refused(refusal(
                where_,
                &format!(
                    "this checkout's migration set could not be read: {}",
                    text.trim()
                ),
                UNMIGRATED_REMEDY,
            ));
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
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|path| path.join("AGENTS.md").is_file() && path.join("db/migrations-ts").is_dir())
        .expect("fixture belongs to the repository")
}

/// What the one preflight connection found.
struct Probe {
    /// Every non-system schema the database holds.
    schemas: Vec<String>,
    /// What the journal had to say, if the caller asked about it at all.
    ledger: Ledger,
}

/// The journal's state, as far as one connection can see it.
///
/// THE THREE ARMS ARE THREE DIFFERENT REFUSALS, which is why this is an enum
/// and not an `Option<usize>` with zero standing in for absent. A journal table
/// that does not exist and a journal table that exists with nothing applied
/// send a reader to the same command today, but they are different statements
/// about the database, and the second one is a half-applied corpus rather than
/// an untouched database.
enum Ledger {
    /// The caller never named [`JOURNAL_SCHEMA`], so the stage is off and no
    /// question about the corpus was asked.
    NotAsked,
    /// There is no journal table at all.
    Absent,
    /// Distinct applied migration checksums.
    Applied(usize),
}

/// Why the preflight could not finish its questions.
///
/// The two arms are the two remedies. A connect that never landed is a backend
/// that is not running; a question refused after a successful connect is a
/// server that IS running and a role that could not answer, and telling someone
/// to restart the former when they have the latter is the misdiagnosis this
/// whole module exists to remove.
enum ProbeFailure {
    /// Nothing answered at that address.
    NoServer(String),
    /// A server answered the connect, then refused a question.
    Refused {
        /// What was being asked, phrased to follow "refused to ...".
        question: &'static str,
        error: String,
    },
}

/// Every non-system schema, which is what lets a refusal say what IS there.
///
/// "84 other schemas" is the line that told a reader the database had been used
/// by something else entirely; "no schema zeroship" alone does not.
const SCHEMAS_SQL: &str = "SELECT nspname FROM pg_namespace \
     WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
     ORDER BY nspname";

/// One connection, every question, and one shutdown on every path.
async fn probe(config: &Config, want_ledger: bool) -> Result<Probe, ProbeFailure> {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .map_err(|e| ProbeFailure::NoServer(e.to_string()))?;
    let driver = compio::runtime::spawn(async move {
        let _ = connection.run().await;
    });

    let found = interrogate(&client, want_ledger).await;

    // Drop the client first so the driver is asked to shut down, then WAIT for
    // it rather than detaching: a detached driver parked on a read holds an
    // io_uring submission, and the submission holds the runtime's inner state
    // alive past `Runtime::drop`, stranding the ring, its eventfd and the
    // socket. `xtask/src/platform_db/admin.rs` carries the long form.
    //
    // It is BELOW the questions and not inside them so that an early return
    // from any of them still passes through here.
    drop(client);
    let _ = driver.await;
    found
}

/// The questions, on a connection that is already open.
async fn interrogate(client: &Client, want_ledger: bool) -> Result<Probe, ProbeFailure> {
    let rows = client
        .query(SCHEMAS_SQL, &[])
        .await
        .map_err(|e| ProbeFailure::asking("list the schemas this database holds", &e))?;
    let schemas = rows.iter().map(|row| row.get::<_, String>(0)).collect();
    let ledger = if want_ledger {
        count_journal(client).await?
    } else {
        Ledger::NotAsked
    };
    Ok(Probe { schemas, ledger })
}

/// Stage two's one question, asked in TWO round trips on purpose.
///
/// `PostgreSQL` resolves the relations a statement names at PARSE time, before
/// any `CASE` in that statement is evaluated. A single guarded statement, of
/// the form
///
/// ```text
/// SELECT CASE WHEN to_regclass($1) IS NULL THEN 0
///             ELSE (SELECT count(DISTINCT checksum) FROM <the journal>) END
/// ```
///
/// therefore does NOT return zero on a database without the table: it fails
/// outright with `42P01`, which reached the caller as "the server did not
/// answer" and printed the remedy for an unreachable backend.
///
/// `to_regclass` takes the name as a VALUE, so a statement that asks ONLY that
/// question names no relation and survives the table's absence. The count is
/// then issued only once the table is known to be there.
async fn count_journal(client: &Client) -> Result<Ledger, ProbeFailure> {
    let exists: bool = client
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&JOURNAL_TABLE])
        .await
        .map_err(|e| ProbeFailure::asking("say whether the migration journal exists", &e))?
        .get(0);
    if !exists {
        return Ok(Ledger::Absent);
    }
    // `JOURNAL_TABLE` is a literal in this file, never caller input; an
    // identifier cannot travel as a parameter, and spelling the name a second
    // time is how the two halves of this function would drift apart.
    let sql = format!(
        "SELECT count(DISTINCT checksum) FROM {JOURNAL_TABLE} WHERE event_kind = 'applied'"
    );
    let count: i64 = client
        .query_one(sql.as_str(), &[])
        .await
        .map_err(|e| ProbeFailure::asking("count the migrations its journal has applied", &e))?
        .get(0);
    Ok(Ledger::Applied(usize::try_from(count).unwrap_or(0)))
}

impl ProbeFailure {
    fn asking(question: &'static str, error: &compio_postgres::Error) -> Self {
        Self::Refused {
            question,
            error: error.to_string(),
        }
    }
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
        let parts = super::overlay::split_dsn(dsn);
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

/// The remediation for a server that answered and then refused a question.
///
/// IT IS NOT [`UNREACHABLE_REMEDY`], and the distinction is the whole point:
/// restarting a server that is already running changes nothing. What separates
/// this state is the ROLE - a DSN whose user cannot read `pg_namespace`, or
/// cannot select the journal, connects fine and fails at the first question.
const ROLE_REMEDY: &str = "\
    The server is up; the role in that DSN could not answer. Check that role\n\
    \x20   can read the catalog and the migration journal, or rewrite the\n\
    \x20   overlay from the backends this tree provisions:\n\
    \x20     tests/provision_test_backends.sh\n";

/// The remediation for a machine that cannot start an `io_uring` runtime.
///
/// It named the overlay until this was measured: no rewrite of a DSN makes a
/// kernel offer `io_uring`, and the reader was sent to edit a file that was not
/// the problem.
const RUNTIME_REMEDY: &str = "\
    This is the machine, not the database. Nothing in this tree runs without\n\
    \x20   io_uring; check the kernel permits it for this process:\n\
    \x20     sysctl kernel.io_uring_disabled\n";

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

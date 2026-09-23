//! The two-mode hardened authorizer.
//!
//! This is the **line-2 confinement** for SQLite migrations: the runtime analog
//! of the Postgres least-privilege `migrator` role. SQLite has no roles / GRANT /
//! `SET ROLE`, so confinement is enforced by a `Connection::authorizer` callback
//! that fires at **`prepare` time** for every statement compiled on the migration
//! connection, and a fail-closed deny matrix.
//!
//! # Two modes, one installed closure
//!
//! `Connection::authorizer(Some(F))` requires `F: FnMut(AuthContext) ->
//! Authorization + Send + 'static`. The mode is therefore an [`Arc<AtomicU8>`]
//! (which is `Send + 'static`) captured **by-move into the single closure
//! installed once at connection open** (`make_authorizer`); flipping the mode is
//! a plain `AuthMode::store` on the shared atomic - it never re-installs the
//! closure (impossible mid-`execute_batch`, which borrows the connection). An
//! `Rc<Cell<_>>` would NOT compile: `Rc`/`Cell` are not `Send`.
//!
//! - **`CreatorUp`** - the creator/AI `up` runs under this mode. The journal's
//!   own objects are immutable: all writes/DDL to a [`JOURNAL_PREFIX`]-named
//!   object are denied; ATTACH / DETACH / PRAGMA / load_extension / CREATE
//!   VTABLE/MODULE are denied; functions are allowlisted (fail-closed on unknown);
//!   creator-authored TRIGGER/VIEW bodies that name a journal object are denied at
//!   CREATE-prepare time, closing the defer-into-engine-mode hole: a body prepared
//!   under `CreatorUp` cannot wait for the engine's own mode to run its writes.
//! - **`EngineJournal`** - only the engine's own journal writes run here. Journal
//!   writes are allowed; ATTACH/DETACH/load_extension stay denied for life; a
//!   single `PRAGMA foreign_keys` toggle is allowed (the 12-step rebuild).
//!
//! # Matching the journal - the OBJECT NAME, not a database (CRITICAL precision)
//!
//! The journal lives in the app's own file, beside the creator's tables, so there
//! is no second database name to key on: `main` carries both. The fence is the
//! object NAME, and it is a PREFIX test ([`is_journal_object`]) - the same fence
//! PostgreSQL applies inside a tenant schema. A creator table whose name merely
//! CONTAINS `__zeroship_` is the creator's, and stays writable and visible; only
//! position zero counts.
//!
//! Every action that can reach a journal object carries that object's name on the
//! action itself (`journal_action_target` enumerates them), so the match is on the
//! per-action name and NOT on [`AuthContext::database_name`], which now says only
//! which file the statement touched and reads `main` for creator and journal
//! alike.

use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

/// The fence every object the migration journal owns inside the app file carries.
///
/// The journal shares the creator's database, so a name is the only thing
/// separating them. This is EXACTLY the prefix PostgreSQL's journal already
/// carries inside a tenant schema - `__zeroship_schema_migrations`,
/// `__zeroship_schema_migrations_inflight`, `__zeroship_schema_backfills`, ... - so
/// folding the SQLite journal in converges the two dialects on one spelling
/// rather than adding a second.
///
/// It is deliberately NARROWER than the platform-wide `__zeroship_`:
/// `__zeroship_audit_unmask` (the per-app unmask ledger) and the workflow
/// journal's `__zeroship_workflow_*` also live in app files and are NOT the
/// migration journal. Fencing them here would deny a creator `up` access it has
/// today, which is a different decision than this one.
///
/// A creator cannot claim a name behind it in the first place:
/// `zeroship_migrate_core::schema::query::validate_collection` refuses the wider
/// `__zeroship` at declaration. The fence is what makes that refusal load-bearing
/// rather than cosmetic.
///
/// This knowledge stays inside the engine. The ORM does not filter the journal
/// out of its catalog and does not need to: PostgreSQL's journal has always sat
/// in the app's own schema and flowed through `LiveSchema` untouched, because the
/// only production consumer (`protection_floor::floor_from_live`) keeps a table
/// solely for its masked or encrypted columns, and the journal has none.
pub(crate) const JOURNAL_PREFIX: &str = "__zeroship_schema_";

/// True iff `name` sits behind the journal's fence.
///
/// A PREFIX test, never a substring one: `notes__zeroship_schema_x` is a creator
/// table and must stay writable and visible, and so is the bare
/// `schema_migrations` this journal was called when a separate file gave it a
/// namespace of its own. Case-insensitive because SQLite compares unquoted
/// identifiers that way, so a `__ZEROSHIP_SCHEMA_` spelling would otherwise slip
/// past a byte-exact compare.
#[must_use]
pub(crate) fn is_journal_object(name: &str) -> bool {
    let fence = JOURNAL_PREFIX.as_bytes();
    let bytes = name.as_bytes();
    bytes.len() >= fence.len() && bytes[..fence.len()].eq_ignore_ascii_case(fence)
}

/// Whether this action names an object behind the journal's fence.
///
/// Every action that can create, drop, read or write a schema object carries that
/// object's name. An action naming TWO objects (an index or trigger and the table
/// it hangs off) is fenced when EITHER is fenced, so a creator can neither hang an
/// index or trigger on a journal table nor claim a fenced name for one of its own.
/// Actions that name no object (`Select`, `Pragma`, `Attach`, `Function`,
/// `Transaction`, ...) have their own arms in [`decide`] and answer `false` here
/// rather than being force-fitted into a name test.
fn journal_action_target(action: &AuthAction<'_>) -> bool {
    match action {
        AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::Read { table_name, .. }
        | AuthAction::CreateTable { table_name }
        | AuthAction::DropTable { table_name }
        | AuthAction::CreateTempTable { table_name }
        | AuthAction::DropTempTable { table_name }
        | AuthAction::Analyze { table_name }
        | AuthAction::AlterTable { table_name, .. }
        | AuthAction::CreateVtable { table_name, .. }
        | AuthAction::DropVtable { table_name, .. } => is_journal_object(table_name),
        AuthAction::CreateIndex {
            index_name,
            table_name,
        }
        | AuthAction::DropIndex {
            index_name,
            table_name,
        }
        | AuthAction::CreateTempIndex {
            index_name,
            table_name,
        }
        | AuthAction::DropTempIndex {
            index_name,
            table_name,
        } => is_journal_object(index_name) || is_journal_object(table_name),
        AuthAction::CreateTrigger {
            trigger_name,
            table_name,
        }
        | AuthAction::DropTrigger {
            trigger_name,
            table_name,
        }
        | AuthAction::CreateTempTrigger {
            trigger_name,
            table_name,
        }
        | AuthAction::DropTempTrigger {
            trigger_name,
            table_name,
        } => is_journal_object(trigger_name) || is_journal_object(table_name),
        AuthAction::CreateView { view_name }
        | AuthAction::DropView { view_name }
        | AuthAction::CreateTempView { view_name }
        | AuthAction::DropTempView { view_name } => is_journal_object(view_name),
        AuthAction::Reindex { index_name } => is_journal_object(index_name),
        _ => false,
    }
}

/// The connection's MAIN database name - the tenant app file. The app
/// file is opened as `main` (NOT attached under a separate alias), so the
/// creator-writable target SQLite names is the literal `"main"`. The app id
/// appears only in the file path, never as a SQL identifier. SQLite also passes
/// `None` for the main/temp namespace on some actions; both `Some("main")` and
/// `None` denote the creator-writable database.
pub(crate) const MAIN_DB: &str = "main";

/// The authorizer mode discriminants stored in the shared [`AuthMode`] atomic.
const MODE_CREATOR_UP: u8 = 0;
const MODE_ENGINE_JOURNAL: u8 = 1;

/// The two authorizer phases. Stored as a `u8` in an [`AtomicU8`] so the
/// flag is `Send + 'static` and can be captured by-move into the single installed
/// authorizer closure and flipped with a plain atomic store (no closure
/// re-install, no `.await` across the flip).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The creator/AI `up` phase: the journal is immutable, capabilities denied.
    CreatorUp,
    /// The engine's own journal-write phase: fenced writes allowed (journal only).
    EngineJournal,
}

impl Mode {
    const fn as_u8(self) -> u8 {
        match self {
            Mode::CreatorUp => MODE_CREATOR_UP,
            Mode::EngineJournal => MODE_ENGINE_JOURNAL,
        }
    }

    const fn from_u8(v: u8) -> Mode {
        // Fail-closed: any unexpected value is treated as the most-restrictive
        // CreatorUp mode (it can never silently grant engine privileges).
        match v {
            MODE_ENGINE_JOURNAL => Mode::EngineJournal,
            _ => Mode::CreatorUp,
        }
    }
}

/// The shared, `Send + 'static` mode flag. Cloned: one handle is moved
/// into the authorizer closure, the other stays with the actor so it can flip the
/// mode between the creator `up` and the journal write.
#[derive(Clone, Debug)]
pub(crate) struct AuthMode(Arc<AtomicU8>);

impl AuthMode {
    /// A fresh flag, defaulting to the most-restrictive `CreatorUp` (fail-closed:
    /// the connection starts confined; the engine opts into `EngineJournal` only
    /// for its own journal writes).
    pub(crate) fn new() -> Self {
        AuthMode(Arc::new(AtomicU8::new(Mode::CreatorUp.as_u8())))
    }

    /// Flip the mode. A plain synchronous atomic store - it does NOT touch the
    /// connection, so it is safe to call between (never inside) `prepare`/`execute`
    /// calls on the single migration connection. `SeqCst` because the flip orders
    /// strictly w.r.t. the statement prepares that read it on the same thread.
    pub(crate) fn store(&self, mode: Mode) {
        self.0.store(mode.as_u8(), Ordering::SeqCst);
    }

    /// The current mode, read at each `prepare`-time authorizer invocation.
    pub(crate) fn load(&self) -> Mode {
        Mode::from_u8(self.0.load(Ordering::SeqCst))
    }
}

/// One recorded DENY: what was refused, where, on whose behalf, and under which
/// mode. Owned `String`s because [`AuthContext`] borrows from SQLite's callback
/// frame and cannot outlive it.
///
/// This is DIAGNOSTIC ONLY. Recording happens strictly AFTER the decision is
/// computed and never feeds back into it: the deny matrix is unchanged, and a
/// failure to record can never turn a Deny into an Allow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Denial {
    /// The refused action, rendered in SQL-ish shape (`PRAGMA data_version`,
    /// `DROP TABLE "__zeroship_schema_migrations"`, `FUNCTION "load_extension"`).
    action: String,
    /// The OUTER `AuthContext::database_name` (`main` or absent).
    database: Option<String>,
    /// The inner-most trigger or view responsible, when the access came from a
    /// creator-authored body rather than top-level SQL.
    accessor: Option<String>,
    /// The mode that produced the decision -- the SAME value the deny matrix
    /// branched on, not a re-read of the flag.
    mode: Mode,
}

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} on ", self.action)?;
        match &self.database {
            Some(db) => write!(f, "{db:?}")?,
            // SQLite passes NO database name on some actions (an unqualified
            // connection-wide PRAGMA, for one). Reported as what it is rather than
            // as `main`: the deny matrix treats an absent name as the app file, but
            // the callback did not actually say `main`, and inventing it would put a
            // fact in the message that SQLite never supplied.
            None => write!(f, "<unqualified>")?,
        }
        if let Some(accessor) = &self.accessor {
            write!(f, ", via {accessor:?}")?;
        }
        write!(f, ", mode={:?}", self.mode)
    }
}

/// The connection's LAST denial slot, shared between the installed authorizer
/// closure and the actor that maps a failure into an error message.
///
/// `Arc<Mutex<_>>` for the same reason the mode is an `Arc<AtomicU8>`: the closure
/// must be `Send + 'static`, so an `Rc<RefCell<_>>` would not compile. The slot
/// holds only the most recent denial -- a statement is refused at the first DENY,
/// so the last one recorded is the one that failed the prepare.
#[derive(Clone, Debug)]
pub(crate) struct DenialLog(Arc<Mutex<Option<Denial>>>);

impl DenialLog {
    /// An empty slot, one per hardened connection.
    pub(crate) fn new() -> Self {
        DenialLog(Arc::new(Mutex::new(None)))
    }

    /// The recorded denial, if a DENY happened since the last clear.
    pub(crate) fn last(&self) -> Option<Denial> {
        self.guard().clone()
    }

    /// Empty the slot. The actor clears before AND after every statement so a
    /// denial can never be attached to a later, unrelated failure.
    pub(crate) fn clear(&self) {
        *self.guard() = None;
    }

    fn record(&self, denial: Denial) {
        *self.guard() = Some(denial);
    }

    /// Never `unwrap()`: this runs inside SQLite's authorizer callback and inside a
    /// `Drop`, both of which can execute while a panic is unwinding. A poisoned
    /// mutex only means an earlier panic happened while the slot was held, and the
    /// slot is a diagnostic string -- recovering the inner value is always sound,
    /// whereas panicking here would abort the process during an unwind.
    fn guard(&self) -> MutexGuard<'_, Option<Denial>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Render the refused action in a SQL-ish shape. Object names are quoted via
/// `{:?}` so an empty or whitespace-bearing identifier is still legible.
fn describe_action(action: &AuthAction<'_>) -> String {
    match action {
        AuthAction::Unknown { code, .. } => format!("unknown action code {code}"),
        AuthAction::CreateIndex {
            index_name,
            table_name,
        } => format!("CREATE INDEX {index_name:?} ON {table_name:?}"),
        AuthAction::CreateTable { table_name } => format!("CREATE TABLE {table_name:?}"),
        AuthAction::CreateTempIndex {
            index_name,
            table_name,
        } => format!("CREATE TEMP INDEX {index_name:?} ON {table_name:?}"),
        AuthAction::CreateTempTable { table_name } => format!("CREATE TEMP TABLE {table_name:?}"),
        AuthAction::CreateTempTrigger {
            trigger_name,
            table_name,
        } => format!("CREATE TEMP TRIGGER {trigger_name:?} ON {table_name:?}"),
        AuthAction::CreateTempView { view_name } => format!("CREATE TEMP VIEW {view_name:?}"),
        AuthAction::CreateTrigger {
            trigger_name,
            table_name,
        } => format!("CREATE TRIGGER {trigger_name:?} ON {table_name:?}"),
        AuthAction::CreateView { view_name } => format!("CREATE VIEW {view_name:?}"),
        AuthAction::Delete { table_name } => format!("DELETE FROM {table_name:?}"),
        AuthAction::DropIndex {
            index_name,
            table_name,
        } => format!("DROP INDEX {index_name:?} ON {table_name:?}"),
        AuthAction::DropTable { table_name } => format!("DROP TABLE {table_name:?}"),
        AuthAction::DropTempIndex {
            index_name,
            table_name,
        } => format!("DROP TEMP INDEX {index_name:?} ON {table_name:?}"),
        AuthAction::DropTempTable { table_name } => format!("DROP TEMP TABLE {table_name:?}"),
        AuthAction::DropTempTrigger {
            trigger_name,
            table_name,
        } => format!("DROP TEMP TRIGGER {trigger_name:?} ON {table_name:?}"),
        AuthAction::DropTempView { view_name } => format!("DROP TEMP VIEW {view_name:?}"),
        AuthAction::DropTrigger {
            trigger_name,
            table_name,
        } => format!("DROP TRIGGER {trigger_name:?} ON {table_name:?}"),
        AuthAction::DropView { view_name } => format!("DROP VIEW {view_name:?}"),
        AuthAction::Insert { table_name } => format!("INSERT INTO {table_name:?}"),
        // The pragma NAME is the whole diagnostic for the PRAGMA deny, so it is
        // rendered bare (`PRAGMA data_version`) rather than quoted, with the value
        // appended when the statement carried one.
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => match pragma_value {
            Some(value) => format!("PRAGMA {pragma_name}={value}"),
            None => format!("PRAGMA {pragma_name}"),
        },
        AuthAction::Read {
            table_name,
            column_name,
        } => format!("READ {table_name:?}.{column_name:?}"),
        AuthAction::Select => "SELECT".to_string(),
        AuthAction::Transaction { operation } => format!("TRANSACTION {operation:?}"),
        AuthAction::Update {
            table_name,
            column_name,
        } => format!("UPDATE {table_name:?}.{column_name:?}"),
        // The ATTACH filename is deliberately omitted: it is a filesystem path the
        // statement already names, and it adds no diagnostic the author lacks.
        AuthAction::Attach { .. } => "ATTACH".to_string(),
        AuthAction::Detach { database_name } => format!("DETACH {database_name:?}"),
        AuthAction::AlterTable {
            database_name,
            table_name,
        } => format!("ALTER TABLE {database_name:?}.{table_name:?}"),
        AuthAction::Reindex { index_name } => format!("REINDEX {index_name:?}"),
        AuthAction::Analyze { table_name } => format!("ANALYZE {table_name:?}"),
        AuthAction::CreateVtable {
            table_name,
            module_name,
        } => format!("CREATE VIRTUAL TABLE {table_name:?} USING {module_name:?}"),
        AuthAction::DropVtable {
            table_name,
            module_name,
        } => format!("DROP VIRTUAL TABLE {table_name:?} USING {module_name:?}"),
        AuthAction::Function { function_name } => format!("FUNCTION {function_name:?}"),
        AuthAction::Savepoint {
            operation,
            savepoint_name,
        } => format!("SAVEPOINT {operation:?} {savepoint_name:?}"),
        AuthAction::Recursive => "RECURSIVE".to_string(),
        // `AuthAction` is `#[non_exhaustive]`: a variant added by a future rusqlite
        // still records SOMETHING rather than dropping the denial on the floor.
        other => format!("{other:?}"),
    }
}

/// The fail-closed `SQLITE_FUNCTION` allowlist.
///
/// A blanket allow on `SQLITE_FUNCTION` cannot distinguish a benign built-in from
/// `load_extension` / `fts3_tokenizer` / a `vec_*` extension function, and vtable
/// modules issue internal SQL. So the callback allowlists by NAME and denies
/// everything else (`load_extension`, all `vec_*` in creator mode, unknown =>
/// Deny). The set is the deterministic built-ins the descriptor-generated DDL can
/// legitimately reference in defaults / CHECK expressions. Kept small and
/// auditable; MUST be kept in lockstep with the emitter's function set (closing
/// note - fail-closed: a new emitter function the allowlist lacks is DENIED).
///
/// `CURRENT_TIMESTAMP`/`CURRENT_DATE`/`CURRENT_TIME` are SQL keywords, but SQLite
/// still reports them through `SQLITE_FUNCTION` in some DML positions. They are
/// listed explicitly so engine-rendered fnSynth timestamp values can compile
/// under CreatorUp.
const FUNCTION_ALLOWLIST: &[&str] = &[
    "abs",
    "coalesce",
    "current_timestamp",
    "current_date",
    "current_time",
    "length",
    "lower",
    "upper",
    "nullif",
    "ifnull",
    "max", // 2-arg scalar form used in CHECK/defaults
    "min", // 2-arg scalar form used in CHECK/defaults
    "round",
    "trim",
    "ltrim",
    "rtrim",
    "substr",
    "replace",
    // Used by the engine's bounded splitPart lowering.
    "instr",
    // Used by portable date-part extraction.
    "strftime",
    "typeof",
    "hex",
    // `randomblob` is emitted only by the engine's exact SQLite UUIDv4 renderer.
    // It is a SQLite builtin with no extension load
    // or tenant escape; without it, a legitimate DB-evaluated UUID insert fails at
    // prepare time under CreatorUp.
    "randomblob",
    "quote",
    // `printf` / `format` are invoked INTERNALLY by SQLite when it rewrites a
    // table's schema during `ALTER TABLE ... ADD COLUMN` (and similar additive DDL)
    // on 3.51 - the authorizer fires `SQLITE_FUNCTION("printf")` for that internal
    // call, so denying it breaks a LEGITIMATE additive creator migration. They are
    // deterministic, sandboxed string-formatting builtins (no extension load, no
    // tenant escape), safe to allow in both modes. (Exposed by the first real
    // ADD COLUMN exercise; the allowlist predated any ADD COLUMN test.)
    "printf",
    "format",
    // `like` is invoked INTERNALLY by SQLite during `ALTER TABLE ... DROP COLUMN`
    // (and other schema rewrites) to scan trigger/view/CHECK bodies for references
    // to the altered object - so denying it breaks a LEGITIMATE additive DROP
    // COLUMN rollback. It is a deterministic, sandboxed pattern builtin (no
    // extension load, no tenant escape). `glob` is its sibling pattern builtin,
    // allowed for the same reason. (Both exposed by the DROP COLUMN rollback;
    // the allowlist predated any ALTER-rewrite test.)
    "like",
    "glob",
    "unlikely",
    "likelihood",
    "likely",
    // Window/aggregate functions the engine's OWN journal net-state queries use
    // (`applied`/`superseded_versions`/`latest_completed_checksums`). These are
    // deterministic built-ins; they are safe to allow in BOTH modes (the journal
    // reads happen under EngineJournal mode, but allowing them in CreatorUp too is
    // harmless - they cannot escape the tenant, and a creator CTE using ROW_NUMBER
    // over `app` tables is benign). They are NOT extension/`vec_*` functions.
    "row_number",
    "count",
    "sum",
    "avg",
    "exists",
];

/// The PRAGMAs the engine may issue in `EngineJournal` mode.
/// `foreign_keys` is the rebuild toggle and `foreign_key_check` is the rebuild's
/// orphan-row integrity gate; the rest are READ-ONLY schema
/// introspection the drift snapshot needs (they emit rows, mutate nothing).
/// Fail-closed: anything not listed (incl. `writable_schema`, `journal_mode`) is
/// denied even in engine mode.
fn is_engine_allowed_pragma(name: &str) -> bool {
    const ENGINE_PRAGMAS: &[&str] = &[
        "foreign_keys",
        // the 12-step rebuild's integrity check. `PRAGMA foreign_key_check`
        // works INSIDE a transaction (unlike `foreign_keys`, a no-op in a txn) and
        // reports orphaned rows; a non-empty result aborts the rebuild. It is
        // read-only (emits violation rows, mutates nothing). Engine-only - a creator
        // never reaches it (this list is only consulted in EngineJournal).
        "foreign_key_check",
        "table_info",
        "index_list",
        "index_info",
        "index_xinfo",
        "foreign_key_list",
    ];
    ENGINE_PRAGMAS.iter().any(|p| name.eq_ignore_ascii_case(p))
}

/// True iff `name` is a SQLite schema table (`sqlite_master` / `sqlite_temp_master`
/// and their legacy aliases). A write authorizer-event on these during an ALTER is
/// SQLite's own schema-edit mechanism (a DIRECT SQL write is already blocked by
/// `DEFENSIVE=ON` before the authorizer runs). Matched by exact name - never a
/// blanket `sqlite_%` - so a creator table named `sqlite_statx` cannot sneak in.
fn is_sqlite_schema_table(name: &str) -> bool {
    name.eq_ignore_ascii_case("sqlite_master")
        || name.eq_ignore_ascii_case("sqlite_temp_master")
        || name.eq_ignore_ascii_case("sqlite_schema")
        || name.eq_ignore_ascii_case("sqlite_temp_schema")
}

/// The INTERNAL SQLite functions the engine invokes (indirectly) when running an
/// `ALTER TABLE ... DROP/RENAME COLUMN` / `RENAME TABLE` (and similar additive
/// schema rewrites). SQLite's own ALTER machinery calls these as part of executing
/// the statement - they are NOT user-callable in any escape-relevant sense (they
/// operate on the connection's own schema text and are gated behind an ALTER the
/// authorizer already vets). Denying them breaks LEGITIMATE additive migrations /
/// rollbacks. Matched by exact name (a fixed, audited set), not a blanket
/// `sqlite_*` prefix, so an unknown `sqlite_*` function still fails closed.
fn is_internal_alter_helper(lower: &str) -> bool {
    const INTERNAL_ALTER_FNS: &[&str] = &[
        "sqlite_rename_test",
        "sqlite_rename_column",
        "sqlite_rename_table",
        "sqlite_rename_quotefix",
        "sqlite_drop_column",
    ];
    INTERNAL_ALTER_FNS.contains(&lower)
}

/// True iff `name` (case-insensitive) is on the function allowlist.
fn function_allowed(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    FUNCTION_ALLOWLIST.iter().any(|f| *f == lower)
}

/// Build the single authorizer closure to install via `conn.authorizer(Some(..))`.
///
/// The returned closure captures the [`AuthMode`] handle by-move and reads it on
/// every `prepare`-time invocation, branching the deny matrix on the current mode
/// It is the load-bearing line-2: the deny is at prepare, BEFORE
/// execution, for EVERY statement compiled on the connection - including
/// runtime-constructed SQL and the AI/raw path.
///
/// It also captures the [`DenialLog`] and writes the LAST DENY into it. The write
/// happens after [`decide`] has already returned, from the decision's own value, so
/// the deny matrix is untouched: `Exec("authorization denied")` gains a name for
/// what was refused without any action becoming more permitted.
pub(crate) fn make_authorizer(
    mode: AuthMode,
    denials: DenialLog,
) -> impl for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static {
    move |ctx: AuthContext<'_>| -> Authorization {
        // Read the mode ONCE and hand the same value to the decision and to the
        // record, so the reported mode is provably the one that was branched on.
        let current = mode.load();
        let decision = decide(current, &ctx);
        if decision == Authorization::Deny {
            denials.record(Denial {
                action: describe_action(&ctx.action),
                database: ctx.database_name.map(str::to_string),
                accessor: ctx.accessor.map(str::to_string),
                mode: current,
            });
        }
        decision
    }
}

/// The deny matrix under an explicitly-supplied mode. `authorize` is the
/// `AuthMode`-reading wrapper the unit tests drive; it is plain text rather than
/// a link because it is `cfg(test)` and rustdoc never compiles that.
///
/// `database_name` is the OUTER `AuthContext.database_name` - the attach alias
/// SQLite passes as the `xAuth` `zDb` argument. We match on it, never on a
/// per-action `database` field (which several variants lack).
fn decide(current: Mode, ctx: &AuthContext<'_>) -> Authorization {
    let db = ctx.database_name;
    // The journal is in the app file now, so `database_name` cannot separate it
    // from the creator's tables. The object NAME does - a prefix fence, checked on
    // the action's own name fields.
    let targets_journal = journal_action_target(&ctx.action);
    // The creator-writable database is `main` (the app file). SQLite names it
    // `Some("main")` on most actions and `None` on the main/temp namespace for a
    // few; both denote main here. ATTACH/DETACH are denied for life, so `main` is
    // the ONLY database this connection can ever name - any other database_name on
    // a write is a foreign alias that must never compile.
    let targets_main = db == Some(MAIN_DB) || db.is_none();

    match &ctx.action {
        // -- Capabilities denied in BOTH modes, for the connection's whole life --
        // ATTACH/DETACH closed by construction: the engine opens the one app file
        // as `main` and attaches nothing; after the authorizer is installed no
        // alias can be bound and none can be dropped - ever. That is what keeps
        // `main` the only database name any later action can carry, which is what
        // lets the foreign-alias arm below fail closed on anything else.
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,

        // PRAGMA: denied in CreatorUp, which closes the `writable_schema=ON`
        // forge. In EngineJournal, a SMALL allowlist is permitted:
        // - `foreign_keys` - the engine's toggle around the 12-step rebuild;
        // - the READ-ONLY schema-introspection pragmas the drift snapshot issues
        // (`table_info`/`index_list`/`index_info`/`foreign_key_list`).
        // These return rows and mutate nothing; they are the SQLite analog of
        // the PG drift path's `information_schema`/`pg_catalog` reads. They run
        // ONLY under engine mode (engine-private introspection); a creator can
        // never reach them.
        // Everything else (writable_schema, journal_mode, ...) stays denied in BOTH
        // modes (fail-closed).
        AuthAction::Pragma { pragma_name, .. } => match current {
            Mode::EngineJournal if is_engine_allowed_pragma(pragma_name) => Authorization::Allow,
            _ => Authorization::Deny,
        },

        // load_extension and any new vtable module: denied in both modes. (Belt
        // and suspenders alongside `load_extension_disable` at open)
        AuthAction::CreateVtable { module_name, .. } => match current {
            // `vec0` only, and ONLY in engine mode. Creator mode can never make a
            // vtable. `fts5` is not allowed: the engine authors no FTS DDL, so
            // conceding the capability would grant a create nothing asks for.
            Mode::EngineJournal if module_name.eq_ignore_ascii_case("vec0") => Authorization::Allow,
            _ => Authorization::Deny,
        },

        // SQLITE_FUNCTION allowlist. Fail-closed: unknown => Deny. `load_extension`
        // and all `vec_*` are simply absent from the allowlist => denied in creator
        // mode. In engine mode the engine's vector DDL may additionally call `vec_*`.
        AuthAction::Function { function_name } => {
            let lower = function_name.to_ascii_lowercase();
            // Allowed iff on the allowlist, OR (engine mode only) a `vec_*` function
            // for engine-emitted vector DDL, OR an internal SQLite ALTER-machinery
            // helper. Everything else (incl. load_extension, any `vec_*` in creator
            // mode, unknown) is fail-closed denied.
            let engine_vec = matches!(current, Mode::EngineJournal) && lower.starts_with("vec_");
            if function_allowed(function_name) || engine_vec || is_internal_alter_helper(&lower) {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }

        // Transaction control.
        //
        // The engine owns BEGIN IMMEDIATE / COMMIT / ROLLBACK and issues them under
        // EngineJournal mode (the phase sequence requires it: step 1 BEGIN,
        // step 6 COMMIT are engine operations). The **creator** `up` (CreatorUp
        // mode) may NOT open or close a transaction - that would break the single
        // atomic transaction wrapping the DDL + journal write. So:
        // - CreatorUp: DENY (creator cannot touch transaction boundaries)
        // - EngineJournal: ALLOW (the engine's own BEGIN/COMMIT/ROLLBACK)
        // SAVEPOINT is denied in both modes (the engine uses plain BEGIN/COMMIT,
        // never savepoints; a creator savepoint is never legitimate).
        AuthAction::Transaction { .. } => match current {
            Mode::EngineJournal => Authorization::Allow,
            Mode::CreatorUp => Authorization::Deny,
        },
        AuthAction::Savepoint { .. } => Authorization::Deny,

        // -- ALTER TABLE - key on the ACTION'S OWN database_name, NOT the outer one --
        // CRITICAL: `SQLITE_ALTER_TABLE` carries its target database in the
        // action's own `database_name` field; the OUTER `AuthContext.database_name`
        // (the `zDb` arg) is NOT the database for this action. For an
        // `ALTER TABLE ... DROP COLUMN` SQLite passes the dropped COLUMN name in the
        // outer field (RENAME COLUMN and ADD COLUMN pass NULL there); so the outer
        // field is unreliable for ALTER TABLE either way, and `targets_main`
        // computed from it would be wrong (false on a DROP COLUMN whose outer field
        // is the column name, false on the NULL of RENAME/ADD) - the generic
        // foreign-alias deny would then wrongly reject a legitimate
        // `ALTER TABLE main.<t>`. So we branch on the inner `database_name` here,
        // ahead of every generic write arm, with the journal's NAME fence taking
        // precedence over the database:
        // - a fenced table => journal immutability: engine-only (CreatorUp denied);
        // - `main` => a creator/engine table alter: allowed (additive ADD/DROP/
        // RENAME COLUMN - the additive rollback + apply path);
        // - other => foreign alias (impossible post-ATTACH-deny) => deny.
        AuthAction::AlterTable { database_name, .. } => {
            let inner_main = *database_name == MAIN_DB;
            if targets_journal {
                match current {
                    Mode::EngineJournal => Authorization::Allow,
                    Mode::CreatorUp => Authorization::Deny,
                }
            } else if inner_main {
                // A table alter on the app file. Allowed in both modes (the creator
                // `up`/`down` additive ADD/DROP/RENAME COLUMN; the engine's own
                // rebuild ALTERs in a later phase). Temp objects / analyze etc. are
                // handled by their own arms; this is strictly ALTER TABLE on main.
                Authorization::Allow
            } else {
                // Foreign alias - impossible once ATTACH/DETACH are denied for life,
                // but fail-closed anyway.
                Authorization::Deny
            }
        }

        // -- SQLite-internal schema-table writes during a vetted ALTER/DDL --
        // `ALTER TABLE ... DROP/RENAME COLUMN` (and other schema rewrites) make SQLite
        // INTERNALLY `Update`/`Insert`/`Delete` the schema tables
        // (`sqlite_master` / `sqlite_temp_master`) to apply the new schema. These
        // authorizer events are NOT a creator data write - a DIRECT
        // `UPDATE sqlite_master ...` from SQL is already blocked by `DEFENSIVE=ON`
        // (set at open) BEFORE the authorizer even sees it, so the ONLY way to reach
        // this event is SQLite's own ALTER machinery executing a statement the
        // authorizer already vetted. We therefore allow a write to a `sqlite_*master`
        // schema table on the `main`/`temp` namespace. Fail-closed: only the exact
        // schema-table names, only on main/temp.
        //
        // `sqlite_master` is NOT behind the journal fence and must not be: the
        // catalog is SQLite's, shared by creator and journal alike, and the thing
        // that keeps a creator out of the JOURNAL'S rows in it is `DEFENSIVE=ON`
        // (which blocks direct SQL writes before the authorizer runs), not a name.
        AuthAction::Insert { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::Delete { table_name }
            if (targets_main || db == Some("temp")) && is_sqlite_schema_table(table_name) =>
        {
            Authorization::Allow
        }

        // -- Journal immutability --
        // Direct writes / DDL to a fenced object are denied in CreatorUp and allowed
        // only in EngineJournal. Matched on the action's OWN name fields, because
        // the journal now shares `main` with the creator's tables and the database
        // name no longer distinguishes them.
        AuthAction::Insert { .. }
        | AuthAction::Update { .. }
        | AuthAction::Delete { .. }
        | AuthAction::DropTable { .. }
        | AuthAction::DropTrigger { .. }
        | AuthAction::DropIndex { .. }
        | AuthAction::DropView { .. }
            if targets_journal =>
        {
            match current {
                Mode::EngineJournal => Authorization::Allow,
                Mode::CreatorUp => Authorization::Deny,
            }
        }

        // CREATE TABLE/INDEX/TRIGGER/VIEW behind the fence: only the engine may
        // create journal objects (bootstrap). Creator mode denied - which is also
        // what stops a creator CLAIMING a fenced name before the engine bootstraps
        // it, the collision the separate file used to make impossible.
        AuthAction::CreateTable { .. }
        | AuthAction::CreateIndex { .. }
        | AuthAction::CreateTrigger { .. }
        | AuthAction::CreateView { .. }
            if targets_journal =>
        {
            match current {
                Mode::EngineJournal => Authorization::Allow,
                Mode::CreatorUp => Authorization::Deny,
            }
        }

        // -- Creator-authored TRIGGER/VIEW bodies that reach the journal --
        // The trigger/view's own target table is the creator's (so the arms above do
        // not fire on the CREATE itself), but each body statement is authorized at
        // the trigger/view's CREATE-prepare time with `accessor` naming the inner
        // trigger/view and the body's own table on the action. Under CreatorUp we
        // DENY any body access that names a fenced object, foreclosing the
        // defer-into-engine-mode vector at its root - the trigger is never created.
        // (The Insert/Update/Delete body writes are already denied by the
        // immutability arm above; this arm additionally denies a body that merely
        // READS the journal, since a creator object has no business referencing it.)
        action
            if targets_journal && ctx.accessor.is_some() && matches!(current, Mode::CreatorUp) =>
        {
            // A creator trigger/view (accessor set) touching the journal: deny.
            let _ = action;
            Authorization::Deny
        }

        // -- Temp objects denied in CreatorUp --
        // A creator `up` has no business creating temp tables/triggers/views/indexes
        // (they can hold cross-statement state, fire on app writes, or shadow journal
        // names). Deny them explicitly BY THE AUTHORIZER. Engine mode never
        // needs them either, so deny in both modes.
        AuthAction::CreateTempTable { .. }
        | AuthAction::CreateTempTrigger { .. }
        | AuthAction::CreateTempView { .. }
        | AuthAction::CreateTempIndex { .. } => Authorization::Deny,

        // -- Analyze denied in CreatorUp --
        // ANALYZE writes `sqlite_stat*` tables into the app db - net-new tables that
        // confound later drift detection (the snapshot would see them as out-of-band
        // objects). A migration's declared DDL has no business running ANALYZE; deny
        // it in creator mode. (Engine mode does not issue it either, but the deny is
        // scoped to CreatorUp so a future engine maintenance op is not foreclosed.)
        AuthAction::Analyze { .. } if matches!(current, Mode::CreatorUp) => Authorization::Deny,

        // -- Reindex on `main`/`temp` ALLOWED in CreatorUp --
        // `SQLITE_REINDEX` fires NOT ONLY for a standalone `REINDEX` statement but
        // also INTRINSICALLY as part of a legitimate `CREATE INDEX` (SQLite reindexes
        // the freshly-created index to populate it). The engine emits the three
        // policy-injected table indexes (`<table>_<col>_idx`) inside the creator
        // `up`'s CREATE TABLE payload, so the creator phase MUST be able to reindex
        // them. A REINDEX rebuilds an existing index B-tree: it creates no table,
        // changes no schema structure, and so does not confound drift (which compares
        // structure, not index physical layout). Allow it on the app file
        // (`main`/`temp`) in both modes, EXCEPT on a fenced index. NOTE: the
        // journal-immutability arm above does NOT cover `Reindex` (its match lists
        // Insert|Update|Delete|DropTable|DropTrigger|DropIndex|DropView only), so
        // the fenced case is handled here, ahead of the allow.
        //
        // EngineJournal keeps it: `CREATE INDEX` fires `SQLITE_REINDEX`
        // INTRINSICALLY with the new index's own name, so denying a fenced REINDEX
        // in both modes would stop the engine creating a fenced index of its own -
        // bootstrap would fail on the very statement that makes the object it is
        // protecting. Every other journal arm splits by mode for the same reason.
        AuthAction::Reindex { .. } if targets_journal => match current {
            Mode::EngineJournal => Authorization::Allow,
            Mode::CreatorUp => Authorization::Deny,
        },
        AuthAction::Reindex { .. } if targets_main || db == Some("temp") => Authorization::Allow,
        // LOAD-BEARING DENY (do NOT remove as "redundant"): any REINDEX naming a
        // foreign alias - impossible post-ATTACH-deny, but failed closed regardless.
        AuthAction::Reindex { .. } => Authorization::Deny,

        // -- Cross-tenant belt-and-suspenders --
        // Any WRITE whose database_name is not `main` (the app file) is denied. New
        // aliases can only appear via ATTACH (already denied), so in practice this
        // only ever sees `main`/None; the rule is here so a write that somehow named
        // a foreign alias cannot execute.
        AuthAction::Insert { .. }
        | AuthAction::Update { .. }
        | AuthAction::Delete { .. }
        | AuthAction::CreateTable { .. }
        | AuthAction::CreateIndex { .. }
        | AuthAction::CreateTrigger { .. }
        | AuthAction::CreateView { .. }
        | AuthAction::DropTable { .. }
        | AuthAction::DropTrigger { .. }
        | AuthAction::DropIndex { .. }
        | AuthAction::DropView { .. }
            if !targets_main =>
        {
            Authorization::Deny
        }

        // -- Total journal confinement in CreatorUp - the catch-all backstop --
        // A creator has NO business touching a fenced object in ANY way, including a
        // plain `SELECT ... FROM __zeroship_schema_migrations` (an
        // `AuthAction::Read` with `accessor: None`, which the trigger/view-body arm
        // above does NOT cover because that arm requires `accessor.is_some`).
        // Without this arm such a Read falls through to the `_ => Allow` catch-all
        // and the creator can read the immutable journal. Deny ANY action naming a
        // fenced object in CreatorUp (Read included), ahead of the catch-all.
        // EngineJournal is unaffected - the engine's own journal reads/writes are
        // allowed by the arms above and by the catch-all in engine mode.
        action if targets_journal && matches!(current, Mode::CreatorUp) => {
            let _ = action;
            Authorization::Deny
        }

        // -- Everything else: allowed (creator DDL/DML on `main`, SELECT/READ) --
        // CreateTable/CreateIndex/CreateTrigger/CreateView/DML on `main` (the app
        // file; database_name `Some("main")` or `None`) flow here, as do
        // SELECT/READ/Recursive. Reads are not a confinement concern (cross-tenant
        // reads are already impossible - no foreign alias is bound). Transaction
        // control, temp creates, and Analyze/Reindex are handled above. The engine's
        // own journal Reads (EngineJournal mode) also land here and are allowed.
        _ => Authorization::Allow,
    }
}

/// The deny-matrix decision driven through the shared mode flag, exactly as the
/// installed closure drives it (extracted so the matrix is unit-testable without a
/// live connection, though every claim is ALSO proven against a real temp-file
/// SQLite in `crates/zeroship-migrate/tests/policy_charter/sqlite_confinement.rs`).
#[cfg(test)]
fn authorize(mode: &AuthMode, ctx: &AuthContext<'_>) -> Authorization {
    decide(mode.load(), ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::hooks::TransactionOperation;

    fn ctx<'a>(
        action: AuthAction<'a>,
        db: Option<&'a str>,
        accessor: Option<&'a str>,
    ) -> AuthContext<'a> {
        AuthContext {
            action,
            database_name: db,
            accessor,
        }
    }

    /// The contexts the recorder is exercised over: a spread of denied and allowed
    /// actions across both modes, reused by the "changes no decision" proof.
    fn decision_matrix<'a>() -> Vec<AuthContext<'a>> {
        vec![
            ctx(
                AuthAction::Pragma {
                    pragma_name: "data_version",
                    pragma_value: None,
                },
                Some(MAIN_DB),
                None,
            ),
            ctx(
                AuthAction::Pragma {
                    pragma_name: "foreign_keys",
                    pragma_value: Some("OFF"),
                },
                None,
                None,
            ),
            ctx(
                AuthAction::Attach {
                    filename: "evil.db",
                },
                None,
                None,
            ),
            ctx(
                AuthAction::Function {
                    function_name: "load_extension",
                },
                None,
                None,
            ),
            ctx(
                AuthAction::Function {
                    function_name: "abs",
                },
                None,
                None,
            ),
            ctx(
                AuthAction::CreateTable {
                    table_name: "users",
                },
                Some(MAIN_DB),
                None,
            ),
            ctx(
                AuthAction::Insert {
                    table_name: "users",
                },
                Some(MAIN_DB),
                None,
            ),
            ctx(
                AuthAction::Insert {
                    table_name: "__zeroship_schema_migrations",
                },
                Some(MAIN_DB),
                None,
            ),
            ctx(
                AuthAction::Insert {
                    table_name: "__zeroship_schema_migrations",
                },
                Some(MAIN_DB),
                Some("creator_trg"),
            ),
            ctx(
                AuthAction::Read {
                    table_name: "__zeroship_schema_migrations",
                    column_name: "version",
                },
                Some(MAIN_DB),
                None,
            ),
            ctx(
                AuthAction::AlterTable {
                    database_name: MAIN_DB,
                    table_name: "users",
                },
                Some("nickname"),
                None,
            ),
            ctx(
                AuthAction::Reindex {
                    index_name: "ix_users",
                },
                Some(MAIN_DB),
                None,
            ),
            ctx(AuthAction::CreateTempTable { table_name: "t" }, None, None),
        ]
    }

    /// The denial recorder is DIAGNOSTIC ONLY. The installed closure returns
    /// exactly what the deny matrix returns for every context in both modes, and it
    /// records a denial for every Deny and nothing at all for an Allow.
    #[test]
    fn recording_a_denial_changes_no_decision() {
        let m = AuthMode::new();
        let log = DenialLog::new();
        let mut installed = make_authorizer(m.clone(), log.clone());
        for mode in [Mode::CreatorUp, Mode::EngineJournal] {
            m.store(mode);
            for case in decision_matrix() {
                let expected = authorize(&m, &case);
                log.clear();
                let actual = installed(case);
                assert_eq!(
                    actual, expected,
                    "the installed closure must return the deny-matrix decision unchanged \
                     for {case:?} in {mode:?}"
                );
                let recorded = log.last();
                match expected {
                    Authorization::Deny => assert!(
                        recorded.is_some(),
                        "a Deny must be recorded for {case:?} in {mode:?}"
                    ),
                    _ => assert!(
                        recorded.is_none(),
                        "an Allow must record nothing for {case:?} in {mode:?}, got {recorded:?}"
                    ),
                }
            }
        }
    }

    /// A recorded denial names the ACTION, the DATABASE, and the MODE, plus the
    /// responsible trigger/view when the access came from a creator-authored body.
    #[test]
    fn a_recorded_denial_names_the_action_database_and_mode() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        let log = DenialLog::new();
        let mut installed = make_authorizer(m.clone(), log.clone());

        // A denied pragma must name the database that it targeted.
        assert_eq!(
            installed(ctx(
                AuthAction::Pragma {
                    pragma_name: "data_version",
                    pragma_value: None
                },
                Some(MAIN_DB),
                None
            )),
            Authorization::Deny
        );
        assert_eq!(
            log.last().expect("the deny was recorded").to_string(),
            "PRAGMA data_version on \"main\", mode=CreatorUp"
        );

        // A creator trigger body writing the journal names the trigger too.
        log.clear();
        assert_eq!(
            installed(ctx(
                AuthAction::Insert {
                    table_name: "__zeroship_schema_migrations"
                },
                Some(MAIN_DB),
                Some("creator_trg")
            )),
            Authorization::Deny
        );
        assert_eq!(
            log.last().expect("the deny was recorded").to_string(),
            "INSERT INTO \"__zeroship_schema_migrations\" on \"main\", via \"creator_trg\", mode=CreatorUp"
        );

        // A pragma carrying a value keeps the value; the mode is the one in force.
        log.clear();
        m.store(Mode::EngineJournal);
        assert_eq!(
            installed(ctx(
                AuthAction::Pragma {
                    pragma_name: "writable_schema",
                    pragma_value: Some("ON")
                },
                None,
                None
            )),
            Authorization::Deny
        );
        assert_eq!(
            log.last().expect("the deny was recorded").to_string(),
            "PRAGMA writable_schema=ON on <unqualified>, mode=EngineJournal"
        );
    }

    #[test]
    fn attach_detach_denied_in_both_modes() {
        let m = AuthMode::new();
        for mode in [Mode::CreatorUp, Mode::EngineJournal] {
            m.store(mode);
            assert_eq!(
                authorize(&m, &ctx(AuthAction::Attach { filename: "x" }, None, None)),
                Authorization::Deny
            );
            assert_eq!(
                authorize(
                    &m,
                    &ctx(AuthAction::Detach { database_name: "x" }, None, None)
                ),
                Authorization::Deny
            );
        }
    }

    /// Every safe SQLite builtin emitted by the portable expression renderer is
    /// accepted in both modes. Extension loading and unknown functions remain
    /// denied.
    #[test]
    fn rendered_portable_functions_are_allow_listed() {
        let m = AuthMode::new();
        for mode in [Mode::CreatorUp, Mode::EngineJournal] {
            m.store(mode);
            for function_name in ["instr", "round", "replace", "strftime", "sum", "avg"] {
                assert_eq!(
                    authorize(
                        &m,
                        &ctx(AuthAction::Function { function_name }, None, None)
                    ),
                    Authorization::Allow,
                    "{function_name} is emitted by the portable renderer and must be allow-listed in {mode:?}"
                );
            }
            assert_eq!(
                authorize(
                    &m,
                    &ctx(
                        AuthAction::Function {
                            function_name: "load_extension"
                        },
                        None,
                        None
                    )
                ),
                Authorization::Deny,
                "load_extension stays denied"
            );
        }
        // Function names are matched case-insensitively.
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Function {
                        function_name: "REPLACE"
                    },
                    None,
                    None
                )
            ),
            Authorization::Allow
        );
    }

    #[test]
    fn pragma_denied_creator_foreign_keys_only_engine() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Pragma {
                        pragma_name: "writable_schema",
                        pragma_value: Some("1")
                    },
                    None,
                    None
                )
            ),
            Authorization::Deny
        );
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Pragma {
                        pragma_name: "foreign_keys",
                        pragma_value: Some("OFF")
                    },
                    None,
                    None
                )
            ),
            Authorization::Deny,
            "foreign_keys toggle is engine-only"
        );
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Pragma {
                        pragma_name: "foreign_keys",
                        pragma_value: Some("OFF")
                    },
                    None,
                    None
                )
            ),
            Authorization::Allow
        );
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Pragma {
                        pragma_name: "writable_schema",
                        pragma_value: Some("1")
                    },
                    None,
                    None
                )
            ),
            Authorization::Deny,
            "writable_schema denied even in engine mode"
        );
    }

    // `foreign_key_check` - the rebuild's orphan-row integrity gate - is
    // allowed in EngineJournal, denied in CreatorUp (a creator can never run the
    // rebuild integrity check; PRAGMA is denied outright in creator mode).
    #[test]
    fn foreign_key_check_engine_only() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Pragma {
                        pragma_name: "foreign_key_check",
                        pragma_value: None
                    },
                    Some(MAIN_DB),
                    None
                )
            ),
            Authorization::Deny,
            "foreign_key_check must be denied in creator mode"
        );
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Pragma {
                        pragma_name: "foreign_key_check",
                        pragma_value: None
                    },
                    Some(MAIN_DB),
                    None
                )
            ),
            Authorization::Allow,
            "foreign_key_check must be allowed in engine mode (rebuild integrity gate, P3b)"
        );
    }

    // The read-only introspection PRAGMAs the drift snapshot uses are allowed
    // in EngineJournal, denied in CreatorUp; writable_schema stays denied in both.
    #[test]
    fn introspection_pragmas_engine_only() {
        let m = AuthMode::new();
        for pragma in ["table_info", "index_list", "index_info", "foreign_key_list"] {
            m.store(Mode::CreatorUp);
            assert_eq!(
                authorize(
                    &m,
                    &ctx(
                        AuthAction::Pragma {
                            pragma_name: pragma,
                            pragma_value: Some("users")
                        },
                        Some(MAIN_DB),
                        None
                    )
                ),
                Authorization::Deny,
                "{pragma} must be denied in creator mode"
            );
            m.store(Mode::EngineJournal);
            assert_eq!(
                authorize(
                    &m,
                    &ctx(
                        AuthAction::Pragma {
                            pragma_name: pragma,
                            pragma_value: Some("users")
                        },
                        Some(MAIN_DB),
                        None
                    )
                ),
                Authorization::Allow,
                "{pragma} must be allowed in engine mode (drift introspection)"
            );
        }
        // writable_schema is NOT an introspection pragma - denied even in engine mode.
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Pragma {
                        pragma_name: "writable_schema",
                        pragma_value: Some("1")
                    },
                    None,
                    None
                )
            ),
            Authorization::Deny,
            "writable_schema stays denied in engine mode"
        );
    }

    // Unlisted pragmas are denied in both modes, regardless of database spelling.
    #[test]
    fn unlisted_data_version_pragma_is_denied_in_both_modes() {
        let m = AuthMode::new();
        for mode in [Mode::CreatorUp, Mode::EngineJournal] {
            m.store(mode);
            for db in [Some(MAIN_DB), None, Some(MAIN_DB)] {
                assert_eq!(
                    authorize(
                        &m,
                        &ctx(
                            AuthAction::Pragma {
                                pragma_name: "data_version",
                                pragma_value: None
                            },
                            db,
                            None
                        )
                    ),
                    Authorization::Deny,
                    "unlisted data_version on db={db:?} must be denied in {mode:?}"
                );
            }
            assert_eq!(
                authorize(
                    &m,
                    &ctx(
                        AuthAction::Pragma {
                            pragma_name: "DATA_VERSION",
                            pragma_value: None
                        },
                        Some(MAIN_DB),
                        None
                    )
                ),
                Authorization::Deny,
                "pragma matching remains case-insensitive"
            );
            assert_eq!(
                authorize(
                    &m,
                    &ctx(
                        AuthAction::Pragma {
                            pragma_name: "data_version_x",
                            pragma_value: None
                        },
                        Some(MAIN_DB),
                        None
                    )
                ),
                Authorization::Deny,
                "the arm matches the pragma name exactly, not by prefix"
            );
        }
    }

    #[test]
    fn journal_writes_denied_in_creator_allowed_in_engine() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        for action in [
            AuthAction::Insert {
                table_name: "__zeroship_schema_migrations",
            },
            AuthAction::Update {
                table_name: "__zeroship_schema_migrations",
                column_name: "checksum",
            },
            AuthAction::Delete {
                table_name: "__zeroship_schema_migrations",
            },
            AuthAction::DropTable {
                table_name: "__zeroship_schema_migrations",
            },
            AuthAction::DropTrigger {
                trigger_name: "__zeroship_schema_migrations_immutable_delete",
                table_name: "__zeroship_schema_migrations",
            },
        ] {
            assert_eq!(
                authorize(&m, &ctx(action, Some(MAIN_DB), None)),
                Authorization::Deny
            );
        }
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Insert {
                        table_name: "__zeroship_schema_migrations"
                    },
                    Some(MAIN_DB),
                    None
                )
            ),
            Authorization::Allow
        );
    }

    // ALTER TABLE keys on the action's OWN database_name, not the outer one.
    // `ALTER TABLE main.<t>` (additive ADD/DROP/RENAME COLUMN) is allowed even when
    // the OUTER database_name carries the column name (SQLite's quirk); a fenced
    // table is engine-only.
    #[test]
    fn alter_table_keys_on_inner_database_name() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        // ADD/DROP COLUMN on main: the OUTER db is the COLUMN name (the quirk we fix),
        // but the inner database_name is "main" => allowed.
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::AlterTable {
                        database_name: MAIN_DB,
                        table_name: "users"
                    },
                    Some("nickname"),
                    None
                )
            ),
            Authorization::Allow,
            "ALTER TABLE main.users must be allowed regardless of the outer database_name"
        );
        // ALTER TABLE on a fenced table is journal tampering => denied in creator mode.
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::AlterTable {
                        database_name: MAIN_DB,
                        table_name: "__zeroship_schema_migrations"
                    },
                    None,
                    None
                )
            ),
            Authorization::Deny,
            "ALTER TABLE on a journal table must be denied in creator mode"
        );
        // Engine mode may alter the journal (the 12-step rebuild's engine ALTERs, later phase).
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::AlterTable {
                        database_name: MAIN_DB,
                        table_name: "__zeroship_schema_migrations"
                    },
                    None,
                    None
                )
            ),
            Authorization::Allow
        );
    }

    #[test]
    fn function_allowlist_fail_closed() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Function {
                        function_name: "abs"
                    },
                    None,
                    None
                )
            ),
            Authorization::Allow
        );
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Function {
                        function_name: "load_extension"
                    },
                    None,
                    None
                )
            ),
            Authorization::Deny
        );
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Function {
                        function_name: "vec_distance_cosine"
                    },
                    None,
                    None
                )
            ),
            Authorization::Deny,
            "vec_* denied in creator mode"
        );
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Function {
                        function_name: "totally_unknown_fn"
                    },
                    None,
                    None
                )
            ),
            Authorization::Deny,
            "unknown function fail-closed"
        );
    }

    #[test]
    fn creator_trigger_body_targeting_the_journal_denied() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        // A trigger body INSERT into the journal with an accessor naming the creator's
        // trigger - denied at CREATE-prepare time.
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Insert {
                        table_name: "__zeroship_schema_migrations"
                    },
                    Some(MAIN_DB),
                    Some("creator_trg")
                )
            ),
            Authorization::Deny
        );
    }

    #[test]
    fn vtable_and_transaction_denied() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::CreateVtable {
                        table_name: "v",
                        module_name: "vec0"
                    },
                    Some(MAIN_DB),
                    None
                )
            ),
            Authorization::Deny
        );
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Transaction {
                        operation: TransactionOperation::Begin
                    },
                    None,
                    None
                )
            ),
            Authorization::Deny
        );
    }

    #[test]
    fn app_ddl_allowed() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        // Creator DDL/DML lands in `main` (the app file): SQLite names it
        // Some("main") on most actions and None on the main namespace for a few.
        for db in [Some(MAIN_DB), None] {
            assert_eq!(
                authorize(
                    &m,
                    &ctx(
                        AuthAction::CreateTable {
                            table_name: "users"
                        },
                        db,
                        None
                    )
                ),
                Authorization::Allow,
                "CREATE TABLE on main (db={db:?}) must be allowed in creator mode"
            );
            assert_eq!(
                authorize(
                    &m,
                    &ctx(
                        AuthAction::Insert {
                            table_name: "users"
                        },
                        db,
                        None
                    )
                ),
                Authorization::Allow,
                "INSERT on main (db={db:?}) must be allowed in creator mode"
            );
        }
    }

    // temp objects are denied BY THE AUTHORIZER (not incidentally), in both modes.
    #[test]
    fn temp_objects_denied_by_authorizer() {
        let m = AuthMode::new();
        for mode in [Mode::CreatorUp, Mode::EngineJournal] {
            m.store(mode);
            for action in [
                AuthAction::CreateTempTable { table_name: "t" },
                AuthAction::CreateTempTrigger {
                    trigger_name: "g",
                    table_name: "t",
                },
                AuthAction::CreateTempView { view_name: "v" },
                AuthAction::CreateTempIndex {
                    index_name: "i",
                    table_name: "t",
                },
            ] {
                assert_eq!(
                    authorize(&m, &ctx(action, None, None)),
                    Authorization::Deny,
                    "temp create must be denied by the authorizer (mode={mode:?})"
                );
            }
        }
    }

    // DROP VIEW on a fenced view is denied (DropView is on the immutability arm),
    // and a creator's own view of the same shape is NOT - the control that makes
    // the deny attributable to the fence rather than to DropView being denied flat.
    #[test]
    fn drop_view_on_the_journal_denied_in_creator() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::DropView {
                        view_name: "__zeroship_schema_some_view"
                    },
                    Some(MAIN_DB),
                    None
                )
            ),
            Authorization::Deny,
            "DROP VIEW on a journal view must be denied"
        );
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::DropView {
                        view_name: "some_view"
                    },
                    Some(MAIN_DB),
                    None
                )
            ),
            Authorization::Allow,
            "a creator's own view stays the creator's to drop"
        );
    }

    // ANALYZE is denied in CreatorUp (it writes net-new sqlite_stat* tables that
    // confound drift detection).
    #[test]
    fn analyze_denied_in_creator() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Analyze {
                        table_name: "users"
                    },
                    Some(MAIN_DB),
                    None
                )
            ),
            Authorization::Deny,
            "ANALYZE must be denied in creator mode"
        );
    }

    // REINDEX on `main`/`temp` is ALLOWED in CreatorUp - it fires
    // intrinsically as part of a legitimate `CREATE INDEX` (which the engine emits
    // for the platform system-field indexes inside the creator `up`). It rebuilds an
    // existing index B-tree: no new table and no schema-structure change.
    #[test]
    fn reindex_on_main_allowed_in_creator() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Reindex {
                        index_name: "ix_users"
                    },
                    Some(MAIN_DB),
                    None
                )
            ),
            Authorization::Allow,
            "REINDEX on main must be allowed (intrinsic to CREATE INDEX)"
        );
        // None database (main/temp namespace) is also the app file.
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Reindex {
                        index_name: "ix_users"
                    },
                    None,
                    None
                )
            ),
            Authorization::Allow,
            "REINDEX with None db (main) must be allowed"
        );
    }

    // a creator `up` doing a plain `SELECT ... FROM __zeroship_schema_migrations` is
    // a `Read { accessor: None }`. The trigger/view-body arm requires
    // `accessor.is_some`, so this must be DENIED in CreatorUp rather than falling
    // through to the `_ => Allow` catch-all - while the engine's own journal reads
    // (EngineJournal mode) stay allowed.
    #[test]
    fn creator_read_of_the_journal_denied_engine_read_allowed() {
        let m = AuthMode::new();
        // Creator mode: a bare Read on the journal (no accessor) must be denied.
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Read {
                        table_name: "__zeroship_schema_migrations",
                        column_name: "version",
                    },
                    Some(MAIN_DB),
                    None
                )
            ),
            Authorization::Deny,
            "creator SELECT FROM __zeroship_schema_migrations must be denied"
        );
        // The engine's own journal reads (EngineJournal mode) stay allowed.
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Read {
                        table_name: "__zeroship_schema_migrations",
                        column_name: "version",
                    },
                    Some(MAIN_DB),
                    None
                )
            ),
            Authorization::Allow,
            "engine journal Read must stay allowed"
        );
    }

    // A REINDEX naming a fenced index is denied in CreatorUp and allowed in
    // EngineJournal. NOTE: the journal-immutability arm does NOT list `Reindex`,
    // so both halves come from the dedicated `Reindex if targets_journal` arm
    // placed AHEAD of the main/temp allow. Without that ordering a fenced REINDEX
    // would be allowed in creator mode too, because its database IS `main` now.
    #[test]
    fn reindex_on_the_journal_is_creator_denied_and_engine_allowed() {
        let m = AuthMode::new();
        let case = || {
            ctx(
                AuthAction::Reindex {
                    index_name: "__zeroship_schema_migrations_ix",
                },
                Some(MAIN_DB),
                None,
            )
        };
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(&m, &case()),
            Authorization::Deny,
            "a creator may not reindex the journal"
        );
        // The engine must keep it: CREATE INDEX fires SQLITE_REINDEX intrinsically
        // with the new index's name, so a both-modes deny would stop the engine
        // creating a fenced index at all.
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(&m, &case()),
            Authorization::Allow,
            "the engine reindexes its own objects, including on CREATE INDEX"
        );
    }

    /// The fence is a PREFIX, and the arms above would all pass for a substring
    /// filter too. This is what separates them: names that merely CONTAIN the fence
    /// are the creator's, and so is the bare `schema_migrations` the SQLite journal
    /// carried while a separate file gave it a namespace of its own.
    #[test]
    fn a_name_that_merely_contains_the_fence_is_the_creators() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        for table_name in [
            "schema_migrations",
            "notes__zeroship_schema_migrations",
            "_zeroship_schema_migrations",
            "zeroship_schema_migrations",
            "__zeroship_audit_unmask",
            "__zeroship_workflow_runs",
        ] {
            assert!(
                !is_journal_object(table_name),
                "{table_name} is not behind the migration journal's fence"
            );
            assert_eq!(
                authorize(
                    &m,
                    &ctx(AuthAction::Insert { table_name }, Some(MAIN_DB), None)
                ),
                Authorization::Allow,
                "a creator `up` must still be able to write {table_name}"
            );
        }
        // The positive half, so the arm cannot pass by fencing nothing at all.
        for table_name in [
            "__zeroship_schema_migrations",
            "__ZEROSHIP_SCHEMA_MIGRATIONS",
            "__zeroship_schema_backfills",
        ] {
            assert!(is_journal_object(table_name), "{table_name} is fenced");
            assert_eq!(
                authorize(
                    &m,
                    &ctx(AuthAction::Insert { table_name }, Some(MAIN_DB), None)
                ),
                Authorization::Deny,
                "a creator `up` must not write {table_name}"
            );
        }
    }
}

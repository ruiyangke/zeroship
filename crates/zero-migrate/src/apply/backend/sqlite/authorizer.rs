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
//! installed once at connection open** ([`make_authorizer`]); flipping the mode is
//! a plain [`AuthMode::store`] on the shared atomic — it never re-installs the
//! closure (impossible mid-`execute_batch`, which borrows the connection). An
//! `Rc<Cell<_>>` would NOT compile: `Rc`/`Cell` are not `Send`.
//!
//! - **`CreatorUp`** — the creator/AI `up` runs under this mode. The journal
//! schema `_mig` is immutable: all writes/DDL to `_mig` are denied; ATTACH /
//! DETACH / PRAGMA / load_extension / CREATE VTABLE/MODULE are denied; functions
//! are allowlisted (fail-closed on unknown); creator-authored TRIGGER/VIEW
//! bodies that target `_mig` are denied at CREATE-prepare time (closing the
//! defer-into-engine-mode hole item 6).
//! - **`EngineJournal`** — only the engine's own journal writes run here. `_mig`
//! writes are allowed (the journal tables only); ATTACH/DETACH/load_extension
//! stay denied for life; a single `PRAGMA foreign_keys` toggle is allowed (the
//! 12-step rebuild).
//!
//! # Matching `_mig` — the OUTER context field (CRITICAL precision)
//!
//! The attach alias is carried on the OUTER [`AuthContext::database_name`] (the
//! 5th `xAuth` `zDb` argument SQLite passes), NOT on a per-action field:
//! `AuthAction::DropTable { table_name }` / `DropTrigger {.. }` carry no database
//! field. So the deny keys on `ctx.database_name == Some(MIG_ALIAS)`, never a
//! pattern-matched `DropTable.database`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

/// The fixed attach alias of the journal database. A fixed ASCII
/// literal — never the (hyphenated-UUID) app id — so it is quote-safe and the
/// authorizer match is a trivial string compare.
pub(crate) const MIG_ALIAS: &str = "_mig";

/// The connection's MAIN database name — the tenant app file. The app
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
    /// The creator/AI `up` phase: `_mig` is immutable, capabilities denied.
    CreatorUp,
    /// The engine's own journal-write phase: `_mig` writes allowed (journal only).
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

    /// Flip the mode. A plain synchronous atomic store — it does NOT touch the
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

/// The fail-closed `SQLITE_FUNCTION` allowlist.
///
/// A blanket allow on `SQLITE_FUNCTION` cannot distinguish a benign built-in from
/// `load_extension` / `fts3_tokenizer` / a `vec_*` extension function, and vtable
/// modules issue internal SQL. So the callback allowlists by NAME and denies
/// everything else (`load_extension`, all `vec_*` in creator mode, unknown ⇒
/// Deny). The set is the deterministic built-ins the descriptor-generated DDL can
/// legitimately reference in defaults / CHECK expressions. Kept small and
/// auditable; MUST be kept in lockstep with the emitter's function set (closing
/// note — fail-closed: a new emitter function the allowlist lacks is DENIED).
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
    // table's schema during `ALTER TABLE … ADD COLUMN` (and similar additive DDL)
    // on 3.51 — the authorizer fires `SQLITE_FUNCTION("printf")` for that internal
    // call, so denying it breaks a LEGITIMATE additive creator migration. They are
    // deterministic, sandboxed string-formatting builtins (no extension load, no
    // tenant escape), safe to allow in both modes. (Exposed by the first real
    // ADD COLUMN exercise; the allowlist predated any ADD COLUMN test.)
    "printf",
    "format",
    // `like` is invoked INTERNALLY by SQLite during `ALTER TABLE … DROP COLUMN`
    // (and other schema rewrites) to scan trigger/view/CHECK bodies for references
    // to the altered object — so denying it breaks a LEGITIMATE additive DROP
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
    // harmless — they cannot escape the tenant, and a creator CTE using ROW_NUMBER
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
        // read-only (emits violation rows, mutates nothing). Engine-only — a creator
        // never reaches it (PRAGMA is denied outright in CreatorUp).
        "foreign_key_check",
        "table_info",
        "index_list",
        "index_info",
        "foreign_key_list",
    ];
    ENGINE_PRAGMAS.iter().any(|p| name.eq_ignore_ascii_case(p))
}

/// True iff `name` is a SQLite schema table (`sqlite_master` / `sqlite_temp_master`
/// and their legacy aliases). A write authorizer-event on these during an ALTER is
/// SQLite's own schema-edit mechanism (a DIRECT SQL write is already blocked by
/// `DEFENSIVE=ON` before the authorizer runs). Matched by exact name — never a
/// blanket `sqlite_%` — so a creator table named `sqlite_statx` cannot sneak in.
fn is_sqlite_schema_table(name: &str) -> bool {
    name.eq_ignore_ascii_case("sqlite_master")
        || name.eq_ignore_ascii_case("sqlite_temp_master")
        || name.eq_ignore_ascii_case("sqlite_schema")
        || name.eq_ignore_ascii_case("sqlite_temp_schema")
}

/// The INTERNAL SQLite functions the engine invokes (indirectly) when running an
/// `ALTER TABLE … DROP/RENAME COLUMN` / `RENAME TABLE` (and similar additive
/// schema rewrites). SQLite's own ALTER machinery calls these as part of executing
/// the statement — they are NOT user-callable in any escape-relevant sense (they
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
/// execution, for EVERY statement compiled on the connection — including
/// runtime-constructed SQL and the AI/raw path.
pub(crate) fn make_authorizer(
    mode: AuthMode,
) -> impl for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static {
    move |ctx: AuthContext<'_>| -> Authorization { authorize(&mode, &ctx) }
}

/// The pure deny-matrix decision (extracted so it is unit-testable without a live
/// connection — though every claim is ALSO proven against a real temp-file SQLite
/// in `tests/sqlite_confinement.rs`).
///
/// `database_name` is the OUTER `AuthContext.database_name` — the attach alias
/// SQLite passes as the `xAuth` `zDb` argument. We match on it, never on a
/// per-action `database` field (which several variants lack).
fn authorize(mode: &AuthMode, ctx: &AuthContext<'_>) -> Authorization {
    let current = mode.load();
    let db = ctx.database_name;
    let targets_mig = db == Some(MIG_ALIAS);
    // The creator-writable database is `main` (the app file). SQLite names it
    // `Some("main")` on most actions and `None` on the main/temp namespace for a
    // few; both denote main here. ATTACH/DETACH are denied for life, so no alias
    // other than `main`/`_mig` can ever exist on this connection — any other
    // database_name on a write is a foreign alias that must never compile.
    let targets_main = db == Some(MAIN_DB) || db.is_none();

    match &ctx.action {
        // -- Capabilities denied in BOTH modes, for the connection's whole life --
        // ATTACH/DETACH closed by construction: the engine attaches the
        // one app + the journal BEFORE installing this authorizer; after install,
        // no new alias can be bound and none can be dropped — ever.
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,

        // PRAGMA: denied in CreatorUp (closes the `writable_schema=ON` forge,
        // item 3). In EngineJournal, a SMALL allowlist is permitted:
        // - `foreign_keys` — the engine's toggle around the 12-step rebuild;
        // - the READ-ONLY schema-introspection pragmas the drift snapshot issues
        // (`table_info`/`index_list`/`index_info`/`foreign_key_list`).
        // These return rows and mutate nothing; they are the SQLite analog of
        // the PG drift path's `information_schema`/`pg_catalog` reads. They run
        // ONLY under engine mode (engine-private introspection); a creator can
        // never reach them (PRAGMA stays denied outright in CreatorUp).
        // Everything else (writable_schema, journal_mode, …) stays denied in BOTH
        // modes (fail-closed).
        AuthAction::Pragma { pragma_name, .. } => match current {
            Mode::EngineJournal if is_engine_allowed_pragma(pragma_name) => Authorization::Allow,
            _ => Authorization::Deny,
        },

        // load_extension and any new vtable module: denied in both modes. (Belt
        // and suspenders alongside `load_extension_disable` at open)
        AuthAction::CreateVtable { module_name, .. } => match current {
            // Engine-emitted goodie DDL may create an fts5/vec0 vtable, ONLY in
            // engine mode. Creator mode can never make a vtable.
            Mode::EngineJournal
                if module_name.eq_ignore_ascii_case("fts5")
                    || module_name.eq_ignore_ascii_case("vec0") =>
            {
                Authorization::Allow
            }
            _ => Authorization::Deny,
        },

        // SQLITE_FUNCTION allowlist. Fail-closed: unknown ⇒ Deny. `load_extension`
        // and all `vec_*` are simply absent from the allowlist ⇒ denied in creator
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
        // mode) may NOT open or close a transaction — that would break the single
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

        // -- ALTER TABLE — key on the ACTION'S OWN database_name, NOT the outer one --
        // CRITICAL: `SQLITE_ALTER_TABLE` carries its target database in the
        // action's own `database_name` field; the OUTER `AuthContext.database_name`
        // (the `zDb` arg) is NOT the database for this action. For an
        // `ALTER TABLE … DROP COLUMN` SQLite passes the dropped COLUMN name in the
        // outer field (RENAME COLUMN and ADD COLUMN pass NULL there); so the outer
        // field is unreliable for ALTER TABLE either way, and `targets_main`/
        // `targets_mig` computed from it would be wrong (false on a DROP COLUMN whose
        // outer field is the column name, false on the NULL of RENAME/ADD) — the
        // generic foreign-alias deny would then wrongly reject a legitimate
        // `ALTER TABLE main.<t>`. So we branch on the inner `database_name` here,
        // ahead of every generic write arm:
        // - `_mig` ⇒ journal immutability: engine-only (CreatorUp denied);
        // - `main` ⇒ a creator/engine table alter: allowed (additive ADD/DROP/
        // RENAME COLUMN — the additive rollback + apply path);
        // - other ⇒ foreign alias (impossible post-ATTACH-deny) ⇒ deny.
        AuthAction::AlterTable { database_name, .. } => {
            let inner_mig = *database_name == MIG_ALIAS;
            let inner_main = *database_name == MAIN_DB;
            if inner_mig {
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
                // Foreign alias — impossible once ATTACH/DETACH are denied for life,
                // but fail-closed anyway.
                Authorization::Deny
            }
        }

        // -- SQLite-internal schema-table writes during a vetted ALTER/DDL --
        // `ALTER TABLE … DROP/RENAME COLUMN` (and other schema rewrites) make SQLite
        // INTERNALLY `Update`/`Insert`/`Delete` the schema tables
        // (`sqlite_master` / `sqlite_temp_master`) to apply the new schema. These
        // authorizer events are NOT a creator data write — a DIRECT
        // `UPDATE sqlite_master …` from SQL is already blocked by `DEFENSIVE=ON`
        // (set at open) BEFORE the authorizer even sees it, so the ONLY way to reach
        // this event is SQLite's own ALTER machinery executing a statement the
        // authorizer already vetted. We therefore allow a write to a `sqlite_*master`
        // schema table on the `main`/`temp` namespace (NEVER `_mig`, which is handled
        // below and stays engine-only). Fail-closed: only the exact schema-table
        // names, only on main/temp, never `_mig`.
        //
        // Defense-in-depth: the guard matches the comment's intent EXACTLY —
        // `targets_main` (the app file: `Some("main")` or `None`) OR the `temp`
        // namespace — rather than the looser `!targets_mig` (which would also admit
        // a foreign alias, impossible post-ATTACH-deny but no reason to leave the
        // door ajar).
        AuthAction::Insert { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::Delete { table_name }
            if (targets_main || db == Some("temp")) && is_sqlite_schema_table(table_name) =>
        {
            Authorization::Allow
        }

        // -- Journal immutability on `_mig` --
        // Direct writes / DDL to `_mig` are denied in CreatorUp and allowed only in
        // EngineJournal (and only the journal tables exist there). Match the OUTER
        // database_name — DropTable/DropTrigger carry no per-action database field.
        AuthAction::Insert { .. }
        | AuthAction::Update { .. }
        | AuthAction::Delete { .. }
        | AuthAction::DropTable { .. }
        | AuthAction::DropTrigger { .. }
        | AuthAction::DropIndex { .. }
        | AuthAction::DropView { .. }
            if targets_mig =>
        {
            match current {
                Mode::EngineJournal => Authorization::Allow,
                Mode::CreatorUp => Authorization::Deny,
            }
        }

        // CREATE TABLE/INDEX/TRIGGER/VIEW that LAND IN `_mig`: only the engine may
        // create journal objects (bootstrap). Creator mode denied.
        AuthAction::CreateTable { .. }
        | AuthAction::CreateIndex { .. }
        | AuthAction::CreateTrigger { .. }
        | AuthAction::CreateView { .. }
            if targets_mig =>
        {
            match current {
                Mode::EngineJournal => Authorization::Allow,
                Mode::CreatorUp => Authorization::Deny,
            }
        }

        // -- Creator-authored TRIGGER/VIEW bodies that WRITE `_mig` --
        // The trigger/view target table is `app` (so the outer match above does not
        // fire on the CREATE itself), but each body statement is authorized at the
        // trigger/view's own CREATE-prepare time with `accessor` naming the inner
        // trigger/view and `database_name == Some("_mig")`. Under CreatorUp we DENY
        // any body access whose database_name is `_mig` (and an `accessor` is set),
        // foreclosing the defer-into-engine-mode vector at its root — the trigger is
        // never created. This is the catch-all for body Read/Select on `_mig` too.
        // (The Insert/Update/Delete-on-`_mig` body writes are already denied by the
        // immutability arm above; this arm additionally denies a body that merely
        // READS `_mig`, since a creator object has no business referencing it.)
        action if targets_mig && ctx.accessor.is_some() && matches!(current, Mode::CreatorUp) => {
            // A creator trigger/view (accessor set) touching `_mig` in any way: deny.
            let _ = action;
            Authorization::Deny
        }

        // -- Temp objects denied in CreatorUp --
        // A creator `up` has no business creating temp tables/triggers/views/indexes
        // (they can hold cross-statement state, fire on app writes, or shadow journal
        // names). These were only INCIDENTALLY blocked before via the temp-master
        // Insert ordering; deny them explicitly BY THE AUTHORIZER. Engine mode never
        // needs them either, so deny in both modes.
        AuthAction::CreateTempTable { .. }
        | AuthAction::CreateTempTrigger { .. }
        | AuthAction::CreateTempView { .. }
        | AuthAction::CreateTempIndex { .. } => Authorization::Deny,

        // -- Analyze denied in CreatorUp --
        // ANALYZE writes `sqlite_stat*` tables into the app db — net-new tables that
        // confound later drift detection (the snapshot would see them as out-of-band
        // objects). A migration's declared DDL has no business running ANALYZE; deny
        // it in creator mode. (Engine mode does not issue it either, but the deny is
        // scoped to CreatorUp so a future engine maintenance op is not foreclosed.)
        AuthAction::Analyze { .. } if matches!(current, Mode::CreatorUp) => Authorization::Deny,

        // -- Reindex on `main`/`temp` ALLOWED in CreatorUp --
        // `SQLITE_REINDEX` fires NOT ONLY for a standalone `REINDEX` statement but
        // also INTRINSICALLY as part of a legitimate `CREATE INDEX` (SQLite reindexes
        // the freshly-created index to populate it). The engine emits the three
        // platform system-field indexes (`<table>_<col>_idx`) inside the creator
        // `up`'s CREATE TABLE payload, so the creator phase MUST be able to reindex
        // them. A REINDEX rebuilds an existing index B-tree: it creates no table,
        // changes no schema structure, never touches `_mig`, and so does not confound
        // drift (which compares structure, not index physical layout). Allow it on the
        // app file (`main`/`temp`) in both modes. NOTE: the journal-immutability arm
        // above does NOT cover `Reindex` (its match lists Insert|Update|Delete|
        // DropTable|DropTrigger|DropIndex|DropView only) — so a REINDEX targeting `_mig`
        // is NOT caught there; it falls through to the catch-all `Deny` at the line
        // below, which is the load-bearing deny for the `_mig` case.
        AuthAction::Reindex { .. } if targets_main || db == Some("temp") => Authorization::Allow,
        // LOAD-BEARING DENY (do NOT remove as "redundant"): this catch-all is what
        // actually denies a `_mig`-targeting REINDEX (the immutability arm above does
        // not list `Reindex`), and any REINDEX naming a foreign alias — impossible
        // post-ATTACH-deny, but failed closed here regardless.
        AuthAction::Reindex { .. } => Authorization::Deny,

        // -- Cross-tenant belt-and-suspenders --
        // Any WRITE whose database_name is neither `main` (the app file) nor `_mig`
        // is denied. New aliases can only appear via ATTACH (already denied), so in
        // practice this only ever sees `main`/`_mig`/None; the rule is here so a
        // write that somehow named a foreign alias cannot execute.
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
            if !targets_main && !targets_mig =>
        {
            Authorization::Deny
        }

        // -- Total `_mig` confinement in CreatorUp — the catch-all backstop --
        // A creator has NO business touching `_mig` in ANY way, including a plain
        // `SELECT … FROM "_mig".schema_migrations` (an `AuthAction::Read` with
        // `accessor: None`, which the trigger/view-body arm above does NOT cover
        // because that arm requires `accessor.is_some`). Without this arm such a
        // Read falls through to the `_ => Allow` catch-all and the creator can read
        // the immutable journal. Deny ANY action whose OUTER database_name is `_mig`
        // in CreatorUp (Read included), ahead of the catch-all. EngineJournal is
        // unaffected — the engine's own journal reads/writes are allowed by the arms
        // above and by the catch-all in engine mode.
        action if targets_mig && matches!(current, Mode::CreatorUp) => {
            let _ = action;
            Authorization::Deny
        }

        // -- Everything else: allowed (creator DDL/DML on `main`, SELECT/READ) --
        // CreateTable/CreateIndex/CreateTrigger/CreateView/DML on `main` (the app
        // file; database_name `Some("main")` or `None`) flow here, as do
        // SELECT/READ/Recursive. Reads are not a confinement concern (cross-tenant
        // reads are already impossible — no foreign alias is bound). Transaction
        // control, temp creates, and Analyze/Reindex are handled above. The engine's
        // own `_mig` Reads (EngineJournal mode) also land here and are allowed.
        _ => Authorization::Allow,
    }
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

    // `foreign_key_check` — the rebuild's orphan-row integrity gate — is
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
        // writable_schema is NOT an introspection pragma — denied even in engine mode.
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

    #[test]
    fn mig_writes_denied_in_creator_allowed_in_engine() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        for action in [
            AuthAction::Insert {
                table_name: "schema_migrations",
            },
            AuthAction::Update {
                table_name: "schema_migrations",
                column_name: "checksum",
            },
            AuthAction::Delete {
                table_name: "schema_migrations",
            },
            AuthAction::DropTable {
                table_name: "schema_migrations",
            },
            AuthAction::DropTrigger {
                trigger_name: "zs_immutable_trg",
                table_name: "schema_migrations",
            },
        ] {
            assert_eq!(
                authorize(&m, &ctx(action, Some(MIG_ALIAS), None)),
                Authorization::Deny
            );
        }
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Insert {
                        table_name: "schema_migrations"
                    },
                    Some(MIG_ALIAS),
                    None
                )
            ),
            Authorization::Allow
        );
    }

    // ALTER TABLE keys on the action's OWN database_name, not the outer one.
    // `ALTER TABLE main.<t>` (additive ADD/DROP/RENAME COLUMN) is allowed even when
    // the OUTER database_name carries the column name (SQLite's quirk); `_mig` is
    // engine-only.
    #[test]
    fn alter_table_keys_on_inner_database_name() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        // ADD/DROP COLUMN on main: the OUTER db is the COLUMN name (the quirk we fix),
        // but the inner database_name is "main" ⇒ allowed.
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
        // ALTER TABLE on _mig is journal tampering ⇒ denied in creator mode.
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::AlterTable {
                        database_name: MIG_ALIAS,
                        table_name: "schema_migrations"
                    },
                    None,
                    None
                )
            ),
            Authorization::Deny,
            "ALTER TABLE on _mig must be denied in creator mode"
        );
        // Engine mode may alter _mig (the 12-step rebuild's engine ALTERs, later phase).
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::AlterTable {
                        database_name: MIG_ALIAS,
                        table_name: "schema_migrations"
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
    fn creator_trigger_body_targeting_mig_denied() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        // A trigger body INSERT into `_mig` with an accessor naming the creator's
        // trigger — denied at CREATE-prepare time.
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Insert {
                        table_name: "schema_migrations"
                    },
                    Some(MIG_ALIAS),
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

    // DROP VIEW "_mig".x is denied (added DropView to the immutability arm).
    #[test]
    fn drop_view_on_mig_denied_in_creator() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::DropView {
                        view_name: "some_view"
                    },
                    Some(MIG_ALIAS),
                    None
                )
            ),
            Authorization::Deny,
            "DROP VIEW on _mig must be denied"
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

    // REINDEX on `main`/`temp` is ALLOWED in CreatorUp — it fires
    // intrinsically as part of a legitimate `CREATE INDEX` (which the engine emits
    // for the platform system-field indexes inside the creator `up`). It rebuilds an
    // existing index B-tree: no new table, no schema-structure change, never `_mig`.
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

    // a creator `up` doing a plain `SELECT … FROM "_mig".schema_migrations` is a
    // `Read { accessor: None }` on `_mig`. Pre-fix it fell through to the `_ => Allow`
    // catch-all (the trigger/view-body arm requires `accessor.is_some`), letting the
    // creator read the immutable journal. It must now be DENIED in CreatorUp — while
    // the engine's own journal reads (EngineJournal mode) stay allowed.
    #[test]
    fn creator_read_of_mig_denied_engine_read_allowed() {
        let m = AuthMode::new();
        // Creator mode: a bare Read on `_mig` (no accessor) must be denied.
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Read {
                        table_name: "schema_migrations",
                        column_name: "version",
                    },
                    Some(MIG_ALIAS),
                    None
                )
            ),
            Authorization::Deny,
            "creator SELECT FROM \"_mig\".schema_migrations must be denied"
        );
        // The engine's own journal reads (EngineJournal mode) stay allowed.
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Read {
                        table_name: "schema_migrations",
                        column_name: "version",
                    },
                    Some(MIG_ALIAS),
                    None
                )
            ),
            Authorization::Allow,
            "engine journal Read on _mig must stay allowed"
        );
    }

    // A REINDEX targeting the journal alias `_mig` stays denied. NOTE: the
    // journal-immutability arm does NOT list `Reindex`, so it is NOT denied there —
    // the deny comes from the catch-all `AuthAction::Reindex {.. } => Deny` (the
    // load-bearing line after the main/temp allow), which the `_mig` case falls
    // through to because it is neither `main` nor `temp`.
    #[test]
    fn reindex_on_mig_denied_in_creator() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Reindex { index_name: "ix" },
                    Some(MIG_ALIAS),
                    None
                )
            ),
            Authorization::Deny,
            "REINDEX on _mig must stay denied (catch-all Reindex Deny, not the \
             immutability arm which omits Reindex)"
        );
    }
}

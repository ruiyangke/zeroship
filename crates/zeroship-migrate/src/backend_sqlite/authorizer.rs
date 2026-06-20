//! The two-mode hardened authorizer (SQLite-parity design §2.2.2 + §2.5.1).
//!
//! This is the **line-2 confinement** for SQLite migrations: the runtime analog
//! of the Postgres least-privilege `migrator` role. SQLite has no roles / GRANT /
//! `SET ROLE`, so confinement is enforced by a `Connection::authorizer` callback
//! that fires at **`prepare` time** for every statement compiled on the migration
//! connection, and a fail-closed deny matrix.
//!
//! # Two modes, one installed closure (§2.2.2)
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
//!   schema `_mig` is immutable: all writes/DDL to `_mig` are denied; ATTACH /
//!   DETACH / PRAGMA / load_extension / CREATE VTABLE/MODULE are denied; functions
//!   are allowlisted (fail-closed on unknown); creator-authored TRIGGER/VIEW
//!   bodies that target `_mig` are denied at CREATE-prepare time (closing the
//!   defer-into-engine-mode hole, §2.2.1 item 6).
//! - **`EngineJournal`** — only the engine's own journal writes run here. `_mig`
//!   writes are allowed (the journal tables only); ATTACH/DETACH/load_extension
//!   stay denied for life; a single `PRAGMA foreign_keys` toggle is allowed (the
//!   12-step rebuild, §2.4).
//!
//! # Matching `_mig` — the OUTER context field (CRITICAL precision, §2.2.1)
//!
//! The attach alias is carried on the OUTER [`AuthContext::database_name`] (the
//! 5th `xAuth` `zDb` argument SQLite passes), NOT on a per-action field:
//! `AuthAction::DropTable { table_name }` / `DropTrigger { .. }` carry no database
//! field. So the deny keys on `ctx.database_name == Some(MIG_ALIAS)`, never a
//! pattern-matched `DropTable.database`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

/// The fixed attach alias of the journal database (§2.2 / L2). A fixed ASCII
/// literal — never the (hyphenated-UUID) app id — so it is quote-safe and the
/// authorizer match is a trivial string compare.
pub(crate) const MIG_ALIAS: &str = "_mig";

/// The fixed attach alias of the one tenant app database (§2.5.2). The app id
/// appears only in the file path, never as a SQL identifier.
pub(crate) const APP_ALIAS: &str = "app";

/// The authorizer mode discriminants stored in the shared [`AuthMode`] atomic.
const MODE_CREATOR_UP: u8 = 0;
const MODE_ENGINE_JOURNAL: u8 = 1;

/// The two authorizer phases (§2.2.2). Stored as a `u8` in an [`AtomicU8`] so the
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

/// The shared, `Send + 'static` mode flag (§2.2.2). Cloned: one handle is moved
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

/// The fail-closed `SQLITE_FUNCTION` allowlist (C2, §2.5.1).
///
/// A blanket allow on `SQLITE_FUNCTION` cannot distinguish a benign built-in from
/// `load_extension` / `fts3_tokenizer` / a `vec_*` extension function, and vtable
/// modules issue internal SQL. So the callback allowlists by NAME and denies
/// everything else (`load_extension`, all `vec_*` in creator mode, unknown ⇒
/// Deny). The set is the deterministic built-ins the descriptor-generated DDL can
/// legitimately reference in defaults / CHECK expressions. Kept small and
/// auditable; MUST be kept in lockstep with the emitter's function set (§4 closing
/// note — fail-closed: a new emitter function the allowlist lacks is DENIED).
///
/// `CURRENT_TIMESTAMP`/`CURRENT_DATE`/`CURRENT_TIME` are SQL *keywords*, not
/// functions, so they do not pass through `SQLITE_FUNCTION` and are not listed.
const FUNCTION_ALLOWLIST: &[&str] = &[
    "abs",
    "coalesce",
    "length",
    "lower",
    "upper",
    "nullif",
    "ifnull",
    "max", // 2-arg scalar form used in CHECK/defaults
    "min", // 2-arg scalar form used in CHECK/defaults
    "trim",
    "ltrim",
    "rtrim",
    "substr",
    "typeof",
    "hex",
    "quote",
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
    "exists",
];

/// True iff `name` (case-insensitive) is on the function allowlist.
fn function_allowed(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    FUNCTION_ALLOWLIST.iter().any(|f| *f == lower)
}

/// Build the single authorizer closure to install via `conn.authorizer(Some(..))`.
///
/// The returned closure captures the [`AuthMode`] handle by-move and reads it on
/// every `prepare`-time invocation, branching the deny matrix on the current mode
/// (§2.5.1). It is the load-bearing line-2: the deny is at prepare, BEFORE
/// execution, for EVERY statement compiled on the connection — including
/// runtime-constructed SQL and the AI/raw path (§2.5.3).
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
/// per-action `database` field (which several variants lack, §2.2.1).
fn authorize(mode: &AuthMode, ctx: &AuthContext<'_>) -> Authorization {
    let current = mode.load();
    let db = ctx.database_name;
    let targets_mig = db == Some(MIG_ALIAS);

    match &ctx.action {
        // -- Capabilities denied in BOTH modes, for the connection's whole life --
        // ATTACH/DETACH closed by construction (§2.5.2): the engine attaches the
        // one app + the journal BEFORE installing this authorizer; after install,
        // no new alias can be bound and none can be dropped — ever.
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,

        // PRAGMA: denied in CreatorUp (closes the `writable_schema=ON` forge,
        // §2.2.1 item 3). In EngineJournal, only the engine's `foreign_keys`
        // toggle around the 12-step rebuild is allowed (§2.4); everything else
        // (incl. writable_schema, journal_mode, etc.) is denied.
        AuthAction::Pragma { pragma_name, .. } => match current {
            Mode::EngineJournal if pragma_name.eq_ignore_ascii_case("foreign_keys") => {
                Authorization::Allow
            }
            _ => Authorization::Deny,
        },

        // load_extension and any new vtable module: denied in both modes. (Belt
        // and suspenders alongside `load_extension_disable()` at open, §2.1.1.)
        AuthAction::CreateVtable { module_name, .. } => match current {
            // Engine-emitted goodie DDL may create an fts5/vec0 vtable, ONLY in
            // engine mode (§2.6). Creator mode can never make a vtable.
            Mode::EngineJournal
                if module_name.eq_ignore_ascii_case("fts5")
                    || module_name.eq_ignore_ascii_case("vec0") =>
            {
                Authorization::Allow
            }
            _ => Authorization::Deny,
        },

        // SQLITE_FUNCTION allowlist (C2). Fail-closed: unknown ⇒ Deny. `load_extension`
        // and all `vec_*` are simply absent from the allowlist ⇒ denied in creator
        // mode. In engine mode the engine's vector DDL may additionally call `vec_*`.
        AuthAction::Function { function_name } => {
            let lower = function_name.to_ascii_lowercase();
            // Allowed iff on the allowlist, OR (engine mode only) a `vec_*` function
            // for engine-emitted vector DDL. Everything else (incl. load_extension,
            // any `vec_*` in creator mode, unknown) is fail-closed denied.
            let engine_vec = matches!(current, Mode::EngineJournal) && lower.starts_with("vec_");
            if function_allowed(function_name) || engine_vec {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }

        // Transaction control (§2.2.2 phase sequence + §2.5.1 row).
        //
        // The engine owns BEGIN IMMEDIATE / COMMIT / ROLLBACK and issues them under
        // EngineJournal mode (the §2.2.2 phase sequence requires it: step 1 BEGIN,
        // step 6 COMMIT are engine operations). The **creator** `up` (CreatorUp
        // mode) may NOT open or close a transaction — that would break the single
        // atomic transaction wrapping the DDL + journal write. So:
        //   - CreatorUp: DENY (creator cannot touch transaction boundaries)
        //   - EngineJournal: ALLOW (the engine's own BEGIN/COMMIT/ROLLBACK)
        // SAVEPOINT is denied in both modes (the engine uses plain BEGIN/COMMIT,
        // never savepoints; a creator savepoint is never legitimate).
        AuthAction::Transaction { .. } => match current {
            Mode::EngineJournal => Authorization::Allow,
            Mode::CreatorUp => Authorization::Deny,
        },
        AuthAction::Savepoint { .. } => Authorization::Deny,

        // -- Journal immutability on `_mig` (§2.2.1 item 4) --
        // Direct writes / DDL to `_mig` are denied in CreatorUp and allowed only in
        // EngineJournal (and only the journal tables exist there). Match the OUTER
        // database_name — DropTable/DropTrigger carry no per-action database field.
        AuthAction::Insert { .. }
        | AuthAction::Update { .. }
        | AuthAction::Delete { .. }
        | AuthAction::DropTable { .. }
        | AuthAction::DropTrigger { .. }
        | AuthAction::DropIndex { .. }
        | AuthAction::AlterTable { .. }
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

        // -- Creator-authored TRIGGER/VIEW bodies that WRITE `_mig` (§2.2.1 item 6) --
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

        // -- Cross-tenant belt-and-suspenders (§2.5.2) --
        // Any WRITE whose database_name is neither `app` nor `_mig` is denied. New
        // aliases can only appear via ATTACH (already denied), so in practice this
        // only ever sees `app`/`_mig`/None; the rule is here so a write that somehow
        // named a foreign alias cannot execute.
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
        | AuthAction::AlterTable { .. }
            if db.is_some() && db != Some(APP_ALIAS) && !targets_mig =>
        {
            Authorization::Deny
        }

        // -- Everything else: allowed (creator DDL/DML on `app`, SELECT/READ) --
        // CreateTable/CreateIndex/CreateTrigger/CreateView/DML on `app` (or None,
        // which SQLite uses for the connection's main/temp namespace) flow here, as
        // do SELECT/READ/Recursive/Reindex/Analyze. Reads are not a confinement
        // concern (cross-tenant reads are already impossible — no foreign alias is
        // bound). Transaction control is handled above.
        _ => Authorization::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::hooks::TransactionOperation;

    fn ctx<'a>(action: AuthAction<'a>, db: Option<&'a str>, accessor: Option<&'a str>) -> AuthContext<'a> {
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
                authorize(&m, &ctx(AuthAction::Detach { database_name: "x" }, None, None)),
                Authorization::Deny
            );
        }
    }

    #[test]
    fn pragma_denied_creator_foreign_keys_only_engine() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(
                &m,
                &ctx(
                    AuthAction::Pragma { pragma_name: "writable_schema", pragma_value: Some("1") },
                    None,
                    None
                )
            ),
            Authorization::Deny
        );
        assert_eq!(
            authorize(
                &m,
                &ctx(AuthAction::Pragma { pragma_name: "foreign_keys", pragma_value: Some("OFF") }, None, None)
            ),
            Authorization::Deny,
            "foreign_keys toggle is engine-only"
        );
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(
                &m,
                &ctx(AuthAction::Pragma { pragma_name: "foreign_keys", pragma_value: Some("OFF") }, None, None)
            ),
            Authorization::Allow
        );
        assert_eq!(
            authorize(
                &m,
                &ctx(AuthAction::Pragma { pragma_name: "writable_schema", pragma_value: Some("1") }, None, None)
            ),
            Authorization::Deny,
            "writable_schema denied even in engine mode"
        );
    }

    #[test]
    fn mig_writes_denied_in_creator_allowed_in_engine() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        for action in [
            AuthAction::Insert { table_name: "schema_migrations" },
            AuthAction::Update { table_name: "schema_migrations", column_name: "checksum" },
            AuthAction::Delete { table_name: "schema_migrations" },
            AuthAction::DropTable { table_name: "schema_migrations" },
            AuthAction::DropTrigger { trigger_name: "zs_immutable_trg", table_name: "schema_migrations" },
        ] {
            assert_eq!(authorize(&m, &ctx(action, Some(MIG_ALIAS), None)), Authorization::Deny);
        }
        m.store(Mode::EngineJournal);
        assert_eq!(
            authorize(&m, &ctx(AuthAction::Insert { table_name: "schema_migrations" }, Some(MIG_ALIAS), None)),
            Authorization::Allow
        );
    }

    #[test]
    fn function_allowlist_fail_closed() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(&m, &ctx(AuthAction::Function { function_name: "abs" }, None, None)),
            Authorization::Allow
        );
        assert_eq!(
            authorize(&m, &ctx(AuthAction::Function { function_name: "load_extension" }, None, None)),
            Authorization::Deny
        );
        assert_eq!(
            authorize(&m, &ctx(AuthAction::Function { function_name: "vec_distance_cosine" }, None, None)),
            Authorization::Deny,
            "vec_* denied in creator mode"
        );
        assert_eq!(
            authorize(&m, &ctx(AuthAction::Function { function_name: "totally_unknown_fn" }, None, None)),
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
                    AuthAction::Insert { table_name: "schema_migrations" },
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
                &ctx(AuthAction::CreateVtable { table_name: "v", module_name: "vec0" }, Some(APP_ALIAS), None)
            ),
            Authorization::Deny
        );
        assert_eq!(
            authorize(
                &m,
                &ctx(AuthAction::Transaction { operation: TransactionOperation::Begin }, None, None)
            ),
            Authorization::Deny
        );
    }

    #[test]
    fn app_ddl_allowed() {
        let m = AuthMode::new();
        m.store(Mode::CreatorUp);
        assert_eq!(
            authorize(&m, &ctx(AuthAction::CreateTable { table_name: "users" }, Some(APP_ALIAS), None)),
            Authorization::Allow
        );
        assert_eq!(
            authorize(&m, &ctx(AuthAction::Insert { table_name: "users" }, Some(APP_ALIAS), None)),
            Authorization::Allow
        );
    }
}

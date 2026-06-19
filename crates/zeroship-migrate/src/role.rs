//! The least-privilege per-project `migrator` role — **line-2 DB-privilege
//! defense** (design §1.3 / §1.7).
//!
//! The SQL guard ([`crate::guard::SqlGuard`]) is line 1: it parses every `up`
//! and denies the dangerous surface at submission. But a parser can be evaded
//! by **runtime-constructed SQL** — e.g. `DO $$ … EXECUTE format('… %I …', s) …`
//! where the target schema is computed at execution and never appears as a
//! parseable identifier the guard can confine. The guard documents these as
//! residuals. **Line 2 backstops them:** the migration's DDL runs under a
//! dedicated, deliberately under-privileged Postgres role that has *no grants*
//! on `control` / `auth` / `billing` / other projects' schemas, so the same op
//! that slips past parse **fails with `permission denied` at execution**.
//!
//! # Role model — `NOLOGIN` + `SET ROLE` (not a login role)
//!
//! `provision_migrator` creates `migrator_<project>` as **`NOLOGIN`**. The
//! executor connects as the privileged admin/control role and runs each
//! migration under `SET ROLE migrator_<project>` (with `RESET ROLE` on exit,
//! scoped exactly like the executor's H2 session-GUC restore). This is chosen
//! over a `LOGIN` role because:
//!
//! - **No per-role passwords / secrets.** A login role needs a credential to be
//!   provisioned, rotated, and handed to the executor; `SET ROLE` reuses the
//!   admin connection the executor already holds.
//! - **No connection churn.** The executor already owns one admin session +
//!   the project advisory lock; switching the *effective* role inside it keeps
//!   the lock and the journal writes on one session.
//! - **Same DB-enforced confinement.** `SET ROLE` to a `NOSUPERUSER` role makes
//!   privilege checks run as that role — a superuser admin that `SET ROLE`s to
//!   a non-superuser is fully constrained by the target role's grants (a
//!   superuser only bypasses checks while it is *itself* the effective role).
//!
//! # The grant set (least privilege)
//!
//! The migrator role gets **exactly**:
//!
//! - `NOSUPERUSER NOCREATEROLE NOCREATEDB NOLOGIN NOBYPASSRLS` — cannot escalate
//!   (`CREATE ROLE`, `ALTER SYSTEM`, `CREATE DATABASE` all denied by attribute).
//! - **owns** the project schema (so its DDL + `ALTER DEFAULT PRIVILEGES`
//!   targets work and objects it creates are owned/usable by it), with
//!   `CREATE, USAGE` on it.
//! - **NO access whatsoever to the meta schema.** This is the C1 fix: the
//!   migrator must not be able to forge the journal. A migration's `up` runs as
//!   the migrator, so if the migrator could `INSERT` into the journal it could
//!   plant a `completed` row (silently suppressing a future legitimate
//!   migration: `pending = set − completed`) or a bogus checksum (wedging the
//!   apply on `ChecksumDrift`). All journal / inflight I/O is therefore done by
//!   the **executor as the admin role** — the migrator gets neither `USAGE` on
//!   the meta schema nor any grant on `schema_migrations` /
//!   `schema_migrations_inflight`. The journal is unforgeable by deny-by-absence.
//! - `search_path` set to the project schema **first**, then the extension
//!   schema(s) (default `public`), via `ALTER ROLE`. The project schema is the
//!   sole writable resolution target; the extension schema(s) ride at the end
//!   purely so an unqualified extension TYPE/function the engine emits
//!   (pgvector's `vector(N)`, `PostGIS`'s `geography(...)`) resolves. The meta
//!   schema is off the migrator's path — defense-in-depth so an unqualified name
//!   in an `up` can never resolve to the journal even if a grant were ever
//!   reintroduced.
//! - **`REVOKE ALL` then `GRANT USAGE` on the extension schema(s) (`public`)**:
//!   the migrator cannot stage objects there (no `CREATE`) nor reach existing
//!   tables (no per-object grant), but `USAGE` lets it *resolve* the shared
//!   extension types. USAGE is resolution-only — it relaxes nothing about
//!   cross-schema **write** confinement. (Matches plugin-db's runtime, which
//!   references the same unqualified `vector`/`geography` types with `public`
//!   reachable on its connection path.)
//! - **No grant whatsoever** on `control` / `auth` / `billing` / `zeroship` /
//!   any other project schema. Deny-by-absence: a role only has what it is
//!   granted, so an unmentioned schema is unreachable. This is the line-2
//!   backstop.
//!
//! # Known residuals (tracked, no behavior change)
//!
//! - **M2 — `CREATE FUNCTION … SET search_path`** is denied by the guard but is
//!   NOT role-backstopped. This is harmless: functions the migrator creates are
//!   `INVOKER` by default, so they run with the *caller's* privileges (no
//!   escalation), and `SECURITY DEFINER` (which would run as the function owner,
//!   the migrator) is itself guard-denied. So the line-2 role gives no extra
//!   confinement here, and none is needed. Tracked only.
//! - **M1 — `pg_roles` enumeration.** The migrator can read `pg_roles` (a
//!   cluster-global catalog `USAGE`-free to all roles). Accepted: role names are
//!   not secrets and there is no privilege to enumerate. No change.
//!
//! # Idempotency
//!
//! `provision_migrator` is safe to run on every deploy: role creation is guarded
//! on `pg_roles`, and every `GRANT` / `ALTER` / `REVOKE` is naturally idempotent
//! (re-granting an existing grant is a no-op, re-altering `search_path` is a no-op).
//! Schema ownership is only (re)assigned when it differs.

use compio_postgres::Client;

use crate::db::ExecutorConfig;

/// Error provisioning or deprovisioning a migrator role.
#[derive(Debug, thiserror::Error)]
pub enum RoleError {
    /// A database error during provisioning.
    #[error("role provisioning db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// The derived role name was empty or otherwise unusable.
    #[error("invalid migrator role name derived from project id '{0}'")]
    BadRoleName(String),
}

/// Quote a SQL identifier (double embedded quotes, wrap in `"`), so a schema /
/// role name is never interpolated as raw SQL. Mirrors `journal::quote_ident`.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Quote a string for a SQL string literal context (double embedded `'`).
fn quote_lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// Run a DDL statement, retrying briefly on `XX000 "tuple concurrently
/// updated"`.
///
/// Several provisioning steps update **shared catalog rows** that are contended
/// when *different* projects are provisioned concurrently: the `public` schema's
/// ACL (`REVOKE … ON SCHEMA public`), the admin role's membership grants
/// (`GRANT migrator TO current_user`), and per-role settings
/// (`ALTER ROLE … SET search_path`). Postgres raises a non-retryable-looking but
/// in fact transient `XX000 tuple concurrently updated` when two sessions update
/// the same catalog tuple at once. Each provisioning step is idempotent, so a
/// bounded retry is safe and resolves the race. This is a real production
/// concern (concurrent project provisioning), not just a test artifact.
async fn exec_retry(admin: &Client, sql: &str) -> Result<(), compio_postgres::Error> {
    const MAX_ATTEMPTS: u32 = 8;
    let mut attempt = 0;
    loop {
        match admin.batch_execute(sql).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let transient = e
                    .as_db_error()
                    .is_some_and(|db| db.message().contains("tuple concurrently updated"));
                attempt += 1;
                if transient && attempt < MAX_ATTEMPTS {
                    // Small backoff proportional to the attempt; the contended
                    // catalog update is brief.
                    compio::time::sleep(std::time::Duration::from_millis(
                        u64::from(attempt) * 10,
                    ))
                    .await;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Derive the deterministic migrator role name for a project.
///
/// `migrator_<project_id>` with the project id sanitized to the Postgres
/// identifier charset (`[a-z0-9_]`); any other char becomes `_`. The result is
/// always quoted at use sites, so this is defense-in-depth, not the sole
/// injection guard.
///
/// # Errors
/// [`RoleError::BadRoleName`] if the project id sanitizes to an empty string.
pub fn migrator_role_name(project_id: &str) -> Result<String, RoleError> {
    let sanitized: String = project_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        return Err(RoleError::BadRoleName(project_id.to_string()));
    }
    // Postgres identifiers max 63 bytes; keep room for the prefix.
    let truncated: String = sanitized.chars().take(50).collect();
    Ok(format!("migrator_{truncated}"))
}

/// Idempotently provision the least-privilege `migrator` role for a project
/// (design §1.3). Run by an admin/control principal with `CREATEROLE`.
///
/// Establishes the role, makes it own the **project schema**, sets its
/// `search_path` to the project schema (writable) plus the extension schema(s)
/// (`public`, resolution-only via `USAGE`), and revokes write reach on `public`
/// and any residual access to the **meta schema**. After this, the executor can `SET ROLE` to it
/// to run migrations under least privilege — but the migrator has **no** access
/// to the journal (the executor does all journal I/O as admin; C1 fix).
///
/// The project schema is expected to exist (project provisioning creates it).
///
/// # Errors
/// - [`RoleError::BadRoleName`] if the project id yields no valid role name.
/// - [`RoleError::Db`] on any DDL failure (e.g. the caller lacks `CREATEROLE`).
pub async fn provision_migrator(admin: &Client, cfg: &ExecutorConfig) -> Result<(), RoleError> {
    let role = migrator_role_name(&cfg.project_id)?;
    let role_q = quote_ident(&role);
    let role_lit = quote_lit(&role);
    let proj_q = quote_ident(&cfg.project_schema);
    let meta_q = quote_ident(&cfg.meta_schema);

    // 1. Create the role idempotently with the locked-down attribute set.
    //    NOLOGIN: reached only via SET ROLE from the admin connection.
    //    NOSUPERUSER NOCREATEROLE NOCREATEDB NOBYPASSRLS: no escalation surface.
    exec_retry(
        admin,
        &format!(
            "DO $prov$ BEGIN
                IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role_lit}') THEN
                    EXECUTE 'CREATE ROLE {role_q} \
                             NOSUPERUSER NOCREATEROLE NOCREATEDB NOLOGIN NOBYPASSRLS';
                END IF;
             END $prov$"
        ),
    )
    .await?;

    // 2. The admin must be able to SET ROLE to the migrator. A superuser admin
    //    always can; a non-superuser CREATEROLE admin is auto-granted membership
    //    in roles it creates on PG 16+, but GRANT it explicitly so a pre-existing
    //    role (created by another principal) is also reachable. Idempotent.
    exec_retry(
        admin,
        &format!(
            "DO $mem$ BEGIN
                IF NOT pg_has_role(current_user, '{role_lit}', 'MEMBER') THEN
                    EXECUTE 'GRANT {role_q} TO ' || quote_ident(current_user);
                END IF;
             END $mem$"
        ),
    )
    .await?;

    // 3. The migrator OWNS the project schema: its DDL, the objects it creates,
    //    and `ALTER DEFAULT PRIVILEGES FOR ROLE migrator` all resolve to an
    //    owner it controls. (It does NOT own the meta schema — see step 5 — so
    //    it can never drop the journal's immutability trigger.)
    exec_retry(admin, &format!("ALTER SCHEMA {proj_q} OWNER TO {role_q}")).await?;

    // 4. CREATE + USAGE on the project schema (redundant with ownership, but
    //    explicit + idempotent, and survives a future ownership change).
    exec_retry(
        admin,
        &format!("GRANT CREATE, USAGE ON SCHEMA {proj_q} TO {role_q}"),
    )
    .await?;

    // 4b. Default privileges: objects the migrator creates in its schema are
    //     usable by it (it is the creator/owner, so this is belt-and-suspenders
    //     for any future grantee model, and harmless idempotent DDL).
    exec_retry(
        admin,
        &format!(
            "ALTER DEFAULT PRIVILEGES FOR ROLE {role_q} IN SCHEMA {proj_q} \
             GRANT ALL ON TABLES TO {role_q}; \
             ALTER DEFAULT PRIVILEGES FOR ROLE {role_q} IN SCHEMA {proj_q} \
             GRANT ALL ON SEQUENCES TO {role_q};"
        ),
    )
    .await?;

    // 5. The migrator must have NO access to the meta schema — the journal is
    //    unforgeable by deny-by-absence (C1 fix). The executor does ALL journal /
    //    inflight I/O as the admin role; the migrator's `up` (which runs as the
    //    migrator) must never be able to write a forged `completed` row or tamper
    //    with the inflight markers. REVOKE explicitly so a re-provision over a
    //    role that previously held these grants (or a future grant slip) is
    //    cleaned up — every REVOKE is idempotent.
    //
    //    NOTE: `IF EXISTS` on the table-level REVOKE so provisioning does not
    //    fail before the journal bootstrap has created the tables (provisioning
    //    no longer depends on `ensure_journal` ordering).
    exec_retry(
        admin,
        &format!(
            "DO $revoke$ BEGIN
                IF EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = '{meta_lit}') THEN
                    EXECUTE 'REVOKE ALL ON ALL TABLES IN SCHEMA {meta_q} FROM {role_q}';
                    EXECUTE 'REVOKE ALL ON SCHEMA {meta_q} FROM {role_q}';
                END IF;
             END $revoke$",
            meta_lit = quote_lit(&cfg.meta_schema),
        ),
    )
    .await?;

    // 6. Pin search_path to the PROJECT schema FIRST, then the extension
    //    schema(s) (default `public`). The project schema is the sole writable
    //    resolution target — an unqualified `CREATE TABLE foo` lands there, never
    //    in an extension schema. The extension schema(s) ride at the END of the
    //    path purely so an UNQUALIFIED extension TYPE/function the engine emits
    //    (pgvector's `vector(N)`, PostGIS's `geography(...)`) RESOLVES — matching
    //    how plugin-db's runtime references the same unqualified types with
    //    `public` reachable. The meta schema stays OFF the path so an unqualified
    //    `INSERT INTO schema_migrations` in an `up` can never resolve to the
    //    journal (defense-in-depth behind the revoked grant; C1 fix).
    //
    //    SECURITY: presence on search_path is RESOLUTION ONLY. Write reach into an
    //    extension schema needs CREATE (kept REVOKEd in step 7) or per-table
    //    grants (never given), so this does not relax cross-schema write
    //    confinement.
    let mut path_parts = vec![proj_q.clone()];
    for ext in &cfg.extension_schemas {
        if ext != &cfg.project_schema {
            path_parts.push(quote_ident(ext));
        }
    }
    exec_retry(
        admin,
        &format!(
            "ALTER ROLE {role_q} SET search_path = {}",
            path_parts.join(", ")
        ),
    )
    .await?;

    // 7. Confine the extension schema(s) to RESOLUTION-ONLY. REVOKE ALL first so
    //    the migrator can never stage objects there (no CREATE) — PG 15+ already
    //    revokes CREATE-on-public from PUBLIC, but be explicit — then GRANT USAGE
    //    so unqualified extension TYPE/function lookup (the `vector`/`geography`
    //    types) succeeds. USAGE permits *referencing* objects in the schema; it
    //    grants NO write/CREATE and NO access to existing tables (those need
    //    per-object grants the migrator never receives). Net: the migrator can
    //    resolve `vector(N)` but cannot create or write anything in `public`.
    for ext in &cfg.extension_schemas {
        if ext == &cfg.project_schema {
            continue;
        }
        let ext_q = quote_ident(ext);
        exec_retry(admin, &format!("REVOKE ALL ON SCHEMA {ext_q} FROM {role_q}")).await?;
        exec_retry(admin, &format!("GRANT USAGE ON SCHEMA {ext_q} TO {role_q}")).await?;
    }

    Ok(())
}

/// Idempotently remove the migrator role for a project (e.g. project teardown).
///
/// Reassigns objects owned by the role back to the current admin and drops the
/// role's privileges before dropping it, so the drop cannot fail on dependent
/// grants. Safe to call when the role does not exist.
///
/// # Errors
/// - [`RoleError::BadRoleName`] if the project id yields no valid role name.
/// - [`RoleError::Db`] on any DDL failure.
pub async fn deprovision_migrator(admin: &Client, cfg: &ExecutorConfig) -> Result<(), RoleError> {
    let role = migrator_role_name(&cfg.project_id)?;
    let role_q = quote_ident(&role);
    let role_lit = quote_lit(&role);

    exec_retry(
        admin,
        &format!(
            "DO $deprov$ BEGIN
                IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role_lit}') THEN
                    EXECUTE 'REASSIGN OWNED BY {role_q} TO ' || quote_ident(current_user);
                    EXECUTE 'DROP OWNED BY {role_q}';
                    EXECUTE 'DROP ROLE {role_q}';
                END IF;
             END $deprov$"
        ),
    )
    .await?;
    Ok(())
}

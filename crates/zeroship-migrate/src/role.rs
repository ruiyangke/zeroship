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
//! - on the **meta schema** (which it does *not* own — the admin owns it so the
//!   migrator cannot drop the immutability trigger / journal table): `USAGE`
//!   only, plus `SELECT, INSERT` on `schema_migrations` and
//!   `SELECT, INSERT, DELETE` on `schema_migrations_inflight` (the two-phase
//!   non-txn protocol writes a `started` marker, inserts the immutable
//!   `completed` row, then deletes the marker).
//! - `search_path` pinned to the project schema (+ meta) via `ALTER ROLE`.
//! - **`REVOKE` of `CREATE`/`USAGE` on `public`** so it cannot stage objects in
//!   the shared `public` schema or resolve unqualified names there.
//! - **No grant whatsoever** on `control` / `auth` / `billing` / `zeroship` /
//!   any other project schema. Deny-by-absence: a role only has what it is
//!   granted, so an unmentioned schema is unreachable. This is the line-2
//!   backstop.
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
/// Establishes the role, makes it own the **project schema**, grants it the
/// minimal journal write surface on the (admin-owned) **meta schema**, pins its
/// `search_path`, and revokes `public`. After this, the executor can `SET ROLE`
/// to it to run migrations under least privilege.
///
/// Both the project schema and the meta schema are expected to exist (the
/// journal bootstrap and project provisioning create them). The grants on the
/// meta journal tables therefore run after [`crate::journal::ensure_journal`].
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

    // 5. The journal write surface on the admin-owned meta schema. USAGE only
    //    (no CREATE — the migrator must not add objects to meta), and the exact
    //    verbs the two-phase journal path needs:
    //      schema_migrations           : SELECT, INSERT  (append-only; the
    //                                    immutability trigger blocks UPDATE/DELETE
    //                                    regardless, and we don't grant them)
    //      schema_migrations_inflight  : SELECT, INSERT, DELETE (marker lifecycle)
    exec_retry(
        admin,
        &format!(
            "GRANT USAGE ON SCHEMA {meta_q} TO {role_q}; \
             GRANT SELECT, INSERT ON {meta_q}.schema_migrations TO {role_q}; \
             GRANT SELECT, INSERT, DELETE ON {meta_q}.schema_migrations_inflight TO {role_q};"
        ),
    )
    .await?;

    // 6. Pin search_path to the project + meta schema. Unqualified names in a
    //    migration resolve into the project schema; the migrator never sees
    //    `public` or `zeroship` on its path.
    exec_retry(
        admin,
        &format!("ALTER ROLE {role_q} SET search_path = {proj_q}, {meta_q}"),
    )
    .await?;

    // 7. REVOKE public-schema reach. Stop the migrator staging objects in the
    //    shared `public` schema or resolving unqualified names there. (PG 15+
    //    already revokes CREATE-on-public from PUBLIC, but be explicit + cover
    //    USAGE so the role is fully confined to its own + meta schema.)
    exec_retry(admin, &format!("REVOKE ALL ON SCHEMA public FROM {role_q}")).await?;

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

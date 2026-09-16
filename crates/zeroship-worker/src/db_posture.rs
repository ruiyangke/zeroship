use compio_postgres::NoTls;

const WORKER_DATABASE_ROLE: &str = "zeroship_worker";

/// How many role memberships `current_user` inherits AMBIENTLY, and one
/// example.
///
/// A SEPARATE query from the big posture SELECT below, for two reasons. It
/// reads only `pg_catalog`, so it runs on any database - the posture SELECT
/// asks about the `zeroship` schema, which a creator database does not have.
/// And a check that can go blind must be independently testable; folded into
/// the other query it could only be exercised somewhere it cannot run, which is
/// how a subquery that always returns 0 stays green forever.
///
/// KEYED ON THE MEMBERSHIP, NOT THE ROLE ATTRIBUTE. PostgreSQL 16+ stores
/// `inherit_option` per row in `pg_auth_members`, and `ALTER ROLE ... NOINHERIT`
/// does not reach back into grants that already exist - so `pg_roles.rolinherit`
/// is the wrong column and reading it would report a fence that is not there.
///
/// KEYED ON THE ROW, NOT THE PAIR. `pg_auth_members` is unique on
/// `(roleid, member, grantor)`, PostgreSQL takes the UNION across grantors, and
/// a plain `REVOKE` - even as superuser - removes only the issuing grantor's
/// row. A check shaped "the membership is non-inheriting" passes while an
/// inheriting sibling row granted by someone else holds the fence open, so this
/// COUNTS every offending row.
///
/// DENY-BY-DEFAULT, not a `LIKE 'app\_%\_role'` allowlist. A name pattern that
/// stops matching after a rename reports green over an empty set, which is the
/// one failure mode a boot gate must not have. There is no exemption: the
/// worker holds no ambient membership of any kind, and a future role that
/// genuinely needs one fails boot loudly.
const INHERITED_MEMBERSHIPS_SQL: &str = r#"
SELECT
  (SELECT count(*)
     FROM pg_auth_members membership
     JOIN pg_roles granted ON granted.oid = membership.roleid
    WHERE membership.member = login.oid
      AND membership.inherit_option) AS inheriting_memberships,
  (SELECT granted.rolname
     FROM pg_auth_members membership
     JOIN pg_roles granted ON granted.oid = membership.roleid
    WHERE membership.member = login.oid
      AND membership.inherit_option
    ORDER BY granted.rolname
    LIMIT 1) AS inheriting_membership_example
FROM pg_roles login
WHERE login.rolname = current_user
"#;

#[derive(Debug)]
struct DatabasePosture {
    current_user: String,
    superuser: bool,
    create_role: bool,
    create_db: bool,
    replication: bool,
    bypass_rls: bool,
    /// Whether this login can reach the platform schema at all. The worker
    /// connects to the CREATOR database, where that schema does not exist; a
    /// login that can see it is pointed at the platform database.
    reaches_platform_schema: bool,
    /// How many role memberships this login inherits ambiently. Must be zero:
    /// see [`validate`].
    inheriting_memberships: i64,
    /// One offending role name, for an error an operator can act on.
    inheriting_membership_example: Option<String>,
}

fn validate(posture: &DatabasePosture) -> Result<(), String> {
    if posture.current_user != WORKER_DATABASE_ROLE {
        return Err(format!(
            "worker database login must be {WORKER_DATABASE_ROLE}, got {}",
            posture.current_user
        ));
    }
    if posture.superuser || posture.create_role || posture.create_db {
        return Err(
            "worker database role must be NOSUPERUSER, NOCREATEROLE, and NOCREATEDB".to_string(),
        );
    }
    if posture.replication || posture.bypass_rls {
        return Err(
            "worker database role must be NOREPLICATION and NOBYPASSRLS; CDC belongs to the relay"
                .to_string(),
        );
    }
    // THE ZONE ARM. The worker executes creator code and belongs to the creator
    // execution zone; the platform catalog belongs to Control's zone and reaches
    // this process over an authenticated HTTP surface, never over SQL. A login
    // that can so much as resolve `zeroship` is connected to the wrong database.
    if posture.reaches_platform_schema {
        return Err(
            "worker database role can reach the platform schema; the worker connects to the \
             creator database and reads app metadata from Control"
                .to_string(),
        );
    }
    // THE FENCE ARM. Everything above bounds which database this login may be
    // on. This one bounds what it may do on TENANT schemas, and it is the only
    // check that makes `SET LOCAL ROLE` a fence rather than an optional
    // narrowing.
    //
    // `zeroship_worker` is a single login role shared by every app, and the
    // migration service grants it each app's runtime role. If those memberships
    // inherit, the worker's ambient authority is the UNION of every tenant it
    // has ever served - on the dev database on 2026-08-28, ALL of the worker's
    // `app_%_role` memberships inherited (the proportion is the point; the
    // count drifts with every test run) - and a query path that omits
    // `SET LOCAL ROLE` does not fail, it succeeds with cross-tenant reach.
    // `runtime_dependents_sql` now grants `WITH INHERIT FALSE`; this refuses to
    // boot against a database still carrying the old posture, so a stale
    // deployment is detected instead of silently trusted.
    if posture.inheriting_memberships > 0 {
        let example = posture
            .inheriting_membership_example
            .as_deref()
            .unwrap_or("<unknown>");
        return Err(format!(
            "worker database role ambiently inherits {} role membership(s) (e.g. {example}); \
             every app-role grant must carry WITH INHERIT FALSE so SET LOCAL ROLE is a fence \
             and not an optional narrowing. Re-run a migration apply for the affected apps to \
             converge the grant, or GRANT <role> TO {WORKER_DATABASE_ROLE} WITH INHERIT FALSE",
            posture.inheriting_memberships
        ));
    }
    Ok(())
}

/// Verify the worker's effective database authority before V8 or a listener is
/// initialized. This inspects privileges, not the spelling of the DSN, so role
/// membership and accidental grants cannot bypass the boundary.
pub async fn validate_database_url(db_url: &str) -> Result<(), String> {
    let (client, connection) = compio_postgres::connect(db_url, NoTls)
        .await
        .map_err(|error| format!("connect to inspect worker database role: {error}"))?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            tracing::error!(%error, "worker database posture connection failed");
        }
    })
    .detach();

    let row = client
        .query_one(
            r#"
SELECT
  current_user::text AS current_user,
  role.rolsuper AS superuser,
  role.rolcreaterole AS create_role,
  role.rolcreatedb AS create_db,
  role.rolreplication AS replication,
  role.rolbypassrls AS bypass_rls,
  EXISTS (
    SELECT 1 FROM pg_namespace
     WHERE nspname = 'zeroship'
       AND has_schema_privilege(current_user, oid, 'USAGE')
  ) AS reaches_platform_schema
FROM pg_roles role
WHERE role.rolname = current_user
            "#,
            &[],
        )
        .await
        .map_err(|error| format!("inspect worker database role: {error}"))?;

    let memberships = client
        .query_one(INHERITED_MEMBERSHIPS_SQL, &[])
        .await
        .map_err(|error| format!("inspect worker role memberships: {error}"))?;

    validate(&DatabasePosture {
        current_user: row.get("current_user"),
        superuser: row.get("superuser"),
        create_role: row.get("create_role"),
        create_db: row.get("create_db"),
        replication: row.get("replication"),
        bypass_rls: row.get("bypass_rls"),
        reaches_platform_schema: row.get("reaches_platform_schema"),
        inheriting_memberships: memberships.get("inheriting_memberships"),
        inheriting_membership_example: memberships.get("inheriting_membership_example"),
    })
}

#[cfg(test)]
mod tests;

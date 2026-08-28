use compio_postgres::NoTls;

const WORKER_DATABASE_ROLE: &str = "zeroship_worker";

/// The ONE role membership the worker is allowed to hold ambiently.
///
/// The workflow journal tables are owned by this role and revoked from PUBLIC
/// and from the app runtime role
/// (`zeroship-plugin-workflow/src/store/pg.rs::reassert_table_revokes`), and
/// only `PgStore::provision` issues an explicit `SET ROLE` - every other
/// journal statement reaches those tables by inheriting this membership. Adding
/// it to the deny-by-default sweep below would refuse every boot.
const AMBIENT_MEMBERSHIP_EXEMPTION: &str = "zeroship_workflow_owner";

/// How many role memberships `current_user` inherits AMBIENTLY, and one
/// example, excluding the role named by `$1`.
///
/// A SEPARATE query from the big posture SELECT above, for two reasons. It
/// reads only `pg_catalog`, so it runs on any database - the posture SELECT
/// interrogates `zeroship.apps` / `plans` / `app_deploys` and errors outright
/// where the platform migrations have not been applied, which is most test
/// databases. And a check that can go blind must be independently testable;
/// folded into the other query it could only be exercised somewhere it cannot
/// run, which is how a subquery that always returns 0 stays green forever.
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
/// one failure mode a boot gate must not have. A future role that genuinely
/// needs ambient inheritance fails boot loudly and gets exempted deliberately.
const INHERITED_MEMBERSHIPS_SQL: &str = r#"
SELECT
  (SELECT count(*)
     FROM pg_auth_members membership
     JOIN pg_roles granted ON granted.oid = membership.roleid
    WHERE membership.member = login.oid
      AND membership.inherit_option
      AND granted.rolname <> $1) AS inheriting_memberships,
  (SELECT granted.rolname
     FROM pg_auth_members membership
     JOIN pg_roles granted ON granted.oid = membership.roleid
    WHERE membership.member = login.oid
      AND membership.inherit_option
      AND granted.rolname <> $1
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
    workflow_owner_member: bool,
    creates_in_system_schema: bool,
    system_table_writes: bool,
    system_column_writes: bool,
    system_sequence_writes: bool,
    unexpected_system_reads: bool,
    required_system_reads: bool,
    /// How many role memberships this login inherits ambiently, excluding
    /// [`AMBIENT_MEMBERSHIP_EXEMPTION`]. Must be zero: see [`validate`].
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
    if !posture.replication || !posture.bypass_rls {
        return Err(
            "worker database role requires only REPLICATION and BYPASSRLS for logical decoding"
                .to_string(),
        );
    }
    if !posture.workflow_owner_member {
        return Err("worker database role is not a member of zeroship_workflow_owner".to_string());
    }
    if posture.creates_in_system_schema {
        return Err("worker database role has CREATE on the zeroship schema".to_string());
    }
    if posture.system_table_writes
        || posture.system_column_writes
        || posture.system_sequence_writes
    {
        return Err("worker database role has an effective platform write privilege".to_string());
    }
    if posture.unexpected_system_reads {
        return Err("worker database role can read platform columns outside its workflow projection"
            .to_string());
    }
    if !posture.required_system_reads {
        return Err("worker database role lacks its required workflow projection".to_string());
    }
    // THE FENCE ARM. Everything above bounds what this login role may do on the
    // PLATFORM schema. This one bounds what it may do on TENANT schemas, and it
    // is the only check that makes `SET LOCAL ROLE` a fence rather than an
    // optional narrowing.
    //
    // `zeroship_worker` is a single login role shared by every app, and the
    // migration service grants it each app's runtime role. If those memberships
    // inherit, the worker's ambient authority is the UNION of every tenant it
    // has ever served - 561 inheriting memberships on the provisioned dev
    // database when this was written - and a query path that omits
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
    SELECT 1 FROM pg_roles owner
     WHERE owner.rolname = 'zeroship_workflow_owner'
       AND pg_has_role(current_user, owner.oid, 'MEMBER')
  ) AS workflow_owner_member,
  has_schema_privilege(current_user, 'zeroship', 'CREATE') AS creates_in_system_schema,
  EXISTS (
    SELECT 1
      FROM pg_class relation
      JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
      CROSS JOIN (VALUES ('INSERT'), ('UPDATE'), ('DELETE'), ('TRUNCATE'),
                         ('REFERENCES'), ('TRIGGER')) privilege(name)
     WHERE namespace.nspname = 'zeroship'
       AND relation.relkind IN ('r', 'p', 'v', 'm', 'f')
       AND has_table_privilege(current_user, relation.oid, privilege.name)
  ) AS system_table_writes,
  EXISTS (
    SELECT 1
      FROM pg_class relation
      JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
      CROSS JOIN (VALUES ('INSERT'), ('UPDATE'), ('REFERENCES')) privilege(name)
     WHERE namespace.nspname = 'zeroship'
       AND relation.relkind IN ('r', 'p', 'v', 'm', 'f')
       AND has_any_column_privilege(current_user, relation.oid, privilege.name)
  ) AS system_column_writes,
  EXISTS (
    SELECT 1
      FROM pg_class relation
      JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
      CROSS JOIN (VALUES ('USAGE'), ('UPDATE')) privilege(name)
     WHERE namespace.nspname = 'zeroship'
       AND relation.relkind = 'S'
       AND has_sequence_privilege(current_user, relation.oid, privilege.name)
  ) AS system_sequence_writes,
  EXISTS (
    SELECT 1
      FROM pg_attribute attribute
      JOIN pg_class relation ON relation.oid = attribute.attrelid
      JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
     WHERE namespace.nspname = 'zeroship'
       AND relation.relkind IN ('r', 'p', 'v', 'm', 'f')
       AND attribute.attnum > 0
       AND NOT attribute.attisdropped
       AND has_column_privilege(current_user, relation.oid, attribute.attnum, 'SELECT')
       AND NOT (
         (relation.relname = 'apps' AND attribute.attname IN ('id', 'plan_id', 'workflows_enabled'))
         OR (relation.relname = 'plans' AND attribute.attname IN
             ('id', 'name', 'runtime_limits_json', 'workflows_allowed', 'archived'))
         OR (relation.relname = 'app_deploys' AND attribute.attname IN
             ('id', 'app_id', 'deploy_hash', 'manifest_json', 'activated_at', 'created_at'))
       )
  ) AS unexpected_system_reads,
  has_column_privilege(current_user, 'zeroship.apps', 'id', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.apps', 'plan_id', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.apps', 'workflows_enabled', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.plans', 'id', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.plans', 'name', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.plans', 'runtime_limits_json', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.plans', 'workflows_allowed', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.plans', 'archived', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.app_deploys', 'id', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.app_deploys', 'app_id', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.app_deploys', 'deploy_hash', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.app_deploys', 'manifest_json', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.app_deploys', 'activated_at', 'SELECT')
    AND has_column_privilege(current_user, 'zeroship.app_deploys', 'created_at', 'SELECT')
    AS required_system_reads
FROM pg_roles role
WHERE role.rolname = current_user
            "#,
            &[],
        )
        .await
        .map_err(|error| format!("inspect worker database role: {error}"))?;

    let memberships = client
        .query_one(INHERITED_MEMBERSHIPS_SQL, &[&AMBIENT_MEMBERSHIP_EXEMPTION])
        .await
        .map_err(|error| format!("inspect worker role memberships: {error}"))?;

    validate(&DatabasePosture {
        current_user: row.get("current_user"),
        superuser: row.get("superuser"),
        create_role: row.get("create_role"),
        create_db: row.get("create_db"),
        replication: row.get("replication"),
        bypass_rls: row.get("bypass_rls"),
        workflow_owner_member: row.get("workflow_owner_member"),
        creates_in_system_schema: row.get("creates_in_system_schema"),
        system_table_writes: row.get("system_table_writes"),
        system_column_writes: row.get("system_column_writes"),
        system_sequence_writes: row.get("system_sequence_writes"),
        unexpected_system_reads: row.get("unexpected_system_reads"),
        required_system_reads: row.get("required_system_reads"),
        inheriting_memberships: memberships.get("inheriting_memberships"),
        inheriting_membership_example: memberships.get("inheriting_membership_example"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn narrow_posture() -> DatabasePosture {
        DatabasePosture {
            current_user: "zeroship_worker".to_string(),
            superuser: false,
            create_role: false,
            create_db: false,
            replication: true,
            bypass_rls: true,
            workflow_owner_member: true,
            creates_in_system_schema: false,
            system_table_writes: false,
            system_column_writes: false,
            system_sequence_writes: false,
            unexpected_system_reads: false,
            required_system_reads: true,
            inheriting_memberships: 0,
            inheriting_membership_example: None,
        }
    }

    #[test]
    fn rejects_any_effective_write_to_the_platform_schema() {
        let mut posture = narrow_posture();
        posture.system_table_writes = true;

        let error = validate(&posture).expect_err("system write privilege must be refused");
        assert!(error.contains("write privilege"), "unexpected error: {error}");
    }

    #[test]
    fn accepts_only_the_named_narrow_worker_posture() {
        validate(&narrow_posture()).expect("narrow worker posture should be accepted");
    }

    /// A database provisioned before `runtime_dependents_sql` carried
    /// `WITH INHERIT FALSE` leaves the worker inheriting every app role it has
    /// ever been granted. Boot must refuse rather than serve requests from a
    /// login whose ambient authority is the union of every tenant.
    #[test]
    fn refuses_a_login_that_ambiently_inherits_an_app_role() {
        let mut posture = narrow_posture();
        posture.inheriting_memberships = 561;
        posture.inheriting_membership_example =
            Some("app_0191e7a2-b3c4-4d5e-8f90-123456789abc_role".to_string());

        let error = validate(&posture).expect_err("an inheriting app-role grant must be refused");
        assert!(
            error.contains("WITH INHERIT FALSE"),
            "the error must name the fix: {error}"
        );
        assert!(
            error.contains("561") && error.contains("app_0191e7a2"),
            "the error must carry the count and an offending role: {error}"
        );
    }

    /// THE CONTROL, differing in one variable. `narrow_posture` already sets
    /// the count to zero, so the case above could pass because of any other
    /// arm; this pins that a SINGLE inheriting membership is what flips it, and
    /// that the arm is a `> 0` count rather than a threshold.
    #[test]
    fn one_inheriting_membership_is_enough_to_refuse_boot() {
        let mut posture = narrow_posture();
        posture.inheriting_memberships = 1;
        assert!(
            validate(&posture).is_err(),
            "one inheriting membership is one tenant too many"
        );

        posture.inheriting_memberships = 0;
        validate(&posture).expect("zero inheriting memberships is the accepted posture");
    }

    /// The exemption is real and must stay narrow. `zeroship_workflow_owner` is
    /// excluded in SQL, not here, so this pins the CONSTANT that the query
    /// binds - a rename that silently widened the exemption to another role
    /// would still have to edit this line.
    #[test]
    fn the_only_exempt_ambient_membership_is_the_workflow_journal_owner() {
        assert_eq!(AMBIENT_MEMBERSHIP_EXEMPTION, "zeroship_workflow_owner");
    }

    /// [`INHERITED_MEMBERSHIPS_SQL`] MUST ACTUALLY SEE AN INHERITING GRANT.
    ///
    /// The four cases above drive `validate` on a hand-built struct, so every
    /// one of them would pass unchanged if the query feeding that struct always
    /// returned 0 - a check that examines nothing reports exactly what a clean
    /// database reports. This runs the real SQL against a real catalog, in two
    /// arms differing in ONE variable: the grant's inherit option.
    ///
    /// The DSN is typed config (`zeroship_core::config::test_database_url`,
    /// backed by the overlay at `deploy/ops/zeroship.test.toml` or the
    /// pre-existing `PG_TEST_URL` override) - no environment variable is
    /// introduced and none is set. `SET SESSION AUTHORIZATION` needs a
    /// superuser connection; without one this fails naming that, never skips.
    #[compio::test]
    async fn the_membership_query_sees_an_inheriting_grant_and_stops_seeing_a_fenced_one() {
        let dsn = zeroship_core::config::test_database_url();
        let (client, connection) = compio_postgres::connect(&dsn, NoTls)
            .await
            .unwrap_or_else(|error| panic!("connect to {dsn}: {error}"));
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();

        let unique = uuid::Uuid::new_v4().to_string().replace('-', "");
        let app_role = format!("zsposture_{unique}_role");
        let login = format!("zsposture_{unique}_login");

        let drop_all = format!(
            "DROP OWNED BY {login} CASCADE; DROP ROLE IF EXISTS {login}; \
             DROP OWNED BY {app_role} CASCADE; DROP ROLE IF EXISTS {app_role};"
        );
        // Sweep this case's own `zsposture_` prefix first. An assertion firing
        // skips the teardown at the end, and this case's whole job is to fail
        // when the query goes blind - two roles per red run would accumulate on
        // a shared server. The pattern reaches only names this case mints, and
        // the case is the sole user of the prefix, so parallel test threads
        // cannot sweep each other.
        client
            .batch_execute(
                "DO $sweep$ DECLARE target text; BEGIN \
                   FOR target IN SELECT rolname FROM pg_roles \
                                  WHERE rolname LIKE 'zsposture\\_%' LOOP \
                     EXECUTE format('DROP OWNED BY %I CASCADE', target); \
                     EXECUTE format('DROP ROLE IF EXISTS %I', target); \
                   END LOOP; END $sweep$;",
            )
            .await
            .expect("sweep leftovers from an earlier failed run");
        client
            .batch_execute(&format!(
                "CREATE ROLE {app_role} NOLOGIN; CREATE ROLE {login} LOGIN;"
            ))
            .await
            .expect("create the scratch app role and stand-in login");

        // Read the query as the stand-in login, so `current_user` inside it is
        // the role under test rather than the admin running the case.
        let count_as_login = async |client: &compio_postgres::Client| -> (i64, Option<String>) {
            client
                .batch_execute(&format!("SET SESSION AUTHORIZATION {login}"))
                .await
                .expect("SET SESSION AUTHORIZATION needs a superuser connection");
            let row = client
                .query_one(INHERITED_MEMBERSHIPS_SQL, &[&AMBIENT_MEMBERSHIP_EXEMPTION])
                .await
                .expect("run the membership query");
            client
                .batch_execute("RESET SESSION AUTHORIZATION")
                .await
                .expect("restore the admin identity");
            (
                row.get("inheriting_memberships"),
                row.get("inheriting_membership_example"),
            )
        };

        // ARM A - a login with no memberships at all. The zero this arm
        // establishes is what makes arm B's non-zero meaningful.
        let (baseline, _) = count_as_login(&client).await;
        assert_eq!(baseline, 0, "a fresh login inherits nothing");

        // ARM B - the pre-fix grant. The query must SEE it.
        client
            .batch_execute(&format!("GRANT {app_role} TO {login}"))
            .await
            .expect("legacy inheriting grant");
        let (leaked, example) = count_as_login(&client).await;
        assert_eq!(
            leaked, 1,
            "the query must see an ambiently inherited app role - if this is 0 \
             the boot gate is blind and every case above proves nothing"
        );
        assert_eq!(example.as_deref(), Some(app_role.as_str()));

        // ARM C - the post-fix grant, re-issued over the same membership with
        // no REVOKE, exactly as `runtime_dependents_sql` does on the next apply.
        client
            .batch_execute(&format!(
                "GRANT {app_role} TO {login} WITH INHERIT FALSE"
            ))
            .await
            .expect("fenced grant");
        let (fenced, fenced_example) = count_as_login(&client).await;
        assert_eq!(fenced, 0, "the fenced grant must not count as ambient");
        assert_eq!(fenced_example, None);

        client.batch_execute(&drop_all).await.expect("teardown");
    }
}

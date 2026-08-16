use compio_postgres::NoTls;

const WORKER_DATABASE_ROLE: &str = "zeroship_worker";

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
}

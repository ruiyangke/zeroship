import { raw } from "@zeroship/migrate";
import { readFileSync } from "node:fs";

// The workflow journal, in the workflow service's own schema.
//
// WHY A PLATFORM MIGRATION AND NOT THE SERVICE AT BOOT. The service holds no
// authority to install it. `Coordinator::verify`
// (crates/zeroship-workflow-server/src/coordinator.rs) refuses to start when
// `has_schema_privilege(current_user,'workflow_manager','CREATE')` is true, so
// the login the service opens `workflow.database_url` with is required, by its
// own startup contract, to have no DDL on this schema. `workflow_manager` and
// every table already in it arrive from 20260911000000_workflow_coordination.ts,
// which is what "the mechanism by which this service installs its own schema"
// means in this tree. The journal joins them.
//
// The other installer this workspace owns cannot target this schema either. The
// migration service's schema-bundle path (`apply_schema_bundle` in
// crates/zeroship-migrate-server/src/bundle.rs) calls `provision_database`
// unconditionally, and that runs `ALTER SCHEMA <target> OWNER TO migrator_<hash>`
// followed by a per-schema runtime login holding DML on ALL TABLES IN SCHEMA.
// Pointed at `workflow_manager` it would take the schema away from
// `zeroship_workflow_migrator` and mint a login with full reach over the
// manager's queue. It stays the installer for a journal in a CREATOR schema,
// which is a different target with different owners.
//
// WHY RAW SQL. Every object here sits behind the platform-reserved
// `__zeroship_` prefix, and `validate_collection`
// (crates/zeroship-migrate-core/src/schema/query.rs) refuses that prefix for
// every recorded operation, at validate, before lower. The op DSL cannot
// declare these tables at all. That is the same reason the schema-bundle path
// carries SQL rather than operations, and the generated artifact is carried
// verbatim rather than re-authored.
//
// ONE ROLE IS GRANTED, AND IT IS NOT NAMED HERE. The grantee is derived below
// from the grants the coordination tables in this schema already carry, and no
// other role receives anything. There is no REVOKE: it would only mask a
// default privilege arriving from somewhere else, and
// crates/zeroship-workflow-server/tests/platform_schema.rs asserts that every
// other role holds nothing, which surfaces one instead.
const schema = "workflow_manager";

// The QUOTED placeholder crates/zeroship-workflow-schema generates its
// PostgreSQL DDL with. Quoted because substituting the bare word would also
// rewrite `__zeroship_workflow_schema_version`, the stamp table.
const placeholder = '"__zeroship_workflow_schema"';

const artifacts = new URL("../../crates/zeroship-workflow-schema/schema/", import.meta.url);
const snapshot = readFileSync(new URL("postgres.sql", artifacts), "utf8");
if (!snapshot.includes(placeholder)) {
  throw new Error("the workflow journal artifact carries no schema placeholder");
}
const journal = snapshot.replaceAll(placeholder, `"${schema}"`);
if (journal.includes("__zeroship_workflow_schema\"")) {
  throw new Error("the workflow journal artifact still carries an unbound placeholder");
}

// The generated descriptor is the authority on which tables the artifact
// creates; a hand-copied list here would drift from it silently.
const descriptor = JSON.parse(readFileSync(new URL("schema.runtime.json", artifacts), "utf8"));
const tables = Object.keys(descriptor.collections).sort();
if (!tables.length) throw new Error("the workflow journal descriptor declares no tables");
for (const name of tables) {
  if (!/^__zeroship_workflow_[a-z_]+$/.test(name)) {
    throw new Error(`unexpected workflow journal table: ${name}`);
  }
  if (!journal.includes(`"${schema}"."${name}"`)) {
    throw new Error(`the workflow journal artifact does not create ${name}`);
  }
}

// THE GRANTEE IS DERIVED FROM THE CATALOG, NOT NAMED HERE.
//
// The journal is the storage of whichever service already serves this schema,
// not a second tenant of it, so the role that must read and write it is the one
// that already holds DML on the coordination tables beside it. Spelling a role
// here would make this a second place to know that name, free to disagree with
// the first the moment the service's login moves; the disagreement would
// surface as a service that starts and then cannot read its own journal. Read
// from the catalog, the two grants cannot drift: whatever the coordination
// tables are granted to, the journal follows in the same schema.
//
// EXACTLY ONE ROLE, OR NOTHING. A lookup finding none has no grantee to follow,
// and one finding several cannot say which service the journal belongs to.
// Both abort. Neither picks.
//
// The lookup reads explicit ACL entries only. PUBLIC and the owner's implicit
// rights are excluded, so it answers with a role that was deliberately granted
// DML rather than one that merely happens to have it.
const grantJournal = `DO $$
DECLARE
  service_role text;
  journal_table text;
BEGIN
  BEGIN
    SELECT DISTINCT served.grantee_name
      INTO STRICT service_role
      FROM (
        SELECT pg_get_userbyid(entry.grantee) AS grantee_name
          FROM pg_class c
          JOIN pg_namespace n ON n.oid = c.relnamespace
          CROSS JOIN LATERAL aclexplode(c.relacl) AS entry
         WHERE n.nspname = '${schema}'
           AND c.relkind = 'r'
           AND NOT starts_with(c.relname::text, '__zeroship_workflow_')
           AND entry.grantee <> 0
           AND entry.grantee <> c.relowner
           AND entry.privilege_type IN ('SELECT', 'INSERT', 'UPDATE', 'DELETE')
         GROUP BY c.oid, entry.grantee
        HAVING count(DISTINCT entry.privilege_type) = 4
      ) AS served;
  EXCEPTION
    WHEN NO_DATA_FOUND THEN
      RAISE EXCEPTION 'no role holds DML on the ${schema} coordination tables, so the workflow journal has no grantee to follow';
    WHEN TOO_MANY_ROWS THEN
      RAISE EXCEPTION 'several roles hold DML on the ${schema} coordination tables, so the workflow journal grantee is ambiguous';
  END;

  FOREACH journal_table IN ARRAY ARRAY[${tables.map(name => `'${name}'`).join(", ")}] LOOP
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I.%I TO %I', '${schema}', journal_table, service_role);
  END LOOP;
END
$$`;

export default {
  name: "workflow_journal",
  schema() {
    // The snapshot is the fold of the whole generated series followed by its
    // stamp, so the DDL and the one row that names its version and fingerprint
    // arrive together. There is one stamp row for the installation, not one per
    // app: the `app_id` columns inside the journal are its tenant discriminator.
    raw({
      sql: journal,
      reason: "the workflow journal's generated schema and its single stamp row",
    });
    raw({
      sql: tables
        .map(name => `ALTER TABLE "${schema}"."${name}" OWNER TO zeroship_workflow_migrator`)
        .join(";\n"),
      reason: "workflow journal ownership belongs to the migration role, as the rest of this schema does",
    });
    raw({
      sql: grantJournal,
      reason: "the journal is read and written by whichever role already serves this schema",
    });
  },
};

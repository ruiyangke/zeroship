import { role, raw, revoke } from "@zeroship/migrate";

// The worker's database authority, narrowed to exactly what its workflow
// dispatcher reads. Originally landed by EDITING three already-applied files in
// place (2a44ea8ef, across 20260702000100_schema_roles_extensions.ts,
// 20260702000900_grants.ts and 20260705000000_durable_workflows_journal.ts).
// Every one of those had been journalled by a deployed database, so the edits
// made the runner's checksum guard refuse - correctly, and permanently. The
// three deltas are one intent and re-land together here.
//
// ORDER IS LOAD-BEARING AND RUNS LATE ON PURPOSE. 20260702000900 grants the
// worker its original, wider privileges; this file must run after it for the
// deny below to be the final word. Filename order puts it after every file that
// grants anything in `zeroship`, which is the same relative position the edit
// held inside 20260702000900 (deny first, then re-grant) - the deny is last
// overall instead of first within one file, and the end state is identical
// because nothing between the two grants the worker anything this does not
// revoke.
//
// THAT USED TO SAY "asserted, not assumed", citing
// crates/zeroship-migrate-adapter/tests/platform_migrate.rs (DELETED 2026-08-28
// in ccda4bb42), which checked the worker holds no write privilege on ANY
// relation in `zeroship` after the whole corpus runs. The check went with the
// crate, so NOTHING ASSERTS THIS TODAY - the end-state argument above is
// reasoning, not a measurement. The claim is stated as unproven rather than
// repointed at a test that does not check the same thing.
//
// (The citation survived the 2026-09-04 sweep that repaired its two siblings
// because it WRAPPED ACROSS TWO LINES, and both citation gates extract per line.
// tests/doc_citation_gate.sh arm 7 now unwraps `//` continuations for exactly
// this reason.)

export default {
  name: "worker_database_authority",
  schema() {
    // NOLOGIN owner of the per-app workflow journals. It owns objects; it is
    // never a connection identity, which is why it is separate from the login
    // role rather than being the login role wearing a second hat.
    role("zeroship_workflow_owner").create({ login: false, ifNotExists: true });

    // The worker streams creator-table WAL and owns only per-app workflow
    // journals. REPLICATION is required for the former; membership in the
    // NOLOGIN workflow owner is required for the latter. Neither capability
    // grants a write path into the platform schema.
    raw({
      sql: "ALTER ROLE zeroship_worker WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE INHERIT REPLICATION BYPASSRLS",
      reason: "the role DSL does not expose PostgreSQL's REPLICATION attribute",
    });
    raw({
      sql: "ALTER ROLE zeroship_workflow_owner WITH NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS",
      reason: "reassert the exact narrow attributes of the workflow journal owner",
    });
    raw({
      sql: "GRANT zeroship_workflow_owner TO zeroship_worker",
      reason: "reassert membership when platform roles predate a fresh migration run",
    });

    // Start from zero effective authority over every current and future platform
    // relation. Later grants give the worker only the columns its workflow
    // dispatcher reads. This schema-wide deny is the class boundary: it includes
    // device_grants and every other authorization-decision table without relying
    // on a hand-maintained sensitive-table list.
    raw({
      sql: "REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA zeroship FROM zeroship_worker",
      reason: "the grant DSL has no ALL TABLES IN SCHEMA target",
    });
    raw({
      sql: "REVOKE ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA zeroship FROM zeroship_worker",
      reason: "the worker must not advance platform-owned sequences",
    });
    // THESE TWO STATEMENTS STORE NOTHING, AND TWO OTHER MIGRATIONS CREDITED
    // THEM WITH DENYING THE WORKER UNTIL 2026-09-07. `ALTER DEFAULT PRIVILEGES
    // ... REVOKE` subtracts from the default privilege set; `zeroship_worker`
    // was never IN that set, so each is a no-op and `pg_default_acl` stays
    // empty. What actually denies the worker on a new platform table is
    // PostgreSQL's owner-only default: a fresh table has a null `relacl` and
    // nobody but the owner holds anything.
    //
    // They are kept because they make the INTENT explicit and would become
    // load-bearing the moment anything grants `zeroship_worker` a schema-wide
    // default. They are not a fence today. Do not cite them as one, and do not
    // read their presence as evidence that a later grant would be contradicted.
    raw({
      sql: "ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship REVOKE ALL PRIVILEGES ON TABLES FROM zeroship_worker",
      reason: "future platform tables must inherit the same deny-by-default boundary",
    });
    raw({
      sql: "ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship REVOKE ALL PRIVILEGES ON SEQUENCES FROM zeroship_worker",
      reason: "future platform sequences must inherit the same deny-by-default boundary",
    });
    revoke({
      privileges: ["create"],
      on: { kind: "schema", names: ["zeroship"] },
      from: ["zeroship_worker", "zeroship_workflow_owner"],
    });

    // The three column-level reads the dispatcher actually performs, restored
    // after the blanket deny above.
    raw({
      sql: "GRANT SELECT (id, plan_id, workflows_enabled) ON zeroship.apps TO zeroship_worker",
      reason: "workflow claims need only the app's plan and workflow enablement fields",
    });
    raw({
      sql: "GRANT SELECT (id, name, runtime_limits_json, workflows_allowed, archived) ON zeroship.plans TO zeroship_worker",
      reason: "workflow claims need only the plan fields that set execution limits and eligibility",
    });
    raw({
      sql: "GRANT SELECT (id, app_id, deploy_hash, manifest_json, activated_at, created_at) ON zeroship.app_deploys TO zeroship_worker",
      reason: "worker workflow dispatch reads active deploy identity and manifest fields only",
    });
  },
};

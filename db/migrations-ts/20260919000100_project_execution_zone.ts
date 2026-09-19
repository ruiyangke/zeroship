import { createFunction, raw, t, table } from "@zeroship/migrate";

// The execution zone moves up to the project, from
// docs/proposals/2026-08-28-app-database-decoupling.md.
//
// `apps.execution_zone_id` (20260914000600_placement_eligibility.ts) landed
// before anything above the app needed a zone. With a project-owned database it
// is the project that has to carry it, or "same project" stops implying "can
// share" and every sharing surface has to explain a second rule. The app keeps
// its copy under a composite foreign key, in the next migration, so the two
// cannot disagree: `instance_serves_app` joins on it and the workflow manager
// holds a column grant on it, and both keep working untouched.
//
// THE FOREIGN KEY THAT SPENDS THE PAIR IS AUTHORED ELSEWHERE, for the reason
// 20260906000200_apps_project_ownership_key.ts states: the engine lowers a
// foreign key against a catalog snapshot taken before the migration runs, so a
// key whose target table is also declared here sees only the columns this file
// declares. `projects.id` is not one of them. What this file does carry is the
// unique constraint and the index that key consumes, because both have to be
// live before it is lowered.
export default {
  name: "project_execution_zone",
  schema() {
    // ---- projects gains the zone ------------------------------------------
    table("projects", { schema: "zeroship" })
      .column("execution_zone_id")
      .add({ type: t.text().required().default("ezn_default000000000000000000") });
    table("projects", { schema: "zeroship" })
      .foreignKey("projects_execution_zone_fkey")
      .add({
        columns: ["execution_zone_id"],
        references: { table: "execution_zones", columns: ["id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
    raw({
      sql: 'ALTER TABLE "zeroship"."projects" ALTER COLUMN "execution_zone_id" TYPE text COLLATE "C"',
      reason: "typed-id text domains need bytewise comparison with execution_zones.id",
    });
    table("projects", { schema: "zeroship" })
      .unique("projects_zone_identity_key")
      .add({ columns: ["id", "execution_zone_id"] });

    // Frozen for the same reason an app's zone is: placement reads the fact
    // after taking its locks and again before commit, and a fact that could move
    // between those two reads would reopen the window the second read closes.
    createFunction({
      schema: "zeroship",
      name: "projects_reject_execution_zone_change",
      returns: "trigger",
      language: "procedural",
      body:
        "BEGIN\n"
        + "  IF NEW.execution_zone_id IS DISTINCT FROM OLD.execution_zone_id THEN\n"
        + "    RAISE EXCEPTION 'a project''s execution zone is fixed when the project is created'\n"
        + "      USING ERRCODE = 'check_violation';\n"
        + "  END IF;\n"
        + "  RETURN NEW;\n"
        + "END;",
    });
    table("projects", { schema: "zeroship" })
      .trigger("projects_frozen_execution_zone")
      .create({
        timing: "before",
        events: ["update"],
        forEach: "row",
        execute: "projects_reject_execution_zone_change",
      });

    // ---- apps: the identity keys the bindings consume ----------------------
    //
    // `apps` carries only `apps_name_key` today, and its composite ownership key
    // points OUTWARD at projects with nothing pointing in. PostgreSQL requires a
    // real unique constraint on referenced columns, so without this the binding
    // foreign key cannot create.
    table("apps", { schema: "zeroship" })
      .unique("apps_project_identity_key")
      .add({ columns: ["id", "project_id"] });
    // And the index the app's zone key reads from, declared rather than left
    // to the engine's emission: a composite foreign key whose local columns no
    // index leads with has one emitted for it, conditional on the live catalog,
    // which makes the plan a different length on a first apply than on a
    // re-apply. 20260702000600_constraints_indexes_fks.ts declares
    // apps_project_organization_idx for the same reason.
    table("apps", { schema: "zeroship" })
      .index("apps_project_zone_fkey_idx")
      .add({ on: ["project_id", "execution_zone_id"] });
  },
};

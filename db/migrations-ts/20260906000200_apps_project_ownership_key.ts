import { table } from "@zeroship/migrate";

// The key that makes `apps.organization_id` a consumed copy rather than a claim.
//
// db/migrations-ts/20260702000200_control_tables.ts declares `apps.project_id`
// and `apps.organization_id`;
// db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts gives
// the organization copy the bytewise collation its parent already has. This file
// spends the pair: `(project_id, organization_id)` is consumed by
// `projects_organization_identity_key`, so no app row exists whose organization
// disagrees with the organization its project belongs to. PostgreSQL re-checks
// that on every write to either side, which is the entire reason a copy is
// allowed here at all.
//
// IT IS A SEPARATE FILE BECAUSE THE KEY CANNOT BE AUTHORED BESIDE THE COLUMN.
// The engine lowers a foreign key against a catalog snapshot taken before the
// migration runs, and a collation in this corpus is applied by a `raw` island
// that no snapshot and no authored contract can see. In one migration the local
// side is therefore a plain authored `text` while the live target is
// `text COLLATE "C"`, and the lowering refuses the pair -- correctly, since a
// mismatched pair is precisely the silent index degradation the collation rule
// exists to stop. Split across two, both sides are live and both are `C`.
// A raw `ADD CONSTRAINT` would have fit in one file and hidden the single most
// load-bearing constraint in this change from the model; a second file does not.
//
// The index this key reads from is `apps_project_organization_idx`, declared
// explicitly in db/migrations-ts/20260702000600_constraints_indexes_fks.ts
// rather than left to the engine's own emission: an emitted index is
// conditional on the live catalog, which would make this plan a different length
// on a first apply than on a re-apply.
export default {
  name: "apps_project_ownership_key",
  schema() {
    table("apps", { schema: "zeroship" })
      .foreignKey("apps_project_ownership_fkey")
      .add({
        columns: ["project_id", "organization_id"],
        references: { table: "projects", columns: ["id", "organization_id"], schema: "zeroship" },
        onDelete: "restrict",
        onUpdate: "restrict",
      });
  },
};

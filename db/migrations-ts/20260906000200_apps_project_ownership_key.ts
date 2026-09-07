import { table } from "@zeroship/migrate";

// The key that makes `apps.organization_id` a consumed copy rather than a claim.
//
// `db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts` adds
// the column and gives it the bytewise collation its parent already has. This
// file spends it: `(project_id, organization_id)` is consumed by
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
// THE SINGLE-COLUMN PARENT EDGE GOES OUT WITH IT. The composite key already
// implies `project_id` exists in `projects`, so keeping `apps_project_id_fkey`
// beside it is two constraints enforcing one fact, with two names to keep in
// agreement and two RESTRICT checks on every project delete. The edge is dropped
// before the composite is added rather than after, so the two never coexist.
//
// The index this key reads from is `apps_project_organization_idx`, declared in
// the previous migration rather than left to the engine's own emission -- that
// file says why, and the short version is that an emitted index is conditional
// on the live catalog and would make THIS plan a different length on a first
// apply than on a re-apply.
export default {
  name: "apps_project_ownership_key",
  schema() {
    table("apps", { schema: "zeroship" }).constraint("apps_project_id_fkey").drop();
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

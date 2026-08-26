import { table, t } from "@zeroship/migrate";

// One table, one ownership column. `owner_id` holds the authenticated caller's
// per-app pairwise subject (`pws_…`) — the value `env.auth.getUser().id`
// returns. Every read in src/index.ts filters on it server-side.
//
// The platform prepends the seven system columns (id, created_at, updated_at,
// created_by, updated_by, version, deleted_at) to every table it creates, so
// they are not declared here.
export default {
  name: "create_notes",
  schema() {
    table("notes").create({
      columns: {
        owner_id: t.text().notNull(),
        title: t.text().notNull(),
        body: t.text().notNull(),
      },
      indexes: [
        // Every list query is `WHERE owner_id = $me`, so this is the one index
        // the app actually needs.
        { name: "notes_owner_id_idx", on: ["owner_id"] },
      ],
    });
  },
};

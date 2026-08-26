import { table, t } from "@zeroship/migrate";

// db-chat's schema, authored migration-first.
//
// WHY THIS FILE EXISTS. This example declared its schema INLINE
// (`export default { schema: dbSchema }`) with no `migrations/` directory,
// which is the #209/#174 mechanism: `env.db` is installed from the generated
// runtime descriptor, folded from COMMITTED MIGRATIONS, and an inline `schema`
// export is not a source for it. Measured 2026-08-12 by running
// `scripts/smoke.sh` against a dev server I booted by hand: `.zeroship/` held
// `kv.redb` and `workflows.sqlite` but NO `dev.sqlite`, and all three setup
// RPCs returned `500 {"message":"internal error"}`.
//
// TWO SPELLING RULES, both learned by failing rather than from the types:
//
//   1. The seven platform system columns (id, created_at, updated_at,
//      created_by, updated_by, version, deleted_at) are INJECTED by the
//      confined charter. Declaring `id` here collides with the injected
//      column and the descriptor is refused.
//
//   2. `t.ref()` is DECLARED in @zeroship/migrate's public types but is not
//      accepted by the vendored engine; a native foreign key is spelled
//      `t.text().references(table, column)`. examples/db-todos and
//      examples/db-e2e carry the same note.
//
// The inline `dbSchema` in src/server.ts stays as the QUERY-side type source
// (it is what gives `db.messages.find(...)` its types); this file is what
// creates the tables. db-todos has the same pair.
//
// UNLIKE examples/db-e2e, this example's schema uses no encrypted columns, no
// vector, no geoPoint and no full-text index, so all of it IS representable in
// a migration. db-e2e stops short at `t.encrypted`, which cannot carry a mode
// or a keyId -- see the comment in its migration.
export default {
  name: "create_db_chat",
  schema() {
    table("users").create({
      columns: {
        handle: t.text().notNull().unique(),
        name: t.text().notNull(),
      },
    });

    table("channels").create({
      columns: {
        slug: t.text().notNull().unique(),
        name: t.text().notNull(),
        // `topic` is the one optional column in this example: the schema
        // declares `t.string()` with no `.required()`.
        topic: t.text(),
      },
    });

    table("messages").create({
      columns: {
        channelId: t.text().notNull().references("channels", "id"),
        authorId: t.text().notNull().references("users", "id"),
        body: t.text().notNull(),
        flagged: t.boolean().notNull().default(false),
      },
      indexes: [{ name: "messages_channel_idx", on: ["channelId"] }],
    });
  },
};

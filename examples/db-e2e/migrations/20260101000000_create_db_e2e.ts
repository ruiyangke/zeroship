import { table, t } from "@zeroship/migrate";

// db-e2e's schema, authored migration-first.
//
// WHY THIS FILE EXISTS. This example used to declare its schema INLINE
// (`export default { schema: dbSchema }` built from `@zeroship/db`'s
// `schema()`/`t.*`) with no `migrations/` directory. That is the #209/#174
// mechanism: the installer builds `env.db` from the generated runtime
// descriptor, which is folded from COMMITTED MIGRATIONS, and an inline
// `schema` export is not a source for it. The app built, served, and answered
// `health` -- and then every data-plane call failed, because `env.db.<name>`
// was undefined. Measured 2026-08-12: `db-e2e.seed-demo` returned
// `500 {"message":"internal error"}` over `TypeError: Cannot read properties
// of undefined (reading 'insertMany')`, and `.zeroship/` held no `dev.sqlite`
// at all. All 15 of this example's RPCs were unreachable and always had been.
//
// TWO SPELLING RULES, both learned by failing rather than from the types:
//
//   1. The seven platform system columns (id, created_at, updated_at,
//      created_by, updated_by, version, deleted_at) are INJECTED by the
//      confined charter. Declaring `id` here collides with the injected
//      column and the descriptor is refused.
//
//   2. `t.ref()` is DECLARED in @zeroship/migrate's public types
//      (sdks/migrate/src/types.ts) but is not accepted by the vendored
//      engine; a native foreign key is spelled
//      `t.text().references(table, column)`. examples/db-todos carries the
//      same note, written after the same failure.
//
// The inline `dbSchema` in src/server.ts stays as the QUERY-side type source
// (it is what gives `db.tasks.find(...)` its types); it is this file that
// creates the tables. db-todos has the same pair.
export default {
  name: "create_db_e2e",
  up() {
    table("workspaces").create({
      columns: {
        slug: t.text().notNull().unique(),
        name: t.text().notNull(),
        tier: t.text().notNull().default("free"),
        region: t.text().notNull(),
      },
      indexes: [{ name: "workspaces_tier_idx", on: ["tier"] }],
    });

    table("users").create({
      columns: {
        workspaceId: t.text().notNull().references("workspaces", "id"),
        handle: t.text().notNull().unique(),
        fullName: t.text().notNull(),
        email: t.text().notNull().unique(),
        // BLOCKED, AND THIS IS THE ONE THING THIS FILE CANNOT EXPRESS.
        // src/server.ts declares `contactEmail` as `mode: "deterministic",
        // keyId: "db_e2e"` and `ssn` as `mode: "randomised", keyId: "db_e2e"`.
        // NEITHER the mode NOR the key id is representable in a migration:
        // `t.encrypted()` takes only `of`, and `keyId` appears ZERO times in
        // the whole of sdks/migrate/src. The vendored engine says so itself, by
        // design and fail-closed rather than silently wrong -- see the comment
        // and test at third_party/zero-migrate/.../render/fold.rs:7930
        // ("op.* can author ONLY a DEFAULT-mode encrypted column ... there is
        // no IR surface for a non-default mode/keyId").
        //
        // The OBSERVABLE consequence, measured 2026-08-12: the generated
        // descriptor comes out carrying `keyId: "default"`, so the running app
        // demands `ZEROSHIP_COLUMN_KEY_DEFAULT` and fails with
        // `Column key 'default' not configured` -- while README.md, the schema
        // and e2e/run.mjs all say the key is `db_e2e`. An app moving from an
        // inline schema to migration-first therefore LOSES its key selection
        // and its encryption mode, and nothing warns.
        //
        // Deliberately NOT worked around: weakening src/server.ts to the
        // default key/mode would make this example pass by deleting the
        // property it exists to cover. It stays red here until the IR can
        // carry mode/keyId.
        contactEmail: t.encrypted({ of: t.text() }),
        ssn: t.encrypted({ of: t.text() }),
        city: t.text().notNull(),
      },
      indexes: [{ name: "users_workspace_idx", on: ["workspaceId"] }],
    });

    table("tasks").create({
      columns: {
        workspaceId: t.text().notNull().references("workspaces", "id"),
        ownerId: t.text().notNull().references("users", "id"),
        title: t.text().notNull(),
        description: t.text().notNull(),
        status: t.text().notNull().default("open"),
        priority: t.double().notNull(),
        score: t.double().notNull(),
        category: t.text().notNull(),
        tags: t.json(),
      },
      indexes: [
        { name: "tasks_workspace_status_idx", on: ["workspaceId", "status"] },
        { name: "tasks_workspace_priority_idx", on: ["workspaceId", "priority"] },
      ],
    });

    table("places").create({
      columns: {
        workspaceId: t.text().notNull().references("workspaces", "id"),
        name: t.text().notNull(),
        description: t.text().notNull(),
        category: t.text().notNull(),
        loc: t.geoPoint().notNull(),
        embedding: t.vector({ dimensions: 4, metric: "cosine" }),
        open: t.boolean().notNull().default(true),
      },
      indexes: [
        { name: "places_workspace_category_idx", on: ["workspaceId", "category"] },
      ],
    });
  },
};

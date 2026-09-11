import { table, t } from "@zeroship/migrate";

// Migrations own the schema and its mask configuration. Generated env.db.ts
// supplies the matching TypeScript surface. The charter injects system fields;
// application relationships are declared with native foreign keys.
export default {
  name: "create_db_e2e",
  schema() {
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
        contactEmail: t.encrypted({ of: t.text() }).mask({ kind: "email", classification: "pii" }),
        ssn: t.encrypted({ of: t.text() }).mask({ kind: "last4", classification: "spi" }),
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

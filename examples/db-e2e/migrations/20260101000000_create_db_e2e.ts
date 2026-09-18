import { table, t } from "@zeroship/migrate";

// Migrations own the schema and its mask configuration. Generated env.db.ts
// supplies the matching TypeScript surface. The charter injects system fields;
// application relationships are declared with native foreign keys.
export default {
  name: "create_db_e2e",
  schema() {
    table("workspaces").create({
      columns: {
        slug: t.text().required().unique(),
        name: t.text().required(),
        tier: t.text().required().default("free"),
        region: t.text().required(),
      },
      indexes: [{ name: "workspaces_tier_idx", on: ["tier"] }],
    });

    table("users").create({
      columns: {
        workspaceId: t.text().required().references("workspaces", "id", { relation: "workspace" }),
        handle: t.text().required().unique(),
        fullName: t.text().required(),
        email: t.text().required().unique(),
        contactEmail: t.text().encrypted().mask({ kind: "email", classification: "pii" }),
        ssn: t.text().encrypted().mask({ kind: "last4", classification: "spi" }),
        city: t.text().required(),
      },
      indexes: [{ name: "users_workspace_idx", on: ["workspaceId"] }],
    });

    table("tasks").create({
      options: { versioning: true, softDelete: true },
      columns: {
        workspaceId: t.text().required().references("workspaces", "id", { relation: "workspace" }),
        ownerId: t.text().required().references("users", "id", { relation: "owner" }),
        title: t.text().required(),
        description: t.text().required(),
        status: t.text().required().default("open"),
        priority: t.double().required(),
        score: t.double().required(),
        category: t.text().required(),
        tags: t.json(),
      },
      indexes: [
        { name: "tasks_workspace_status_idx", on: ["workspaceId", "status"] },
        { name: "tasks_workspace_priority_idx", on: ["workspaceId", "priority"] },
      ],
    });

    table("places").create({
      columns: {
        workspaceId: t.text().required().references("workspaces", "id", { relation: "workspace" }),
        name: t.text().required(),
        description: t.text().required(),
        category: t.text().required(),
        loc: t.geoPoint().required(),
        embedding: t.vector({ dimensions: 4, metric: "cosine" }),
        open: t.boolean().required().default(true),
      },
      indexes: [
        { name: "places_workspace_category_idx", on: ["workspaceId", "category"] },
      ],
    });
  },
};

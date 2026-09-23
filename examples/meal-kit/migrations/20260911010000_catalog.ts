import { table, t } from "@zeroship/migrate";

export default {
  name: "catalog",
  schema() {
    table("meal_recipes").create({
      columns: {
        slug: t.text().required().unique(),
        draft: t.json().required(),
        archived: t.boolean().required().default(false),
      },
    });
    table("meal_recipe_versions").create({
      columns: {
        recipe_id: t.text().required().references("meal_recipes", "id"),
        revision: t.int().required(),
        content: t.json().required(),
        approved_by: t.text().required(),
        approval_note: t.text().required(),
        approved_at: t.text().required(),
      },
      indexes: [
        {
          name: "meal_recipe_revision",
          on: ["recipe_id", "revision"],
          unique: true,
        },
      ],
    });
    table("meal_menus").create({
      columns: {
        menu_key: t.text().required().unique(),
        market: t.text().required(),
        delivery_date: t.text().required(),
        draft: t.json().required(),
        published_version_id: t.text(),
        status: t.text().required().default("draft"),
        history: t.json().required(),
      },
    });
    table("meal_menu_versions").create({
      columns: {
        menu_id: t.text().required().references("meal_menus", "id"),
        market: t.text().required(),
        revision: t.int().required(),
        content: t.json().required(),
        published_by: t.text().required(),
        published_at: t.text().required(),
      },
      indexes: [
        {
          name: "meal_menu_revision",
          on: ["menu_id", "revision"],
          unique: true,
        },
      ],
    });
  },
};

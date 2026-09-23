import { table, t } from "@zeroship/migrate";

export default {
  name: "recipe_feedback",
  schema() {
    table("meal_recipe_feedback").create({
      columns: {
        owner_id: t.text().required(),
        order_id: t.text().required().references("meal_orders", "id"),
        market: t.text().required(),
        recipe_id: t.text().required(),
        recipe_version_id: t
          .text()
          .required()
          .references("meal_recipe_versions", "id"),
        rating: t.int().required(),
        cook_again: t.boolean(),
        comment: t.text().required(),
        last_request_key: t.text().required(),
      },
      indexes: [
        {
          name: "meal_feedback_order_recipe",
          on: ["order_id", "recipe_version_id"],
          unique: true,
        },
        { name: "meal_feedback_owner", on: ["owner_id"] },
        { name: "meal_feedback_market", on: ["market"] },
      ],
    });
  },
};

import { table, t } from "@zeroship/migrate";
export default {
  name: "meal_kit",
  schema() {
    table("meal_profiles").create({
      columns: {
        owner_id: t.text().required().unique(),
        preferences: t.json().required(),
      },
    });
    table("meal_orders").create({
      columns: {
        owner_id: t.text().required(),
        market: t.text().required(),
        request_key: t.text().required().unique(),
        status: t.text().required(),
        payment: t.text().required(),
        fulfillment: t.text().required(),
        total: t.int().required(),
        refunded: t.int().required().default(0),
        snapshot: t.json().required(),
        timeline: t.json().required(),
      },
      indexes: [{ name: "meal_orders_owner", on: ["owner_id"] }],
    });
    table("meal_plans").create({
      columns: {
        owner_id: t.text().required(),
        market: t.text().required(),
        status: t.text().required(),
        configuration: t.json().required(),
        next_date: t.text().required(),
        skipped: t.json().required(),
      },
      indexes: [
        {
          name: "meal_plans_owner_market",
          on: ["owner_id", "market"],
          unique: true,
        },
      ],
    });
    table("meal_addresses").create({
      columns: {
        owner_id: t.text().required(),
        market: t.text().required(),
        label: t.text().required(),
        address: t.json().required(),
        is_default: t.boolean().required(),
      },
      indexes: [
        { name: "meal_addresses_owner_market", on: ["owner_id", "market"] },
      ],
    });
    table("meal_privacy_requests").create({
      columns: {
        owner_id: t.text().required(),
        request_key: t.text().required().unique(),
        kind: t.text().required(),
        status: t.text().required(),
        history: t.json().required(),
        object_key: t.text(),
      },
      indexes: [{ name: "meal_privacy_owner", on: ["owner_id"] }],
    });
    table("meal_inventory").create({
      columns: {
        stock_key: t.text().required().unique(),
        market: t.text().required(),
        recipe_id: t.text().required(),
        available: t.int().required(),
        published: t.boolean().required().default(true),
      },
    });
    table("meal_cases").create({
      columns: {
        owner_id: t.text().required(),
        order_id: t.text().required().references("meal_orders", "id"),
        category: t.text().required(),
        message: t.text().required(),
        status: t.text().required(),
        resolution: t.text(),
        attachment: t.text(),
      },
      indexes: [{ name: "meal_cases_owner", on: ["owner_id"] }],
    });
    table("meal_events").create({
      columns: {
        event_key: t.text().required().unique(),
        kind: t.text().required(),
        payload: t.json().required(),
      },
    });
    table("meal_waitlist").create({
      columns: {
        contact_key: t.text().required().unique(),
        email: t.text().required(),
        market: t.text().required(),
        postal: t.text().required(),
        area: t.json().required(),
      },
    });
  },
};

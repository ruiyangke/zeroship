import { table, t } from "@zeroship/migrate";

export default {
  name: "checkout_reservations",
  schema() {
    table("meal_checkout_attempts").create({
      columns: {
        order_id: t.text().required().references("meal_orders", "id"),
        owner_id: t.text().required(),
        market: t.text().required(),
        payment: t.text().required(),
        reservation: t.text().required(),
        expires_at: t.text().required(),
        cart: t.json().required(),
        total: t.int().required(),
        currency: t.text().required(),
      },
      indexes: [
        { name: "meal_checkout_order", on: ["order_id"] },
        {
          name: "meal_checkout_expiry",
          on: ["market", "reservation", "expires_at"],
        },
      ],
    });
  },
};

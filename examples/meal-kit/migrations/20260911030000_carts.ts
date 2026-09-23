import { table, t } from "@zeroship/migrate";

export default {
  name: "saved_carts",
  schema() {
    table("meal_carts").create({
      columns: {
        principal: t.text().required(),
        market: t.text().required(),
        cart: t.json().required(),
        expires_at: t.text().required(),
        last_request_key: t.text().required(),
      },
      indexes: [
        { name: "meal_carts_principal_market", on: ["principal", "market"], unique: true },
        { name: "meal_carts_expiry", on: ["expires_at"] },
      ],
    });
  },
};

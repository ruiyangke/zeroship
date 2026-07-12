import { view } from "zero-migrate";

export default {
  up() {
    view("order_rollups").create({
      as: (q) => q
        .from("orders")
        .select([
          "customer_id",
          { kind: "expr", alias: "item_names", expr: (col) => col("item_name").stringAgg(", ") },
          { kind: "expr", alias: "order_ids", expr: (col) => col("id").arrayAgg() },
          { kind: "expr", alias: "all_fulfilled", expr: (col) => col("fulfilled").boolAnd() },
        ])
        .groupBy(["customer_id"])
        .having((col) => col("id").count().gt(1)),
    });
  },
};

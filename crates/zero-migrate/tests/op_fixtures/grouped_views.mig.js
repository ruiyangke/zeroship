import { countStar, view } from "zero-migrate";

export default {
  schema() {
    view("order_totals").create({
      as: (q) => q
        .from("orders")
        .select([
          "customer_id",
          { kind: "expr", alias: "n", expr: () => countStar() },
          { kind: "expr", alias: "revenue", expr: (col) => col("amount").sum() },
        ])
        .where((col) => col("status").eq("paid"))
        .groupBy(["customer_id"])
        .having((col) => col("id").count().gt(5)),
    });
  },
};

import { byteValue, table, t } from "zero-migrate";

export const name = "complete_dml_flow";

export default {
  up() {
    table("dml_flow_items").create({
      columns: {
        id: t.int().primaryKey(),
        label: t.text().notNull(),
        stage: t.text().notNull(),
        score: t.int().notNull(),
        payload: t.bytes().notNull(),
      },
    });

    table("dml_flow_items").insert({
      rows: [
        {
          id: 1,
          label: "alpha",
          stage: "pending",
          score: 1,
          payload: byteValue(new Uint8Array([0, 1, 0x7f, 0x80, 0xff])),
        },
        {
          id: 2,
          label: "remove-me",
          stage: "pending",
          score: 2,
          payload: byteValue(new Uint8Array([1, 2, 3])),
        },
        {
          id: 3,
          label: "gamma",
          stage: "pending",
          score: 3,
          payload: byteValue(new Uint8Array([0xde, 0xad, 0xbe, 0xef])),
        },
      ],
    });

    table("dml_flow_items").update({
      set: { stage: "ready" },
      where: (column) => column("id").eq(1),
    });

    table("dml_flow_items").delete({
      where: (column) => column("id").eq(2),
    });

    table("dml_flow_items").backfill({
      set: { score: (column) => column("score").add(100) },
      where: (column) => column("id").gt(0),
      cursorColumns: ["id"],
      cursorStability: { mode: "guardUpdates" },
      batchSize: 1,
      name: "raise_remaining_scores",
    });
  },
};

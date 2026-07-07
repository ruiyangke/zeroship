// op.* migration fixture — partitioned-table authoring surface. Covers
// createTable.partitionBy, createPartition range/default bounds, detach/drop
// partition lifecycle ops, and PG partition-index facets (BRIN/INCLUDE/WITH/ONLY).
import { table, t } from "@zeroship/migrate";

export const name = "partition";

export function up() {
  table("events").create({
    columns: {
      ts: t.timestamp(),
      tenant_id: t.text(),
    },
    partitionBy: { range: ["ts"] },
  });

  table("events").partition("events_2026_05").create({
    from: ["2026-05-01T00:00:00Z"],
    to: ["2026-06-01T00:00:00Z"],
  });
  table("events").partition("events_default").create({ default: true });

  table("events").partition("events_2026_05").detach({ concurrently: true });
  table("events").partition("events_2026_05").drop({ cascade: true });

  table("events")
    .index("events_ts_brin_idx")
    .add({
      on: ["ts"],
      using: "brin",
      include: ["tenant_id"],
      with: { pagesPerRange: 32 },
      only: true,
    });
}

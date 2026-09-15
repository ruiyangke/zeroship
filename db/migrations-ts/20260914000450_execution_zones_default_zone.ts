import { table } from "@zeroship/migrate";

// The deployment's single execution zone, split from
// 20260914000400_execution_zones_and_worker_enrollers.ts because a migration
// module is EITHER schema() (DDL only) OR data()+inverse()/irreversible (DML
// only) - never both. `schema()`'s host recorder refuses a recorded DML
// operation outright, so the seed row belongs in its own data phase.
//
// A fixed, hand-assigned id rather than a runtime-minted one: nothing in a
// migration can call the Rust base36 typed-id encoder, and a literal
// satisfying `execution_zones_id_shape` is exactly as authoritative as a
// minted one for a row this migration is the sole author of.
const DEFAULT_ZONE_ID = "ezn_default000000000000000000";

export default {
  name: "execution_zones_default_zone",
  data() {
    table("execution_zones", { schema: "zeroship" }).insert({
      rows: [{ id: DEFAULT_ZONE_ID, name: "default", status: "active" }],
    });
  },
  inverse() {
    table("execution_zones", { schema: "zeroship" }).delete({
      where: (col) => col("id").eq(DEFAULT_ZONE_ID),
    });
  },
};

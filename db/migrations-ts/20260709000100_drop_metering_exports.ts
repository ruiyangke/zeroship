import { table } from "@zeroship/migrate";

export default {
  name: "drop_metering_exports",
  schema() {
    table("metering_exports", { schema: "zeroship" }).drop({ ifExists: true, cascade: true });
  },
};

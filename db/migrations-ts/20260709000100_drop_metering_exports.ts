import { table } from "@zeroship/migrate";

export const name = "drop_metering_exports";

export function up() {
  table("metering_exports", { schema: "zeroship" }).drop({ ifExists: true, cascade: true });
}

export function down() {

}

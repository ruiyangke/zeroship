// Import-intent regression fixture. Before SynchronizeIdentity this public
// surface and canonical wire operation did not exist.
import { table } from "zero-migrate";

export const name = "synchronize_identity";

export function schema() {
  table("orders", { schema: "app" }).column("id").synchronizeIdentity({
    writesQuiesced: "orders_import_window",
  });
}

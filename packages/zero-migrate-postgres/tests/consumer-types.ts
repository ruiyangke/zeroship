// Compile this file against the package's emitted declarations. It proves the vendor
// package AUGMENTS @zeroship/migrate rather than replacing its public module surface.
import "../dist/index.js";
import {
  t,
  table,
  type VendorAttributeNamespaces,
  type VendorIndexAttributeNamespaces,
} from "@zeroship/migrate";

const tableAttributes: VendorAttributeNamespaces = {
  postgres: { fillfactor: 85 },
};
const indexAttributes: VendorIndexAttributeNamespaces = {
  postgres: { pages_per_range: 32 },
};

table("users").create({
  columns: { id: t.int() },
  postgres: { fillfactor: 85 },
});
table("users").index("users_id_idx").add({
  on: ["id"],
  postgres: { pages_per_range: 32 },
});

void tableAttributes;
void indexAttributes;

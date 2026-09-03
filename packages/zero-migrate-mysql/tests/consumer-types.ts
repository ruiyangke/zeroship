// Compile this file against the package's emitted declarations. It proves the vendor
// package AUGMENTS @zeroship/migrate rather than replacing its public module surface.
import "../dist/index.js";
import {
  t,
  table,
  type VendorAttributeNamespaces,
} from "@zeroship/migrate";

const attributes: VendorAttributeNamespaces = {
  mysql: { engine: "InnoDB", row_format: "DYNAMIC" },
};

table("users").create({
  columns: { id: t.int() },
  mysql: { engine: "InnoDB", row_format: "DYNAMIC" },
});

void attributes;

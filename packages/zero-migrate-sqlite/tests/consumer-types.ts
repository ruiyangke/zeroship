// Compile this file against the package's emitted declarations. It proves the vendor
// package AUGMENTS @zeroship/migrate rather than replacing its public module surface.
import "../dist/index.js";
import {
  t,
  table,
  type VendorAttributeNamespaces,
} from "@zeroship/migrate";

const attributes: VendorAttributeNamespaces = {
  sqlite: { strict: true, without_rowid: true },
};

table("users").create({
  columns: { id: t.int() },
  sqlite: { strict: true, without_rowid: true },
});

void attributes;

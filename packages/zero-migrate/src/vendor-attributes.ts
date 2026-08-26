// The extension point for backend-specific table options — and the ONLY thing the
// neutral package knows about them.
//
// This file names no vendor, and must not start to. `zero-migrate` is the neutral
// authoring DSL; a `postgres.fillfactor` typing in here would be the TypeScript mirror
// of the Cargo rule that forbids `zeroship-migrate-core` from depending on a vendor crate.
//
// # How a vendor gets its keys in
//
// TypeScript's declaration merging. The interface below is deliberately EMPTY. Each
// vendor's npm package augments it from its own generated typings:
//
//     // in zero-migrate-postgres
//     declare module "zero-migrate" {
//       interface VendorAttributeNamespaces {
//         postgres: { fillfactor?: number; tablespace?: string; ... };
//       }
//     }
//
// So `npm install zero-migrate-postgres` is literally what makes `postgres: {...}`
// typecheck on `create()`. Without it the key is a type error rather than a silently
// ignored option — which is the property that matters, because an attribute for a
// backend nobody registered is skipped at validate by design.
//
// The namespace key is the backend's DIALECT ID, generated from the vendor's own
// declaration. There is no hand-picked alias anywhere in the chain: the key is
// `postgres` because the dialect is `postgres`.
//
// # Why an empty interface rather than a union or a generic
//
// A union would have to enumerate the backends here — the thing this file exists to
// avoid. A generic parameter would push the choice onto every call site. An empty
// interface is the one shape that lets N independently-published packages each add a
// key without any of them, or this file, knowing the others exist.

/**
 * Backend-specific option namespaces available on the authoring surface.
 *
 * Empty here by design — each installed vendor package augments it with its own dialect
 * id as the key. With no vendor package installed this is `{}`, and every vendor key is
 * correctly rejected.
 */
// eslint-disable-next-line @typescript-eslint/no-empty-interface
export interface VendorAttributeNamespaces {}

/**
 * The vendor-attribute half of an authoring call's arguments.
 *
 * Spread into `create()`'s argument object alongside the portable keys, so a table is
 * authored as one object literal:
 *
 *     create({ columns: [...], postgres: { fillfactor: 85 } })
 *
 * rather than a chain. Every namespace is optional: a table carrying attributes for
 * several backends stays portable to all of them, and one carrying none is the
 * overwhelmingly common case.
 */
export type VendorAttributeArgs = {
  [K in keyof VendorAttributeNamespaces]?: VendorAttributeNamespaces[K];
};

/**
 * A single attribute value as it travels on the wire.
 *
 * Deliberately narrower than the IR's `IrScalar`: the shapes a vendor may declare are
 * `bool | int | enum | text`, so decimals and bytes cannot reach an attribute and are
 * not offered here.
 */
export type AttributeValue = boolean | number | string;

/**
 * Flatten authored namespaces into the wire form: `{ postgres: { fillfactor: 85 } }`
 * becomes `{ "postgres.fillfactor": 85 }`.
 *
 * Generic on purpose — it joins `${namespace}.${leaf}` and never learns a single key
 * name. That is what keeps the runtime as vendor-blind as the types: adding a backend
 * adds no branch here.
 *
 * Key order does not matter: the Rust side stores attributes in a `BTreeMap`, so the
 * canonical order used by the checksum is imposed there rather than depending on the
 * order an author happened to write.
 */
export function flattenVendorAttributes(
  args: VendorAttributeArgs,
): Record<string, AttributeValue> {
  const flat: Record<string, AttributeValue> = {};
  for (const [namespace, leaves] of Object.entries(args)) {
    if (leaves === undefined || leaves === null) continue;
    for (const [leaf, value] of Object.entries(leaves as Record<string, unknown>)) {
      // `undefined` means "not set", which must not become a present key carrying null.
      if (value === undefined) continue;
      flat[`${namespace}.${leaf}`] = value as AttributeValue;
    }
  }
  return flat;
}

/**
 * Backend-specific option namespaces available on an INDEX.
 *
 * A second, separate map rather than a nested key inside
 * {@link VendorAttributeNamespaces}, because the two are augmented independently and a
 * vendor may declare index options without table options or the reverse. PostgreSQL is
 * the first with both: `fillfactor` is legal on a table AND on an index, declared twice
 * because declaration identity is the (key, scope) pair.
 *
 * Empty here by design, for the same reason as its table sibling — this package names no
 * vendor.
 */
// eslint-disable-next-line @typescript-eslint/no-empty-interface
export interface VendorIndexAttributeNamespaces {}

/**
 * The vendor-attribute half of an index-authoring call's arguments.
 */
export type VendorIndexAttributeArgs = {
  [K in keyof VendorIndexAttributeNamespaces]?: VendorIndexAttributeNamespaces[K];
};

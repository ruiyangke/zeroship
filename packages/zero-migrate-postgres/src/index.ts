// Vendor attributes for the `postgres` backend.
//
// This package exists so the neutral `@zeroship/migrate` package can stay neutral. The
// authoring DSL knows there is an extension point; it does not know that `postgres` fills
// one. Installing this package is what supplies that knowledge, via TypeScript
// declaration merging into `VendorAttributeNamespaces`.
//
//     import "zero-migrate-postgres";
//
//     create({
//       columns: [...],
//       postgres: { fillfactor: 85, autovacuum_enabled: false },
//     });
//
// Without the import, `postgres:` is a type error rather than a silently ignored key —
// which matters, because at validate an attribute for an unregistered backend is
// SKIPPED by design, so a runtime-only mistake would be invisible.
//
// The module has no runtime surface of its own: the flattening from `postgres: {…}` to the
// wire key `postgres.fillfactor` is done generically by the neutral package, which joins
// `${namespace}.${leaf}` and never learns a key name.
import "./generated/attributes.js";

export {};

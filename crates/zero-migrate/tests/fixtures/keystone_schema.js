// Keystone-schema fixture. A schema authored DECLARATIVELY,
// exercising the facets the author->generate->fold chain must
// round-trip LOSSLESSLY: type matrix, ref (Id brand), vector + metric, and encrypted.
// CHECK-borne enum/min/max facets are handled once the Expr->SQL
// renderer lands.
import { dbType as internalDbType } from "zero-migrate";

const users = {
  // Internal platform id. Its wire value is the engine's base62 UUIDv7 format,
  // not a public TypeID; this local compatibility builder does not retain prefixes.
  id: internalDbType.id(),
  email: internalDbType.string().required().unique(),
  age: internalDbType.number(),
  role: internalDbType.string(),
  active: internalDbType.boolean(),
  // A DEFAULT-mode encrypted column. `internalDbType.encrypted()`
  // stamps `encrypted: { mode:"randomised", keyId:"default", wraps:"string" }` AND a
  // fail-safe auto-mask `{ kind:"full", classification:"pii" }`; the author->generate
  // ->fold chain must recover BOTH byte-identically.
  token: internalDbType.encrypted(),
};

const docs = {
  title: internalDbType.string().required(),
  body: internalDbType.json(),
  createdAt: internalDbType.timestamp(),
  // FK -> users; the ref brand recovered from the FK constraint.
  authorId: internalDbType.ref("users", { onDelete: "cascade" }),
  // Vector + metric — the metric is the other DECLARED-ONLY carried facet.
  embedding: internalDbType.vector(1536, { metric: "innerProduct" }),
};

export default {
  schema: { users, docs },
};

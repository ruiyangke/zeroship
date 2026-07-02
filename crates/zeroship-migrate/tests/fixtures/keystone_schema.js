// Keystone fixture (Migration-first P2b §6b). A schema authored DECLARATIVELY,
// exercising the P0-supported facets the author->generate->fold chain must
// round-trip LOSSLESSLY: type matrix, ref (Id brand), vector + metric, encrypted,
// and id-prefix. CHECK-borne enum/min/max facets are P1 once the Expr->SQL
// renderer lands.
import { t } from "@zeroship/db";

const users = {
  // Typed-id with an explicit prefix — a DECLARED-ONLY facet (idPrefix) that DB
  // introspection cannot recover; carried on the IR column.
  id: t.id("acct"),
  email: t.string().required().unique(),
  age: t.number(),
  role: t.string(),
  active: t.boolean(),
  // A DEFAULT-mode encrypted column — the §6 keystone goodie. `t.encrypted()`
  // stamps `encrypted: { mode:"randomised", keyId:"default", wraps:"string" }` AND a
  // fail-safe auto-mask `{ kind:"full", classification:"pii" }`; the author->generate
  // ->fold chain must recover BOTH byte-identically (HIGH-1).
  token: t.encrypted(),
};

const docs = {
  title: t.string().required(),
  body: t.json(),
  createdAt: t.timestamp(),
  // FK -> users; the ref brand recovered from the FK constraint.
  authorId: t.ref("users", { onDelete: "cascade" }),
  // Vector + metric — the metric is the other DECLARED-ONLY carried facet.
  embedding: t.vector(1536, { metric: "innerProduct" }),
};

export default {
  schema: { users, docs },
};

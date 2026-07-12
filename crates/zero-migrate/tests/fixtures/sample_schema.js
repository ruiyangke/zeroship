// Sample creator schema.js for the JS-front-end tests. Uses the REAL
// `zero-migrate` `t.*` DSL (resolved to the embedded dist by the eval graph),
// exercising the full-capability facets: encrypted (auto-masked), vector, an
// explicit mask, and a ref.
import { t } from "zero-migrate";

const users = {
  email: t.string().required().unique(),
  // Encrypted (deterministic so .unique() is coherent); auto-masks PII.
  ssn: t.encrypted({ mode: "deterministic", keyId: "pii_key" }),
  // Explicit mask on a plain string.
  phone: t.string().mask({ kind: "last4", classification: "pii" }),
  age: t.number(),
};

const docs = {
  title: t.string().required(),
  // 1536-dim cosine vector (OpenAI-style embedding).
  embedding: t.vector(1536, { metric: "cosine" }),
  // FK to users with cascade delete.
  authorId: t.ref("users", { onDelete: "cascade" }),
};

export default {
  schema: { users, docs },
};

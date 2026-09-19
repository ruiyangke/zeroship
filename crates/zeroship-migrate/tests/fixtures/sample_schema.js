// Sample runtime schema.js for the JS-front-end tests. Uses the legacy internal
// `zero-migrate` db-type DSL (resolved to the embedded dist by the eval graph),
// exercising the full-capability facets: encrypted (auto-masked), vector, an
// explicit mask, and a ref.
import { dbType } from "@zeroship/migrate";

const users = {
  email: dbType.string().required().unique(),
  // Encrypted; auto-masks PII.
  ssn: dbType.encrypted(),
  // Explicit mask on a plain string.
  phone: dbType.string().mask({ kind: "last4", classification: "pii" }),
  age: dbType.double(),
};

const docs = {
  title: dbType.string().required(),
  // 1536-dim cosine vector (OpenAI-style embedding).
  embedding: dbType.vector(1536, { metric: "cosine" }),
  // Legacy runtime-schema FK to users with cascade delete. Migration schemas use
  // an explicit local type plus `.references("users", "id", ...)` instead.
  authorId: dbType.ref("users", { onDelete: "cascade" }),
};

export default {
  schema: { users, docs },
};

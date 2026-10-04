import { table } from "../../../../packages/zero-migrate/dist/index.js";

// Delivery authority is fenced by the logical job, enrolled worker and attempt.
// The journal task records the worker and delivery attempt that opened it.
export function up(namespace) {
  table("tasks", { schema: namespace }).column("assignment_revision").drop();
  return { identifiers: new Set(), columns: new Set() };
}

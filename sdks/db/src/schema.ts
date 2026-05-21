/**
 * `NormalizedSchema` — canonical post-normalisation field map consumed
 * by `Collection`'s validation and CRUD path.
 *
 * Stage 7 of the refactor moved `normalizeSchema`, `expandUnionToFlatColumns`,
 * and `validateRefTargets` into `@zeroship/bootstrap/install-schema`
 * (the framework-internal coordination package). Only the type alias
 * stays here — `Collection` and `validate.ts` reference it.
 */
import type { FieldDef } from "./types.js";

/** A normalized schema mapping field names to their FieldDef. */
export type NormalizedSchema = Record<string, FieldDef>;

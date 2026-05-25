import {
  requireBoundNativeCapability,
  type NativeCollection,
} from "../native.js";
import type { Result, Row, Actor } from "../types.js";

export interface MaskingCollectionInternals<S> {
  _run<T>(fn: () => Promise<T>): Promise<Result<T>>;
  _nativeCollection(): NativeCollection;
  _toColumn(field: string): string;
  _toField(column: string): string;
}

/**
 * **P5.5 PR 7** — bulk unmask a set of (id, columns) pairs in one
 * V8↔Rust round-trip.
 *
 * Routes through `env.db.bulkUnmaskFields`. Authorisation is
 * **atomic**: a single denied (id, column) pair rejects the WHOLE
 * call with `BULK_UNMASK_PARTIAL_UNAUTHORIZED`. On success the
 * resolved map carries plaintext for every requested pair.
 */
export function bulkUnmaskCollection<S>(
  self: MaskingCollectionInternals<S>,
  items: ReadonlyArray<{
    id: string;
    columns: readonly (string & keyof Row<S>)[];
  }>,
  opts: { actor: Actor; reason?: string },
): Promise<Result<Map<string, Record<string, unknown>>>> {
  return self._run(async () => {
    const bulkUnmask = requireBoundNativeCapability(
      self._nativeCollection(),
      "bulkUnmask",
      {
        code: "BULK_UNMASK_NOT_AVAILABLE",
        message:
          "@zeroship/db: Collection.bulkUnmask not available — " +
          "runtime is missing the P9 PR 2 bulk unmask surface.",
      },
    );
    const wireItems = items.map((it) => ({
      rowPk: String(it.id),
      columns: it.columns.map((c) => self._toColumn(c as string)),
    }));
    const result = await bulkUnmask(wireItems, {
      actor: opts.actor,
      reason: opts.reason,
    });
    const out = new Map<string, Record<string, unknown>>();
    for (const [rowPk, cols] of Object.entries(result.results ?? {})) {
      const mapped: Record<string, unknown> = {};
      for (const [col, plaintext] of Object.entries(cols)) {
        mapped[self._toField(col)] = plaintext;
      }
      out.set(rowPk, mapped);
    }
    return out;
  });
}
